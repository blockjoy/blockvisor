//! Here we have the code related to the metrics for nodes. We

use crate::node::BabelEngine;
use crate::node_state::VmStatus;
use crate::nodes_manager::{MaybeNode, NodesManager};
use crate::pal::{NodeConnection, Pal};
use crate::services::api::{common, pb};
use babel_api::plugin::NodeHealth;
use babel_api::{
    engine::{JobStatus, JobsInfo},
    plugin::ProtocolStatus,
};
use eyre::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use tracing::warn;

/// The interval by which we collect metrics from each of the nodes.
pub const COLLECT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
/// The max duration we will wait for a node to return a metric.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// Type alias for a uuid that is the id of a node.
type NodeId = uuid::Uuid;

/// The metrics for a group of nodes.
#[derive(Serialize)]
pub struct Metrics(HashMap<NodeId, Metric>);

/// The metrics for a single node.
#[derive(Serialize, Deserialize, Debug)]
pub struct Metric {
    pub height: Option<u64>,
    pub block_age: Option<u64>,
    pub consensus: Option<bool>,
    pub protocol_status: Option<ProtocolStatus>,
    pub jobs: JobsInfo,
}

impl Metrics {
    pub fn has_any(&self) -> bool {
        self.0.values().any(|m| {
            m.height.is_some()
                || m.block_age.is_some()
                || m.consensus.is_some()
                || m.protocol_status.is_some()
                || !m.jobs.is_empty()
        })
    }
}

impl Deref for Metrics {
    type Target = HashMap<NodeId, Metric>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Metrics {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Given a list of nodes, returns for each node their metric. It does this concurrently for each
/// running node, but queries the different metrics sequentially for a given node. Normally this would not
/// be efficient, but since we are dealing with a virtual socket the latency is very low, in the
/// hundres of nanoseconds. Furthermore, we require unique access to the node to query a metric, so
/// sequentially is easier to program.
pub async fn collect_metrics<P>(nodes_manager: Arc<NodesManager<P>>) -> Metrics
where
    P: Pal + Send + Sync + Debug + 'static,
    P::NodeConnection: Send + Sync,
    P::ApiServiceConnector: Send + Sync,
    P::VirtualMachine: Send + Sync,
    P::RecoveryBackoff: Send + Sync + 'static,
{
    let nodes_lock = nodes_manager.nodes_list().await;
    let metrics_fut: Vec<_> = nodes_lock
        .values()
        .map(|n| async move {
            if let MaybeNode::Node(n) = n {
                match n.try_write() {
                    Err(_) => None,
                    Ok(mut node) => {
                        if node.status().await == VmStatus::Running && !node.state.dev_mode {
                            collect_metric(&mut node.babel_engine)
                                .await
                                .map(|metric| (node.id(), metric))
                        } else {
                            // don't collect metrics for not running or dev nodes
                            None
                        }
                    }
                }
            } else {
                None
            }
        })
        .collect();
    let metrics: Vec<_> = futures_util::future::join_all(metrics_fut).await;
    Metrics(metrics.into_iter().flatten().collect())
}

/// Returns the metric for a single node.
pub async fn collect_metric<N: NodeConnection>(
    babel_engine: &mut BabelEngine<N>,
) -> Option<Metric> {
    let protocol_status = timeout(babel_engine.protocol_status()).await.ok();
    match &protocol_status {
        None => {
            let jobs = timeout(babel_engine.get_jobs())
                .await
                .ok()
                .unwrap_or_default();

            Some(Metric {
                height: None,
                block_age: None,
                consensus: None,
                protocol_status,
                jobs,
            })
        }
        Some(ProtocolStatus { state, .. })
            if state == babel_api::plugin_config::DOWNLOADING_STATE_NAME
                || state == babel_api::plugin_config::UPLOADING_STATE_NAME =>
        {
            let jobs = timeout(babel_engine.get_jobs())
                .await
                .ok()
                .unwrap_or_default();

            Some(Metric {
                height: None,
                block_age: None,
                consensus: None,
                protocol_status,
                jobs,
            })
        }
        Some(ProtocolStatus { state, .. })
            if state == babel_api::plugin_config::STARTING_STATE_NAME =>
        {
            None
        }
        _ => {
            let height = match babel_engine.has_capability("height") {
                true => timeout(babel_engine.height()).await.ok(),
                false => None,
            };
            let block_age = match babel_engine.has_capability("block_age") {
                true => timeout(babel_engine.block_age()).await.ok(),
                false => None,
            };
            let consensus = match babel_engine.has_capability("consensus") {
                true => timeout(babel_engine.consensus()).await.ok(),
                false => None,
            };
            let jobs = timeout(babel_engine.get_jobs())
                .await
                .ok()
                .unwrap_or_default();

            Some(Metric {
                // these could be optional
                height,
                block_age,
                consensus,
                // these are expected in every chain
                protocol_status,
                jobs,
            })
        }
    }
}

async fn timeout<F, T>(fut: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    match tokio::time::timeout(TIMEOUT, fut).await {
        Ok(Ok(res)) => Ok(res),
        Ok(Err(e)) => {
            warn!("Collecting node metric failed! `{e:#}`");
            Err(e)
        }
        Err(e) => {
            warn!("Collecting node metric timed out! `{e:#}`");
            Err(e.into())
        }
    }
}

/// Here is how we convert the metrics we aggregated to their representation that we use over gRPC.
/// Note that even though this may fail, i.e. the protocol_status may not be
/// something we can make sense of, we still provide an infallible `From` implementation (not
/// `TryFrom`). This is because if the node goes to the sad place, it may start sending malformed
/// responses (think instead of block_height: 3 it will send block_height: aaaaaaaaaaaa). Even when
/// this is the case we still want to send as many metrics to the api as possible, so after duly
/// logging them, we ignore these failures.
impl From<Metrics> for pb::MetricsServiceNodeRequest {
    fn from(metrics: Metrics) -> Self {
        let metrics = metrics
            .0
            .into_iter()
            .map(|(k, v)| {
                let jobs = v
                    .jobs
                    .into_iter()
                    .map(|(name, info)| {
                        let (status, exit_code, message) = match info.status {
                            JobStatus::Pending { .. } => {
                                (common::NodeJobStatus::Pending, None, None)
                            }
                            JobStatus::Running => (common::NodeJobStatus::Running, None, None),
                            JobStatus::Finished { exit_code, message } => (
                                if exit_code == Some(0) {
                                    common::NodeJobStatus::Finished
                                } else {
                                    common::NodeJobStatus::Failed
                                },
                                exit_code,
                                Some(message),
                            ),
                            JobStatus::Stopped => (common::NodeJobStatus::Stopped, None, None),
                        };
                        common::NodeJob {
                            name,
                            status: status.into(),
                            exit_code,
                            message,
                            logs: info.logs,
                            restarts: info.restart_count as u64,
                            progress: info.progress.map(|progress| common::NodeJobProgress {
                                total: Some(progress.total),
                                current: Some(progress.current),
                                message: Some(progress.message),
                            }),
                        }
                    })
                    .collect();
                let metrics = pb::NodeMetrics {
                    height: v.height,
                    block_age: v.block_age,
                    consensus: v.consensus,
                    node_status: v
                        .protocol_status
                        .map(|protocol_status| protocol_status.into()),
                    jobs,
                };
                (k.to_string(), metrics)
            })
            .collect();
        Self { metrics }
    }
}

impl From<ProtocolStatus> for common::NodeStatus {
    fn from(value: ProtocolStatus) -> Self {
        let health: common::NodeHealth = value.health.into();
        Self {
            state: common::NodeState::Running.into(),
            next: None,
            protocol: Some(common::ProtocolStatus {
                state: value.state,
                health: health.into(),
            }),
        }
    }
}

impl From<NodeHealth> for common::NodeHealth {
    fn from(value: NodeHealth) -> Self {
        match value {
            NodeHealth::Unhealthy => common::NodeHealth::Unhealthy,
            NodeHealth::Neutral => common::NodeHealth::Neutral,
            NodeHealth::Healthy => common::NodeHealth::Healthy,
        }
    }
}

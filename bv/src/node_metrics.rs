//! Here we have the code related to the metrics for nodes. We

use crate::babel_engine::BabelEngine;
use crate::node_data::NodeStatus;
use crate::nodes::Nodes;
use crate::pal::Pal;
use crate::services::api::pb;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use tracing::warn;

/// The interval by which we collect metrics from each of the nodes.
pub const COLLECT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// The max duration we will wait for a node to return a metric.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Type alias for a uuid that is the id of a node.
type NodeId = uuid::Uuid;

/// The metrics for a group of nodes.
#[derive(serde::Serialize)]
pub struct Metrics(HashMap<NodeId, Metric>);

/// The metrics for a single node.
#[derive(serde::Serialize)]
pub struct Metric {
    pub height: Option<u64>,
    pub block_age: Option<u64>,
    pub staking_status: Option<String>,
    pub consensus: Option<bool>,
    pub application_status: Option<String>,
    pub sync_status: Option<String>,
}

/// Given a list of nodes, returns for each node their metric. It does this concurrently for each
/// running node, but queries the different metrics sequentially for a given node. Normally this would not
/// be efficient, but since we are dealing with a virtual socket the latency is very low, in the
/// hundres of nanoseconds. Furthermore, we require unique access to the node to query a metric, so
/// sequentially is easier to program.
pub async fn collect_metrics<P: Pal + Debug + 'static>(nodes: Arc<Nodes<P>>) -> Metrics {
    let nodes_lock = nodes.nodes.read().await;
    let metrics_fut: Vec<_> = nodes_lock
        .values()
        .map(|n| async {
            let mut node = n.write().await;
            if node.status() == NodeStatus::Running {
                None
            } else {
                Some((node.id(), collect_metric(&mut node.babel_engine).await))
            }
        })
        .collect();
    let metrics: Vec<_> = futures_util::future::join_all(metrics_fut).await;
    Metrics(metrics.into_iter().flatten().collect())
}

/// Returns the metric for a single node.
pub async fn collect_metric(babel_engine: &mut BabelEngine) -> Metric {
    use babel_api::BabelMethod;

    let capabilities = babel_engine.capabilities();
    let height = match capabilities.contains(&BabelMethod::Height.to_string()) {
        true => timeout(babel_engine.height()).await.ok(),
        false => None,
    };
    let block_age = match capabilities.contains(&BabelMethod::BlockAge.to_string()) {
        true => timeout(babel_engine.block_age()).await.ok(),
        false => None,
    };
    let staking_status = match capabilities.contains(&BabelMethod::StakingStatus.to_string()) {
        true => timeout(babel_engine.staking_status()).await.ok(),
        false => None,
    };
    let consensus = match capabilities.contains(&BabelMethod::Consensus.to_string()) {
        true => timeout(babel_engine.consensus()).await.ok(),
        false => None,
    };
    let sync_status = match capabilities.contains(&BabelMethod::SyncStatus.to_string()) {
        true => timeout(babel_engine.sync_status()).await.ok(),
        false => None,
    };

    Metric {
        // these could be optional
        height,
        block_age,
        staking_status,
        consensus,
        sync_status,
        // these are expected in every chain
        application_status: timeout(babel_engine.application_status()).await.ok(),
    }
}

async fn timeout<F, T>(fut: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    match tokio::time::timeout(TIMEOUT, fut).await {
        Ok(Ok(res)) => Ok(res),
        Ok(Err(e)) => {
            warn!("Collecting node metric failed! `{e}`");
            Err(e)
        }
        Err(e) => {
            warn!("Collecting node metric timed out! `{e}`");
            Err(e.into())
        }
    }
}

/// Here is how we convert the metrics we aggregated to their representation that we use over gRPC.
/// Note that even though this may fail, i.e. the application_status or sync_status may not be
/// something we can make sense of, we still provide an infallible `From` implementation (not
/// `TryFrom`). This is because if the node goes to the sad place, it may start sending malformed
/// responses (think instead of block_height: 3 it will send block_height: aaaaaaaaaaaa). Even when
/// this is the case we still want to send as many metrics to the api as possible, so after duly
/// logging them, we ignore these failures.
impl From<Metrics> for pb::NodeMetricsRequest {
    fn from(metrics: Metrics) -> Self {
        use crate::services::api::pb::node_info::{ApplicationStatus, StakingStatus, SyncStatus};

        let metrics = metrics
            .0
            .into_iter()
            .map(|(k, v)| {
                let application_status = v.application_status.and_then(|s| {
                    ApplicationStatus::from_str_name(&s)
                        .ok_or_else(|| warn!("Could not parse `{s}` as application status"))
                        .ok()
                });
                let sync_status = v.sync_status.and_then(|s| {
                    SyncStatus::from_str_name(&s)
                        .ok_or_else(|| warn!("Could not parse `{s}` as sync status"))
                        .ok()
                });
                let staking_status = v.staking_status.and_then(|s| {
                    StakingStatus::from_str_name(&s)
                        .ok_or_else(|| warn!("Could not parse `{s}` as staking status"))
                        .ok()
                });

                let metrics = pb::NodeMetrics {
                    height: v.height,
                    block_age: v.block_age,
                    staking_status: staking_status.map(Into::into),
                    consensus: v.consensus,
                    application_status: application_status.map(Into::into),
                    sync_status: sync_status.map(Into::into),
                };
                (k.to_string(), metrics)
            })
            .collect();
        Self { metrics }
    }
}

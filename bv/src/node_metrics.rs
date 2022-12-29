//! Here we have the code related to the metrics for nodes. We

use crate::node;
use crate::node_data::NodeStatus;
use crate::services::api::pb;
use std::collections::HashMap;
use tracing::warn;

/// The interval by which we collect metrics from each of the nodes.
pub const COLLECT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// The max duration we will wait for a node to return a metric.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

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
    pub staking_status: Option<i32>,
    pub consensus: Option<bool>,
}

/// Given a list of nodes, returns for each node their metric. It does this concurrently for each
/// running node, but queries the different metrics sequentially for a given node. Normally this would not
/// be efficient, but since we are dealing with a virtual socket the latency is very low, in the
/// hundres of nanoseconds. Furthermore, we require unique access to the node to query a metric, so
/// sequentially is easier to program.
pub async fn collect_metrics(nodes: impl Iterator<Item = &mut node::Node>) -> Metrics {
    let metrics_fut: Vec<_> = nodes
        .filter(|n| n.status() == NodeStatus::Running)
        .map(collect_metric)
        .collect();
    let metrics: Vec<_> = futures_util::future::join_all(metrics_fut).await;
    Metrics(metrics.into_iter().collect())
}

/// Returns the metric for a single node.
pub async fn collect_metric(node: &mut node::Node) -> (NodeId, Metric) {
    let metric = Metric {
        height: timeout(node.height()).await.ok(),
        block_age: timeout(node.block_age()).await.ok(),
        staking_status: timeout(node.stake_status()).await.ok(),
        consensus: timeout(node.consensus()).await.ok(),
    };
    (node.id(), metric)
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

impl From<Metrics> for pb::NodeMetricsRequest {
    fn from(metrics: Metrics) -> Self {
        let metrics = metrics
            .0
            .into_iter()
            .map(|(k, v)| {
                let metrics = pb::NodeMetrics {
                    height: v.height,
                    block_age: v.block_age,
                    staking_status: v.staking_status,
                    consensus: v.consensus,
                };
                (k.to_string(), metrics)
            })
            .collect();
        Self { metrics }
    }
}

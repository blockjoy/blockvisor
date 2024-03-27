use crate::config::Config;
use bv_utils::system::get_ip_address;
use chitchat::{
    spawn_chitchat, transport::UdpTransport, Chitchat, ChitchatConfig, ChitchatId,
    FailureDetectorConfig,
};
use eyre::{anyhow, Result};
use std::sync::Arc;
use tokio::{sync::Mutex, time::Duration};

const DEFAULT_GOSSIP_PORT: u32 = 1000;

pub struct ClusterData {
    pub chitchat: Arc<Mutex<Chitchat>>,
}

pub async fn start_server(config: &Config) -> Result<Option<ClusterData>> {
    if let Some(cluster_id) = &config.cluster_id {
        let port = config.cluster_port.unwrap_or(DEFAULT_GOSSIP_PORT);
        let seed_nodes = config
            .cluster_seed_urls
            .clone()
            .ok_or_else(|| anyhow!("cluster_seed_urls not set"))?;
        let listen_addr = format!("0.0.0.0:{port}");
        let public_addr = format!("{}:{port}", get_ip_address(&config.iface)?);
        let chitchat_id = ChitchatId::new(config.id.clone(), 0, public_addr.parse()?);

        let chitchat_config = ChitchatConfig {
            cluster_id: cluster_id.clone(),
            chitchat_id,
            gossip_interval: Duration::from_secs(1),
            listen_addr: listen_addr.parse()?,
            seed_nodes,
            failure_detector_config: FailureDetectorConfig {
                // host will be marked as dead quickly
                phi_threshold: 8.0,
                // but it will take ~100 sec to get confidence it's alive after it's back
                sampling_window_size: 100,
                // wait period for heartbeets
                max_interval: Duration::from_secs(10),
                // frequency of heartbeets
                initial_interval: Duration::from_secs(5),
                // host will be auto-removed after this period of being dead
                // it still can be auto rejoined if other hosts have it
                // in the list of seeds
                dead_node_grace_period: Duration::from_secs(300),
            },
            // TODO: what's that?
            marked_for_deletion_grace_period: 10_000,
        };
        let chitchat_handler = spawn_chitchat(chitchat_config, Vec::new(), &UdpTransport)
            .await
            .map_err(|e| anyhow!(e))?;
        let chitchat = chitchat_handler.chitchat();

        Ok(Some(ClusterData { chitchat }))
    } else {
        Ok(None)
    }
}

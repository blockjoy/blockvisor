use crate::{bv_context::BvContext, node_state::NodeState};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::fs;
use tracing::warn;

pub const NODE_ENV_FILE_PATH: &str = "var/lib/babel/node_env";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NodeEnv {
    node_id: String,
    node_name: String,
    node_version: String,
    protocol: String,
    node_type: String,
    ip: String,
    gateway: String,
    standalone: bool,
    bv_id: String,
    bv_name: String,
    bv_api_url: String,
    org_id: String,
}

impl NodeEnv {
    pub fn new(bv_context: &BvContext, node_state: &NodeState) -> Self {
        Self {
            node_id: node_state.id.to_string(),
            node_name: node_state.name.clone(),
            node_type: node_state.image.node_type.clone(),
            protocol: node_state.image.protocol.clone(),
            node_version: node_state.image.node_version.clone(),
            ip: node_state.network_interface.ip.to_string(),
            gateway: node_state.network_interface.gateway.to_string(),
            standalone: node_state.standalone,
            bv_id: bv_context.id.clone(),
            bv_name: bv_context.name.clone(),
            bv_api_url: bv_context.url.clone(),
            org_id: node_state.org_id.clone(),
        }
    }

    pub async fn save(&self, babel_root: &Path) -> eyre::Result<()> {
        let mut node_env = format!(
            "BV_HOST_ID=\"{}\"\n\
             BV_HOST_NAME=\"{}\"\n\
             BV_API_URL=\"{}\"\n\
             NODE_ID=\"{}\"\n\
             NODE_NAME=\"{}\"\n\
             NODE_TYPE=\"{}\"\n\
             ORG_ID=\"{}\"\n\
             BLOCKCHAIN_TYPE=\"{}\"\n\
             NODE_VERSION=\"{}\"\n\
             NODE_IP=\"{}\"\n\
             NODE_GATEWAY=\"{}\"\n\
             NODE_STANDALONE=\"{}\"\n",
            self.bv_id,
            self.bv_name,
            self.bv_api_url,
            self.node_id,
            self.node_name,
            self.node_type,
            self.org_id,
            self.protocol,
            self.node_version,
            self.ip,
            self.gateway,
            self.standalone
        );
        if let Ok(rust_log) = std::env::var("RUST_LOG") {
            node_env.push_str(&format!("RUST_LOG=\"{rust_log}\"\n"))
        }
        if let Err(err) = fs::write(babel_root.join(NODE_ENV_FILE_PATH), node_env).await {
            warn!("failed to write node_env file: {err:#}");
        }
        Ok(())
    }
}

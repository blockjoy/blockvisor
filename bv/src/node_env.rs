use crate::{bv_context::BvContext, node_state::NodeState};
use babel_api::engine::NodeEnv;
use std::path::Path;
use tokio::fs;
use tracing::warn;

pub const NODE_ENV_FILE_PATH: &str = "var/lib/babel/node_env";

pub fn new(bv_context: &BvContext, node_state: &NodeState) -> NodeEnv {
    NodeEnv {
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

pub async fn save(env: &NodeEnv, babel_root: &Path) -> eyre::Result<()> {
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
        env.bv_id,
        env.bv_name,
        env.bv_api_url,
        env.node_id,
        env.node_name,
        env.node_type,
        env.org_id,
        env.protocol,
        env.node_version,
        env.ip,
        env.gateway,
        env.standalone
    );
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        node_env.push_str(&format!("RUST_LOG=\"{rust_log}\"\n"))
    }
    if let Err(err) = fs::write(babel_root.join(NODE_ENV_FILE_PATH), node_env).await {
        warn!("failed to write node_env file: {err:#}");
    }
    Ok(())
}

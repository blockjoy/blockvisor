use crate::{bv_context::BvContext, node_state::NodeState};
use babel_api::engine::NodeEnv;
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::warn;

pub const NODE_ENV_FILE_PATH: &str = "var/lib/babel/node_env";

pub fn new(
    bv_context: &BvContext,
    node_state: &NodeState,
    data_mount_point: PathBuf,
    protocol_data_path: PathBuf,
) -> NodeEnv {
    NodeEnv {
        node_id: node_state.id.to_string(),
        node_name: node_state.name.clone(),
        node_version: node_state.image.version.clone(),
        node_protocol: node_state.image_key.protocol_key.clone(),
        node_variant: node_state.image_key.variant_key.clone(),
        node_ip: node_state.ip.to_string(),
        node_gateway: node_state.gateway.to_string(),
        dev_mode: node_state.dev_mode,
        bv_host_id: bv_context.id.clone(),
        bv_host_name: bv_context.name.clone(),
        bv_api_url: bv_context.url.clone(),
        org_id: node_state.org_id.clone(),
        data_mount_point,
        protocol_data_path,
    }
}

pub async fn save(env: &NodeEnv, babel_root: &Path) -> eyre::Result<()> {
    let mut node_env = format!(
        "NODE_ID=\"{}\"\n\
         NODE_NAME=\"{}\"\n\
         NODE_VERSION=\"{}\"\n\
         NODE_PROTOCOL=\"{}\"\n\
         NODE_VARIANT=\"{}\"\n\
         NODE_IP=\"{}\"\n\
         NODE_GATEWAY=\"{}\"\n\
         DEV_MODE=\"{}\"\n\
         BV_HOST_ID=\"{}\"\n\
         BV_HOST_NAME=\"{}\"\n\
         BV_API_URL=\"{}\"\n\
         ORG_ID=\"{}\"\n\
         DATA_MOUNT_POINT=\"{}\"\n\
         PROTOCOL_DATA_PATH=\"{}\"\n",
        env.node_id,
        env.node_name,
        env.node_version,
        env.node_protocol,
        env.node_variant,
        env.node_ip,
        env.node_gateway,
        env.dev_mode,
        env.bv_host_id,
        env.bv_host_name,
        env.bv_api_url,
        env.org_id,
        env.data_mount_point.display(),
        env.protocol_data_path.display(),
    );
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        node_env.push_str(&format!("RUST_LOG=\"{rust_log}\"\n"))
    }
    if let Err(err) = fs::write(babel_root.join(NODE_ENV_FILE_PATH), node_env).await {
        warn!("failed to write node_env file: {err:#}");
    }
    Ok(())
}

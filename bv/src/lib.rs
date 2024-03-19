pub mod babel_engine;
pub mod babel_engine_service;
mod bare_machine;
pub mod blockvisord;
pub mod bv;
pub mod cli;
pub mod cluster;
pub mod commands;
pub mod config;
pub mod firecracker_machine;
pub mod hosts;
pub mod installer;
pub mod internal_server;
pub mod linux_bare_platform;
pub mod linux_fc_platform;
pub mod linux_platform;
pub mod node;
pub mod node_connection;
pub mod node_context;
pub mod node_data;
pub mod node_metrics;
pub mod nodes_manager;
pub mod pal;
pub mod pal_config;
pub mod pretty_table;
pub mod self_updater;
pub mod services;
pub mod utils;
pub mod workspace;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

pub const BV_VAR_PATH: &str = "var/lib/blockvisor";

#[derive(PartialEq, Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ServiceStatus {
    Undefined,
    Ok,
    Updating,
    Broken,
}

lazy_static::lazy_static! {
    pub static ref BV_STATUS: RwLock<ServiceStatus> = RwLock::new(ServiceStatus::Undefined);
}

pub async fn set_bv_status(value: ServiceStatus) {
    let mut status = crate::BV_STATUS.write().await;
    *status = value;
}

pub async fn try_set_bv_status(value: ServiceStatus) {
    let mut bv_status = crate::BV_STATUS.write().await;
    if *bv_status != ServiceStatus::Broken {
        *bv_status = value;
    }
}

pub async fn get_bv_status() -> ServiceStatus {
    *crate::BV_STATUS.read().await
}

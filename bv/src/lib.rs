pub mod api_config;
pub mod apptainer_machine;
pub mod apptainer_platform;
pub mod babel_engine;
pub mod babel_engine_service;
pub mod blockvisord;
pub mod bv;
pub mod bv_cli;
pub mod bv_config;
pub mod bv_context;
pub mod cluster;
pub mod commands;
mod cpu_registry;
pub mod firewall;
pub mod hosts;
pub mod installer;
pub mod internal_server;
pub mod linux_platform;
pub mod nib;
pub mod nib_cli;
pub mod nib_config;
pub mod nib_meta;
pub mod node;
pub mod node_context;
pub mod node_env;
pub mod node_metrics;
pub mod node_state;
pub mod nodes_manager;
pub mod pal;
pub mod pretty_table;
pub mod scheduler;
pub mod self_updater;
pub mod services;
pub mod ufw_wrapper;
pub mod utils;

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

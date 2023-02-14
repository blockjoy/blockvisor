extern crate core;

pub mod cli;
pub mod config;
pub mod hosts;
pub mod installer;
pub mod linux_platform;
pub mod logging;
pub mod node;
pub mod node_connection;
pub mod node_data;
pub mod node_metrics;
pub mod nodes;
pub mod pal;
pub mod pretty_table;
pub mod render;
pub mod self_updater;
pub mod server;
pub mod services;
pub mod utils;

use crate::server::bv_pb;
use tokio::sync::RwLock;

pub const BV_VAR_PATH: &str = "var/lib/blockvisor";

lazy_static::lazy_static! {
    pub static ref BV_STATUS: RwLock<server::bv_pb::ServiceStatus> = RwLock::new(server::bv_pb::ServiceStatus::UndefinedServiceStatus);
}

pub async fn set_bv_status(value: bv_pb::ServiceStatus) {
    let mut status = crate::BV_STATUS.write().await;
    *status = value;
}

pub async fn try_set_bv_status(value: bv_pb::ServiceStatus) {
    let mut bv_status = crate::BV_STATUS.write().await;
    if *bv_status != bv_pb::ServiceStatus::Broken {
        *bv_status = value;
    }
}

pub async fn get_bv_status() -> bv_pb::ServiceStatus {
    *crate::BV_STATUS.read().await
}

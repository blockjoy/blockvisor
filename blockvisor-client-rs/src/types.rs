use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct HostCreateRequest {
    pub org_id: Option<Uuid>,
    pub name: String,
    pub version: Option<String>,
    pub location: Option<String>,
    pub cpu_count: Option<i64>,
    pub mem_size: Option<i64>,
    pub disk_size: Option<i64>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub ip_addr: String,
    pub val_ip_addrs: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct HostCredentials {
    pub host_id: String,
    pub token: String,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Command {
    pub id: String,
    pub host_id: String,
    pub cmd: String,
    pub sub_cmd: Option<String>,
    pub response: Option<String>,
    pub exit_status: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct CommandStatusUpdate {
    pub response: String,
    pub exit_status: i32,
}

use crate::bv_config::{ApptainerConfig, Config};

#[derive(Debug, PartialEq, Clone)]
pub struct BvContext {
    pub id: String,
    pub name: String,
    pub url: String,
    pub bridge: Option<String>,
}

impl BvContext {
    pub fn from_config(config: Config, apptainer_config: Option<ApptainerConfig>) -> Self {
        Self {
            id: config.id,
            name: config.name,
            url: config.api_config.blockjoy_api_url,
            bridge: if !apptainer_config.unwrap_or(config.apptainer).host_network {
                Some(config.iface)
            } else {
                None
            },
        }
    }
}

use crate::bv_config::Config;

#[derive(Debug, PartialEq, Clone)]
pub struct BvContext {
    pub id: String,
    pub name: String,
    pub url: String,
    pub iface: String,
}

impl BvContext {
    pub fn from_config(config: Config) -> Self {
        Self {
            id: config.id,
            name: config.name,
            url: config.api_config.blockjoy_api_url,
            iface: config.iface,
        }
    }
}

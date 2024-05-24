use crate::config::Config;

#[derive(Debug, PartialEq, Clone)]
pub struct BvContext {
    pub id: String,
    pub name: String,
    pub url: String,
}

impl BvContext {
    pub fn from_config(config: Config) -> Self {
        Self {
            id: config.id,
            name: config.name,
            url: config.blockjoy_api_url,
        }
    }
}

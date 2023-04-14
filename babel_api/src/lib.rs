pub mod api;
pub mod babel;
pub mod babelsup;
pub mod config;
pub mod engine;
pub mod metadata;
pub mod plugin;
pub mod rhai_plugin;
pub mod utils;

use anyhow::{bail, Result};
pub use api::*;
use cidr_utils::cidr::IpCidr;
pub use config::Config;

pub fn check_babel_config(babel: &config::Babel) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let min_babel_version = babel.config.min_babel_version.as_str();
    if version < min_babel_version {
        bail!("Required minimum babel version is `{min_babel_version}`, running is `{version}`");
    }
    if let Some(firewall) = &babel.firewall {
        check_firewall_rules(&firewall.rules)?;
    }
    Ok(())
}

pub fn check_firewall_rules(rules: &[config::firewall::Rule]) -> Result<()> {
    for rule in rules {
        match &rule.ips {
            Some(ip) if !IpCidr::is_ip_cidr(ip) => bail!(
                "invalid ip address '{}' in firewall rule '{}'",
                ip,
                rule.name
            ),
            _ => {}
        }
    }
    Ok(())
}

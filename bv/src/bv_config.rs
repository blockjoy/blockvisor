use crate::{api_config::ApiConfig, services::AuthToken, utils};
use bv_utils::cmd::run_cmd;
use cidr_utils::cidr::IpCidr;
use eyre::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, path::Path, str::FromStr};
use sysinfo::{System, SystemExt};
use tokio::fs;
use tracing::debug;

pub const CONFIG_PATH: &str = "etc/blockvisor.json";
pub const DEFAULT_BRIDGE_IFACE: &str = "bvbr0";

pub fn default_blockvisor_port() -> u16 {
    9001
}

pub fn default_iface() -> String {
    DEFAULT_BRIDGE_IFACE.to_string()
}

pub fn default_hostname() -> String {
    let mut sys = System::new_all();
    sys.refresh_all();
    sys.host_name().unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct SharedConfig {
    pub config: std::sync::Arc<tokio::sync::RwLock<Config>>,
    pub bv_root: std::path::PathBuf,
}

impl SharedConfig {
    pub fn new(config: Config, bv_root: std::path::PathBuf) -> Self {
        Self {
            config: std::sync::Arc::new(tokio::sync::RwLock::new(config)),
            bv_root,
        }
    }

    pub async fn read(&self) -> Config {
        self.config.read().await.clone()
    }

    pub async fn set_mqtt_url(&self, mqtt_url: Option<String>) {
        self.config.write().await.blockjoy_mqtt_url = mqtt_url;
    }

    pub async fn token(&self) -> Result<AuthToken, tonic::Status> {
        let api_config = self.read().await.api_config.clone();
        Ok(AuthToken(if api_config.token_expired()? {
            let token = {
                let mut write_lock = self.config.write().await;
                // A concurrent update may have written to the jwt field, check if the token has become
                // unexpired while we have unique access.
                if write_lock.api_config.token_expired()? {
                    write_lock.api_config.refresh_token().await?;
                }
                write_lock.api_config.token.clone()
            };
            self.read()
                .await
                .save(&self.bv_root)
                .await
                .map_err(|err| tonic::Status::internal(format!("failed to save token: {err:#}")))?;
            token
        } else {
            api_config.token
        }))
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct ApptainerConfig {
    pub extra_args: Option<Vec<String>>,
    pub host_network: bool,
    pub cpu_limit: bool,
    pub memory_limit: bool,
}

impl Default for ApptainerConfig {
    fn default() -> Self {
        Self {
            extra_args: None,
            host_network: false,
            cpu_limit: true,
            memory_limit: true,
        }
    }
}

#[derive(Default, Deserialize, Serialize, Debug, Clone)]
pub struct Config {
    /// Host uuid
    pub id: String,
    /// Org id the host belongs to.
    pub org_id: Option<String>,
    /// Org uuid to which private host belongs to.
    pub private_org_id: Option<String>,
    /// Host name
    #[serde(default = "default_hostname")]
    pub name: String,
    #[serde(flatten)]
    pub api_config: ApiConfig,
    /// Url for mqtt broker to receive commands and updates from.
    pub blockjoy_mqtt_url: Option<String>,
    /// Self update check interval - how often blockvisor shall check for new version of itself
    pub update_check_interval_secs: Option<u64>,
    /// Port to be used by blockvisor internal service
    /// 0 has special meaning - pick first free port
    #[serde(default = "default_blockvisor_port")]
    pub blockvisor_port: u16,
    /// Network interface name
    #[serde(default = "default_iface")]
    pub iface: String,
    #[serde(default)]
    pub net_conf: NetConf,
    /// Host's cluster id
    pub cluster_id: Option<String>,
    /// Cluster gossip listen port
    pub cluster_port: Option<u32>,
    /// Addresses of the seed nodes for cluster discovery and announcements
    pub cluster_seed_urls: Option<Vec<String>>,
    /// Apptainer configuration
    pub apptainer: ApptainerConfig,
    /// Run in maintenance mode - use on your own risk.
    #[serde(default)]
    pub maintenance_mode: bool,
}

impl Config {
    pub async fn load(bv_root: &Path) -> Result<Config> {
        let path = bv_root.join(CONFIG_PATH);
        debug!("Reading host config: {}", path.display());
        let config = fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read host config: {}", path.display()))?;
        let mut config: Config = serde_json::from_str(&config)
            .with_context(|| format!("failed to parse host config: {}", path.display()))?;
        if config.net_conf.host_ip.is_unspecified() {
            config.net_conf = NetConf::new(&config.iface).await?;
        }
        config.save(bv_root).await?;
        Ok(config)
    }

    pub async fn save(&self, bv_root: &Path) -> Result<()> {
        let path = bv_root.join(CONFIG_PATH);
        let parent = path.parent().unwrap();
        debug!("Ensuring config dir is present: {}", parent.display());
        fs::create_dir_all(parent).await?;
        debug!("Writing host config: {}", path.display());
        utils::careful_save(&path, serde_json::to_string(&self)?.as_bytes()).await?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Clone)]
pub struct NetConf {
    pub gateway_ip: IpAddr,
    pub host_ip: IpAddr,
    pub prefix: u8,
    pub available_ips: Vec<IpAddr>,
}

impl Default for NetConf {
    fn default() -> Self {
        Self {
            gateway_ip: IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)),
            host_ip: IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)),
            prefix: 32,
            available_ips: vec![],
        }
    }
}

impl NetConf {
    pub async fn new(ifa_name: &str) -> Result<Self> {
        Self::from_json(&run_cmd("ip", ["--json", "route"]).await?, ifa_name)
    }

    fn from_json(json: &str, ifa_name: &str) -> Result<Self> {
        let routes = serde_json::from_str::<Vec<IpRoute>>(json)?;
        let gateway_str = routes
            .iter()
            .find(|route| route.dst.as_ref().is_some_and(|v| v == "default"))
            .ok_or(anyhow!("default route not found"))?
            .gateway
            .as_ref()
            .ok_or(anyhow!("default route 'gateway' not found"))?;
        let gateway = IpAddr::from_str(gateway_str)
            .with_context(|| format!("failed to parse gateway ip '{gateway_str}'"))?;
        for route in &routes {
            if is_dev_ip(route, ifa_name) {
                if let Some(dst) = &route.dst {
                    let cidr = IpCidr::from_str(dst)
                        .with_context(|| format!("cannot parse '{dst}' as cidr"))?;
                    if cidr.contains(gateway) {
                        let perfsrc = route
                            .prefsrc
                            .as_ref()
                            .ok_or(anyhow!("'prefsrc' not found for host network"))?;
                        let host_ip = IpAddr::from_str(perfsrc).with_context(|| {
                            format!("cannot parse host network perfsrc '{perfsrc}'")
                        })?;
                        let prefix = get_bits(&cidr);
                        let mut ips = cidr.iter();
                        if prefix <= 30 {
                            // For routing mask values <= 30, first and last IPs are
                            // base and broadcast addresses and are unusable.
                            ips.next();
                            ips.next_back();
                        }
                        return Ok(NetConf {
                            gateway_ip: gateway,
                            host_ip,
                            prefix,
                            available_ips: if let (Some(ip_from), Some(ip_to)) =
                                (ips.next(), ips.next_back())
                            {
                                let mut ips = ips_from_range(ip_from, ip_to)?;
                                ips.retain(|ip| *ip != host_ip && *ip != gateway);
                                ips
                            } else {
                                bail!("Failed to resolve ip range")
                            },
                        });
                    }
                }
            }
        }
        bail!("failed to discover network config");
    }

    pub fn override_host_ip(&mut self, value: &str) -> Result<()> {
        self.host_ip =
            IpAddr::from_str(value).with_context(|| format!("invalid host ip '{value}'"))?;
        Ok(())
    }

    pub fn override_gateway_ip(&mut self, value: &str) -> Result<()> {
        self.gateway_ip =
            IpAddr::from_str(value).with_context(|| format!("invalid gateway ip '{value}'"))?;
        Ok(())
    }

    pub fn override_ips(&mut self, value: &str) -> Result<()> {
        self.available_ips.clear();
        for item in value.split(",") {
            if item.contains("/") {
                let cidr = IpCidr::from_str(item)
                    .with_context(|| format!("cannot parse '{item}' as cidr"))?;
                let prefix = get_bits(&cidr);
                let mut ips = cidr.iter();
                if prefix <= 30 {
                    // For routing mask values <= 30, first and last IPs are
                    // base and broadcast addresses and are unusable.
                    ips.next();
                    ips.next_back();
                }
                self.available_ips.append(&mut ips.collect::<Vec<_>>());
            } else if item.contains("-") {
                let mut split = item.split("-");
                let ip_from = split
                    .next()
                    .ok_or(anyhow!("missing from"))
                    .and_then(|ip| IpAddr::from_str(ip).map_err(|err| anyhow!("{err:#}")))
                    .with_context(|| format!("invalid ip range '{item}'"))?;
                let ip_to = split
                    .next()
                    .ok_or(anyhow!("missing to"))
                    .and_then(|ip| IpAddr::from_str(ip).map_err(|err| anyhow!("{err:#}")))
                    .with_context(|| format!("invalid ip range '{item}'"))?;
                if split.next().is_some() {
                    bail!("invalid ip range '{item}'")
                }
                self.available_ips
                    .append(&mut ips_from_range(ip_from, ip_to)?);
            } else {
                self.available_ips.push(
                    IpAddr::from_str(item)
                        .with_context(|| format!("invalid ip provided '{item}'"))?,
                );
            }
        }
        self.available_ips
            .retain(|ip| *ip != self.host_ip && *ip != self.gateway_ip);
        Ok(())
    }
}

fn ips_from_range(ip_from: IpAddr, ip_to: IpAddr) -> Result<Vec<IpAddr>> {
    Ok(match (ip_from, ip_to) {
        (IpAddr::V4(ip_from), IpAddr::V4(ip_to)) => Ok(ipnet::IpAddrRange::from(
            ipnet::Ipv4AddrRange::new(ip_from, ip_to),
        )),
        (IpAddr::V6(ip_from), IpAddr::V6(ip_to)) => Ok(ipnet::IpAddrRange::from(
            ipnet::Ipv6AddrRange::new(ip_from, ip_to),
        )),
        _ => Err(anyhow!("invalid ip range: ({ip_from}, {ip_to}")),
    }?
    .collect())
}

/// Struct to capture output of linux `ip --json route` command
#[derive(Deserialize, Serialize, Debug)]
struct IpRoute {
    pub dst: Option<String>,
    pub gateway: Option<String>,
    pub dev: Option<String>,
    pub prefsrc: Option<String>,
    pub protocol: Option<String>,
}

fn is_dev_ip(route: &IpRoute, dev: &str) -> bool {
    route.dev.as_deref() == Some(dev)
        && route
            .dst
            .as_ref()
            .is_some_and(|v| v != "default" && IpCidr::is_ip_cidr(v))
        && route.prefsrc.is_some()
        && route.protocol.as_deref() == Some("kernel")
}

fn get_bits(cidr: &IpCidr) -> u8 {
    match cidr {
        IpCidr::V4(cidr) => cidr.get_bits(),
        IpCidr::V6(cidr) => cidr.get_bits(),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_net_conf_from_json() {
        let json = r#"[
            {
               "dev" : "bvbr0",
               "dst" : "default",
               "flags" : [],
               "gateway" : "192.69.220.81",
               "protocol" : "static"
            },
            {
               "dev" : "bvbr0",
               "dst" : "192.69.220.80/28",
               "flags" : [],
               "prefsrc" : "192.69.220.82",
               "protocol" : "kernel",
               "scope" : "link"
            }
         ]
         "#;
        let expected = NetConf {
            gateway_ip: IpAddr::from(Ipv4Addr::from_str("192.69.220.81").unwrap()),
            host_ip: IpAddr::from(Ipv4Addr::from_str("192.69.220.82").unwrap()),
            prefix: 28,
            available_ips: vec![
                IpAddr::from(Ipv4Addr::from_str("192.69.220.83").unwrap()),
                IpAddr::from(Ipv4Addr::from_str("192.69.220.84").unwrap()),
                IpAddr::from(Ipv4Addr::from_str("192.69.220.85").unwrap()),
                IpAddr::from(Ipv4Addr::from_str("192.69.220.86").unwrap()),
                IpAddr::from(Ipv4Addr::from_str("192.69.220.87").unwrap()),
                IpAddr::from(Ipv4Addr::from_str("192.69.220.88").unwrap()),
                IpAddr::from(Ipv4Addr::from_str("192.69.220.89").unwrap()),
                IpAddr::from(Ipv4Addr::from_str("192.69.220.90").unwrap()),
                IpAddr::from(Ipv4Addr::from_str("192.69.220.91").unwrap()),
                IpAddr::from(Ipv4Addr::from_str("192.69.220.92").unwrap()),
                IpAddr::from(Ipv4Addr::from_str("192.69.220.93").unwrap()),
                IpAddr::from(Ipv4Addr::from_str("192.69.220.94").unwrap()),
            ],
        };
        assert_eq!(expected, NetConf::from_json(json, "bvbr0").unwrap());
    }
}

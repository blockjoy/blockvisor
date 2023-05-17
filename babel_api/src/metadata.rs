use anyhow::{bail, Result};
use cidr_utils::cidr::IpCidr;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type KeysConfig = HashMap<String, String>;

pub fn check_metadata(meta: &BlockchainMetadata) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let min_babel_version = meta.min_babel_version.as_str();
    if version < min_babel_version {
        bail!("Required minimum babel version is `{min_babel_version}`, running is `{version}`");
    }
    check_firewall_rules(&meta.firewall.rules)?;
    Ok(())
}

pub fn check_firewall_rules(rules: &[firewall::Rule]) -> Result<()> {
    for rule in rules {
        match &rule.ips {
            Some(ip) if !IpCidr::is_ip_cidr(ip) => bail!(
                "invalid ip address `{}` in firewall rule `{}`",
                ip,
                rule.name
            ),
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BlockchainMetadata {
    /// A semver version of the blockchain node program.
    pub node_version: String,
    /// Name of the blockchain protocol.
    pub protocol: String,
    /// Type of the node (validator, beacon, etc).
    pub node_type: String,
    /// Some description of the node.
    pub description: Option<String>,
    /// Blockchain resource requirements.
    pub requirements: Requirements,
    /// Supported blockchain networks.
    pub nets: HashMap<String, NetConfiguration>,
    /// A semver version of the babel program, indicating the minimum version of the babel
    /// program that a babel script is compatible with.
    pub min_babel_version: String,
    /// Configuration of Babel - agent running inside VM.
    pub babel_config: BabelConfig,
    /// Node firewall configuration.
    pub firewall: firewall::Config,
    /// Configuration of blockchain keys.
    pub keys: Option<KeysConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BabelConfig {
    /// Path to mount data drive to.
    pub data_directory_mount_point: String,
    /// Capacity of log buffer (in lines).
    pub log_buffer_capacity_ln: usize,
    /// Size of swap file created on the node, in MB.
    pub swap_size_mb: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Requirements {
    /// Virtual cores to share with VM.
    pub vcpu_count: usize,
    /// RAM allocated to VM in MB.
    pub mem_size_mb: usize,
    /// Size of data drive for storing blockchain data (not to be confused with OS drive).
    pub disk_size_gb: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NetType {
    Dev,
    Test,
    Main,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NetConfiguration {
    /// Url for given blockchain network.
    pub url: String,
    /// Blockchain network type.
    pub net_type: NetType,
    /// Custom network metadata.
    #[serde(flatten)]
    pub meta: HashMap<String, String>,
}

pub mod firewall {
    use super::*;

    /// Single firewall rule.
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct Rule {
        /// Unique rule name.
        pub name: String,
        /// Action applied on packet that match rule.
        pub action: Action,
        /// Traffic direction for which rule applies.
        pub direction: Direction,
        /// Protocol - `Both` by default.
        pub protocol: Option<Protocol>,
        /// Ip(s) compliant with CIDR notation.
        pub ips: Option<String>,
        /// List of ports. Empty means all.  
        pub ports: Vec<u16>,
    }

    /// Firewall configuration that is applied on node start.
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct Config {
        /// Option to disable firewall at all. Only for debugging purpose - use on your own risk!
        pub enabled: bool,
        /// Fallback action for inbound traffic used when packet doesn't match any rule.
        pub default_in: Action,
        /// Fallback action for outbound traffic used when packet doesn't match any rule.
        pub default_out: Action,
        /// Set of rules to be applied.
        pub rules: Vec<Rule>,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    pub enum Action {
        /// Allow packets.
        Allow,
        /// Deny packets without any response.
        Deny,
        /// Reject packets with explicit response.
        Reject,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    pub enum Direction {
        /// Outbound
        Out,
        /// Inbound
        In,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    pub enum Protocol {
        Tcp,
        Udp,
        Both,
    }
}

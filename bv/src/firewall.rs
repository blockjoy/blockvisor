use cidr_utils::cidr::IpCidr;
use eyre::{bail, Result};
use serde::{Deserialize, Serialize};

pub fn check_rules(rules: &[Rule]) -> Result<()> {
    for rule in rules {
        for ip in &rule.ips {
            if !IpCidr::is_ip_cidr(ip) {
                bail!(
                    "invalid ip address `{}` in firewall rule `{}`",
                    ip,
                    rule.name
                )
            }
        }
    }
    Ok(())
}

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
    pub ips: Vec<String>,
    /// List of ports. Empty means all.
    pub ports: Vec<u16>,
}

/// Firewall configuration that is applied to node traffic.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Fallback action for inbound traffic used when packet doesn't match any rule.
    pub default_in: Action,
    /// Fallback action for outbound traffic used when packet doesn't match any rule.
    pub default_out: Action,
    /// Set of rules to be applied.
    pub rules: Vec<Rule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_in: Action::Reject,
            default_out: Action::Allow,
            rules: vec![],
        }
    }
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

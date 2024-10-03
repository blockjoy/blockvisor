use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Protocol {
    /// Globally unique protocol key.
    pub key: String,
    /// Display name visible in frontend - can be modified.
    pub name: String,
    /// Uuid of organization where which protocol belongs to, or null if public.
    pub org_id: Option<String>,
    pub ticker: Option<String>,
    /// Brief protocol description.
    pub description: Option<String>,
    /// Protocols visibility.
    pub visibility: Visibility,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ImageKey {
    pub protocol_key: String,
    pub variant_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Image {
    /// Set by image provider, shall follow semver.
    pub version: String,
    pub key: ImageKey,
    pub org_id: Option<String>,
    pub description: Option<String>,
    pub visibility: Visibility,
    pub properties: Vec<ImageProperty>,
    pub firewall_config: FirewallConfig,
    pub min_cpu: u64,
    pub min_memory_bytes: u64,
    pub min_disk_bytes: u64,
    pub ramdisks: Vec<RamdiskConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RamdiskConfig {
    pub mount: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FirewallConfig {
    pub default_in: Action,
    pub default_out: Action,
    pub rules: Vec<FirewallRule>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FirewallRule {
    pub key: String,
    pub description: Option<String>,
    pub protocol: NetProtocol,
    pub direction: Direction,
    pub action: Action,
    pub ips: Vec<IpName>,
    pub ports: Vec<PortName>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Deny,
    Reject,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Out,
    In,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NetProtocol {
    Tcp,
    Udp,
    Both,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Private,
    Public,
    Development,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct IpName {
    pub ip: String,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PortName {
    pub port: u32,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ImageProperty {
    pub key: String,
    pub description: Option<String>,
    pub required: bool,
    pub dynamic_value: bool,
    pub default_value: Option<String>,
    pub ui_type: UiType,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ImageImpact {
    pub new_archive: bool,
    pub add_cpu: Option<i64>,
    pub add_memory_bytes: Option<i64>,
    pub add_disk_bytes: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum UiType {
    Switch {
        on_impact: Option<ImageImpact>,
        off_impact: Option<ImageImpact>,
    },
    Text(Option<ImageImpact>),
    Password(Option<ImageImpact>),
    Enum(Vec<EnumVariant>),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EnumVariant {
    key: String,
    impact: Option<ImageImpact>,
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::fs;

    #[test]
    pub fn test_create_example() {
        let protocols = vec![
            Protocol {
                key: "test_chain".to_string(),
                name: "test".to_string(),
                org_id: Some("4931bafa-92d9-4521-9fc6-a77eee047530".to_string()),
                ticker: None,
                description: Some("Testing protocol definition".to_string()),
                visibility: Visibility::Private,
            },
            Protocol {
                key: "eth".to_string(),
                name: "Ethereum".to_string(),
                org_id: None,
                ticker: None,
                description: Some("Ethereum protocol definition".to_string()),
                visibility: Visibility::Public,
            },
        ];
        let image = Image {
            version: "0.1.1".to_string(),
            key: ImageKey {
                protocol_key: "test_chain".to_string(),
                variant_key: "test_client-testnet".to_string(),
            },
            org_id: None,
            description: Some("Test image description".to_string()),
            visibility: Visibility::Private,
            properties: vec![
                ImageProperty {
                    key: "arbitrary text property".to_string(),
                    description: Some("this is some text property example".to_string()),
                    required: true,
                    default_value: Some("default_value".to_string()),
                    dynamic_value: true,
                    ui_type: UiType::Text(Some(ImageImpact {
                        new_archive: false,
                        add_cpu: Some(1),
                        add_memory_bytes: Some(-1),
                        add_disk_bytes: Some(10),
                    })),
                },
                ImageProperty {
                    key: "switch property".to_string(),
                    description: Some("this is some switch example".to_string()),
                    required: true,
                    default_value: Some("off".to_string()),
                    dynamic_value: true,
                    ui_type: UiType::Switch {
                        on_impact: Some(ImageImpact {
                            new_archive: true,
                            add_cpu: Some(3),
                            add_memory_bytes: Some(10_000_000),
                            add_disk_bytes: Some(10_000_000_000),
                        }),
                        off_impact: None,
                    },
                },
                ImageProperty {
                    key: "enum property".to_string(),
                    description: Some("this is some enum example".to_string()),
                    required: true,
                    default_value: Some("variant_b".to_string()),
                    dynamic_value: true,
                    ui_type: UiType::Enum(vec![
                        EnumVariant {
                            key: "variant_a".to_string(),
                            impact: None,
                        },
                        EnumVariant {
                            key: "variant_b".to_string(),
                            impact: None,
                        },
                        EnumVariant {
                            key: "variant_c".to_string(),
                            impact: Some(ImageImpact {
                                new_archive: true,
                                add_cpu: Some(-1),
                                add_memory_bytes: Some(1_000_000),
                                add_disk_bytes: Some(1_000_000_000),
                            }),
                        },
                    ]),
                },
            ],
            firewall_config: FirewallConfig {
                default_in: Action::Deny,
                default_out: Action::Allow,
                rules: vec![FirewallRule {
                    key: "Testing rule".to_string(),
                    action: Action::Allow,
                    direction: Direction::In,
                    protocol: NetProtocol::Both,
                    ips: vec![],
                    ports: vec![PortName {
                        port: 78901,
                        name: Some("service port".to_string()),
                    }],
                    description: None,
                }],
            },
            min_cpu: 1,
            min_memory_bytes: 1_000_000_000,
            min_disk_bytes: 2_000_000_000,
            ramdisks: vec![RamdiskConfig {
                mount: "/mnt/ramdisk".to_string(),
                size_bytes: 100_000_000,
            }],
        };
        fs::write(
            "./protocols.yaml",
            serde_yaml::to_string(&protocols).unwrap(),
        )
        .unwrap();
        fs::write("./image.yaml", serde_yaml::to_string(&image).unwrap()).unwrap();
    }
}

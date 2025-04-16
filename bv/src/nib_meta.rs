use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Protocol {
    /// Globally unique protocol key.
    pub key: String,
    /// Display name visible in frontend - can be modified.
    pub name: String,
    /// Uuid of organization where which protocol belongs to, or null if public.
    pub org_id: Option<Uuid>,
    pub ticker: Option<String>,
    /// Brief protocol description.
    pub description: Option<String>,
    /// Protocols visibility.
    pub visibility: Visibility,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Variant {
    pub key: String,
    pub metadata: Option<Vec<VariantMetadata>>,
    pub sku_code: String,
    pub archive_pointers: Vec<ArchivePointer>,
    pub min_cpu: u64,
    pub min_memory_mb: u64,
    pub min_disk_gb: u64,
    #[serde(default)]
    pub ramdisks: Vec<RamdiskConfig>,

    // overrides
    pub dns_scheme: Option<String>,
    pub description: Option<String>,
    pub visibility: Option<Visibility>,
    pub properties: Option<Vec<ImageProperty>>,
    pub firewall_config: Option<FirewallConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ArchivePointer {
    pub pointer: StorePointer,
    #[serde(default)]
    pub new_archive_properties: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StorePointer {
    CombinationDisallowed,
    StoreKey(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VariantMetadata {
    pub key: String,
    pub value: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Image {
    /// Set by image provider, shall follow semver.
    pub version: String,
    pub container_uri: String,
    pub protocol_key: String,
    pub org_id: Option<Uuid>,
    pub dns_scheme: Option<String>,
    pub description: Option<String>,
    pub visibility: Visibility,
    pub variants: Vec<Variant>,
    pub properties: Vec<ImageProperty>,
    pub firewall_config: FirewallConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RamdiskConfig {
    pub mount: String,
    pub size_mb: u64,
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
    #[serde(default)]
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
    pub port: u16,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ImageProperty {
    pub key: String,
    pub name: String,
    pub description: Option<String>,
    pub dynamic_value: bool,
    pub default_value: String,
    pub ui_type: UiType,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ImageImpact {
    #[serde(default)]
    pub new_archive: bool,
    pub add_cpu: Option<i64>,
    pub add_memory_mb: Option<i64>,
    pub add_disk_gb: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum UiType {
    Switch {
        on: Option<ImageImpact>,
        off: Option<ImageImpact>,
    },
    Text(Option<ImageImpact>),
    Password(Option<ImageImpact>),
    Enum(Vec<EnumVariant>),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EnumVariant {
    pub value: String,
    pub name: String,
    pub impact: Option<ImageImpact>,
}

use eyre::Result;
use serde::{Deserialize, Serialize};

/// Interface to be implemented by babel plugin.
/// Babel plugin adds support for some protocol type.
pub trait Plugin {
    /// Get list of supported method names.
    fn capabilities(&self) -> Vec<String>;

    /// Init method is called by engine on node start.
    fn init(&mut self) -> Result<()>;

    /// Upload protocol data to remote storage.
    fn upload(&self) -> Result<()>;

    /// Returns the height of the blockchain (in blocks).
    fn height(&self) -> Result<u64>;

    /// Returns the block age of the blockchain (in seconds).
    fn block_age(&self) -> Result<u64>;

    /// Returns the name of the node. This is usually some random generated name that you may use
    /// to recognise the node, but the purpose may vary per protocol.
    /// ### Example
    /// `chilly-peach-kangaroo`
    fn name(&self) -> Result<String>;

    /// The address of the node. The meaning of this varies from protocol to protocol.
    /// ### Example
    /// `/p2p/11Uxv9YpMpXvLf8ZyvGWBdbgq3BXv8z1pra1LBqkRS5wmTEHNW3`
    fn address(&self) -> Result<String>;

    /// Returns whether this node is in consensus or not.
    fn consensus(&self) -> Result<bool>;

    /// Returns protocol status.
    fn protocol_status(&self) -> Result<ProtocolStatus>;

    /// Call custom protocol method by `name`, that gets String param as input and returns String as well.
    /// It is recommended to use Json string for more complex input/output.
    fn call_custom_method(&self, name: &str, param: &str) -> Result<String>;
}

/// Describe the node's protocol related status. These states are used to describe the
/// node's states as seen by the protocol.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProtocolStatus {
    pub state: String,
    pub health: NodeHealth,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NodeHealth {
    Healthy,
    Neutral,
    Unhealthy,
}

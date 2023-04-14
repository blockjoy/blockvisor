use crate::metadata::BlockchainMetadata;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Interface to be implemented by babel plugin.
/// Babel plugin adds support for some blockchain type.
pub trait Plugin {
    /// Get blockchain metadata.
    fn metadata(&self) -> Result<BlockchainMetadata>;

    /// Get list of supported method names.
    fn capabilities(&self) -> Vec<String>;

    /// Check if method with given `name` is supported.
    fn has_capability(&self, name: &str) -> bool;

    /// Init method is called by engine on node start.
    fn init(&self, secret_keys: &HashMap<String, String>) -> Result<()>;

    /// Returns the height of the blockchain (in blocks).
    fn height(&self) -> Result<u64>;

    /// Returns the block age of the blockchain (in seconds).
    fn block_age(&self) -> Result<u64>;

    /// Returns the name of the node. This is usually some random generated name that you may use
    /// to recognise the node, but the purpose may vary per blockchain.
    /// ### Example
    /// `chilly-peach-kangaroo`
    fn name(&self) -> Result<String>;

    /// The address of the node. The meaning of this varies from blockchain to blockchain.
    /// ### Example
    /// `/p2p/11Uxv9YpMpXvLf8ZyvGWBdbgq3BXv8z1pra1LBqkRS5wmTEHNW3`
    fn address(&self) -> Result<String>;

    /// Returns whether this node is in consensus or not.
    fn consensus(&self) -> Result<bool>;

    /// Returns blockchain application status.
    fn application_status(&self) -> Result<ApplicationStatus>;

    /// Returns blockchain synchronization status.
    fn sync_status(&self) -> Result<SyncStatus>;

    /// Returns blockchain staking status.
    fn staking_status(&self) -> Result<StakingStatus>;

    /// Generates keys on node
    fn generate_keys(&self) -> Result<()>;

    /// Call custom blockchain method by `name`, that gets String param as input and returns String as well.
    /// It is recommended to use Json string for more complex input/output.
    fn call_custom_method(&self, name: &str, param: &str) -> Result<String>;
}

/// Describe the node's staking status
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StakingStatus {
    Follower,
    Staked,
    Staking,
    Validating,
    Consensus,
    Unstaked,
}

/// Describe the node's syncing status
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    Syncing,
    Synced,
}

/// Describe the node's chain related status
/// Generic, NOT chain specific states. These states are used to describe the
/// node's states as seen by the blockchain. Babel is responsible for mapping
/// chain specific states to the one in here.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationStatus {
    Provisioning,
    Broadcasting,
    Cancelled,
    Delegating,
    Delinquent,
    Disabled,
    Earning,
    Electing,
    Elected,
    Exported,
    Ingesting,
    Mining,
    Minting,
    Processing,
    Relaying,
    Removed,
    Removing,
}

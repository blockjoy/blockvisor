use crate::utils::{Binary, BinaryStatus};
use serde::{Deserialize, Serialize};

#[tonic_rpc::tonic_rpc(bincode)]
pub trait BabelSup {
    /// Get installed version of babelsup. It is needed since babelsup is build-into node image
    /// and not auto-updated with BV.
    fn get_version() -> String;
    /// Check if babel binary exists and its checksum match given one.
    fn check_babel(checksum: u32) -> BinaryStatus;
    /// Sent fresh version of babel binary and (re)start it.
    #[client_streaming]
    fn start_new_babel(babel_bin: Binary);
    /// Setup basic babelsup configuration. Required to be called at least once after node is created
    /// and on first start. Function is idempotent, so it's safe to call it on every node start.  
    fn setup_supervisor(config: SupervisorConfig);
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct SupervisorConfig {
    ///  if entry_point stay alive given amount of time (in miliseconds) backof is reset
    pub backoff_timeout_ms: u64,
    /// base time (in miliseconds) for backof, multiplied by consecutive power of 2 each time
    pub backoff_base_ms: u64,
}

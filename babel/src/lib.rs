// TODO: What are we going to use as backup when vsock is disabled?
pub mod config;
pub mod error;
pub mod log_buffer;
pub mod logging;
pub mod msg_handler;
pub mod run_flag;
pub mod sup_handler;
pub mod supervisor;
#[cfg(target_os = "linux")]
pub mod vsock;

type Result<T, E = error::Error> = std::result::Result<T, E>;

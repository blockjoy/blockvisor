pub mod async_pid_watch;
pub mod babel_service;
pub mod babelsup_service;
pub mod job_runner;
pub mod jobs;
pub mod jobs_manager;
pub mod log_buffer;
pub mod logging;
pub mod logs_service;
pub mod supervisor;
pub mod ufw_wrapper;
pub mod utils;

use std::path::Path;

lazy_static::lazy_static! {
    pub static ref BABEL_LOGS_UDS_PATH: &'static Path = Path::new("/var/lib/babel/logs.socket");
}

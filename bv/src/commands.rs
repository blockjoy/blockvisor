use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

pub type Result = eyre::Result<(), Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("BV internal error: {0:#}")]
    Internal(#[from] eyre::Error),
    #[error("BV service not ready, try again later")]
    ServiceNotReady,
    #[error("BV service is broken, call support")]
    ServiceBroken,
    #[error("Command is not supported")]
    NotSupported,
    #[error("Node with {0} not found")]
    NodeNotFound(Uuid),
    #[error("Can't proceed while 'upgrade_blocking' job is running. Try again after {} seconds.", retry_hint.as_secs())]
    BlockingJobRunning { retry_hint: Duration },
}

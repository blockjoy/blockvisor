use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

#[macro_export]
macro_rules! command_failed {
    ($err:expr $(,)?) => {
        return Err($err)
    };
}

pub fn into_internal(err: impl Into<eyre::Report>) -> Error {
    Error::Internal(err.into())
}

pub type Result<T> = eyre::Result<T, Error>;

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

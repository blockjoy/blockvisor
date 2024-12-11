use std::time::Duration;
use thiserror::Error;

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
    #[error("BV internal error: '{0:#}'")]
    Internal(#[from] eyre::Error),
    #[error("BV service not ready, try again later")]
    ServiceNotReady,
    #[error("BV service is broken, call support")]
    ServiceBroken,
    #[error("command is not supported")]
    NotSupported,
    #[error("node not found")]
    NodeNotFound,
    #[error("can't proceed while 'upgrade_blocking' job is running. Try again after {} seconds.", retry_hint.as_secs())]
    BlockingJobRunning { retry_hint: Duration },
    #[error("node upgrade failed with: '{0:#}'; but then successfully rolled back")]
    NodeUpgradeRollback(eyre::Error),
    #[error("node upgrade failed with: '{0:#}'; and then rollback failed with: '{1:#}'; node ended in failed state")]
    NodeUpgradeFailure(String, eyre::Error),
}

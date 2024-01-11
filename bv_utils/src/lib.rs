pub mod cmd;
pub mod logging;
pub mod rpc;
pub mod run_flag;
pub mod system;
pub mod timer;

#[macro_export]
macro_rules! with_retry {
    ($fun:expr) => {{
        const RPC_RETRY_MAX: u32 = 3;
        const RPC_BACKOFF_BASE_MS: u64 = 300;
        $crate::_with_retry!($fun, RPC_RETRY_MAX, RPC_BACKOFF_BASE_MS)
    }};
    ($fun:expr, $retry_max:expr, $backoff_base_ms:expr) => {{
        $crate::_with_retry!($fun, $retry_max, $backoff_base_ms)
    }};
}

#[macro_export]
macro_rules! with_selective_retry {
    ($fun:expr, $non_retriable:expr) => {{
        const RPC_RETRY_MAX: u32 = 3;
        const RPC_BACKOFF_BASE_MS: u64 = 300;
        $crate::_with_selective_retry!($fun, $non_retriable, RPC_RETRY_MAX, RPC_BACKOFF_BASE_MS)
    }};
    ($fun:expr, $non_retriable:expr, $retry_max:expr, $backoff_base_ms:expr) => {{
        $crate::_with_selective_retry!($fun, $non_retriable, $retry_max, $backoff_base_ms)
    }};
}

#[macro_export]
macro_rules! _with_selective_retry {
    ($fun:expr, $non_retriable:expr, $retry_max:expr, $backoff_base_ms:expr) => {{
        const RPC_RETRY_MAX: u32 = 3;
        const RPC_BACKOFF_BASE_MS: u64 = 300;
        let mut retry_count = 0;
        loop {
            match $fun.await {
                Ok(res) => break Ok(res),
                Err(err) if !$non_retriable.contains(&err.code()) => {
                    if retry_count < $retry_max {
                        retry_count += 1;
                        let backoff = $backoff_base_ms * 2u64.pow(retry_count);
                        tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                        continue;
                    } else {
                        break Err(err);
                    }
                }
                Err(err) => {
                    break Err(err);
                }
            }
        }
    }};
}

#[macro_export]
macro_rules! _with_retry {
    ($fun:expr, $retry_max:expr, $backoff_base_ms:expr) => {{
        let mut retry_count = 0;
        loop {
            match $fun.await {
                Ok(res) => break Ok(res),
                Err(err) => {
                    if retry_count < $retry_max {
                        retry_count += 1;
                        if !cfg!(test) {
                            let backoff = $backoff_base_ms * 2u64.pow(retry_count);
                            tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                        }
                        continue;
                    } else {
                        break Err(err);
                    }
                }
            }
        }
    }};
}

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

pub mod babel_engine_mock;
pub mod rpc;
use std::sync::atomic::AtomicBool;
use std::sync::{atomic, Arc};
use std::thread::panicking;

/// Helper struct that adds a panic hook and checks if it was called on `Drop`.
/// It is needed when a mock object is moved to another (e.g. server) thread.
/// By default panics from threads different from main test thread are suppressed,
/// and tests pass even if Mock assertion fails. Creating an `AsyncPanicChecker` struct in a test will
/// make sure that the test fails in such case.
pub struct AsyncPanicChecker {
    flag: Arc<AtomicBool>,
}

impl Drop for AsyncPanicChecker {
    fn drop(&mut self) {
        if !panicking() {
            assert!(!self.flag.load(atomic::Ordering::Relaxed));
        }
    }
}

impl Default for AsyncPanicChecker {
    fn default() -> Self {
        let flag = Arc::new(AtomicBool::new(false));
        let async_panic = flag.clone();
        let default_panic = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            default_panic(info);
            async_panic.store(true, atomic::Ordering::Relaxed);
        }));
        Self { flag }
    }
}

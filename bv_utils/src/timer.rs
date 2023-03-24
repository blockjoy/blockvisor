use async_trait::async_trait;
use mockall::automock;
use std::time::{Duration, Instant};

/// Time abstraction for better testing.
#[automock]
#[async_trait]
pub trait AsyncTimer {
    fn now(&self) -> Instant;
    async fn sleep(&self, duration: Duration);
}

/// Time abstraction for better testing.
#[automock]
pub trait Timer {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

pub struct SysTimer;

#[async_trait]
impl AsyncTimer for SysTimer {
    fn now(&self) -> Instant {
        Instant::now()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await
    }
}

#[async_trait]
impl Timer for SysTimer {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration)
    }
}

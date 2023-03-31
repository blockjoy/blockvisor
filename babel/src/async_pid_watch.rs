#[cfg(not(target_os = "linux"))]
compile_error!("async_pid only works on Linux");
use libc::pid_t;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use sysinfo::{Pid, PidExt};
use tokio::io::unix::AsyncFd;

/// Asynchronous version of PID that uses non-blocking file descriptor for async wait
/// without spawning threads.
pub struct AsyncPidWatch {
    inner: AsyncFd<NonBlockingPidFd>,
}

impl AsyncPidWatch {
    /// Consume given `pid` and create `AsyncPid`.
    pub fn new(pid: Pid) -> io::Result<Self> {
        Ok(Self {
            inner: AsyncFd::new(NonBlockingPidFd::from_pid(pid)?)?,
        })
    }

    pub async fn watch(&self) {
        let _ = self.inner.readable().await;
    }
}

/// Non-blocking process file descriptor.
struct NonBlockingPidFd(RawFd);

impl NonBlockingPidFd {
    pub fn from_pid(pid: Pid) -> io::Result<Self> {
        Ok(Self(pidfd_open(pid.as_u32() as pid_t)?))
    }
}

impl AsRawFd for NonBlockingPidFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Remember to close file descriptor.
impl Drop for NonBlockingPidFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Set flag to O_NONBLOCK, which should be equal to PIDFD_NONBLOCK
/// supported since Linux 5.10, see: https://man7.org/linux/man-pages/man2/pidfd_open.2.html
fn pidfd_open(pid: libc::pid_t) -> io::Result<RawFd> {
    let ret = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, libc::O_NONBLOCK) };
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret as RawFd)
    }
}

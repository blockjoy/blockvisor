use babel_api::engine::PosixSignal;
use bv_utils::{run_flag::RunFlag, system::is_process_running, timer::AsyncTimer};
use eyre::{bail, Context, ContextCompat};
use futures::StreamExt;
use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, Instant},
};
use sysinfo::{Pid, PidExt, Process, ProcessExt, ProcessRefreshKind, Signal, System, SystemExt};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
};
use tokio_stream::Stream;
use tonic::Status;

const ENV_BV_USER: &str = "BV_USER";
const PROCESS_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// User to run sh commands and long running jobs
fn bv_user() -> Option<String> {
    std::env::var(ENV_BV_USER).ok()
}

/// Build shell command in form of cmd and args, in order to run something in shell
///
/// If we want to run as custom user, we will be using `su`, otherwise just `sh`
pub fn bv_shell(body: &str) -> (&str, Vec<String>) {
    if let Some(user) = bv_user() {
        (
            "su",
            vec!["-".to_owned(), user, "-c".to_owned(), body.to_owned()],
        )
    } else {
        ("sh", vec!["-c".to_owned(), body.to_owned()])
    }
}

/// Kill all processes that match `cmd` and passed `args`.
pub fn kill_all_processes(cmd: &str, args: &[&str], timeout: Duration, signal: PosixSignal) {
    let mut sys = System::new();
    sys.refresh_processes();
    let ps = sys.processes();

    let procs = find_processes(cmd, args, ps);
    let now = Instant::now();
    for (_, proc) in procs {
        kill_process_tree(proc, ps, now, timeout, into_sysinfo_signal(signal));
    }
}

/// Kill process and all its descendents.
fn kill_process_tree(
    proc: &Process,
    ps: &HashMap<Pid, Process>,
    now: Instant,
    timeout: Duration,
    signal: Signal,
) {
    // Better to kill parent first, since it may implement some child restart mechanism.
    // Try to interrupt the process, and kill it after timeout in case it has not finished.
    proc.kill_with(signal);
    while is_process_running(proc.pid().as_u32()) {
        if now.elapsed() > timeout {
            proc.kill();
            proc.wait();
            break;
        }
        std::thread::sleep(PROCESS_CHECK_INTERVAL)
    }
    let children = ps.iter().filter(|(_, p)| p.parent() == Some(proc.pid()));
    for (_, child) in children {
        kill_process_tree(child, ps, now, timeout, signal);
    }
}

pub fn gracefully_terminate_process(pid: &Pid, timeout: Duration) -> bool {
    let mut sys = System::new();
    if !sys.refresh_process_specifics(*pid, ProcessRefreshKind::new()) {
        return true;
    }
    if let Some(proc) = sys.process(*pid) {
        proc.kill_with(Signal::Term);
        let now = std::time::Instant::now();
        while is_process_running(pid.as_u32()) {
            if now.elapsed() < timeout {
                std::thread::sleep(Duration::from_secs(1))
            } else {
                return false;
            }
        }
    }
    true
}

/// Find all processes that match `cmd` and passed `args`.
pub fn find_processes<'a>(
    cmd: &'a str,
    args: &'a [&'a str],
    ps: &'a HashMap<Pid, Process>,
) -> impl Iterator<Item = (&'a Pid, &'a Process)> {
    ps.iter().filter(move |(_, process)| {
        let proc_call = process
            .cmd()
            .iter()
            .map(|item| item.as_str())
            .collect::<Vec<_>>();
        if let Some(proc_cmd) = proc_call.first() {
            // first element is cmd, rest are arguments
            (cmd == *proc_cmd && *args == proc_call[1..])
            // if not a binary, but a script (with shebang) is executed,
            // then the process looks like: /bin/sh ./lalala.sh,
            // so first element is shebang, second is cmd, rest are arguments
            || (proc_call.len() > 1 && cmd == proc_call[1] && *args == proc_call[2..])
        } else {
            false
        }
    })
}

/// Restart backoff procedure helper.
pub struct Backoff<T> {
    counter: u32,
    timestamp: Instant,
    backoff_base_ms: u64,
    reset_timeout: Duration,
    run: RunFlag,
    timer: T,
}

#[derive(PartialEq)]
enum TimeoutStatus {
    Expired,
    ShouldWait,
}

#[derive(PartialEq)]
pub enum LimitStatus {
    Ok,
    Exceeded,
}

impl<T: AsyncTimer> Backoff<T> {
    /// Create new backoff state object.
    pub fn new(timer: T, run: RunFlag, backoff_base_ms: u64, reset_timeout: Duration) -> Self {
        Self {
            counter: 0,
            timestamp: timer.now(),
            backoff_base_ms,
            reset_timeout,
            run,
            timer,
        }
    }

    /// Must be called on first start to record timestamp.
    pub fn start(&mut self) {
        self.timestamp = self.timer.now();
    }

    /// Calculates timeout according to configured backoff procedure and asynchronously wait.
    pub async fn wait(&mut self) {
        if self.check_timeout().await == TimeoutStatus::ShouldWait {
            self.backoff().await;
        }
    }

    /// Calculates timeout according to configured backoff procedure and asynchronously wait.
    /// Returns `LimitStatus::Exceeded` immediately if no timeout, but exceeded retry limit;
    /// `LimitStatus::Ok` otherwise.
    pub async fn wait_with_limit(&mut self, max_retries: u32) -> LimitStatus {
        if self.check_timeout().await == TimeoutStatus::Expired {
            LimitStatus::Ok
        } else if self.counter >= max_retries {
            LimitStatus::Exceeded
        } else {
            self.backoff().await;
            LimitStatus::Ok
        }
    }

    async fn check_timeout(&mut self) -> TimeoutStatus {
        let now = self.timer.now();
        let duration = now.duration_since(self.timestamp);
        if duration > self.reset_timeout {
            self.counter = 0;
            TimeoutStatus::Expired
        } else {
            TimeoutStatus::ShouldWait
        }
    }

    async fn backoff(&mut self) {
        let sleep = self.timer.sleep(Duration::from_millis(
            self.backoff_base_ms * 2u64.pow(self.counter),
        ));
        self.run.select(sleep).await;
        self.counter += 1;
    }
}

pub async fn file_checksum(path: &Path) -> eyre::Result<u32> {
    let file = File::open(path).await?;
    let mut reader = BufReader::new(file);
    let mut buf = [0; 16384];
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);
    let mut digest = crc.digest();
    while let Ok(size) = reader.read(&mut buf[..]).await {
        if size == 0 {
            break;
        }
        digest.update(&buf[0..size]);
    }
    Ok(digest.finalize())
}

/// Write binary stream into the file.
pub async fn save_bin_stream<S: Stream<Item = Result<babel_api::utils::Binary, Status>> + Unpin>(
    bin_path: &Path,
    stream: &mut S,
) -> eyre::Result<u32> {
    let _ = tokio::fs::remove_file(bin_path).await;
    let file = OpenOptions::new()
        .write(true)
        .mode(0o770)
        .append(false)
        .create(true)
        .open(bin_path)
        .await
        .with_context(|| "failed to open binary file")?;
    let mut writer = BufWriter::new(file);
    let mut expected_checksum = None;
    while let Some(part) = stream.next().await {
        match part? {
            babel_api::utils::Binary::Bin(bin) => {
                writer
                    .write(&bin)
                    .await
                    .with_context(|| "failed to save binary")?;
            }
            babel_api::utils::Binary::Checksum(checksum) => {
                expected_checksum = Some(checksum);
            }
        }
    }
    writer
        .flush()
        .await
        .with_context(|| "failed to save binary")?;
    let expected_checksum =
        expected_checksum.with_context(|| "incomplete binary stream - missing checksum")?;

    let checksum = file_checksum(bin_path)
        .await
        .with_context(|| "failed to calculate binary checksum")?;

    if expected_checksum != checksum {
        bail!(
            "received binary checksum ({checksum})\
                 doesn't match expected ({expected_checksum})"
        );
    }
    Ok(checksum)
}

fn into_sysinfo_signal(posix: PosixSignal) -> Signal {
    match posix {
        PosixSignal::SIGABRT => Signal::Abort,
        PosixSignal::SIGALRM => Signal::Alarm,
        PosixSignal::SIGBUS => Signal::Bus,
        PosixSignal::SIGCHLD => Signal::Child,
        PosixSignal::SIGCLD => Signal::Child,
        PosixSignal::SIGCONT => Signal::Continue,
        PosixSignal::SIGEMT => Signal::Trap,
        PosixSignal::SIGFPE => Signal::FloatingPointException,
        PosixSignal::SIGHUP => Signal::Hangup,
        PosixSignal::SIGILL => Signal::Illegal,
        PosixSignal::SIGINFO => Signal::Power,
        PosixSignal::SIGINT => Signal::Interrupt,
        PosixSignal::SIGIO => Signal::IO,
        PosixSignal::SIGIOT => Signal::IOT,
        PosixSignal::SIGKILL => Signal::Kill,
        PosixSignal::SIGPIPE => Signal::Pipe,
        PosixSignal::SIGPOLL => Signal::Poll,
        PosixSignal::SIGPROF => Signal::Profiling,
        PosixSignal::SIGPWR => Signal::Power,
        PosixSignal::SIGQUIT => Signal::Quit,
        PosixSignal::SIGSEGV => Signal::Segv,
        PosixSignal::SIGSTOP => Signal::Stop,
        PosixSignal::SIGTSTP => Signal::TSTP,
        PosixSignal::SIGSYS => Signal::Sys,
        PosixSignal::SIGTERM => Signal::Term,
        PosixSignal::SIGTRAP => Signal::Trap,
        PosixSignal::SIGTTIN => Signal::TTIN,
        PosixSignal::SIGTTOU => Signal::TTOU,
        PosixSignal::SIGUNUSED => Signal::Sys,
        PosixSignal::SIGURG => Signal::Urgent,
        PosixSignal::SIGUSR1 => Signal::User1,
        PosixSignal::SIGUSR2 => Signal::User2,
        PosixSignal::SIGVTALRM => Signal::VirtualAlarm,
        PosixSignal::SIGXCPU => Signal::XCPU,
        PosixSignal::SIGXFSZ => Signal::XFSZ,
        PosixSignal::SIGWINCH => Signal::Winch,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use assert_fs::TempDir;
    use eyre::Result;
    use std::{fs, io::Write, os::unix::fs::OpenOptionsExt};
    use tokio::process::Command;

    async fn wait_for_process(control_file: &Path) {
        // asynchronously wait for dummy babel to start
        tokio::time::timeout(Duration::from_secs(3), async {
            while !control_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[allow(clippy::write_literal)]
    pub fn create_dummy_bin(path: &Path, ctrl_file: &Path, wait_for_sigterm: bool) {
        let _ = fs::remove_file(path);
        let mut babel = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .mode(0o770)
            .open(path)
            .unwrap();
        writeln!(babel, "#!/bin/bash").unwrap();
        writeln!(babel, "touch {}", ctrl_file.to_string_lossy()).unwrap();
        if wait_for_sigterm {
            writeln!(
                babel,
                "{}",
                r#"
                trap "exit" SIGINT SIGTERM SIGKILL
                while true
                do
                    sleep 0.1
                done
                "#
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn test_kill_all_processes() -> Result<()> {
        let tmp_root = TempDir::new()?.to_path_buf();
        fs::create_dir_all(&tmp_root)?;
        let ctrl_file = tmp_root.join("cmd_started");
        let cmd_path = tmp_root.join("test_cmd");
        create_dummy_bin(&cmd_path, &ctrl_file, true);

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("{} a b c", cmd_path.display()));
        let child = cmd.spawn()?;
        let pid = child.id().unwrap();
        wait_for_process(&ctrl_file).await;
        kill_all_processes(
            &cmd_path.to_string_lossy(),
            &["a", "b", "c"],
            Duration::from_secs(3),
            PosixSignal::SIGTERM,
        );
        tokio::time::timeout(Duration::from_secs(60), async {
            while is_process_running(pid) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_save_bin_stream() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        fs::create_dir_all(&tmp_dir)?;
        let file_path = tmp_dir.join("test_file");

        let incomplete_bin = vec![
            Ok(babel_api::utils::Binary::Bin(vec![
                1, 2, 3, 4, 6, 7, 8, 9, 10,
            ])),
            Ok(babel_api::utils::Binary::Bin(vec![
                11, 12, 13, 14, 16, 17, 18, 19, 20,
            ])),
            Ok(babel_api::utils::Binary::Bin(vec![
                21, 22, 23, 24, 26, 27, 28, 29, 30,
            ])),
        ];

        let _ = save_bin_stream(&file_path, &mut tokio_stream::iter(incomplete_bin.clone()))
            .await
            .unwrap_err();
        let mut invalid_bin = incomplete_bin.clone();
        invalid_bin.push(Ok(babel_api::utils::Binary::Checksum(123)));
        let _ = save_bin_stream(&file_path, &mut tokio_stream::iter(invalid_bin.clone()))
            .await
            .unwrap_err();
        let mut correct_bin = incomplete_bin.clone();
        correct_bin.push(Ok(babel_api::utils::Binary::Checksum(4135829304)));
        assert_eq!(
            4135829304,
            save_bin_stream(&file_path, &mut tokio_stream::iter(correct_bin.clone())).await?
        );
        assert_eq!(4135829304, file_checksum(&file_path).await.unwrap());
        Ok(())
    }

    #[tokio::test]
    async fn test_file_checksum() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        fs::create_dir_all(&tmp_dir)?;
        let file_path = tmp_dir.join("test_file");
        let _ = file_checksum(&file_path).await.unwrap_err();
        fs::write(&file_path, "dummy content")?;
        assert_eq!(2134916024, file_checksum(&file_path).await?);
        Ok(())
    }
}

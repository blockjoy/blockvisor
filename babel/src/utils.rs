use bv_utils::{run_flag::RunFlag, system::is_process_running, timer::AsyncTimer};
use eyre::{bail, Context, ContextCompat};
use futures::StreamExt;
use std::{
    collections::HashMap,
    fs,
    path::Path,
    process::Output,
    time::{Duration, Instant},
};
use sysinfo::{Pid, PidExt, Process, ProcessExt, Signal, System, SystemExt};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
};
use tokio_stream::Stream;
use tonic::Status;

const ENV_BV_USER: &str = "BV_USER";
const PROCESS_SIGNAL_TIMEOUT: Duration = Duration::from_secs(30);
const PROCESS_SIGNAL_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// User to run sh commands and long running jobs
fn bv_user() -> Option<String> {
    std::env::var(ENV_BV_USER).ok()
}

/// Build shell command in form of cmd and args, in order to run something in shell
///
/// If we want to run as custom user, we will be using `su`, otherwise just `sh`
pub fn bv_shell(body: &str) -> (String, Vec<String>) {
    if let Some(user) = bv_user() {
        (
            "su".to_owned(),
            vec!["-".to_owned(), user, "-c".to_owned(), body.to_owned()],
        )
    } else {
        ("sh".to_owned(), vec!["-c".to_owned(), body.to_owned()])
    }
}

/// Kill all processes that match `cmd` and passed `args`.
///
/// TODO: (maybe) try to use &[&str] instead of Vec<String>
pub fn kill_all_processes(cmd: &str, args: Vec<String>, force: bool) {
    let mut sys = System::new();
    sys.refresh_processes();
    let ps = sys.processes();

    let remnants = find_processes(cmd, args, ps);
    for (_, proc) in remnants {
        kill_process_tree(proc, ps, force);
    }
}

/// Kill process and all its descendents.
fn kill_process_tree(proc: &Process, ps: &HashMap<Pid, Process>, force: bool) {
    // Better to kill parent first, since it may implement some child restart mechanism.
    if force {
        // Just kill the process
        proc.kill();
        proc.wait();
    } else {
        // Try to interrupt the process, and kill it after timeout in case it did not finish
        proc.kill_with(Signal::Term);
        let now = std::time::Instant::now();
        while is_process_running(proc.pid().as_u32()) {
            if now.elapsed() < PROCESS_SIGNAL_TIMEOUT {
                std::thread::sleep(PROCESS_SIGNAL_RETRY_INTERVAL)
            } else {
                proc.kill();
                proc.wait();
            }
        }
    }
    let children = ps.iter().filter(|(_, p)| p.parent() == Some(proc.pid()));
    for (_, child) in children {
        kill_process_tree(child, ps, force);
    }
}

/// Find all processes that match `cmd` and passed `args`.
pub fn find_processes<'a>(
    cmd: &'a str,
    args: Vec<String>,
    ps: &'a HashMap<Pid, Process>,
) -> impl Iterator<Item = (&'a Pid, &'a Process)> {
    ps.iter().filter(move |(_, process)| {
        let proc_call = process.cmd();
        if let Some(proc_cmd) = proc_call.first() {
            // if not a binary, but a script (with shebang) is executed,
            // then the process looks like: /bin/sh ./lalala.sh
            // TODO: consider matching not only /bin/sh but other kinds of interpreters
            if proc_cmd == "/bin/sh" {
                // first element is shell, second is cmd, rest are arguments
                proc_call.len() > 1 && cmd == proc_call[1] && args == proc_call[2..]
            } else {
                // first element is cmd, rest are arguments
                cmd == proc_cmd && args == proc_call[1..]
            }
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

pub async fn mount_drive(drive: &str, dir: &str) -> eyre::Result<Output> {
    fs::create_dir_all(dir)?;
    tokio::process::Command::new("mount")
        .args([drive, dir])
        .output()
        .await
        .with_context(|| "failed to mount drive")
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::TempDir;
    use eyre::Result;
    use std::{fs, io::Write, os::unix::fs::OpenOptionsExt};
    use sysinfo::SystemExt;
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

    #[tokio::test]
    async fn test_kill_remnants() -> Result<()> {
        let tmp_root = TempDir::new()?.to_path_buf();
        fs::create_dir_all(&tmp_root)?;
        let ctrl_file = tmp_root.join("cmd_started");
        let cmd_path = tmp_root.join("test_cmd");
        {
            let mut cmd_file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .mode(0o770)
                .open(&cmd_path)?;
            writeln!(cmd_file, "#!/bin/sh")?;
            writeln!(cmd_file, "touch {}", ctrl_file.to_string_lossy())?;
            writeln!(cmd_file, "sleep infinity")?;
        }

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("{} a b c", cmd_path.display()));
        let child = cmd.spawn()?;
        let pid = child.id().unwrap();
        wait_for_process(&ctrl_file).await;
        kill_all_processes(
            &cmd_path.to_string_lossy(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            true,
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

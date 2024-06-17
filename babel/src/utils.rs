use babel_api::engine::FileLocation;
use bv_utils::{exp_backoff_timeout, run_flag::RunFlag, timer::AsyncTimer};
use eyre::{bail, Context, ContextCompat};
use futures::StreamExt;
use nu_glob::Pattern;
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
};
use tokio_stream::Stream;
use tonic::Status;

const ENV_BV_USER: &str = "BV_USER";

/// User to run sh commands and long running jobs
fn bv_user() -> Option<String> {
    std::env::var(ENV_BV_USER).ok()
}

/// Build shell command in form of cmd and args, in order to run something in shell
///
/// If we want to run as custom user, we will be using `su`, otherwise just `sh`
pub fn bv_shell(body: &str) -> (&str, Vec<String>) {
    let body = body.trim().to_owned();
    if let Some(user) = bv_user() {
        ("su", vec!["-".to_owned(), user, "-c".to_owned(), body])
    } else {
        ("sh", vec!["-c".to_owned(), body])
    }
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
        let sleep = self
            .timer
            .sleep(exp_backoff_timeout(self.backoff_base_ms, self.counter));
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
        .create(true)
        .truncate(true)
        .open(bin_path)
        .await
        .with_context(|| format!("failed to open binary file '{}'", bin_path.display()))?;
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

/// Prepare list of all source files, recursively walking down the source directory.
pub fn sources_list(
    source_path: &Path,
    exclude: &[Pattern],
) -> eyre::Result<(u64, Vec<FileLocation>)> {
    let mut sources: Vec<_> = Default::default();
    let mut total_size = 0;
    'sources: for entry in walkdir::WalkDir::new(source_path) {
        let entry = entry?;
        let path = entry.path();

        if let Some(relative_path) = pathdiff::diff_paths(path, source_path) {
            for pattern in exclude {
                if pattern.matches(&relative_path.to_string_lossy()) {
                    continue 'sources;
                }
            }
        }

        if path.is_file() {
            let size = entry.metadata()?.len();
            sources.push(FileLocation {
                path: path.to_path_buf(),
                pos: 0,
                size,
            });
            total_size += size
        }
    }
    Ok((total_size, sources))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::pal::BabelEngineConnector;
    use assert_fs::TempDir;
    use babel_api::engine::{PosixSignal, UploadManifest};
    use babel_api::{babel::babel_engine_client::BabelEngineClient, engine::DownloadManifest};
    use eyre::Result;
    use mockall::mock;
    use std::path::PathBuf;
    use std::{fs, io::Write, os::unix::fs::OpenOptionsExt};
    use sysinfo::{Pid, PidExt};
    use tokio::process::Command;
    use tonic::codegen::InterceptedService;
    use tonic::{transport::Channel, Request, Response, Status};

    mock! {
        pub BabelEngine {}

        #[tonic::async_trait]
        impl babel_api::babel::babel_engine_server::BabelEngine for BabelEngine {
            async fn put_download_manifest(&self, request: Request<DownloadManifest>) -> Result<Response<()>, Status>;
            async fn get_download_manifest(&self, request: Request<()>) -> Result<Response<DownloadManifest>, Status>;
            async fn get_upload_manifest(&self, request: Request<(u32, u32, Option<u64>)>) -> Result<Response<UploadManifest>, Status>;
            async fn upgrade_blocking_jobs_finished(&self, request: Request<()>) -> Result<Response<()>, Status>;
            async fn bv_error(&self, request: Request<String>) -> Result<Response<()>, Status>;
        }
    }

    pub struct DummyConnector {
        pub tmp_dir: PathBuf,
    }

    impl BabelEngineConnector for DummyConnector {
        fn connect(
            &self,
        ) -> BabelEngineClient<InterceptedService<Channel, bv_utils::rpc::DefaultTimeout>> {
            BabelEngineClient::with_interceptor(
                bv_tests_utils::rpc::test_channel(&self.tmp_dir),
                bv_utils::rpc::DefaultTimeout(Duration::from_secs(1)),
            )
        }
    }

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
            .truncate(true)
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
        bv_utils::system::kill_all_processes(
            &cmd_path.to_string_lossy(),
            &["a", "b", "c"],
            Duration::from_secs(3),
            PosixSignal::SIGTERM,
        );
        tokio::time::timeout(Duration::from_secs(60), async {
            while bv_utils::system::is_process_running(Pid::from_u32(pid)) {
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

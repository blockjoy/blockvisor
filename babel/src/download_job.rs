/// This module implements job runner for downloading data. It downloads data according to given
/// manifest and destination dir. In case of recoverable errors download is retried according to given
/// `RestartPolicy`, with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset if download continue without errors for at least `backoff_timeout_ms`.
use crate::{
    checksum,
    compression::{Coder, NoCoder, ZstdDecoder},
    job_runner::{ConnectionPool, JobBackoff, JobRunner, JobRunnerImpl, TransferConfig},
    jobs::{cleanup_job_data, load_job_data, save_job_data},
};
use async_trait::async_trait;
use babel_api::engine::{
    Checksum, Chunk, Compression, DownloadManifest, FileLocation, JobProgress, JobStatus,
    RestartPolicy,
};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer, with_retry};
use eyre::{anyhow, bail, ensure, Context, Result};
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use reqwest::header::RANGE;
use std::sync::Arc;
use std::{
    cmp::min,
    collections::{hash_map::Entry, HashMap, HashSet},
    fs,
    fs::File,
    io::Write,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    slice::Iter,
    usize,
};
use sysinfo::{DiskExt, System, SystemExt};
use tokio::sync::Semaphore;
use tokio::task::JoinError;
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};
use tracing::{error, info};

pub fn cleanup_job(parts_file_path: &Path, chunks: Option<(&Path, &Vec<Chunk>)>) {
    cleanup_job_data(parts_file_path);
    if let Some((destination_dir, chunks)) = chunks {
        for chunk in chunks {
            for destination in &chunk.destinations {
                let _ = fs::remove_file(destination_dir.join(&destination.path));
            }
        }
    }
}

pub struct DownloadJob<T> {
    downloader: Downloader,
    restart_policy: RestartPolicy,
    timer: T,
}

struct Downloader {
    manifest: DownloadManifest,
    destination_dir: PathBuf,
    config: TransferConfig,
}

impl<T: AsyncTimer + Send> DownloadJob<T> {
    pub fn new(
        timer: T,
        manifest: DownloadManifest,
        destination_dir: PathBuf,
        restart_policy: RestartPolicy,
        config: TransferConfig,
    ) -> Result<Self> {
        Ok(Self {
            downloader: Downloader {
                manifest,
                destination_dir,
                config,
            },
            restart_policy,
            timer,
        })
    }

    pub async fn run(self, run: RunFlag, name: &str, jobs_dir: &Path) -> JobStatus {
        let parts_file_path = self.downloader.config.parts_file_path.clone();
        let destination_dir = self.downloader.destination_dir.clone();
        let chunks = self.downloader.manifest.chunks.clone();
        let job_status = <Self as JobRunner>::run(self, run, name, jobs_dir).await;
        match &job_status {
            JobStatus::Finished {
                exit_code: Some(0), ..
            }
            | JobStatus::Stopped
            | JobStatus::Pending
            | JobStatus::Running => {
                // job finished successfully or is going to be continued after restart, so do nothing
            }
            JobStatus::Finished { .. } => {
                // job failed - remove both parts metadata and partially downloaded files
                cleanup_job(&parts_file_path, Some((&destination_dir, &chunks)));
            }
        }
        job_status
    }
}

#[async_trait]
impl<T: AsyncTimer + Send> JobRunnerImpl for DownloadJob<T> {
    /// Run and restart downloader until `backoff.stopped` return `JobStatus` or job runner
    /// is stopped explicitly.  
    async fn try_run_job(mut self, mut run: RunFlag, name: &str) -> Result<(), JobStatus> {
        info!(
            "download job '{name}' started, with manifest: {:?}",
            self.downloader.manifest
        );

        let mut backoff = JobBackoff::new(name, self.timer, run.clone(), &self.restart_policy);
        while run.load() {
            backoff.start();
            match self.downloader.download(run.clone()).await {
                Ok(_) => {
                    let message = format!("download job '{name}' finished");
                    backoff.stopped(Some(0), message).await?;
                }
                Err(err) => {
                    backoff
                        .stopped(
                            Some(-1),
                            format!("download_job '{name}' failed with: {err:#}"),
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }
}

impl Downloader {
    async fn download(&mut self, mut run: RunFlag) -> Result<()> {
        let downloaded_chunks: HashSet<String> = load_job_data(&self.config.parts_file_path);
        self.check_disk_space(&downloaded_chunks)?;
        let (tx, rx) = mpsc::channel(self.config.max_runners);
        let mut parallel_downloaders_run = run.child_flag();
        let writer = self.init_writer(
            parallel_downloaders_run.clone(),
            downloaded_chunks.clone(),
            rx,
        );

        let mut downloaders = ParallelChunkDownloaders::new(
            parallel_downloaders_run.clone(),
            tx,
            downloaded_chunks,
            &self.manifest.chunks,
            self.config.clone(),
        );
        let mut downloaders_result = Ok(());
        loop {
            if parallel_downloaders_run.load() {
                downloaders.launch_more();
            }
            match downloaders.wait_for_next().await {
                Some(Err(err)) => {
                    if downloaders_result.is_ok() {
                        downloaders_result = Err(err);
                        parallel_downloaders_run.stop();
                    }
                }
                Some(Ok(_)) => {}
                None => break,
            }
        }
        downloaders_result?;
        drop(downloaders); // drop last sender so writer know that download is done

        writer.await??;
        if !run.load() {
            bail!("download interrupted");
        }
        cleanup_job(&self.config.parts_file_path, None);
        Ok(())
    }

    fn check_disk_space(&self, downloaded_chunks: &HashSet<String>) -> Result<()> {
        let mut sys = System::new_all();
        sys.refresh_all();
        let available_space = bv_utils::system::find_disk_by_path(&sys, &self.destination_dir)
            .map(|disk| disk.available_space())
            .ok_or_else(|| anyhow!("Cannot get available disk space"))?;
        let downloaded_bytes = self.manifest.chunks.iter().fold(0, |acc, item| {
            if downloaded_chunks.contains(&item.key) {
                acc + item.size
            } else {
                acc
            }
        });
        if self.manifest.total_size - downloaded_bytes > available_space {
            bail!(
                "Can't download {} bytes of data while only {} available",
                self.manifest.total_size,
                available_space
            )
        }
        Ok(())
    }

    fn init_writer(
        &self,
        mut run: RunFlag,
        downloaded_chunks: HashSet<String>,
        rx: mpsc::Receiver<ChunkData>,
    ) -> JoinHandle<Result<()>> {
        let writer = Writer::new(
            rx,
            self.destination_dir.clone(),
            self.config.max_opened_files,
            self.config.progress_file_path.clone(),
            self.config.parts_file_path.clone(),
            self.manifest.chunks.len(),
            downloaded_chunks,
        );
        tokio::spawn(writer.run(run.clone()))
    }
}

struct ParallelChunkDownloaders<'a> {
    run: RunFlag,
    tx: mpsc::Sender<ChunkData>,
    downloaded_chunks: HashSet<String>,
    config: TransferConfig,
    futures: FuturesUnordered<BoxFuture<'a, Result<Result<()>, JoinError>>>,
    chunks: Iter<'a, Chunk>,
    connection_pool: ConnectionPool,
}

impl<'a> ParallelChunkDownloaders<'a> {
    fn new(
        run: RunFlag,
        tx: mpsc::Sender<ChunkData>,
        downloaded_chunks: HashSet<String>,
        chunks: &'a [Chunk],
        config: TransferConfig,
    ) -> Self {
        let connection_pool = Arc::new(Semaphore::new(config.max_connections));
        Self {
            run,
            tx,
            downloaded_chunks,
            config,
            futures: FuturesUnordered::new(),
            chunks: chunks.iter(),
            connection_pool,
        }
    }

    fn launch_more(&mut self) {
        while self.futures.len() < self.config.max_runners {
            let Some(chunk) = self.chunks.next() else {
                break;
            };
            if self.downloaded_chunks.contains(&chunk.key) {
                // skip chunks successfully downloaded in previous run
                continue;
            }
            let downloader = ChunkDownloader::new(
                chunk.clone(),
                self.tx.clone(),
                self.config.clone(),
                self.connection_pool.clone(),
            );
            self.futures
                .push(Box::pin(tokio::spawn(downloader.run(self.run.clone()))));
        }
    }

    async fn wait_for_next(&mut self) -> Option<Result<()>> {
        self.futures
            .next()
            .await
            .map(|r| r.unwrap_or_else(|err| bail!("{err}")))
    }
}

#[derive(Debug)]
enum ChunkData {
    FilePart {
        path: PathBuf,
        pos: u64,
        data: Vec<u8>,
    },
    EndOfChunk {
        key: String,
    },
}

struct ChunkDownloader {
    chunk: Chunk,
    tx: mpsc::Sender<ChunkData>,
    client: reqwest::Client,
    config: TransferConfig,
    connection_pool: ConnectionPool,
}

impl ChunkDownloader {
    fn new(
        chunk: Chunk,
        tx: mpsc::Sender<ChunkData>,
        config: TransferConfig,
        connection_pool: ConnectionPool,
    ) -> Self {
        Self {
            chunk,
            tx,
            client: reqwest::Client::new(),
            config,
            connection_pool,
        }
    }

    async fn run(self, run: RunFlag) -> Result<()> {
        match self.config.compression {
            None => self.run_with_decoder(run, NoCoder::default()).await,
            Some(Compression::ZSTD(_)) => self.run_with_decoder(run, ZstdDecoder::new()?).await,
        }
    }

    async fn run_with_decoder<D: Coder>(self, run: RunFlag, decoder: D) -> Result<()> {
        match &self.chunk.checksum {
            Checksum::Sha1(checksum) => {
                self.download_chunk(run, decoder, sha1_smol::Sha1::new(), checksum)
                    .await
            }
            Checksum::Sha256(checksum) => {
                self.download_chunk(
                    run,
                    decoder,
                    <sha2::Sha256 as sha2::Digest>::new(),
                    checksum,
                )
                .await
            }
            Checksum::Blake3(checksum) => {
                self.download_chunk(run, decoder, blake3::Hasher::new(), checksum)
                    .await
            }
        }
        .with_context(|| format!("chunk '{}' download failed", self.chunk.key))
    }

    async fn download_chunk<C: checksum::Checksum, D: Coder>(
        &self,
        mut run: RunFlag,
        mut decoder: D,
        mut digest: C,
        expected_checksum: &C::Bytes,
    ) -> Result<()> {
        let chunk_size = usize::try_from(self.chunk.size)?;
        let mut pos = 0;
        let mut destination = DestinationsIter::new(self.chunk.destinations.clone())?;
        while pos < chunk_size {
            if let Some(res) = run
                .select(self.download_part_with_retry(pos, chunk_size))
                .await
            {
                let buffer = res?;
                pos += buffer.len();
                digest.update(&buffer);
                decoder.feed(buffer)?;
                self.send_to_writer(decoder.consume()?, &mut destination)
                    .await?;
            } else {
                bail!("download interrupted");
            }
        }
        let calculated_checksum = digest.into_bytes();
        ensure!(
            &calculated_checksum == expected_checksum,
            "chunk checksum mismatch - expected {expected_checksum:?}, actual {calculated_checksum:?}"
        );
        self.send_to_writer(decoder.finalize()?, &mut destination)
            .await?;
        self.tx
            .send(ChunkData::EndOfChunk {
                key: self.chunk.key.clone(),
            })
            .await?;
        Ok(())
    }

    async fn download_part_with_retry(&self, pos: usize, chunk_size: usize) -> Result<Vec<u8>> {
        with_retry!(
            self.download_part(pos, chunk_size),
            self.config.max_retries,
            self.config.backoff_base_ms
        )
    }

    async fn download_part(&self, pos: usize, chunk_size: usize) -> Result<Vec<u8>> {
        let buffer_size = min(chunk_size - pos, self.config.max_buffer_size);
        let mut buffer = Vec::with_capacity(buffer_size);
        let connection_permit = self.connection_pool.acquire().await?;
        let mut resp = self
            .client
            .get(&self.chunk.url)
            .header(RANGE, format!("bytes={}-{}", pos, pos + buffer_size - 1))
            .send()
            .await?;
        drop(connection_permit);
        ensure!(
            resp.status().is_success(),
            anyhow!("server responded with {}", resp.status())
        );
        while let Some(bytes) = resp.chunk().await? {
            ensure!(
                bytes.len() + buffer.len() <= buffer_size,
                anyhow!("server error: received more bytes than requested")
            );
            buffer.append(&mut bytes.to_vec());
        }
        ensure!(
            buffer.len() == buffer_size,
            anyhow!("server error: received less bytes than requested")
        );
        Ok(buffer)
    }

    /// Map data in the buffer to file parts and sent to writer.
    async fn send_to_writer(
        &self,
        mut buffer: Vec<u8>,
        destination: &mut DestinationsIter,
    ) -> Result<()> {
        // split buffer into file parts and send to writer
        while let Some(next) = destination.next(u64::try_from(buffer.len())?) {
            let reminder = buffer.split_off(usize::try_from(next.size)?);
            self.tx
                .send(ChunkData::FilePart {
                    path: next.path,
                    pos: next.pos,
                    data: buffer,
                })
                .await?;
            buffer = reminder;
        }
        if !buffer.is_empty() {
            bail!("(decompressed) chunk size doesn't mach expected one");
        };
        Ok(())
    }
}

struct DestinationsIter(Vec<FileLocation>);

impl DestinationsIter {
    fn new(mut iter: Vec<FileLocation>) -> Result<Self> {
        if iter.is_empty() {
            error!("corrupted manifest - this is internal BV error, manifest shall be already validated");
            bail!("corrupted manifest - expected at least one destination file in chunk");
        }
        iter.reverse();
        Ok(Self(iter))
    }

    fn next(&mut self, bytes: u64) -> Option<FileLocation> {
        if let Some(current) = self.0.last_mut() {
            if current.size <= bytes {
                self.0.pop()
            } else if bytes > 0 {
                let current_part = FileLocation {
                    path: current.path.clone(),
                    pos: current.pos,
                    size: bytes,
                };
                current.pos += bytes;
                current.size -= bytes;
                Some(current_part)
            } else {
                None
            }
        } else {
            None
        }
    }
}

struct Writer {
    opened_files: HashMap<PathBuf, (Instant, File)>,
    destination_dir: PathBuf,
    rx: mpsc::Receiver<ChunkData>,
    max_opened_files: usize,
    progress_file_path: PathBuf,
    parts_file_path: PathBuf,
    total_chunks_count: usize,
    downloaded_chunks: HashSet<String>,
}

impl Writer {
    fn new(
        rx: mpsc::Receiver<ChunkData>,
        destination_dir: PathBuf,
        max_opened_files: usize,
        progress_file_path: PathBuf,
        parts_file_path: PathBuf,
        total_chunks_count: usize,
        downloaded_chunks: HashSet<String>,
    ) -> Self {
        Self {
            opened_files: HashMap::new(),
            destination_dir,
            rx,
            max_opened_files,
            progress_file_path,
            parts_file_path,
            total_chunks_count,
            downloaded_chunks,
        }
    }

    async fn run(mut self, mut run: RunFlag) -> Result<()> {
        while run.load() {
            let Some(chunk_data) = self.rx.recv().await else {
                // stop writer when all senders/downloaders are dropped
                break;
            };
            if let Err(err) = self.handle_chunk_data(chunk_data).await {
                run.stop();
                bail!("Writer error: {err:#}")
            }
        }
        Ok(())
    }

    async fn handle_chunk_data(&mut self, chunk_data: ChunkData) -> Result<()> {
        match chunk_data {
            ChunkData::FilePart { path, pos, data } => {
                self.write_to_file(path, pos, data).await?;
            }
            ChunkData::EndOfChunk { key } => {
                self.downloaded_chunks.insert(key);
                save_job_data(&self.parts_file_path, &self.downloaded_chunks)?;
                save_job_data(
                    &self.progress_file_path,
                    &JobProgress {
                        total: u32::try_from(self.total_chunks_count)?,
                        current: u32::try_from(self.downloaded_chunks.len())?,
                        message: "chunks".to_string(),
                    },
                )?;
            }
        }
        Ok(())
    }

    async fn write_to_file(&mut self, path: PathBuf, pos: u64, data: Vec<u8>) -> Result<()> {
        // Instant::now() is used (instead of T: AsyncTimer) just for simplicity
        // otherwise we would need to have 2 instances of T (one for job, second for Writer task),
        // or T to be Cloneable, which complicates test code, but doesn't bring a lot of benefits.
        match self.opened_files.entry(path) {
            Entry::Occupied(mut entry) => {
                if !data.is_empty() {
                    // already have opened handle to the file, so just update timestamp
                    let (timestamp, file) = entry.get_mut();
                    *timestamp = Instant::now();
                    file.write_all_at(&data, pos)?;
                    file.flush()?;
                }
            }
            Entry::Vacant(entry) => {
                let absolute_path = self.destination_dir.join(entry.key());
                if let Some(parent) = absolute_path.parent() {
                    if !parent.exists() {
                        fs::create_dir_all(parent)?;
                    }
                }
                // file not opened yet
                let file = File::options()
                    .create(true)
                    .write(true)
                    .open(absolute_path)?;
                if !data.is_empty() {
                    let (_, file) = entry.insert((Instant::now(), file));
                    file.write_all_at(&data, pos)?;
                    file.flush()?;
                }
            }
        }
        if self.opened_files.len() >= self.max_opened_files {
            // can't have to many files opened at the same time, so close one with
            // oldest timestamp
            if let Some(oldest) = self
                .opened_files
                .iter()
                .min_by(|(_, (a, _)), (_, (b, _))| a.cmp(b))
                .map(|(k, _)| k.clone())
            {
                self.opened_files.remove(&oldest);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression::ZstdEncoder;
    use assert_fs::TempDir;
    use babel_api::engine::{Checksum, RestartConfig};
    use bv_utils::timer::SysTimer;
    use mockito::{Server, ServerGuard};
    use std::fs;

    struct TestEnv {
        tmp_dir: PathBuf,
        download_parts_path: PathBuf,
        download_progress_path: PathBuf,
        server: ServerGuard,
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_dir = TempDir::new()?.to_path_buf();
        fs::create_dir_all(&tmp_dir)?;
        let server = Server::new();
        let download_parts_path = tmp_dir.join("download.parts");
        let download_progress_path = tmp_dir.join("download.progress");
        Ok(TestEnv {
            tmp_dir,
            server,
            download_parts_path,
            download_progress_path,
        })
    }

    impl TestEnv {
        fn download_job(&self, manifest: DownloadManifest) -> DownloadJob<SysTimer> {
            DownloadJob {
                downloader: Downloader {
                    manifest,
                    destination_dir: self.tmp_dir.clone(),
                    config: TransferConfig {
                        max_opened_files: 1,
                        max_runners: 4,
                        max_connections: 4,
                        max_buffer_size: 150,
                        max_retries: 0,
                        backoff_base_ms: 1,
                        parts_file_path: self.download_parts_path.clone(),
                        progress_file_path: self.download_progress_path.clone(),
                        compression: None,
                    },
                },
                restart_policy: RestartPolicy::Never,
                timer: SysTimer,
            }
        }

        fn url(&self, path: &str) -> String {
            format!("{}/{}", self.server.url(), path)
        }
    }

    #[tokio::test]
    async fn test_full_download_ok() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let manifest = DownloadManifest {
            total_size: 924,
            compression: None,
            chunks: vec![
                Chunk {
                    key: "first_chunk".to_string(),
                    url: test_env.url("first_chunk"),
                    checksum: Checksum::Blake3([
                        85, 66, 30, 123, 210, 245, 146, 94, 153, 129, 249, 169, 140, 22, 44, 8,
                        190, 219, 61, 95, 17, 159, 253, 17, 201, 75, 37, 225, 103, 226, 202, 150,
                    ]),
                    size: 600,
                    destinations: vec![
                        FileLocation {
                            path: PathBuf::from("zero.file"),
                            pos: 0,
                            size: 100,
                        },
                        FileLocation {
                            path: PathBuf::from("first.file"),
                            pos: 0,
                            size: 200,
                        },
                        FileLocation {
                            path: PathBuf::from("second.file"),
                            pos: 0,
                            size: 300,
                        },
                        FileLocation {
                            path: PathBuf::from("empty.file"),
                            pos: 0,
                            size: 0,
                        },
                    ],
                },
                Chunk {
                    key: "second_chunk".to_string(),
                    url: test_env.url("second_chunk"),
                    checksum: Checksum::Sha256([
                        104, 0, 168, 43, 116, 130, 25, 151, 217, 240, 13, 245, 96, 88, 86, 0, 75,
                        93, 168, 15, 58, 171, 248, 94, 149, 167, 72, 202, 179, 227, 164, 214,
                    ]),
                    size: 324,
                    destinations: vec![FileLocation {
                        path: PathBuf::from("second.file"),
                        pos: 300,
                        size: 324,
                    }],
                },
            ],
        };

        test_env
            .server
            .mock("GET", "/first_chunk")
            .match_header("range", "bytes=0-149")
            .with_header("content-type", "application/octet-stream")
            .with_body([vec![0u8; 100], vec![1u8; 50]].concat())
            .create();
        test_env
            .server
            .mock("GET", "/first_chunk")
            .match_header("range", "bytes=150-299")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![1u8; 150])
            .create();
        test_env
            .server
            .mock("GET", "/first_chunk")
            .match_header("range", "bytes=300-449")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![2u8; 150])
            .create();
        test_env
            .server
            .mock("GET", "/first_chunk")
            .match_header("range", "bytes=450-599")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![2u8; 150])
            .create();
        test_env
            .server
            .mock("GET", "/second_chunk")
            .match_header("range", "bytes=0-149")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![3u8; 150])
            .create();
        test_env
            .server
            .mock("GET", "/second_chunk")
            .match_header("range", "bytes=150-299")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![3u8; 150])
            .create();
        test_env
            .server
            .mock("GET", "/second_chunk")
            .match_header("range", "bytes=300-323")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![3u8; 24])
            .create();
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: "".to_string()
            },
            test_env
                .download_job(manifest)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );

        assert_eq!(
            vec![0u8; 100],
            fs::read(test_env.tmp_dir.join("zero.file"))?
        );
        assert_eq!(
            vec![1u8; 200],
            fs::read(test_env.tmp_dir.join("first.file"))?
        );
        assert_eq!(
            [vec![2u8; 300], vec![3u8; 324]].concat(),
            fs::read(test_env.tmp_dir.join("second.file"))?
        );
        assert_eq!(0, test_env.tmp_dir.join("empty.file").metadata()?.len());
        assert!(!test_env.download_parts_path.exists());
        assert!(test_env.download_progress_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_download_with_compression() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let manifest = DownloadManifest {
            total_size: 924,
            compression: Some(Compression::ZSTD(5)),
            chunks: vec![
                Chunk {
                    key: "first_chunk".to_string(),
                    url: test_env.url("first_chunk"),
                    checksum: Checksum::Blake3([
                        98, 29, 20, 118, 237, 197, 62, 255, 36, 9, 121, 104, 72, 128, 94, 22, 60,
                        213, 118, 168, 232, 175, 160, 41, 157, 152, 131, 73, 147, 183, 91, 246,
                    ]),
                    size: 24,
                    destinations: vec![
                        FileLocation {
                            path: PathBuf::from("zero.file"),
                            pos: 0,
                            size: 100,
                        },
                        FileLocation {
                            path: PathBuf::from("first.file"),
                            pos: 0,
                            size: 200,
                        },
                        FileLocation {
                            path: PathBuf::from("second.file"),
                            pos: 0,
                            size: 300,
                        },
                        FileLocation {
                            path: PathBuf::from("empty.file"),
                            pos: 0,
                            size: 0,
                        },
                    ],
                },
                Chunk {
                    key: "second_chunk".to_string(),
                    url: test_env.url("second_chunk"),
                    checksum: Checksum::Sha256([
                        148, 46, 25, 243, 16, 182, 201, 222, 228, 39, 228, 26, 8, 94, 74, 188, 93,
                        151, 210, 123, 9, 65, 196, 29, 185, 113, 189, 51, 227, 124, 158, 177,
                    ]),
                    size: 18,
                    destinations: vec![FileLocation {
                        path: PathBuf::from("second.file"),
                        pos: 300,
                        size: 324,
                    }],
                },
            ],
        };

        test_env
            .server
            .mock("GET", "/first_chunk")
            .match_header("range", "bytes=0-23")
            .with_status(404)
            .create();
        let mut encoder = ZstdEncoder::new(5)?;
        encoder.feed(vec![0u8; 100])?;
        encoder.feed(vec![1u8; 200])?;
        encoder.feed(vec![2u8; 300])?;
        let compressed_chunk = encoder.finalize()?;
        test_env
            .server
            .mock("GET", "/first_chunk")
            .match_header("range", "bytes=0-23")
            .with_header("content-type", "application/octet-stream")
            .with_body(compressed_chunk)
            .create();
        let mut encoder = ZstdEncoder::new(5)?;
        encoder.feed(vec![3u8; 324])?;
        let compressed_chunk = encoder.finalize()?;
        test_env
            .server
            .mock("GET", "/second_chunk")
            .match_header("range", "bytes=0-17")
            .with_header("content-type", "application/octet-stream")
            .with_body(compressed_chunk)
            .create();
        let mut job = test_env.download_job(manifest);
        job.downloader.config.compression = Some(Compression::ZSTD(5));
        job.restart_policy = RestartPolicy::OnFailure(RestartConfig {
            backoff_timeout_ms: 1000,
            backoff_base_ms: 1,
            max_retries: Some(1),
        });
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: "".to_string()
            },
            job.run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );

        assert_eq!(
            vec![0u8; 100],
            fs::read(test_env.tmp_dir.join("zero.file"))?
        );
        assert_eq!(
            vec![1u8; 200],
            fs::read(test_env.tmp_dir.join("first.file"))?
        );
        assert_eq!(
            [vec![2u8; 300], vec![3u8; 324]].concat(),
            fs::read(test_env.tmp_dir.join("second.file"))?
        );
        assert_eq!(0, test_env.tmp_dir.join("empty.file").metadata()?.len());
        assert!(!test_env.download_parts_path.exists());
        assert!(test_env.download_progress_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_empty_download_ok() -> Result<()> {
        let test_env = setup_test_env()?;

        let manifest = DownloadManifest {
            total_size: 0,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: test_env.url("first_chunk"),
                checksum: Checksum::Blake3([
                    175, 19, 73, 185, 245, 249, 161, 166, 160, 64, 77, 234, 54, 220, 201, 73, 155,
                    203, 37, 201, 173, 193, 18, 183, 204, 154, 147, 202, 228, 31, 50, 98,
                ]),
                size: 0,
                destinations: vec![
                    FileLocation {
                        path: PathBuf::from("empty_1.file"),
                        pos: 0,
                        size: 0,
                    },
                    FileLocation {
                        path: PathBuf::from("empty_2.file"),
                        pos: 0,
                        size: 0,
                    },
                ],
            }],
        };
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: "".to_string()
            },
            test_env
                .download_job(manifest)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );

        assert_eq!(0, test_env.tmp_dir.join("empty_1.file").metadata()?.len());
        assert_eq!(0, test_env.tmp_dir.join("empty_2.file").metadata()?.len());
        assert!(!test_env.download_parts_path.exists());
        assert!(test_env.download_progress_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_invalid_manifest() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let manifest_wo_destination = DownloadManifest {
            total_size: 600,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: test_env.url("first_chunk"),
                checksum: Checksum::Sha1([0u8; 20]),
                size: 600,
                destinations: vec![],
            }],
        };
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "download_job 'name' failed with: chunk 'first_chunk' download failed: corrupted manifest - expected at least one destination file in chunk".to_string()
            },
            test_env
                .download_job(manifest_wo_destination)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );

        let manifest_inconsistent_destination = DownloadManifest {
            total_size: 200,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: test_env.url("chunk"),
                checksum: Checksum::Sha1([0u8; 20]),
                size: 200,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 100,
                }],
            }],
        };
        test_env
            .server
            .mock("GET", "/chunk")
            .match_header("range", "bytes=0-149")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![0u8; 150])
            .create();

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "download_job 'name' failed with: chunk 'first_chunk' download failed: (decompressed) chunk size doesn't mach expected one".to_string()
            },
            test_env
                .download_job(manifest_inconsistent_destination)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_server_errors() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let manifest = DownloadManifest {
            total_size: 100,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: test_env.url("first_chunk"),
                checksum: Checksum::Sha1([0u8; 20]),
                size: 100,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 100,
                }],
            }],
        };

        test_env
            .server
            .mock("GET", "/first_chunk")
            .match_header("range", "bytes=0-99")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![0u8; 150])
            .create();

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "download_job 'name' failed with: chunk 'first_chunk' download failed: server error: received more bytes than requested"
                    .to_string()
            },
            test_env
                .download_job(manifest)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );

        let manifest = DownloadManifest {
            total_size: 100,
            compression: None,
            chunks: vec![Chunk {
                key: "second_chunk".to_string(),
                url: test_env.url("second_chunk"),
                checksum: Checksum::Sha1([0u8; 20]),
                size: 100,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 100,
                }],
            }],
        };

        test_env
            .server
            .mock("GET", "/second_chunk")
            .match_header("range", "bytes=0-99")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![0u8; 50])
            .create();

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "download_job 'name' failed with: chunk 'second_chunk' download failed: server error: received less bytes than requested"
                    .to_string()
            },
            test_env
                .download_job(manifest)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );

        let manifest = DownloadManifest {
            total_size: 100,
            compression: None,
            chunks: vec![Chunk {
                key: "third_chunk".to_string(),
                url: test_env.url("third_chunk"),
                checksum: Checksum::Sha1([0u8; 20]),
                size: 100,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 100,
                }],
            }],
        };

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "download_job 'name' failed with: chunk 'third_chunk' download failed: server responded with 501 Not Implemented"
                    .to_string()
            },
            test_env
                .download_job(manifest)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_restore_download_ok() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let manifest = DownloadManifest {
            total_size: 450,
            compression: None,
            chunks: vec![
                Chunk {
                    key: "first_chunk".to_string(),
                    url: test_env.url("first_chunk"),
                    checksum: Checksum::Sha1([0u8; 20]),
                    size: 150,
                    destinations: vec![
                        FileLocation {
                            path: PathBuf::from("zero.file"),
                            pos: 0,
                            size: 50,
                        },
                        FileLocation {
                            path: PathBuf::from("first.file"),
                            pos: 0,
                            size: 100,
                        },
                    ],
                },
                Chunk {
                    key: "second_chunk".to_string(),
                    url: test_env.url("second_chunk"),
                    checksum: Checksum::Sha1([
                        152, 150, 127, 36, 91, 230, 29, 94, 132, 21, 41, 252, 81, 126, 200, 137,
                        69, 117, 117, 134,
                    ]),
                    size: 150,
                    destinations: vec![FileLocation {
                        path: PathBuf::from("second.file"),
                        pos: 0,
                        size: 150,
                    }],
                },
                Chunk {
                    key: "third_chunk".to_string(),
                    url: test_env.url("third_chunk"),
                    checksum: Checksum::Sha1([
                        250, 1, 243, 45, 249, 202, 133, 7, 222, 104, 232, 37, 207, 74, 223, 126, 2,
                        142, 95, 170,
                    ]),
                    size: 150,
                    destinations: vec![FileLocation {
                        path: PathBuf::from("third.file"),
                        pos: 0,
                        size: 150,
                    }],
                },
            ],
        };

        test_env
            .server
            .mock("GET", "/second_chunk")
            .match_header("range", "bytes=0-149")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![2u8; 150])
            .create();
        test_env
            .server
            .mock("GET", "/third_chunk")
            .match_header("range", "bytes=0-149")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![3u8; 150])
            .create();

        // mark first file as downloaded
        let mut parts = HashSet::new();
        parts.insert("first_chunk");
        fs::write(
            &test_env.download_parts_path,
            serde_json::to_string(&parts)?,
        )?;
        // partially downloaded second file
        fs::write(test_env.tmp_dir.join("second.file"), vec![1u8; 55])?;

        let mut job = test_env.download_job(manifest);
        job.downloader.config.max_runners = 1;
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: "".to_string()
            },
            job.run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );

        assert!(!test_env.tmp_dir.join("zero.file").exists());
        assert_eq!(
            vec![2u8; 150],
            fs::read(test_env.tmp_dir.join("second.file"))?
        );
        assert_eq!(
            vec![3u8; 150],
            fs::read(test_env.tmp_dir.join("third.file"))?
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_not_enough_disk_space() -> Result<()> {
        let test_env = setup_test_env()?;

        let manifest = DownloadManifest {
            total_size: u64::MAX,
            compression: None,
            chunks: vec![],
        };

        if let JobStatus::Finished {
            exit_code: Some(-1),
            message,
        } = test_env
            .download_job(manifest)
            .run(RunFlag::default(), "name", &test_env.tmp_dir)
            .await
        {
            assert!(message.starts_with(&format!(
                "download_job 'name' failed with: Can't download {} bytes of data while only",
                u64::MAX
            )));
        } else {
            bail!("unexpected success")
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_writer_error() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let manifest = DownloadManifest {
            total_size: 100,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: test_env.url("first_chunk"),
                checksum: Checksum::Sha1([
                    237, 74, 119, 209, 181, 106, 17, 137, 56, 120, 143, 197, 48, 55, 117, 155, 108,
                    80, 30, 61,
                ]),
                size: 100,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 100,
                }],
            }],
        };

        test_env
            .server
            .mock("GET", "/first_chunk")
            .match_header("range", "bytes=0-99")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![0u8; 100])
            .create();

        let job = test_env.download_job(manifest);
        let mut perms = fs::metadata(&job.downloader.destination_dir)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(&job.downloader.destination_dir, perms)?;

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message:
                    "download_job 'name' failed with: Writer error: Permission denied (os error 13)"
                        .to_string()
            },
            job.run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_cleanup_after_fail() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let manifest = DownloadManifest {
            total_size: 924,
            compression: None,
            chunks: vec![
                Chunk {
                    key: "first_chunk".to_string(),
                    url: test_env.url("first_chunk"),
                    checksum: Checksum::Blake3([
                        85, 66, 30, 123, 210, 245, 146, 94, 153, 129, 249, 169, 140, 22, 44, 8,
                        190, 219, 61, 95, 17, 159, 253, 17, 201, 75, 37, 225, 103, 226, 202, 150,
                    ]),
                    size: 600,
                    destinations: vec![
                        FileLocation {
                            path: PathBuf::from("zero.file"),
                            pos: 0,
                            size: 100,
                        },
                        FileLocation {
                            path: PathBuf::from("first.file"),
                            pos: 0,
                            size: 200,
                        },
                        FileLocation {
                            path: PathBuf::from("second.file"),
                            pos: 0,
                            size: 300,
                        },
                    ],
                },
                Chunk {
                    key: "second_chunk".to_string(),
                    url: test_env.url("second_chunk"),
                    checksum: Checksum::Sha1([0u8; 20]),
                    size: 324,
                    destinations: vec![FileLocation {
                        path: PathBuf::from("third.file"),
                        pos: 0,
                        size: 324,
                    }],
                },
            ],
        };

        test_env
            .server
            .mock("GET", "/first_chunk")
            .match_header("range", "bytes=0-149")
            .with_header("content-type", "application/octet-stream")
            .with_body([vec![0u8; 100], vec![1u8; 50]].concat())
            .create();
        test_env
            .server
            .mock("GET", "/first_chunk")
            .match_header("range", "bytes=150-299")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![1u8; 150])
            .create();
        test_env
            .server
            .mock("GET", "/first_chunk")
            .match_header("range", "bytes=300-449")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![2u8; 150])
            .create();
        test_env
            .server
            .mock("GET", "/first_chunk")
            .match_header("range", "bytes=450-599")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![2u8; 150])
            .create();
        test_env
            .server
            .mock("GET", "/second_chunk")
            .match_header("range", "bytes=0-149")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![3u8; 150])
            .create();
        test_env
            .server
            .mock("GET", "/second_chunk")
            .match_header("range", "bytes=150-299")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![3u8; 150])
            .create();
        test_env
            .server
            .mock("GET", "/second_chunk")
            .match_header("range", "bytes=300-323")
            .with_header("content-type", "application/octet-stream")
            .with_body(vec![3u8; 24])
            .create();

        let mut job = test_env.download_job(manifest);
        job.downloader.config.max_runners = 1;
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "download_job 'name' failed with: chunk 'second_chunk' download failed: chunk checksum mismatch - expected [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], actual [223, 133, 134, 120, 124, 112, 193, 150, 43, 123, 78, 114, 164, 121, 55, 99, 61, 88, 63, 101]".to_string()
            },
            job.run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );

        assert!(!test_env.tmp_dir.join("zero.file").exists());
        assert!(!test_env.tmp_dir.join("first.file").exists());
        assert!(!test_env.tmp_dir.join("second.file").exists());
        assert!(!test_env.tmp_dir.join("third.file").exists());
        assert!(!test_env.download_parts_path.exists());
        assert!(test_env.download_progress_path.exists());
        let progress = fs::read_to_string(&test_env.download_progress_path).unwrap();
        assert_eq!(&progress, r#"{"total":2,"current":1,"message":"chunks"}"#);
        Ok(())
    }
}

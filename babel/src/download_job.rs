use crate::job_runner::Runner;
/// This module implements job runner for downloading data. It downloads data according to given
/// manifest and destination dir. In case of recoverable errors download is retried according to given
/// `RestartPolicy`, with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset if download continue without errors for at least `backoff_timeout_ms`.
use crate::{
    checksum,
    compression::{Coder, NoCoder, ZstdDecoder},
    job_runner::{ConnectionPool, TransferConfig},
    jobs,
    jobs::{load_job_data, save_job_data},
    pal::BabelEngineConnector,
};
use async_trait::async_trait;
use babel_api::engine::{
    Checksum, Chunk, Compression, DownloadManifest, FileLocation, JobProgress,
};
use bv_utils::{run_flag::RunFlag, with_retry};
use eyre::{anyhow, bail, ensure, Context, Result};
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use reqwest::header::RANGE;
use std::sync::Arc;
use std::time::Duration;
use std::{
    cmp::min,
    collections::{hash_map::Entry, HashMap, HashSet},
    fs,
    fs::File,
    io::Write,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    slice::Iter,
};
use tokio::sync::Semaphore;
use tokio::task::JoinError;
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};
use tracing::error;

// if downloading single part (about 100Mb) takes more than 10min, it mean that something
// is not ok
const DOWNLOAD_SINGLE_PART_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const COMPLETED_FILENAME: &str = "download.completed";
const PARTS_FILENAME: &str = "download.parts";
const MANIFEST_FILENAME: &str = "manifest.json";

pub fn cleanup_job(meta_dir: &Path, destination_dir: &Path) -> Result<()> {
    let manifest_path = meta_dir.join(MANIFEST_FILENAME);
    if manifest_path.exists() {
        remove_remnants(
            load_job_data::<DownloadManifest>(&manifest_path)?,
            destination_dir,
        )?;
        remove_manifest(&manifest_path)?;
    }
    Ok(())
}

pub fn is_download_completed() -> bool {
    jobs::ARCHIVE_JOBS_META_DIR
        .join(COMPLETED_FILENAME)
        .exists()
}

pub fn remove_manifest(parts_path: &Path) -> Result<()> {
    fs::remove_file(parts_path).with_context(|| {
        format!(
            "failed to cleanup download parts file `{}`",
            parts_path.display()
        )
    })?;
    Ok(())
}

pub fn remove_remnants(manifest: DownloadManifest, destination_dir: &Path) -> Result<()> {
    for chunk in manifest.chunks {
        for destination in &chunk.destinations {
            let file_path = destination_dir.join(&destination.path);
            if file_path.exists() {
                fs::remove_file(&file_path).with_context(|| {
                    format!("failed to remove remnant file '{}'", file_path.display())
                })?;
            }
        }
    }
    Ok(())
}

pub struct Downloader<C> {
    pub connector: C,
    pub manifest: DownloadManifest,
    pub destination_dir: PathBuf,
    pub config: TransferConfig,
}

#[async_trait]
impl<C: BabelEngineConnector + Copy + Send + Sync + 'static> Runner for Downloader<C> {
    async fn run(&mut self, mut run: RunFlag) -> Result<()> {
        let completed_path = self.config.archive_jobs_meta_dir.join(COMPLETED_FILENAME);
        if completed_path.exists() {
            // download job shall be idempotent, to avoid downloading everything again after upgrade
            return Ok(());
        }
        let downloaded_chunks = self.try_resume_or_cleanup()?;
        self.check_disk_space(&downloaded_chunks)?;
        let (tx, rx) = mpsc::channel(self.config.max_runners);
        let mut parallel_downloaders_run = run.child_flag();
        let writer = self.init_writer(
            parallel_downloaders_run.clone(),
            downloaded_chunks.clone(),
            rx,
        );

        let mut downloaders = ParallelChunkDownloaders::new(
            self.connector,
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
        drop(downloaders); // drop last sender so writer know that download is done

        writer.await??;
        downloaders_result?;
        self.unify_file_times()?;
        if !run.load() {
            bail!("download interrupted");
        }
        File::create(completed_path)?;
        Ok(())
    }
}

impl<C: BabelEngineConnector + Copy + Send + Sync + 'static> Downloader<C> {
    pub async fn prepare(
        connector: C,
        destination_dir: PathBuf,
        mut config: TransferConfig,
    ) -> Result<Self> {
        let manifest = connector
            .connect()
            .get_download_manifest(())
            .await?
            .into_inner();
        manifest.validate()?;
        config.compression = manifest.compression;
        Ok(Self {
            connector,
            manifest,
            destination_dir,
            config,
        })
    }

    fn try_resume_or_cleanup(&self) -> Result<HashSet<String>> {
        let mut downloaded_chunks = Default::default();
        let manifest_path = self.config.archive_jobs_meta_dir.join(MANIFEST_FILENAME);
        if manifest_path.exists() {
            let manifest = load_job_data::<DownloadManifest>(&manifest_path)?;
            if self.is_manifest_compatible(&manifest) {
                downloaded_chunks =
                    load_job_data(&self.config.archive_jobs_meta_dir.join(PARTS_FILENAME))
                        .unwrap_or_default();
            } else {
                remove_remnants(manifest, &self.destination_dir)?;
                save_job_data(&manifest_path, &self.manifest)?;
            }
        } else {
            save_job_data(&manifest_path, &self.manifest)?;
        }
        Ok(downloaded_chunks)
    }

    fn is_manifest_compatible(&self, old_manifest: &DownloadManifest) -> bool {
        old_manifest.total_size == self.manifest.total_size
            && old_manifest.compression == self.manifest.compression
            && old_manifest.chunks.iter().all(|old_chunk| {
                self.manifest
                    .chunks
                    .iter()
                    .find(|chunk| old_chunk.key == chunk.key)
                    .is_some_and(|chunk| {
                        old_chunk.size == chunk.size && old_chunk.checksum == chunk.checksum
                            || old_chunk.destinations == chunk.destinations
                    })
            })
    }

    fn check_disk_space(&self, downloaded_chunks: &HashSet<String>) -> Result<()> {
        let available_space =
            bv_utils::system::available_disk_space_by_path(&self.destination_dir)?;

        let required_space = required_disk_space(downloaded_chunks, &self.manifest)?;
        if required_space > available_space {
            bail!(
                "Can't download {} bytes of data while only {} available",
                required_space,
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
            self.config.archive_jobs_meta_dir.join(PARTS_FILENAME),
            self.manifest.chunks.len(),
            downloaded_chunks,
        );
        tokio::spawn(writer.run(run.clone()))
    }

    /// Update all downloaded files times with the same value.
    fn unify_file_times(&self) -> Result<()> {
        let now = std::time::SystemTime::now();
        let times = fs::FileTimes::new().set_accessed(now).set_modified(now);
        let paths = self
            .manifest
            .chunks
            .iter()
            .fold(HashSet::new(), |paths, chunk| {
                chunk.destinations.iter().fold(paths, |mut paths, dest| {
                    paths.insert(&dest.path);
                    paths
                })
            });
        for path in paths {
            let full_path = self.destination_dir.join(path);
            let file = File::options()
                .write(true)
                .open(&full_path)
                .with_context(|| format!("can't open `{}` to change times", full_path.display()))?;
            file.set_times(times)
                .with_context(|| format!("can't set times for `{}`", full_path.display()))?;
        }
        Ok(())
    }
}

fn required_disk_space(
    downloaded_chunks: &HashSet<String>,
    manifest: &DownloadManifest,
) -> Result<u64> {
    let downloaded_bytes = manifest.chunks.iter().fold(0, |acc, item| {
        if downloaded_chunks.contains(&item.key) {
            acc + item
                .destinations
                .iter()
                .fold(0, |acc, destination| acc + destination.size)
        } else {
            acc
        }
    });
    ensure!(
        manifest.total_size >= downloaded_bytes,
        anyhow!(
            "invalid download manifest - total_size {} is smaller than already downloaded data {}",
            manifest.total_size,
            downloaded_bytes
        )
    );
    Ok(manifest.total_size - downloaded_bytes)
}

struct ParallelChunkDownloaders<'a, C> {
    connector: C,
    run: RunFlag,
    tx: mpsc::Sender<ChunkData>,
    downloaded_chunks: HashSet<String>,
    config: TransferConfig,
    futures: FuturesUnordered<BoxFuture<'a, Result<Result<()>, JoinError>>>,
    chunks: Iter<'a, Chunk>,
    connection_pool: ConnectionPool,
}

impl<'a, C: BabelEngineConnector + Copy + Send + Sync + 'static> ParallelChunkDownloaders<'a, C> {
    fn new(
        connector: C,
        run: RunFlag,
        tx: mpsc::Sender<ChunkData>,
        downloaded_chunks: HashSet<String>,
        chunks: &'a [Chunk],
        config: TransferConfig,
    ) -> Self {
        let connection_pool = Arc::new(Semaphore::new(config.max_connections));
        Self {
            connector,
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
                self.connector,
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
            .map(|r| r.unwrap_or_else(|err| bail!("{err:#}")))
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

struct ChunkDownloader<C> {
    connector: C,
    chunk: Chunk,
    tx: mpsc::Sender<ChunkData>,
    client: reqwest::Client,
    config: TransferConfig,
    connection_pool: ConnectionPool,
}

impl<C: BabelEngineConnector> ChunkDownloader<C> {
    fn new(
        connector: C,
        chunk: Chunk,
        tx: mpsc::Sender<ChunkData>,
        config: TransferConfig,
        connection_pool: ConnectionPool,
    ) -> Self {
        Self {
            connector,
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

    async fn download_chunk<S: checksum::Checksum, D: Coder>(
        &self,
        mut run: RunFlag,
        mut decoder: D,
        mut digest: S,
        expected_checksum: &S::Bytes,
    ) -> Result<()> {
        let chunk_size = usize::try_from(self.chunk.size)?;
        let mut pos = 0;
        let mut destination =
            DestinationsIter::new(self.chunk.destinations.clone(), &self.connector).await?;
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
            .get(
                self.chunk
                    .url
                    .as_ref()
                    .ok_or_else(|| anyhow!("missing chunk {} url", self.chunk.key))?
                    .clone(),
            )
            .header(RANGE, format!("bytes={}-{}", pos, pos + buffer_size - 1))
            .timeout(DOWNLOAD_SINGLE_PART_TIMEOUT)
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
    async fn new(
        mut iter: Vec<FileLocation>,
        connector: &impl BabelEngineConnector,
    ) -> Result<Self> {
        if iter.is_empty() {
            let err_msg = "corrupted manifest - this is internal BV error, manifest shall be already validated";
            error!(err_msg);
            let mut client = connector.connect();
            let _ = with_retry!(client.bv_error(err_msg.to_string()));
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
                    .truncate(false)
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
    use crate::chroot_platform::UdsConnector;
    use crate::compression::ZstdEncoder;
    use crate::job_runner::ArchiveJobRunner;
    use assert_fs::TempDir;
    use babel_api::engine::{Checksum, JobStatus, RestartConfig, RestartPolicy};
    use bv_utils::timer::SysTimer;
    use mockito::{Server, ServerGuard};
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    struct TestEnv {
        tmp_dir: PathBuf,
        dest_dir: PathBuf,
        meta_dir: PathBuf,
        download_parts_path: PathBuf,
        manifest_path: PathBuf,
        completed_path: PathBuf,
        download_progress_path: PathBuf,
        server: ServerGuard,
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_dir = TempDir::new()?.to_path_buf();
        let server = Server::new();
        let dest_dir = tmp_dir.join("data");
        let meta_dir = tmp_dir.join(".meta");
        fs::create_dir_all(&meta_dir)?;
        let download_parts_path = meta_dir.join(PARTS_FILENAME);
        let manifest_path = meta_dir.join(MANIFEST_FILENAME);
        let completed_path = meta_dir.join(COMPLETED_FILENAME);
        let download_progress_path = tmp_dir.join("download.progress");
        Ok(TestEnv {
            tmp_dir,
            dest_dir,
            meta_dir,
            server,
            download_parts_path,
            manifest_path,
            completed_path,
            download_progress_path,
        })
    }

    impl TestEnv {
        fn download_job(
            &self,
            manifest: DownloadManifest,
        ) -> ArchiveJobRunner<SysTimer, Downloader<UdsConnector>> {
            ArchiveJobRunner::new(
                SysTimer,
                RestartPolicy::Never,
                Downloader {
                    connector: UdsConnector,
                    manifest,
                    destination_dir: self.dest_dir.clone(),
                    config: TransferConfig {
                        max_opened_files: 1,
                        max_runners: 4,
                        max_connections: 4,
                        max_buffer_size: 150,
                        max_retries: 0,
                        backoff_base_ms: 1,
                        archive_jobs_meta_dir: self.meta_dir.clone(),
                        progress_file_path: self.download_progress_path.clone(),
                        compression: None,
                    },
                },
            )
        }

        fn url(&self, path: &str) -> Option<url::Url> {
            Some(url::Url::parse(&format!("{}/{}", self.server.url(), path)).unwrap())
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
                .download_job(manifest.clone())
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );

        assert_eq!(
            vec![0u8; 100],
            fs::read(test_env.dest_dir.join("zero.file"))?
        );
        assert_eq!(
            vec![1u8; 200],
            fs::read(test_env.dest_dir.join("first.file"))?
        );
        assert_eq!(
            [vec![2u8; 300], vec![3u8; 324]].concat(),
            fs::read(test_env.dest_dir.join("second.file"))?
        );
        assert_eq!(0, test_env.dest_dir.join("empty.file").metadata()?.len());
        assert_eq!(
            test_env.dest_dir.join("empty.file").metadata()?.mtime(),
            test_env.dest_dir.join("second.file").metadata()?.mtime()
        );
        assert_eq!(
            test_env.dest_dir.join("first.file").metadata()?.mtime(),
            test_env.dest_dir.join("second.file").metadata()?.mtime()
        );
        assert!(test_env.download_parts_path.exists());
        assert!(test_env.manifest_path.exists());
        assert!(test_env.completed_path.exists());
        assert!(test_env.download_progress_path.exists());

        // idempotency
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
        job.runner.config.compression = Some(Compression::ZSTD(5));
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
            fs::read(test_env.dest_dir.join("zero.file"))?
        );
        assert_eq!(
            vec![1u8; 200],
            fs::read(test_env.dest_dir.join("first.file"))?
        );
        assert_eq!(
            [vec![2u8; 300], vec![3u8; 324]].concat(),
            fs::read(test_env.dest_dir.join("second.file"))?
        );
        assert_eq!(0, test_env.dest_dir.join("empty.file").metadata()?.len());
        assert!(test_env.download_parts_path.exists());
        assert!(test_env.manifest_path.exists());
        assert!(test_env.completed_path.exists());
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

        assert_eq!(0, test_env.dest_dir.join("empty_1.file").metadata()?.len());
        assert_eq!(0, test_env.dest_dir.join("empty_2.file").metadata()?.len());
        assert!(test_env.download_parts_path.exists());
        assert!(test_env.manifest_path.exists());
        assert!(test_env.completed_path.exists());
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
                message: "job 'name' failed with: chunk 'first_chunk' download failed: corrupted manifest - expected at least one destination file in chunk".to_string()
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
                message: "job 'name' failed with: chunk 'first_chunk' download failed: (decompressed) chunk size doesn't mach expected one".to_string()
            },
            test_env
                .download_job(manifest_inconsistent_destination)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );
        assert!(test_env.manifest_path.exists());
        assert!(!test_env.completed_path.exists());
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
                message: "job 'name' failed with: chunk 'first_chunk' download failed: server error: received more bytes than requested"
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
                message: "job 'name' failed with: chunk 'second_chunk' download failed: server error: received less bytes than requested"
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
                message: "job 'name' failed with: chunk 'third_chunk' download failed: server responded with 501 Not Implemented"
                    .to_string()
            },
            test_env
                .download_job(manifest)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );
        assert!(test_env.manifest_path.exists());
        assert!(!test_env.completed_path.exists());
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

        // create files form first chunk
        fs::create_dir_all(&test_env.dest_dir)?;
        fs::write(test_env.dest_dir.join("zero.file"), "")?;
        fs::write(test_env.dest_dir.join("first.file"), "")?;
        // mark first file as downloaded
        let mut parts = HashSet::new();
        parts.insert("first_chunk");
        fs::write(
            &test_env.download_parts_path,
            serde_json::to_string(&parts)?,
        )?;
        fs::write(&test_env.manifest_path, serde_json::to_string(&manifest)?)?;
        // partially downloaded second file
        fs::write(test_env.dest_dir.join("second.file"), vec![1u8; 55])?;

        let mut job = test_env.download_job(manifest);
        job.runner.config.max_runners = 1;
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: "".to_string()
            },
            job.run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );

        assert_eq!(
            vec![2u8; 150],
            fs::read(test_env.dest_dir.join("second.file"))?
        );
        assert_eq!(
            vec![3u8; 150],
            fs::read(test_env.dest_dir.join("third.file"))?
        );

        assert!(test_env.download_parts_path.exists());
        assert!(test_env.manifest_path.exists());
        assert!(test_env.completed_path.exists());
        assert!(test_env.download_progress_path.exists());
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
                "job 'name' failed with: Can't download {} bytes of data while only",
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
        fs::create_dir_all(&test_env.dest_dir)?;
        let mut perms = fs::metadata(&job.runner.destination_dir)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(&job.runner.destination_dir, perms)?;

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "job 'name' failed with: Writer error: Permission denied (os error 13)"
                    .to_string()
            },
            job.run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_remnants_after_fail() -> Result<()> {
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

        let mut job = test_env.download_job(manifest.clone());
        job.runner.config.max_runners = 1;
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "job 'name' failed with: chunk 'second_chunk' download failed: chunk checksum mismatch - expected [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], actual [223, 133, 134, 120, 124, 112, 193, 150, 43, 123, 78, 114, 164, 121, 55, 99, 61, 88, 63, 101]".to_string()
            },
            job.run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );

        assert!(test_env.dest_dir.join("zero.file").exists());
        assert!(test_env.dest_dir.join("first.file").exists());
        assert!(test_env.dest_dir.join("second.file").exists());
        assert!(test_env.dest_dir.join("third.file").exists());
        assert!(test_env.download_parts_path.exists());
        assert_eq!(
            load_job_data::<DownloadManifest>(&test_env.manifest_path).unwrap(),
            manifest
        );
        assert!(!test_env.completed_path.exists());
        assert!(test_env.download_progress_path.exists());
        let progress = fs::read_to_string(&test_env.download_progress_path).unwrap();
        assert_eq!(&progress, r#"{"total":2,"current":1,"message":"chunks"}"#);

        let another_manifest = DownloadManifest {
            total_size: 1,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: test_env.url("first_chunk"),
                checksum: Checksum::Blake3([
                    85, 66, 30, 123, 210, 245, 146, 94, 153, 129, 249, 169, 140, 22, 44, 8, 190,
                    219, 61, 95, 17, 159, 253, 17, 201, 75, 37, 225, 103, 226, 202, 150,
                ]),
                size: 1,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 1,
                }],
            }],
        };
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "job 'name' failed with: chunk 'first_chunk' download failed: server responded with 501 Not Implemented".to_string()
            },
            test_env.download_job(another_manifest.clone()).run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );
        assert!(!test_env.dest_dir.join("zero.file").exists());
        assert!(!test_env.dest_dir.join("first.file").exists());
        assert!(!test_env.dest_dir.join("second.file").exists());
        assert!(!test_env.dest_dir.join("third.file").exists());
        assert!(test_env.download_parts_path.exists());
        assert_eq!(
            load_job_data::<DownloadManifest>(&test_env.manifest_path).unwrap(),
            another_manifest
        );
        assert!(!test_env.completed_path.exists());
        assert!(test_env.download_progress_path.exists());
        let progress = fs::read_to_string(&test_env.download_progress_path).unwrap();
        assert_eq!(&progress, r#"{"total":2,"current":1,"message":"chunks"}"#);

        Ok(())
    }

    #[test]
    fn test_required_disk_space() {
        let manifest = DownloadManifest {
            total_size: 978,
            compression: Some(Compression::ZSTD(3)),
            chunks: vec![
                Chunk {
                    key: "first_chunk".to_string(),
                    url: None,
                    checksum: Checksum::Sha1([0; 20]),
                    size: 300,
                    destinations: vec![
                        FileLocation {
                            path: PathBuf::from("zero.file"),
                            pos: 0,
                            size: 128,
                        },
                        FileLocation {
                            path: PathBuf::from("first.file"),
                            pos: 0,
                            size: 201,
                        },
                        FileLocation {
                            path: PathBuf::from("second.file"),
                            pos: 0,
                            size: 301,
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
                    url: None,
                    checksum: Checksum::Sha1([0; 20]),
                    size: 200,
                    destinations: vec![FileLocation {
                        path: PathBuf::from("second.file"),
                        pos: 300,
                        size: 324,
                    }],
                },
                Chunk {
                    key: "third_chunk".to_string(),
                    url: None,
                    checksum: Checksum::Sha1([0; 20]),
                    size: 19,
                    destinations: vec![FileLocation {
                        path: PathBuf::from("third.file"),
                        pos: 0,
                        size: 24,
                    }],
                },
            ],
        };
        assert_eq!(
            24,
            required_disk_space(
                &HashSet::from(["first_chunk".to_string(), "second_chunk".to_string()]),
                &manifest
            )
            .unwrap()
        );
        assert_eq!(
            324,
            required_disk_space(
                &HashSet::from(["first_chunk".to_string(), "third_chunk".to_string()]),
                &manifest
            )
            .unwrap()
        );
        assert_eq!(
            654,
            required_disk_space(&HashSet::from(["second_chunk".to_string()]), &manifest).unwrap()
        );
    }
}

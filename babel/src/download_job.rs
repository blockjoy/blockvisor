/// This module implements job runner for downloading data. It downloads data according to given
/// manifest and destination dir. In case of recoverable errors download is retried according to given
/// `RestartPolicy`, with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset if download continue without errors for at least `backoff_timeout_ms`.
use crate::{
    checksum,
    job_runner::{JobBackoff, JobRunnerImpl},
};
use async_trait::async_trait;
use babel_api::engine::{
    Checksum, Chunk, DownloadManifest, FileLocation, JobStatus, RestartPolicy,
};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer};
use eyre::{anyhow, bail, ensure, Context, Result};
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use reqwest::header::RANGE;
use std::{
    cmp::min,
    collections::{hash_map::Entry, HashMap, HashSet},
    fs,
    fs::File,
    io::Write,
    mem,
    ops::{Deref, DerefMut},
    os::unix::fs::FileExt,
    path::PathBuf,
    slice::Iter,
    usize,
    vec::IntoIter,
};
use sysinfo::{DiskExt, System, SystemExt};
use tokio::{select, sync::mpsc, task::JoinHandle, time::Instant};
use tracing::{debug, error, info};

const MAX_OPENED_FILES: u64 = 1024;
const MAX_DOWNLOADERS: usize = 8;
const MAX_BUFFER_SIZE: usize = 128 * 1024 * 1024;

#[derive(Clone)]
pub struct DownloaderConfig {
    max_opened_files: usize,
    max_downloaders: usize,
    max_buffer_size: usize,
    progress_file_path: PathBuf,
}

impl DownloaderConfig {
    pub fn new(progress_file_path: PathBuf) -> Result<Self> {
        let max_opened_files = usize::try_from(rlimit::increase_nofile_limit(MAX_OPENED_FILES)?)?;
        Ok(Self {
            max_opened_files,
            max_downloaders: MAX_DOWNLOADERS,
            max_buffer_size: MAX_BUFFER_SIZE,
            progress_file_path,
        })
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
    config: DownloaderConfig,
}

impl<T: AsyncTimer + Send> DownloadJob<T> {
    pub fn new(
        timer: T,
        manifest: DownloadManifest,
        destination_dir: PathBuf,
        restart_policy: RestartPolicy,
        config: DownloaderConfig,
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
}

impl Downloader {
    async fn download(&mut self, mut run: RunFlag) -> Result<()> {
        self.check_disk_space()?;
        let (tx, rx) = mpsc::channel(self.config.max_downloaders);
        let (downloaded_chunks, writer) = self.init_writer(run.clone(), rx);

        let mut downloaders = ParallelChunkDownloaders::new(
            run.clone(),
            tx,
            downloaded_chunks,
            &self.manifest.chunks,
            self.config.clone(),
        );
        while run.load() {
            downloaders.launch_more();
            match downloaders.wait_for_next().await {
                Some(Err(err)) => {
                    run.stop();
                    bail!(err);
                }
                Some(Ok(_)) => {}
                None => break,
            }
        }
        drop(downloaders); // drop last sender so writer know that download is done

        writer.await??;
        Ok(())
    }

    fn check_disk_space(&self) -> Result<()> {
        let mut sys = System::new_all();
        sys.refresh_all();
        let available_space = bv_utils::system::find_disk_by_path(&sys, &self.destination_dir)
            .map(|disk| disk.available_space())
            .ok_or_else(|| anyhow!("Cannot get available disk space"))?;
        if self.manifest.total_size > available_space {
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
        rx: mpsc::Receiver<ChunkData>,
    ) -> (HashSet<String>, JoinHandle<Result<()>>) {
        let writer = Writer::new(
            rx,
            self.destination_dir.clone(),
            self.config.max_opened_files,
            self.config.progress_file_path.clone(),
        );
        (
            writer.downloaded_chunks.clone(),
            tokio::spawn(writer.run(run.clone())),
        )
    }
}

struct ParallelChunkDownloaders<'a> {
    run: RunFlag,
    tx: mpsc::Sender<ChunkData>,
    downloaded_chunks: HashSet<String>,
    config: DownloaderConfig,
    futures: FuturesUnordered<BoxFuture<'a, Result<()>>>,
    chunks: Iter<'a, Chunk>,
}

impl<'a> ParallelChunkDownloaders<'a> {
    fn new(
        run: RunFlag,
        tx: mpsc::Sender<ChunkData>,
        downloaded_chunks: HashSet<String>,
        chunks: &'a [Chunk],
        config: DownloaderConfig,
    ) -> Self {
        Self {
            run,
            tx,
            downloaded_chunks,
            config,
            futures: FuturesUnordered::new(),
            chunks: chunks.iter(),
        }
    }

    fn launch_more(&mut self) {
        while self.futures.len() < self.config.max_downloaders {
            if let Some(chunk) = self.chunks.next() {
                if self.downloaded_chunks.contains(&chunk.key) {
                    // skip chunks successfully downloaded in previous run
                    continue;
                }
                let downloader = ChunkDownloader::new(
                    chunk.clone(),
                    self.tx.clone(),
                    self.config.max_buffer_size,
                );
                self.futures
                    .push(Box::pin(downloader.run(self.run.clone())));
            } else {
                break;
            }
        }
    }

    async fn wait_for_next(&mut self) -> Option<Result<()>> {
        self.futures.next().await
    }
}

#[async_trait]
impl<T: AsyncTimer + Send> JobRunnerImpl for DownloadJob<T> {
    /// Run and restart downloader until `backoff.stopped` return `JobStatus` or job runner
    /// is stopped explicitly.  
    async fn try_run_job(mut self, mut run: RunFlag, name: &str) -> Result<(), JobStatus> {
        info!("download job '{name}' started");
        debug!("with manifest: {:?}", self.downloader.manifest);

        let mut backoff = JobBackoff::new(self.timer, run.clone(), &self.restart_policy);
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
    max_buffer_size: usize,
}

impl ChunkDownloader {
    fn new(chunk: Chunk, tx: mpsc::Sender<ChunkData>, max_buffer_size: usize) -> Self {
        Self {
            chunk,
            tx,
            client: reqwest::Client::new(),
            max_buffer_size,
        }
    }

    async fn run(self, run: RunFlag) -> Result<()> {
        match &self.chunk.checksum {
            Checksum::Sha1(checksum) => {
                self.download_chunk(run, sha1_smol::Sha1::new(), checksum)
                    .await
            }
            Checksum::Sha256(checksum) => {
                self.download_chunk(run, <sha2::Sha256 as sha2::Digest>::new(), checksum)
                    .await
            }
            Checksum::Blake3(checksum) => {
                self.download_chunk(run, blake3::Hasher::new(), checksum)
                    .await
            }
        }
        .with_context(|| format!("chunk '{}' download failed", self.chunk.key))
    }

    async fn download_chunk<C: checksum::Checksum>(
        &self,
        mut run: RunFlag,
        mut digest: C,
        expected_checksum: &C::Bytes,
    ) -> Result<()> {
        let chunk_size = usize::try_from(self.chunk.size)?;
        let mut pos = 0;
        let mut destination = DestinationsIter::new(self.chunk.destinations.clone().into_iter())?;
        while run.load() && pos < chunk_size {
            select!(
                res = self.download_part(pos, chunk_size) => {//TODO with_retry!
                    let buffer = res?;
                    pos += buffer.len();
                    digest.update(&buffer);

                    // TODO add decompression support here

                    self.send_to_writer(buffer, &mut destination).await?;
                }
                _ = run.wait() => {}
            );
        }
        let calculated_checksum = digest.into_bytes();
        ensure!(
            &calculated_checksum == expected_checksum,
            "chunk checksum mismatch - expected {expected_checksum:?}, actual {calculated_checksum:?}"
        );
        self.tx
            .send(ChunkData::EndOfChunk {
                key: self.chunk.key.clone(),
            })
            .await?;
        Ok(())
    }

    async fn download_part(&self, pos: usize, chunk_size: usize) -> Result<Vec<u8>> {
        let buffer_size = min(chunk_size - pos, self.max_buffer_size);
        let mut buffer = Vec::with_capacity(buffer_size);
        let mut resp = self
            .client
            .get(self.chunk.url.clone())
            .header(RANGE, format!("bytes={}-{}", pos, pos + buffer_size))
            .send()
            .await?;
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
        if destination.size == 0 {
            // last destination file is full, go to next one
            destination.go_next()?;
        }
        // split buffer into file parts and send to writer
        while u64::try_from(buffer.len())? > destination.size {
            let destination = destination.go_next()?;
            let reminder = buffer.split_off(destination.size as usize);
            self.tx
                .send(ChunkData::FilePart {
                    path: destination.path,
                    pos: destination.pos,
                    data: buffer,
                })
                .await?;
            buffer = reminder;
        }
        // send remaining
        let bytes_sent = u64::try_from(buffer.len())?;
        self.tx
            .send(ChunkData::FilePart {
                path: destination.path.clone(),
                pos: destination.pos,
                data: buffer,
            })
            .await?;
        // and update last destination position and size
        destination.pos += bytes_sent;
        destination.size -= bytes_sent;
        Ok(())
    }
}

struct DestinationsIter {
    iter: IntoIter<FileLocation>,
    current: FileLocation,
}

impl DestinationsIter {
    fn new(mut iter: IntoIter<FileLocation>) -> Result<Self> {
        let current = match iter.next() {
            None => {
                error!("corrupted manifest - this is internal BV error, manifest shall be already validated");
                Err(anyhow!(
                    "corrupted manifest - expected at least one destination file in chunk"
                ))
            }
            Some(current) => Ok(current),
        }?;
        Ok(Self { iter, current })
    }

    fn go_next(&mut self) -> Result<FileLocation> {
        Ok(mem::replace(
            &mut self.current,
            self.iter.next().ok_or(anyhow!(
                "(decompressed) chunk size doesn't mach expected one"
            ))?,
        ))
    }
}
impl Deref for DestinationsIter {
    type Target = FileLocation;

    fn deref(&self) -> &Self::Target {
        &self.current
    }
}

impl DerefMut for DestinationsIter {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.current
    }
}

struct Writer {
    opened_files: HashMap<PathBuf, (Instant, File)>,
    destination_dir: PathBuf,
    rx: mpsc::Receiver<ChunkData>,
    max_opened_files: usize,
    progress_file_path: PathBuf,
    downloaded_chunks: HashSet<String>,
}

impl Writer {
    fn new(
        rx: mpsc::Receiver<ChunkData>,
        destination_dir: PathBuf,
        max_opened_files: usize,
        progress_file_path: PathBuf,
    ) -> Self {
        let downloaded_chunks = if progress_file_path.exists() {
            fs::read_to_string(&progress_file_path)
                .and_then(|json| Ok(serde_json::from_str(&json)?))
                .unwrap_or_default()
        } else {
            Default::default()
        };

        Self {
            opened_files: HashMap::new(),
            destination_dir,
            rx,
            max_opened_files,
            progress_file_path,
            downloaded_chunks,
        }
    }

    async fn run(mut self, mut run: RunFlag) -> Result<()> {
        while run.load() {
            if let Some(chunk_data) = self.rx.recv().await {
                if let Err(err) = self.handle_chunk_data(chunk_data).await {
                    run.stop();
                    bail!("Writer error: {err}")
                }
            } else {
                // stop writer when all senders/downloaders are dropped
                break;
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
                fs::write(
                    &self.progress_file_path,
                    serde_json::to_string(&self.downloaded_chunks)?,
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
                // already have opened handle to the file, so just update timestamp
                let (timestamp, file) = entry.get_mut();
                *timestamp = Instant::now();
                file.write_all_at(&data, pos)?;
                file.flush()?;
            }
            Entry::Vacant(entry) => {
                // file not opened yet
                let file = File::options()
                    .create(true)
                    .write(true)
                    .open(self.destination_dir.join(entry.key()))?;
                let (_, file) = entry.insert((Instant::now(), file));
                file.write_all_at(&data, pos)?;
                file.flush()?;
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
    use assert_fs::TempDir;
    use babel_api::engine::Checksum;
    use httpmock::prelude::*;
    use reqwest::Url;
    use std::fs;

    struct TestEnv {
        tmp_dir: PathBuf,
        download_progress_path: PathBuf,
        server: MockServer,
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_dir = TempDir::new()?.to_path_buf();
        fs::create_dir_all(&tmp_dir)?;
        let server = MockServer::start();
        let download_progress_path = tmp_dir.join("download.progress");
        Ok(TestEnv {
            tmp_dir,
            server,
            download_progress_path,
        })
    }

    impl TestEnv {
        fn downloader(&self, manifest: DownloadManifest) -> Downloader {
            Downloader {
                manifest,
                destination_dir: self.tmp_dir.clone(),
                config: DownloaderConfig {
                    max_opened_files: 1,
                    max_downloaders: 4,
                    max_buffer_size: 150,
                    progress_file_path: self.download_progress_path.clone(),
                },
            }
        }

        fn url(&self, path: &str) -> Result<Url> {
            Ok(Url::parse(&self.server.url(path))?)
        }
    }

    #[tokio::test]
    async fn full_download_ok() -> Result<()> {
        let test_env = setup_test_env()?;

        let manifest = DownloadManifest {
            total_size: 924,
            compression: None,
            chunks: vec![
                Chunk {
                    key: "first_chunk".to_string(),
                    url: test_env.url("/first_chunk")?,
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
                    url: test_env.url("/second_chunk")?,
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

        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-150")
                .path("/first_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body([vec![0u8; 100], vec![1u8; 50]].concat());
        });
        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=150-300")
                .path("/first_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![1u8; 150]);
        });
        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=300-450")
                .path("/first_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![2u8; 150]);
        });
        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=450-600")
                .path("/first_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![2u8; 150]);
        });

        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-150")
                .path("/second_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![3u8; 150]);
        });
        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=150-300")
                .path("/second_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![3u8; 150]);
        });
        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=300-324")
                .path("/second_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![3u8; 24]);
        });
        test_env
            .downloader(manifest)
            .download(RunFlag::default())
            .await?;

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
        Ok(())
    }

    #[tokio::test]
    async fn invalid_manifest() -> Result<()> {
        let test_env = setup_test_env()?;

        let manifest_wo_destination = DownloadManifest {
            total_size: 600,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: test_env.url("/first_chunk")?,
                checksum: Checksum::Sha1([0u8; 20]),
                size: 600,
                destinations: vec![],
            }],
        };
        assert_eq!(
            format!(
                "{:#}",
                test_env
                    .downloader(manifest_wo_destination)
                    .download(RunFlag::default())
                    .await
                    .unwrap_err()
            ),
            "chunk 'first_chunk' download failed: corrupted manifest - expected at least one destination file in chunk"
        );

        let manifest_inconsistent_destination = DownloadManifest {
            total_size: 200,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: test_env.url("/chunk")?,
                checksum: Checksum::Sha1([0u8; 20]),
                size: 200,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 100,
                }],
            }],
        };
        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-150")
                .path("/chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![0u8; 150]);
        });
        assert_eq!(
            format!(
                "{:#}",
                test_env
                    .downloader(manifest_inconsistent_destination)
                    .download(RunFlag::default())
                    .await
                    .unwrap_err()
            ),
            "chunk 'first_chunk' download failed: (decompressed) chunk size doesn't mach expected one"
        );

        Ok(())
    }

    #[tokio::test]
    async fn server_errors() -> Result<()> {
        let test_env = setup_test_env()?;

        let manifest = DownloadManifest {
            total_size: 100,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: test_env.url("/first_chunk")?,
                checksum: Checksum::Sha1([0u8; 20]),
                size: 100,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 100,
                }],
            }],
        };

        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-100")
                .path("/first_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![0u8; 150]);
        });

        assert_eq!(
            format!(
                "{:#}",
                test_env
                    .downloader(manifest)
                    .download(RunFlag::default())
                    .await
                    .unwrap_err()
            ),
            "chunk 'first_chunk' download failed: server error: received more bytes than requested"
        );

        let manifest = DownloadManifest {
            total_size: 100,
            compression: None,
            chunks: vec![Chunk {
                key: "second_chunk".to_string(),
                url: test_env.url("/second_chunk")?,
                checksum: Checksum::Sha1([0u8; 20]),
                size: 100,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 100,
                }],
            }],
        };

        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-100")
                .path("/second_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![0u8; 50]);
        });

        assert_eq!(
            format!(
                "{:#}",
                test_env
                    .downloader(manifest)
                    .download(RunFlag::default())
                    .await
                    .unwrap_err()
            ),
            "chunk 'second_chunk' download failed: server error: received less bytes than requested"
        );

        let manifest = DownloadManifest {
            total_size: 100,
            compression: None,
            chunks: vec![Chunk {
                key: "third_chunk".to_string(),
                url: test_env.url("/third_chunk")?,
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
            format!(
                "{:#}",
                test_env
                    .downloader(manifest)
                    .download(RunFlag::default())
                    .await
                    .unwrap_err()
            ),
            "chunk 'third_chunk' download failed: server responded with 404 Not Found"
        );

        Ok(())
    }

    #[tokio::test]
    async fn restore_download_ok() -> Result<()> {
        let test_env = setup_test_env()?;

        let manifest = DownloadManifest {
            total_size: 450,
            compression: None,
            chunks: vec![
                Chunk {
                    key: "first_chunk".to_string(),
                    url: test_env.url("/first_chunk")?,
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
                    url: test_env.url("/second_chunk")?,
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
                    url: test_env.url("/third_chunk")?,
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

        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-150")
                .path("/second_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![2u8; 150]);
        });
        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-150")
                .path("/third_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![3u8; 150]);
        });

        // mark first file as downloaded
        let mut progress = HashSet::new();
        progress.insert("first_chunk");
        fs::write(
            &test_env.download_progress_path,
            serde_json::to_string(&progress)?,
        )?;
        // partially downloaded second file
        fs::write(&test_env.tmp_dir.join("second.file"), vec![1u8; 55])?;

        let mut job = test_env.downloader(manifest);
        job.config.max_downloaders = 1;
        job.download(RunFlag::default()).await?;

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
    async fn not_enough_disk_space() -> Result<()> {
        let test_env = setup_test_env()?;

        let manifest_wo_destination = DownloadManifest {
            total_size: u64::MAX,
            compression: None,
            chunks: vec![],
        };
        assert!(test_env
            .downloader(manifest_wo_destination)
            .download(RunFlag::default())
            .await
            .unwrap_err()
            .to_string()
            .contains(&format!(
                "Can't download {} bytes of data while only",
                u64::MAX
            )));
        Ok(())
    }

    #[tokio::test]
    async fn writer_error() -> Result<()> {
        let test_env = setup_test_env()?;

        let manifest = DownloadManifest {
            total_size: 100,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: test_env.url("/first_chunk")?,
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

        test_env.server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-100")
                .path("/first_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![0u8; 100]);
        });

        let mut job = test_env.downloader(manifest);
        job.destination_dir = PathBuf::from("/tmp/non/existing/destination");

        assert!(job
            .download(RunFlag::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("Writer error: No such file or directory"));

        Ok(())
    }
}

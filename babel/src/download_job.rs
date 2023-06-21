/// This module implements job runner for fetching blockchain data. It downloads blockchain data
/// according to to given manifest and destination dir. In case of recoverable errors download
/// is retried according to given `RestartPolicy`, with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset if download continue without errors for at least `backoff_timeout_ms`.
use crate::job_runner::JobRunnerImpl;
use async_trait::async_trait;
use babel_api::engine::{Chunk, DownloadManifest, FileLocation, JobStatus, RestartPolicy};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer};
use eyre::{anyhow, ensure, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use reqwest::header::RANGE;
use std::{
    cmp::min,
    collections::hash_map::Entry,
    collections::HashMap,
    fs::File,
    mem,
    ops::{Deref, DerefMut},
    os::unix::fs::FileExt,
    path::PathBuf,
    vec::IntoIter,
};
use tokio::{sync::mpsc, time::Instant};
use tracing::{debug, info};

const MAX_OPENED_FILES: usize = 16; // TODO: read/set `ulimit -n`
const MAX_DOWNLOADERS: usize = 8;
const MAX_BUFFER_SIZE: usize = 128 * 1024 * 1024;

pub struct DownloaderConfig {
    max_opened_files: usize,
    max_downloaders: usize,
    max_buffer_size: usize,
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            max_opened_files: MAX_OPENED_FILES,
            max_downloaders: MAX_DOWNLOADERS,
            max_buffer_size: MAX_BUFFER_SIZE,
        }
    }
}

pub struct DownloadJob<T> {
    manifest: DownloadManifest,
    destination_dir: PathBuf,
    restart_policy: RestartPolicy,
    timer: T,
    config: DownloaderConfig,
}
impl<T: AsyncTimer> DownloadJob<T> {
    pub fn new(
        timer: T,
        manifest: DownloadManifest,
        destination_dir: PathBuf,
        restart_policy: RestartPolicy,
        config: DownloaderConfig,
    ) -> Result<Self> {
        Ok(Self {
            manifest,
            destination_dir,
            restart_policy,
            timer,
            config,
        })
    }

    async fn download(&mut self, mut run: RunFlag) -> Result<()> {
        let (tx, rx) = mpsc::channel(self.config.max_downloaders);
        let writer = tokio::spawn(
            Writer::new(
                rx,
                self.destination_dir.clone(),
                self.config.max_opened_files,
            )
            .run(run.clone()),
        );

        let mut downloaders = FuturesUnordered::new();
        let mut chunks = self.manifest.chunks.clone();
        // TODO add multipart download support for low numer of chunks case
        // let max_parts = if chunks.len() < self.config.max_downloaders {
        //     self.config.max_downloaders / chunks.len()
        // } else {
        //     1
        // };
        while run.load() {
            while downloaders.len() < self.config.max_downloaders {
                if let Some(chunk) = chunks.pop() {
                    let downloader = ChunkDownloader::new(
                        chunk,
                        // max_parts,
                        tx.clone(),
                        self.config.max_buffer_size,
                    );
                    downloaders.push(downloader.run());
                } else {
                    break;
                }
            }
            if let Some(result) = downloaders.next().await {
                match self.restart_policy {
                    RestartPolicy::Never => result?,
                    _ => {
                        // TODO implement retry
                        self.timer.now();
                        unimplemented!()
                    }
                }
            } else {
                break;
            }
        }
        drop(tx); // drop last sender so writer know that download is done.

        writer.await??;
        Ok(())
    }
}

#[async_trait]
impl<T: AsyncTimer + Send> JobRunnerImpl for DownloadJob<T> {
    /// Run and restart job child process until `backoff.stopped` return `JobStatus` or job runner
    /// is stopped explicitly.  
    async fn try_run_job(&mut self, run: RunFlag, name: &str) -> Result<(), JobStatus> {
        info!("download job '{name}' started");
        debug!("with manifest: {:?}", self.manifest);
        self.download(run).await.map_err(|err| JobStatus::Finished {
            exit_code: Some(-1),
            message: err.to_string(),
        })
    }
}

#[derive(Debug)]
struct FilePart {
    path: PathBuf,
    pos: u64,
    data: Vec<u8>,
}

struct ChunkDownloader {
    chunk: Chunk,
    tx: mpsc::Sender<FilePart>,
    client: reqwest::Client,
    max_buffer_size: usize,
}

impl ChunkDownloader {
    fn new(chunk: Chunk, tx: mpsc::Sender<FilePart>, max_buffer_size: usize) -> Self {
        Self {
            chunk,
            tx,
            client: reqwest::Client::new(),
            max_buffer_size,
        }
    }

    async fn run(self) -> Result<()> {
        let chunk_size = usize::try_from(self.chunk.size)?;
        let mut pos = 0;
        let mut destination = DestinationsIter::new(self.chunk.destinations.clone().into_iter())?;
        while pos < chunk_size {
            let buffer = self.download_part(pos, chunk_size).await?;
            pos += buffer.len();

            // TODO add decompression support here

            self.send_to_writer(buffer, &mut destination).await?;
        }
        // TODO verify calculated checksum
        Ok(())
    }

    async fn download_part(&self, pos: usize, chunk_size: usize) -> Result<Vec<u8>> {
        // TODO add multipart download support
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
                .send(FilePart {
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
            .send(FilePart {
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
        let current = iter.next().ok_or(anyhow!(
            "corrupted manifest - expected at least one destination file in chunk"
        ))?;
        Ok(Self { iter, current })
    }

    fn go_next(&mut self) -> Result<FileLocation> {
        Ok(mem::replace(
            &mut self.current,
            self.iter.next().ok_or(anyhow!(
                "corrupted manifest - expected destination, but not found"
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
    rx: mpsc::Receiver<FilePart>,
    max_opened_files: usize,
}

impl Writer {
    fn new(
        rx: mpsc::Receiver<FilePart>,
        destination_dir: PathBuf,
        max_opened_files: usize,
    ) -> Self {
        Self {
            opened_files: HashMap::new(),
            destination_dir,
            rx,
            max_opened_files,
        }
    }

    async fn run(mut self, mut run: RunFlag) -> Result<()> {
        while run.load() {
            if let Some(part) = self.rx.recv().await {
                match self.opened_files.entry(part.path.clone()) {
                    Entry::Occupied(mut entry) => {
                        // already have opened handle to the file, so just update timestamp
                        let (timestamp, file) = entry.get_mut();
                        *timestamp = Instant::now();
                        file.write_all_at(&part.data, part.pos)?;
                    }
                    Entry::Vacant(entry) => {
                        // file not opened yet
                        let file = File::options()
                            .create(true)
                            .write(true)
                            .open(self.destination_dir.join(entry.key()))?;
                        let (_, file) = entry.insert((Instant::now(), file));
                        file.write_all_at(&part.data, part.pos)?;
                    }
                }
                if self.opened_files.len() > self.max_opened_files {
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
            } else {
                // stop writer when all senders/downloaders are dropped
                break;
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
    use bv_utils::timer::MockAsyncTimer;
    use httpmock::prelude::*;
    use reqwest::Url;
    use std::fs;

    #[tokio::test]
    async fn full_download_ok() -> Result<()> {
        let tmp_dir = TempDir::new()?;
        let server = MockServer::start();
        let manifest = DownloadManifest {
            total_size: 924,
            compression: None,
            chunks: vec![
                Chunk {
                    key: "first_chunk".to_string(),
                    url: Url::parse(&server.url("/first_chunk"))?,
                    checksum: Checksum::Sha1(vec![]),
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
                    url: Url::parse(&server.url("/second_chunk"))?,
                    checksum: Checksum::Sha1(vec![]),
                    size: 324,
                    destinations: vec![FileLocation {
                        path: PathBuf::from("second.file"),
                        pos: 300,
                        size: 324,
                    }],
                },
            ],
        };

        server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-150")
                .path("/first_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body([vec![0u8; 100], vec![1u8; 50]].concat());
        });
        server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=150-300")
                .path("/first_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![1u8; 150]);
        });
        server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=300-450")
                .path("/first_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![2u8; 150]);
        });
        server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=450-600")
                .path("/first_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![2u8; 150]);
        });

        server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-150")
                .path("/second_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![3u8; 150]);
        });
        server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=150-300")
                .path("/second_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![3u8; 150]);
        });
        server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=300-324")
                .path("/second_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![3u8; 24]);
        });
        DownloadJob::new(
            MockAsyncTimer::new(),
            manifest,
            tmp_dir.path().to_path_buf(),
            RestartPolicy::Never,
            DownloaderConfig {
                max_opened_files: 1,
                max_downloaders: 4,
                max_buffer_size: 150,
            },
        )?
        .download(RunFlag::default())
        .await?;

        assert_eq!(vec![0u8; 100], fs::read(tmp_dir.path().join("zero.file"))?);
        assert_eq!(vec![1u8; 200], fs::read(tmp_dir.path().join("first.file"))?);
        assert_eq!(
            [vec![2u8; 300], vec![3u8; 324]].concat(),
            fs::read(tmp_dir.path().join("second.file"))?
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_manifest() -> Result<()> {
        let tmp_dir = TempDir::new()?;
        let server = MockServer::start();

        let manifest_wo_destination = DownloadManifest {
            total_size: 600,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: Url::parse(&server.url("/first_chunk"))?,
                checksum: Checksum::Sha1(vec![]),
                size: 600,
                destinations: vec![],
            }],
        };
        assert!(DownloadJob::new(
            MockAsyncTimer::new(),
            manifest_wo_destination,
            tmp_dir.path().to_path_buf(),
            RestartPolicy::Never,
            Default::default(),
        )?
        .download(RunFlag::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("corrupted manifest - expected at least one destination file in chunk"));

        let manifest_inconsistent_destination = DownloadManifest {
            total_size: 200,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: Url::parse(&server.url("/chunk"))?,
                checksum: Checksum::Sha1(vec![]),
                size: 200,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 100,
                }],
            }],
        };
        server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-200")
                .path("/chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![0u8; 200]);
        });
        assert!(DownloadJob::new(
            MockAsyncTimer::new(),
            manifest_inconsistent_destination,
            tmp_dir.path().to_path_buf(),
            RestartPolicy::Never,
            Default::default(),
        )?
        .download(RunFlag::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("corrupted manifest - expected destination, but not found"));

        Ok(())
    }

    #[tokio::test]
    async fn server_errors() -> Result<()> {
        let tmp_dir = TempDir::new()?;
        let server = MockServer::start();
        let manifest = DownloadManifest {
            total_size: 100,
            compression: None,
            chunks: vec![Chunk {
                key: "first_chunk".to_string(),
                url: Url::parse(&server.url("/first_chunk"))?,
                checksum: Checksum::Sha1(vec![]),
                size: 100,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 100,
                }],
            }],
        };

        server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-100")
                .path("/first_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![0u8; 150]);
        });

        assert!(DownloadJob::new(
            MockAsyncTimer::new(),
            manifest,
            tmp_dir.path().to_path_buf(),
            RestartPolicy::Never,
            Default::default(),
        )?
        .download(RunFlag::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("server error: received more bytes than requested"));

        let manifest = DownloadManifest {
            total_size: 100,
            compression: None,
            chunks: vec![Chunk {
                key: "second_chunk".to_string(),
                url: Url::parse(&server.url("/second_chunk"))?,
                checksum: Checksum::Sha1(vec![]),
                size: 100,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 100,
                }],
            }],
        };

        server.mock(|when, then| {
            when.method(GET)
                .header("range", "bytes=0-100")
                .path("/second_chunk");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(vec![0u8; 50]);
        });

        assert!(DownloadJob::new(
            MockAsyncTimer::new(),
            manifest,
            tmp_dir.path().to_path_buf(),
            RestartPolicy::Never,
            Default::default(),
        )?
        .download(RunFlag::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("server error: received less bytes than requested"));

        let manifest = DownloadManifest {
            total_size: 100,
            compression: None,
            chunks: vec![Chunk {
                key: "third_chunk".to_string(),
                url: Url::parse(&server.url("/third_chunk"))?,
                checksum: Checksum::Sha1(vec![]),
                size: 100,
                destinations: vec![FileLocation {
                    path: PathBuf::from("zero.file"),
                    pos: 0,
                    size: 100,
                }],
            }],
        };
        drop(server);

        assert!(DownloadJob::new(
            MockAsyncTimer::new(),
            manifest,
            tmp_dir.path().to_path_buf(),
            RestartPolicy::Never,
            Default::default(),
        )?
        .download(RunFlag::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("server responded with 404"));

        Ok(())
    }
}

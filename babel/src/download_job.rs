/// This module implements job runner for fetching blockchain data. It downloads blockchain data
/// according to to given manifest and destination dir. In case of recoverable errors download
/// is retried according to given `RestartPolicy`, with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset if download continue without errors for at least `backoff_timeout_ms`.
use crate::job_runner::JobRunnerImpl;
use async_trait::async_trait;
use babel_api::engine::{Chunk, DownloadManifest, FileLocation, JobStatus, RestartPolicy};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer};
use eyre::{anyhow, Result};
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

pub struct DownloadJob<T> {
    manifest: DownloadManifest,
    destination_dir: PathBuf,
    restart_policy: RestartPolicy,
    timer: T,
}
impl<T: AsyncTimer> DownloadJob<T> {
    pub fn new(
        timer: T,
        manifest: DownloadManifest,
        destination_dir: PathBuf,
        restart_policy: RestartPolicy,
    ) -> Result<Self> {
        Ok(Self {
            manifest,
            destination_dir,
            restart_policy,
            timer,
        })
    }

    async fn download(&mut self, mut run: RunFlag) -> Result<()> {
        let (tx, rx) = mpsc::channel(MAX_DOWNLOADERS);
        let writer = tokio::spawn(Writer::new(rx, self.destination_dir.clone()).run(run.clone()));

        let mut downloaders = FuturesUnordered::new();
        let mut chunks = self.manifest.chunks.clone();
        // TODO add multipart download support for low numer of chunks case
        // let max_parts = if chunks.len() < MAX_DOWNLOADERS {
        //     MAX_DOWNLOADERS / chunks.len()
        // } else {
        //     1
        // };
        while run.load() {
            while downloaders.len() < MAX_DOWNLOADERS {
                if let Some(chunk) = chunks.pop() {
                    let downloader = ChunkDownloader::new(
                        chunk,
                        // max_parts,
                        tx.clone(),
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
}

impl ChunkDownloader {
    fn new(chunk: Chunk, tx: mpsc::Sender<FilePart>) -> Self {
        Self {
            chunk,
            tx,
            client: reqwest::Client::new(),
        }
    }

    async fn run(self) -> Result<()> {
        let chunk_size = self.get_chunk_len().await?;
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

    async fn get_chunk_len(&self) -> Result<usize> {
        let content_len = self
            .client
            .get(self.chunk.url.clone())
            .send()
            .await?
            .content_length()
            .ok_or_else(|| {
                anyhow!(
                    "can't get chunk content length; key: {}, url: {}",
                    self.chunk.key,
                    self.chunk.url
                )
            })?;
        Ok(usize::try_from(content_len)?)
    }

    async fn download_part(&self, pos: usize, chunk_size: usize) -> Result<Vec<u8>> {
        // TODO add multipart download support
        let buffer_size = min(chunk_size - pos, MAX_BUFFER_SIZE);
        let mut buffer = Vec::with_capacity(buffer_size);
        let mut resp = self
            .client
            .get(self.chunk.url.clone())
            .header(RANGE, format!("bytes={}-{}", pos, pos + buffer_size))
            .send()
            .await?;
        while let Some(bytes) = resp.chunk().await? {
            buffer.append(&mut bytes.to_vec());
        }
        Ok(buffer)
    }

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
}

impl Writer {
    fn new(rx: mpsc::Receiver<FilePart>, destination_dir: PathBuf) -> Self {
        Self {
            opened_files: HashMap::new(),
            destination_dir,
            rx,
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
                            .write(true)
                            .append(true)
                            .create(true)
                            .open(self.destination_dir.join(entry.key()))?;
                        let (_, file) = entry.insert((Instant::now(), file));
                        file.write_all_at(&part.data, part.pos)?;
                    }
                }
                if self.opened_files.len() > MAX_OPENED_FILES {
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

/// This module implements job runner for uploading data. It uploads data according to given
/// manifest and source dir. In case of recoverable errors upload is retried according to given
/// `RestartPolicy`, with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset if upload continue without errors for at least `backoff_timeout_ms`.
use crate::job_runner::{
    cleanup_progress_data, read_progress_data, write_progress_data, JobBackoff, JobRunner,
    JobRunnerImpl, TransferConfig,
};
use async_trait::async_trait;
use babel_api::engine::{
    Checksum, Chunk, DownloadManifest, FileLocation, JobStatus, RestartPolicy, UploadManifest,
};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer};
use eyre::{anyhow, bail, ensure, Context, Result};
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use std::{
    cmp::min,
    collections::HashSet,
    fs::File,
    mem,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    usize,
    vec::IntoIter,
};
use tokio::select;
use tracing::{debug, error, info};

pub struct UploadJob<T> {
    uploader: Uploader,
    restart_policy: RestartPolicy,
    timer: T,
}

struct Uploader {
    manifest: UploadManifest,
    source_dir: PathBuf,
    config: TransferConfig,
}

impl<T: AsyncTimer + Send> UploadJob<T> {
    pub fn new(
        timer: T,
        manifest: UploadManifest,
        source_dir: PathBuf,
        restart_policy: RestartPolicy,
        config: TransferConfig,
    ) -> Result<Self> {
        Ok(Self {
            uploader: Uploader {
                manifest,
                source_dir,
                config,
            },
            restart_policy,
            timer,
        })
    }

    pub async fn run(self, run: RunFlag, name: &str, jobs_dir: &Path) -> JobStatus {
        let progress_file_path = self.uploader.config.progress_file_path.clone();
        let job_status = <Self as JobRunner>::run(self, run, name, jobs_dir).await;
        match &job_status {
            JobStatus::Finished {
                exit_code: Some(0), ..
            }
            | JobStatus::Pending
            | JobStatus::Running => {
                // job finished successfully or is going to be continued after restart, so do nothing
            }
            JobStatus::Finished { .. } | JobStatus::Stopped => {
                // job failed or manually stopped - remove both progress metadata
                cleanup_progress_data(&progress_file_path);
            }
        }
        job_status
    }
}

#[async_trait]
impl<T: AsyncTimer + Send> JobRunnerImpl for UploadJob<T> {
    /// Run and restart uploader until `backoff.stopped` return `JobStatus` or job runner
    /// is stopped explicitly.  
    async fn try_run_job(mut self, mut run: RunFlag, name: &str) -> Result<(), JobStatus> {
        info!("upload job '{name}' started");
        debug!("with manifest: {:?}", self.uploader.manifest);

        let mut backoff = JobBackoff::new(self.timer, run.clone(), &self.restart_policy);
        while run.load() {
            backoff.start();
            match self.uploader.upload(run.clone()).await {
                Ok(_) => {
                    let message = format!("upload job '{name}' finished");
                    backoff.stopped(Some(0), message).await?;
                }
                Err(err) => {
                    backoff
                        .stopped(
                            Some(-1),
                            format!("upload_job '{name}' failed with: {err:#}"),
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }
}

impl Uploader {
    async fn upload(&mut self, mut run: RunFlag) -> Result<()> {
        let blueprint = self.prepare_blueprint()?;
        let mut manifest = DownloadManifest {
            total_size: blueprint.total_size,
            compression: blueprint.compression,
            chunks: Vec::with_capacity(blueprint.chunks.len()),
        };
        let mut uploaded_chunks = read_progress_data(&self.config.progress_file_path);
        let mut uploaders = ParallelChunkUploaders::new(
            run.clone(),
            uploaded_chunks.clone(),
            blueprint.chunks,
            self.config.clone(),
        );
        while run.load() {
            uploaders.launch_more();
            match uploaders.wait_for_next().await {
                Some(Err(err)) => {
                    run.stop();
                    bail!(err);
                }
                Some(Ok(chunk)) => {
                    uploaded_chunks.insert(chunk.key.clone());
                    write_progress_data(&self.config.progress_file_path, &uploaded_chunks)?;
                    manifest.chunks.push(chunk);
                }
                None => break,
            }
        }
        // finally upload manifest file as json
        let resp = reqwest::Client::new()
            .put(&self.manifest.manifest_slot.url)
            .body(serde_json::to_string(&manifest)?)
            .send()
            .await?;
        ensure!(
            resp.status().is_success(),
            anyhow!(
                "failed to upload manifest file - server responded with {}",
                resp.status()
            )
        );

        cleanup_progress_data(&self.config.progress_file_path);
        Ok(())
    }

    /// Prepare DownloadManifest blueprint with files to chunks mapping, based on provided slots.
    fn prepare_blueprint(&self) -> Result<DownloadManifest> {
        if self.manifest.slots.is_empty() {
            bail!("invalid upload manifest - no slots granted");
        }
        let (total_size, mut sources) = sources_list(&self.source_dir)?;
        let chunk_size = total_size / u64::try_from(self.manifest.slots.len())?;
        let mut chunks: Vec<_> = Default::default();
        for slot in &self.manifest.slots {
            let destinations = build_destinations(chunk_size, &mut sources);
            if destinations.is_empty() {
                // no more files - skip rest of the slots
                break;
            }
            chunks.push(Chunk {
                key: slot.key.clone(),
                url: slot.key.clone(),
                checksum: Checksum::Sha1(Default::default()), // unknown yet
                size: Default::default(),                     // unknown yet
                destinations,
            });
        }
        Ok(DownloadManifest {
            total_size,
            compression: None,
            chunks,
        })
    }
}

/// Prepare list of all source files, recursively walking down the source directory.
fn sources_list(source_path: &Path) -> Result<(u64, Vec<FileLocation>)> {
    let mut sources: Vec<_> = Default::default();
    let mut total_size = 0;
    for entry in source_path.read_dir()? {
        let entry = entry?;
        let path = entry.path();
        total_size += if path.is_dir() {
            let (size, mut sub_sources) = sources_list(&path)?;
            sources.append(&mut sub_sources);
            size
        } else if path.is_file() {
            let size = entry.metadata()?.len();
            sources.push(FileLocation { path, pos: 0, size });
            size
        } else {
            // skip symlinks
            0
        };
    }
    Ok((total_size, sources))
}

/// Consumes `sources` and put them into chunk destinations list, until chunk is full
/// (according to given `chunk_size`).
fn build_destinations(chunk_size: u64, sources: &mut Vec<FileLocation>) -> Vec<FileLocation> {
    let mut destinations: Vec<_> = Default::default();
    let mut bytes_in_slot = 0;
    while bytes_in_slot < chunk_size {
        while let Some(FileLocation { size: 0, .. }) = sources.first() {
            // skip empty files
            sources.pop();
        }
        let Some(file) = sources.first_mut() else { break; };

        let dest_size = min(file.size, chunk_size - bytes_in_slot);
        destinations.push(FileLocation {
            path: file.path.clone(),
            pos: file.pos,
            size: dest_size,
        });
        file.size -= dest_size;
        file.pos += dest_size;
        bytes_in_slot += dest_size;
    }
    destinations
}

struct ParallelChunkUploaders<'a> {
    run: RunFlag,
    uploaded_chunks: HashSet<String>,
    config: TransferConfig,
    futures: FuturesUnordered<BoxFuture<'a, Result<Chunk>>>,
    chunks: Vec<Chunk>,
}

impl<'a> ParallelChunkUploaders<'a> {
    fn new(
        run: RunFlag,
        uploaded_chunks: HashSet<String>,
        chunks: Vec<Chunk>,
        mut config: TransferConfig,
    ) -> Self {
        config.max_runners = min(config.max_runners, config.max_opened_files);
        Self {
            run,
            uploaded_chunks,
            config,
            futures: FuturesUnordered::new(),
            chunks,
        }
    }

    fn launch_more(&mut self) {
        while self.futures.len() < self.config.max_runners {
            let Some(chunk) = self.chunks.pop() else {
                break;
            };
            if self.uploaded_chunks.contains(&chunk.key) {
                // skip chunks successfully uploaded in previous run
                continue;
            }
            let uploader = ChunkUploader::new(chunk.clone(), self.config.max_buffer_size);
            self.futures.push(Box::pin(uploader.run(self.run.clone())));
        }
    }

    async fn wait_for_next(&mut self) -> Option<Result<Chunk>> {
        self.futures.next().await
    }
}

struct ChunkUploader {
    chunk: Chunk,
    client: reqwest::Client,
    max_buffer_size: usize,
}

impl ChunkUploader {
    fn new(chunk: Chunk, max_buffer_size: usize) -> Self {
        Self {
            chunk,
            client: reqwest::Client::new(),
            max_buffer_size,
        }
    }

    async fn run(self, run: RunFlag) -> Result<Chunk> {
        let key = self.chunk.key.clone();
        self.upload_chunk(run)
            .await
            .with_context(|| format!("chunk '{key}' upload failed"))
    }

    async fn upload_chunk(mut self, mut run: RunFlag) -> Result<Chunk> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let reader = DestinationsReader::new(
            self.chunk.destinations.clone().into_iter(),
            self.max_buffer_size,
            tx,
        )?;
        let stream = futures_util::stream::iter(reader);
        let body = reqwest::Body::wrap_stream(stream);

        select!(
            resp = self.client.put(&self.chunk.url).body(body).send() => {
                let resp = resp?;
                ensure!(
                    resp.status().is_success(),
                    anyhow!("server responded with {}", resp.status())
                );
                (self.chunk.size, self.chunk.checksum) = rx.await?;
            }
            _ = run.wait() => {
                bail!("upload interrupted");
            }
        );
        Ok(self.chunk)
    }
}

struct FileDescriptor {
    file: File,
    offset: u64,
    bytes_remaining: usize,
}

struct DestinationsReader {
    iter: IntoIter<FileLocation>,
    digest: blake3::Hasher,
    chunk_size: u64,
    current: FileDescriptor,
    buffer: Vec<u8>,
    summary_tx: Option<tokio::sync::oneshot::Sender<(u64, Checksum)>>,
}

impl DestinationsReader {
    fn new(
        mut iter: IntoIter<FileLocation>,
        max_buffer_size: usize,
        summary_tx: tokio::sync::oneshot::Sender<(u64, Checksum)>,
    ) -> Result<Self> {
        let first = match iter.next() {
            None => {
                error!("corrupted manifest - this is internal BV error, manifest shall be already validated");
                Err(anyhow!(
                    "corrupted manifest - expected at least one destination file in chunk"
                ))
            }
            Some(first) => Ok(first),
        }?;
        Ok(Self {
            iter,
            digest: blake3::Hasher::new(),
            chunk_size: 0,
            current: FileDescriptor {
                file: File::open(&first.path)?,
                offset: first.pos,
                bytes_remaining: usize::try_from(first.size)?,
            },
            buffer: Vec::with_capacity(max_buffer_size),
            summary_tx: Some(summary_tx),
        })
    }

    fn try_next(&mut self) -> Result<Option<Vec<u8>>> {
        self.buffer.clear();
        let buffer_capacity = self.buffer.capacity();
        while self.buffer.len() < buffer_capacity {
            if self.current.bytes_remaining == 0 {
                let Some(next) = self.iter.next() else { break; };
                self.current = FileDescriptor {
                    file: File::open(&next.path)?,
                    offset: next.pos,
                    bytes_remaining: usize::try_from(next.size)?,
                };
            }
            let buffer_size = min(self.current.bytes_remaining, buffer_capacity);
            self.buffer.resize(buffer_size, 0);
            self.current
                .file
                .read_exact_at(&mut self.buffer, self.current.offset)?;
            self.current.bytes_remaining -= buffer_size;
            self.current.offset += u64::try_from(buffer_size)?;
        }
        Ok(if !self.buffer.is_empty() {
            // TODO add compression support here

            self.digest.update(&self.buffer);
            self.chunk_size += u64::try_from(self.buffer.len())?;
            Some(mem::replace(
                &mut self.buffer,
                Vec::with_capacity(buffer_capacity),
            ))
        } else {
            if let Some(tx) = self.summary_tx.take() {
                let _ = tx.send((
                    self.chunk_size,
                    Checksum::Blake3(self.digest.finalize().into()),
                ));
            }
            None
        })
    }
}

impl Iterator for DestinationsReader {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.try_next() {
            Ok(Some(value)) => Some(Ok(value)),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::TempDir;
    use bv_utils::timer::SysTimer;
    use httpmock::prelude::*;
    use std::fs;

    struct TestEnv {
        tmp_dir: PathBuf,
        upload_progress_path: PathBuf,
        server: MockServer,
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_dir = TempDir::new()?.to_path_buf();
        fs::create_dir_all(&tmp_dir)?;
        let server = MockServer::start();
        let upload_progress_path = tmp_dir.join("upload.progress");
        Ok(TestEnv {
            tmp_dir,
            server,
            upload_progress_path,
        })
    }

    impl TestEnv {
        fn upload_job(&self, manifest: UploadManifest) -> UploadJob<SysTimer> {
            UploadJob {
                uploader: Uploader {
                    manifest,
                    source_dir: self.tmp_dir.clone(),
                    config: TransferConfig {
                        max_opened_files: 1,
                        max_runners: 4,
                        max_buffer_size: 150,
                        max_retries: 0,
                        backoff_base_ms: 1,
                        progress_file_path: self.upload_progress_path.clone(),
                    },
                },
                restart_policy: RestartPolicy::Never,
                timer: SysTimer,
            }
        }

        fn url(&self, path: &str) -> Result<String> {
            Ok(self.server.url(path))
        }
    }

    #[tokio::test]
    async fn test_sources_list() -> Result<()> {
        Ok(())
    }

    #[tokio::test]
    async fn test_prepare_blueprint() -> Result<()> {
        Ok(())
    }

    #[tokio::test]
    async fn test_build_slot_destinations() -> Result<()> {
        Ok(())
    }
}

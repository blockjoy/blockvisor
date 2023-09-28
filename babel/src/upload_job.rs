/// This module implements job runner for uploading data. It uploads data according to given
/// manifest and source dir. In case of recoverable errors upload is retried according to given
/// `RestartPolicy`, with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset if upload continue without errors for at least `backoff_timeout_ms`.
use crate::{
    compression::{Coder, NoCoder, ZstdEncoder},
    job_runner::{
        cleanup_job_data, load_job_data, save_job_data, JobBackoff, JobRunner, JobRunnerImpl,
        TransferConfig,
    },
};
use async_trait::async_trait;
use babel_api::engine::{
    Checksum, Chunk, Compression, DownloadManifest, FileLocation, JobProgress, JobStatus,
    RestartPolicy, UploadManifest,
};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer};
use eyre::{anyhow, bail, ensure, Context, Result};
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use nu_glob::{Pattern, PatternError};
use std::{
    cmp::min,
    fs::File,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    usize,
};
use tracing::{debug, error, info};

pub struct UploadJob<T> {
    uploader: Uploader,
    restart_policy: RestartPolicy,
    timer: T,
}

struct Uploader {
    manifest: UploadManifest,
    source_dir: PathBuf,
    exclude: Vec<Pattern>,
    config: TransferConfig,
}

impl<T: AsyncTimer + Send> UploadJob<T> {
    pub fn new(
        timer: T,
        manifest: UploadManifest,
        source_dir: PathBuf,
        exclude: Vec<String>,
        restart_policy: RestartPolicy,
        config: TransferConfig,
    ) -> Result<Self> {
        Ok(Self {
            uploader: Uploader {
                manifest,
                source_dir,
                exclude: exclude
                    .iter()
                    .map(|pattern_str| Pattern::new(pattern_str))
                    .collect::<Result<Vec<Pattern>, PatternError>>()?,
                config,
            },
            restart_policy,
            timer,
        })
    }

    pub async fn run(self, run: RunFlag, name: &str, jobs_dir: &Path) -> JobStatus {
        let parts_file_path = self.uploader.config.parts_file_path.clone();
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
                cleanup_job_data(&parts_file_path);
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
        let mut manifest = if let Some(manifest) = load_job_data(&self.config.parts_file_path) {
            manifest
        } else {
            self.prepare_blueprint()?
        };
        let total_chunks_count = u32::try_from(manifest.chunks.len())?;
        let mut uploaded_chunks = u32::try_from(
            manifest
                .chunks
                .iter()
                .filter(|chunk| chunk.size != 0)
                .count(),
        )?;
        let mut parallel_uploaders_run = run.child_flag();
        let mut uploaders = ParallelChunkUploaders::new(
            parallel_uploaders_run.clone(),
            manifest.chunks.clone(),
            self.config.clone(),
        );
        let mut uploaders_result = Ok(());
        loop {
            if parallel_uploaders_run.load() {
                uploaders.launch_more();
            }
            match uploaders.wait_for_next().await {
                Some(Ok(chunk)) => {
                    let Some(blueprint) = manifest
                        .chunks
                        .iter_mut()
                        .find(|item| item.key == chunk.key)
                    else {
                        bail!("internal error - finished upload of chunk that doesn't exists in manifest");
                    };
                    *blueprint = chunk;
                    uploaded_chunks += 1;
                    save_job_data(&self.config.parts_file_path, &Some(&manifest))?;
                    save_job_data(
                        &self.config.progress_file_path,
                        &JobProgress {
                            total: total_chunks_count,
                            current: uploaded_chunks,
                            message: "chunks".to_string(),
                        },
                    )?;
                }
                Some(Err(err)) => {
                    if uploaders_result.is_ok() {
                        uploaders_result = Err(err);
                        parallel_uploaders_run.stop();
                    }
                }
                None => break,
            }
        }
        uploaders_result?;
        if !run.load() {
            bail!("upload interrupted");
        }
        // make destinations paths relative to source_dir
        for chunk in &mut manifest.chunks {
            for destination in &mut chunk.destinations {
                destination.path = destination
                    .path
                    .strip_prefix(&self.source_dir)?
                    .to_path_buf();
            }
        }
        // finally upload manifest file as json
        let manifest_body = serde_json::to_string(&manifest)?;
        let resp = reqwest::Client::new()
            .put(&self.manifest.manifest_slot.url)
            .body(manifest_body)
            .send()
            .await?;
        ensure!(
            resp.status().is_success(),
            anyhow!(
                "failed to upload manifest file - server responded with {}",
                resp.status()
            )
        );

        cleanup_job_data(&self.config.parts_file_path);
        Ok(())
    }

    /// Prepare DownloadManifest blueprint with files to chunks mapping, based on provided slots.
    fn prepare_blueprint(&self) -> Result<DownloadManifest> {
        if self.manifest.slots.is_empty() {
            bail!("invalid upload manifest - no slots granted");
        }
        let (total_size, mut sources) = sources_list(&self.source_dir, &self.exclude)?;
        sources.sort_by(|a, b| a.path.cmp(&b.path));
        let number_of_slots = u64::try_from(self.manifest.slots.len())?;
        let chunk_size = total_size / number_of_slots;
        let last_chunk_size = chunk_size + total_size % number_of_slots;
        let mut chunks: Vec<_> = Default::default();
        let mut slots = self.manifest.slots.iter();
        while let Some(slot) = slots.next() {
            let chunk_size = if slots.len() > 0 {
                chunk_size
            } else {
                last_chunk_size
            };
            let destinations = build_destinations(chunk_size, &mut sources);
            if destinations.is_empty() {
                // no more files - skip rest of the slots
                break;
            }
            chunks.push(Chunk {
                key: slot.key.clone(),
                url: slot.url.clone(),
                checksum: Checksum::Sha1(Default::default()), // unknown yet
                size: 0,                                      // unknown yet
                destinations,
            });
        }
        Ok(DownloadManifest {
            total_size,
            compression: self.config.compression,
            chunks,
        })
    }
}

/// Prepare list of all source files, recursively walking down the source directory.
fn sources_list(source_path: &Path, exclude: &[Pattern]) -> Result<(u64, Vec<FileLocation>)> {
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

/// Consumes `sources` and put them into chunk destinations list, until chunk is full
/// (according to given `chunk_size`).
fn build_destinations(chunk_size: u64, sources: &mut Vec<FileLocation>) -> Vec<FileLocation> {
    let mut destinations: Vec<_> = Default::default();
    let mut bytes_in_slot = 0;
    while bytes_in_slot <= chunk_size {
        let Some(file) = sources.last_mut() else {
            // no more files
            break;
        };
        if bytes_in_slot >= chunk_size && file.size > 0 {
            // when slot is full, still can add only empty files (if any), or break otherwise
            break;
        }
        let dest_size = min(file.size, chunk_size - bytes_in_slot);
        destinations.push(FileLocation {
            path: file.path.clone(),
            pos: file.pos,
            size: dest_size,
        });
        file.size -= dest_size;
        file.pos += dest_size;
        bytes_in_slot += dest_size;
        if file.size == 0 {
            // go to next file
            sources.pop();
        }
    }
    destinations
}

struct ParallelChunkUploaders<'a> {
    run: RunFlag,
    config: TransferConfig,
    futures: FuturesUnordered<BoxFuture<'a, Result<Chunk>>>,
    chunks: Vec<Chunk>,
}

impl<'a> ParallelChunkUploaders<'a> {
    fn new(run: RunFlag, chunks: Vec<Chunk>, mut config: TransferConfig) -> Self {
        config.max_runners = min(config.max_runners, config.max_opened_files);
        Self {
            run,
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
            if chunk.size != 0 {
                // skip chunks successfully uploaded in previous run
                continue;
            }
            let uploader = ChunkUploader::new(chunk, self.config.clone());
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
    config: TransferConfig,
}

impl ChunkUploader {
    fn new(chunk: Chunk, config: TransferConfig) -> Self {
        Self {
            chunk,
            client: reqwest::Client::new(),
            config,
        }
    }

    async fn run(self, run: RunFlag) -> Result<Chunk> {
        let key = self.chunk.key.clone();
        self.upload_chunk(run)
            .await
            .with_context(|| format!("chunk '{key}' upload failed"))
    }

    async fn upload_chunk(mut self, mut run: RunFlag) -> Result<Chunk> {
        self.chunk.size = self
            .chunk
            .destinations
            .iter()
            .fold(0, |acc, item| acc + item.size);
        let (checksum_tx, checksum_rx) =
            tokio::sync::watch::channel(Checksum::Blake3(blake3::Hasher::new().finalize().into()));
        if self.chunk.size > 0 {
            let body = match self.config.compression {
                None => {
                    reqwest::Body::wrap_stream(futures_util::stream::iter(DestinationsReader::new(
                        self.chunk.destinations.clone(),
                        self.config.clone(),
                        NoCoder::default(),
                        checksum_tx,
                    )?))
                }
                Some(Compression::ZSTD(level)) => {
                    let (parts, compressed_size) = consume_reader(
                        run.clone(),
                        DestinationsReader::new(
                            self.chunk.destinations.clone(),
                            self.config.clone(),
                            ZstdEncoder::new(level)?,
                            checksum_tx,
                        )?,
                    )
                    .await?;
                    self.chunk.size = compressed_size; // update chunk size after compression
                    reqwest::Body::wrap_stream(futures_util::stream::iter(parts))
                }
            };
            if let Some(resp) = run
                .select(
                    self.client
                        .put(&self.chunk.url)
                        .header("Content-Length", format!("{}", self.chunk.size))
                        .body(body)
                        .send(),
                )
                .await
            {
                let resp = resp?;
                ensure!(
                    resp.status().is_success(),
                    anyhow!("server responded with {}", resp.status())
                );
            } else {
                bail!("upload interrupted");
            }
        }
        self.chunk.checksum = checksum_rx.borrow().clone();
        self.chunk.url.clear();
        Ok(self.chunk)
    }
}

async fn consume_reader<E: Coder>(
    mut run: RunFlag,
    reader: DestinationsReader<E>,
) -> Result<(Vec<Result<Vec<u8>>>, u64)> {
    let mut parts = Vec::default();
    let mut total_size = 0;
    for part in reader {
        if !run.load() {
            bail!("reading chunk data interrupted");
        }
        let part = part?;
        total_size += part.len();
        parts.push(Ok(part));
    }
    Ok((parts, u64::try_from(total_size)?))
}

struct FileDescriptor {
    file: File,
    offset: u64,
    bytes_remaining: usize,
}

struct InterimBuffer<E> {
    digest: blake3::Hasher,
    encoder: E,
    checksum_tx: tokio::sync::watch::Sender<Checksum>,
}

struct DestinationsReader<E> {
    iter: Vec<FileLocation>,
    current: FileDescriptor,
    bytes_read: u64,
    config: TransferConfig,
    interim_buffer: Option<InterimBuffer<E>>,
}

impl<E: Coder> DestinationsReader<E> {
    fn new(
        mut iter: Vec<FileLocation>,
        config: TransferConfig,
        encoder: E,
        checksum_tx: tokio::sync::watch::Sender<Checksum>,
    ) -> Result<Self> {
        iter.reverse();
        let last = match iter.pop() {
            None => {
                error!("corrupted manifest - this is internal BV error, manifest shall be already validated");
                Err(anyhow!(
                    "corrupted manifest - expected at least one destination file in chunk"
                ))
            }
            Some(first) => Ok(first),
        }?;
        let current = FileDescriptor {
            file: File::open(&last.path)?,
            offset: last.pos,
            bytes_remaining: usize::try_from(last.size)?,
        };
        Ok(Self {
            iter,
            current,
            bytes_read: 0,
            config,
            interim_buffer: Some(InterimBuffer {
                digest: blake3::Hasher::new(),
                encoder,
                checksum_tx,
            }),
        })
    }

    fn try_next(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(if self.interim_buffer.is_some() {
            loop {
                // first try to read some data from disk
                let buffer = self.read_data()?;
                self.bytes_read += u64::try_from(buffer.len())?;
                if self.iter.is_empty() && self.current.bytes_remaining == 0 {
                    // it is end of chunk, so we can take interim_buffer (won't be used anymore) and finalize it's members
                    let mut interim = self.interim_buffer.take().unwrap();
                    // feed encoder with last data if any
                    interim.encoder.feed(buffer)?;
                    // finalize compression, since no more data will come
                    let compressed = interim.encoder.finalize()?;
                    interim.digest.update(&compressed);
                    // finalize checksum and send summary to uploader
                    let _ = interim
                        .checksum_tx
                        .send(Checksum::Blake3(interim.digest.finalize().into()));
                    // return last part of data
                    break Some(compressed);
                } else {
                    // somewhere in the middle, so take only reference to interim_buffer
                    let interim = self.interim_buffer.as_mut().unwrap();
                    interim.encoder.feed(buffer)?;
                    // try to consume compressed data from encoder
                    let compressed = interim.encoder.consume()?;
                    if !compressed.is_empty() {
                        interim.digest.update(&compressed);
                        break Some(compressed);
                    }
                }
            }
        } else {
            None
        })
    }

    fn read_data(&mut self) -> Result<Vec<u8>> {
        let mut buffer = Vec::with_capacity(self.config.max_buffer_size);
        while buffer.len() < self.config.max_buffer_size {
            if self.current.bytes_remaining == 0 {
                let Some(next) = self.iter.pop() else {
                    break;
                };
                self.current = FileDescriptor {
                    file: File::open(&next.path)?,
                    offset: next.pos,
                    bytes_remaining: usize::try_from(next.size)?,
                };
            }
            let bytes_to_read = min(
                self.current.bytes_remaining,
                buffer.capacity() - buffer.len(),
            );
            let start = buffer.len();
            buffer.resize(start + bytes_to_read, 0);
            self.current.file.read_exact_at(
                &mut buffer[start..(start + bytes_to_read)],
                self.current.offset,
            )?;
            self.current.bytes_remaining -= bytes_to_read;
            self.current.offset += u64::try_from(bytes_to_read)?;
        }
        Ok(buffer)
    }
}

impl<E: Coder> Iterator for DestinationsReader<E> {
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
    use babel_api::engine::{RestartConfig, Slot};
    use bv_utils::timer::SysTimer;
    use mockito::{Matcher, Server, ServerGuard};
    use std::fs;

    struct TestEnv {
        tmp_dir: PathBuf,
        upload_parts_path: PathBuf,
        upload_progress_path: PathBuf,
        server: ServerGuard,
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_dir = TempDir::new()?.to_path_buf();
        dummy_sources(&tmp_dir)?;
        let server = Server::new();
        let upload_parts_path = tmp_dir.join("upload.parts");
        let upload_progress_path = tmp_dir.join("upload.progress");
        Ok(TestEnv {
            tmp_dir,
            server,
            upload_parts_path,
            upload_progress_path,
        })
    }

    fn dummy_sources(path: &Path) -> Result<()> {
        fs::create_dir_all(path)?;
        File::create(path.join("empty_file"))?;
        fs::write(path.join("a"), "7 bytes")?;
        fs::create_dir_all(path.join("d1").join("d2"))?;
        fs::write(path.join("d1").join("b"), "9   bytes")?;
        fs::write(path.join("d1").join("d2").join("c"), "333")?;
        fs::write(path.join("x"), [120u8; 256])?;
        fs::create_dir_all(path.join("sub_with_ignored"))?;
        fs::write(
            path.join("sub_with_ignored").join("ignored_file"),
            [1u8; 16],
        )?;
        Ok(())
    }

    impl TestEnv {
        fn upload_job(&self, manifest: UploadManifest) -> UploadJob<SysTimer> {
            UploadJob {
                uploader: Uploader {
                    manifest,
                    source_dir: self.tmp_dir.clone(),
                    exclude: vec![Pattern::new("**/ignored_*").unwrap()],
                    config: TransferConfig {
                        max_opened_files: 1,
                        max_runners: 4,
                        max_buffer_size: 50,
                        max_retries: 0,
                        backoff_base_ms: 1,
                        parts_file_path: self.upload_parts_path.clone(),
                        progress_file_path: self.upload_progress_path.clone(),
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

        fn dummy_upload_manifest(&self) -> Result<UploadManifest> {
            Ok(UploadManifest {
                slots: vec![
                    Slot {
                        key: "KeyA".to_string(),
                        url: self.url("url.a"),
                    },
                    Slot {
                        key: "KeyB".to_string(),
                        url: self.url("url.b"),
                    },
                    Slot {
                        key: "KeyC".to_string(),
                        url: self.url("url.c"),
                    },
                ],
                manifest_slot: Slot {
                    key: "KeyM".to_string(),
                    url: self.url("url.m"),
                },
            })
        }

        fn expected_download_manifest(&self) -> Result<String> {
            Ok(serde_json::to_string(&DownloadManifest {
                total_size: 275,
                compression: None,
                chunks: vec![
                    Chunk {
                        key: "KeyA".to_string(),
                        url: Default::default(),
                        checksum: Checksum::Blake3([
                            119, 175, 9, 4, 145, 218, 117, 139, 245, 72, 66, 12, 252, 244, 95, 29,
                            198, 151, 102, 4, 20, 229, 205, 55, 90, 194, 137, 167, 103, 54, 187,
                            43,
                        ]),
                        size: 91,
                        destinations: vec![FileLocation {
                            path: PathBuf::from("x"),
                            pos: 0,
                            size: 91,
                        }],
                    },
                    Chunk {
                        key: "KeyB".to_string(),
                        url: Default::default(),
                        checksum: Checksum::Blake3([
                            119, 175, 9, 4, 145, 218, 117, 139, 245, 72, 66, 12, 252, 244, 95, 29,
                            198, 151, 102, 4, 20, 229, 205, 55, 90, 194, 137, 167, 103, 54, 187,
                            43,
                        ]),
                        size: 91,
                        destinations: vec![FileLocation {
                            path: PathBuf::from("x"),
                            pos: 91,
                            size: 91,
                        }],
                    },
                    Chunk {
                        key: "KeyC".to_string(),
                        url: Default::default(),
                        checksum: Checksum::Blake3([
                            140, 55, 10, 200, 137, 153, 26, 146, 228, 4, 166, 66, 49, 76, 128, 117,
                            82, 90, 240, 126, 94, 112, 246, 153, 33, 150, 131, 176, 100, 171, 105,
                            248,
                        ]),
                        size: 93,
                        destinations: vec![
                            FileLocation {
                                path: PathBuf::from("x"),
                                pos: 182,
                                size: 74,
                            },
                            FileLocation {
                                path: PathBuf::from("empty_file"),
                                pos: 0,
                                size: 0,
                            },
                            FileLocation {
                                path: PathBuf::from("d1/d2/c"),
                                pos: 0,
                                size: 3,
                            },
                            FileLocation {
                                path: PathBuf::from("d1/b"),
                                pos: 0,
                                size: 9,
                            },
                            FileLocation {
                                path: PathBuf::from("a"),
                                pos: 0,
                                size: 7,
                            },
                        ],
                    },
                ],
            })?)
        }
    }

    fn normalize_manifest(manifest: &mut DownloadManifest) {
        manifest.chunks.sort_by(|a, b| a.key.cmp(&b.key));
        for chunk in &mut manifest.chunks {
            chunk
                .destinations
                .sort_by(|a, b| a.path.file_stem().unwrap().cmp(b.path.file_stem().unwrap()));
        }
    }

    #[tokio::test]
    async fn test_full_upload_ok() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let expected_body = String::from_utf8([120u8; 91].to_vec())?;
        test_env
            .server
            .mock("PUT", "/url.a")
            .match_body(Matcher::Exact(expected_body))
            .create();
        let expected_body = String::from_utf8([120u8; 91].to_vec())?;
        test_env
            .server
            .mock("PUT", "/url.b")
            .match_body(Matcher::Exact(expected_body))
            .create();
        let expected_body = format!(
            "{}3339   bytes7 bytes",
            String::from_utf8([120u8; 74].to_vec())?
        );
        test_env
            .server
            .mock("PUT", "/url.c")
            .match_body(Matcher::Exact(expected_body))
            .create();
        let expected_manifest = test_env.expected_download_manifest()?;
        test_env
            .server
            .mock("PUT", "/url.m")
            .match_body(Matcher::Exact(expected_manifest))
            .create();

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: Default::default(),
            },
            test_env
                .upload_job(test_env.dummy_upload_manifest()?)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );

        assert!(!test_env.upload_parts_path.exists());
        assert!(test_env.upload_progress_path.exists());
        let progress = fs::read_to_string(&test_env.upload_progress_path).unwrap();
        assert_eq!(&progress, r#"{"total":3,"current":3,"message":"chunks"}"#);

        Ok(())
    }

    #[tokio::test]
    async fn test_upload_with_compression() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let mut encoder = ZstdEncoder::new(5)?;
        encoder.feed([120u8; 91].to_vec())?;
        test_env
            .server
            .mock("PUT", "/url.a")
            .match_body(Matcher::from(encoder.finalize()?))
            .create();
        let mut encoder = ZstdEncoder::new(5)?;
        encoder.feed([120u8; 91].to_vec())?;
        let expected_b_body = encoder.finalize()?;
        test_env
            .server
            .mock("PUT", "/url.b")
            .with_status(404)
            .match_body(Matcher::from(expected_b_body.clone()))
            .create();
        test_env
            .server
            .mock("PUT", "/url.b")
            .match_body(Matcher::from(expected_b_body))
            .create();
        let mut encoder = ZstdEncoder::new(5)?;
        encoder.feed(
            format!(
                "{}3339   bytes7 bytes",
                String::from_utf8([120u8; 74].to_vec())?
            )
            .into_bytes(),
        )?;
        test_env
            .server
            .mock("PUT", "/url.c")
            .match_body(Matcher::from(encoder.finalize()?))
            .create();
        let expected_manifest = serde_json::to_string(&DownloadManifest {
            total_size: 275,
            compression: Some(Compression::ZSTD(5)),
            chunks: vec![
                Chunk {
                    key: "KeyA".to_string(),
                    url: Default::default(),
                    checksum: Checksum::Blake3([
                        53, 33, 159, 172, 97, 35, 0, 46, 105, 43, 143, 177, 157, 51, 214, 88, 140,
                        186, 13, 108, 22, 182, 47, 45, 167, 111, 224, 73, 196, 7, 128, 213,
                    ]),
                    size: 17,
                    destinations: vec![FileLocation {
                        path: PathBuf::from("x"),
                        pos: 0,
                        size: 91,
                    }],
                },
                Chunk {
                    key: "KeyB".to_string(),
                    url: Default::default(),
                    checksum: Checksum::Blake3([
                        53, 33, 159, 172, 97, 35, 0, 46, 105, 43, 143, 177, 157, 51, 214, 88, 140,
                        186, 13, 108, 22, 182, 47, 45, 167, 111, 224, 73, 196, 7, 128, 213,
                    ]),
                    size: 17,
                    destinations: vec![FileLocation {
                        path: PathBuf::from("x"),
                        pos: 91,
                        size: 91,
                    }],
                },
                Chunk {
                    key: "KeyC".to_string(),
                    url: Default::default(),
                    checksum: Checksum::Blake3([
                        34, 152, 245, 110, 117, 204, 105, 42, 58, 8, 75, 147, 111, 165, 203, 58,
                        40, 146, 100, 173, 198, 47, 52, 132, 182, 129, 65, 57, 84, 73, 187, 158,
                    ]),
                    size: 36,
                    destinations: vec![
                        FileLocation {
                            path: PathBuf::from("x"),
                            pos: 182,
                            size: 74,
                        },
                        FileLocation {
                            path: PathBuf::from("empty_file"),
                            pos: 0,
                            size: 0,
                        },
                        FileLocation {
                            path: PathBuf::from("d1/d2/c"),
                            pos: 0,
                            size: 3,
                        },
                        FileLocation {
                            path: PathBuf::from("d1/b"),
                            pos: 0,
                            size: 9,
                        },
                        FileLocation {
                            path: PathBuf::from("a"),
                            pos: 0,
                            size: 7,
                        },
                    ],
                },
            ],
        })?;
        test_env
            .server
            .mock("PUT", "/url.m")
            .match_body(Matcher::Exact(expected_manifest))
            .create();

        let mut job = test_env.upload_job(test_env.dummy_upload_manifest()?);
        job.uploader.config.compression = Some(Compression::ZSTD(5));
        job.restart_policy = RestartPolicy::OnFailure(RestartConfig {
            backoff_timeout_ms: 1000,
            backoff_base_ms: 1,
            max_retries: Some(1),
        });
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: Default::default(),
            },
            job.run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_empty_upload_ok() -> Result<()> {
        let mut test_env = setup_test_env()?;
        fs::remove_dir_all(&test_env.tmp_dir)?;
        fs::create_dir_all(&test_env.tmp_dir)?;
        File::create(test_env.tmp_dir.join("empty_file_1"))?;
        File::create(test_env.tmp_dir.join("empty_file_2"))?;

        let expected_manifest = serde_json::to_string(&DownloadManifest {
            total_size: 0,
            compression: None,
            chunks: vec![Chunk {
                key: "KeyA".to_string(),
                url: Default::default(),
                checksum: Checksum::Blake3([
                    175, 19, 73, 185, 245, 249, 161, 166, 160, 64, 77, 234, 54, 220, 201, 73, 155,
                    203, 37, 201, 173, 193, 18, 183, 204, 154, 147, 202, 228, 31, 50, 98,
                ]),
                size: 0,
                destinations: vec![
                    FileLocation {
                        path: PathBuf::from("empty_file_2"),
                        pos: 0,
                        size: 0,
                    },
                    FileLocation {
                        path: PathBuf::from("empty_file_1"),
                        pos: 0,
                        size: 0,
                    },
                ],
            }],
        })?;
        test_env
            .server
            .mock("PUT", "/url.m")
            .match_body(Matcher::Exact(expected_manifest))
            .create();

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: Default::default(),
            },
            test_env
                .upload_job(test_env.dummy_upload_manifest()?)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_restore_upload_ok() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let expected_body = format!(
            "{}3339   bytes7 bytes",
            String::from_utf8([120u8; 74].to_vec())?
        );
        test_env
            .server
            .mock("PUT", "/url.c")
            .match_body(Matcher::Exact(expected_body))
            .create();
        let expected_manifest = test_env.expected_download_manifest()?;
        test_env
            .server
            .mock("PUT", "/url.m")
            .match_body(Matcher::Exact(expected_manifest))
            .create();

        let job = test_env.upload_job(test_env.dummy_upload_manifest()?);
        // mark first two as uploaded
        let mut progress = job.uploader.prepare_blueprint()?;
        let chunk_a = progress
            .chunks
            .iter_mut()
            .find(|item| item.key == "KeyA")
            .unwrap();
        chunk_a.size = 91;
        chunk_a.checksum = Checksum::Blake3([
            119, 175, 9, 4, 145, 218, 117, 139, 245, 72, 66, 12, 252, 244, 95, 29, 198, 151, 102,
            4, 20, 229, 205, 55, 90, 194, 137, 167, 103, 54, 187, 43,
        ]);
        chunk_a.url.clear();
        let chunk_b = progress
            .chunks
            .iter_mut()
            .find(|item| item.key == "KeyB")
            .unwrap();
        chunk_b.size = 91;
        chunk_b.checksum = Checksum::Blake3([
            119, 175, 9, 4, 145, 218, 117, 139, 245, 72, 66, 12, 252, 244, 95, 29, 198, 151, 102,
            4, 20, 229, 205, 55, 90, 194, 137, 167, 103, 54, 187, 43,
        ]);
        chunk_b.url.clear();
        save_job_data(&job.uploader.config.parts_file_path, &Some(&progress))?;

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: Default::default(),
            },
            job.run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );
        assert!(!test_env.upload_parts_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_server_errors() -> Result<()> {
        let test_env = setup_test_env()?;

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "upload_job 'name' failed with: chunk 'KeyC' upload failed: server responded with 501 Not Implemented"
                    .to_string()
            },
            test_env
                .upload_job(test_env.dummy_upload_manifest()?)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_invalid_source_dir() -> Result<()> {
        let test_env = setup_test_env()?;
        let mut job = test_env.upload_job(test_env.dummy_upload_manifest()?);
        job.uploader.source_dir = PathBuf::from("some/invalid/source");
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "upload_job 'name' failed with: IO error for operation on some/invalid/source: No such file or directory (os error 2): No such file or directory (os error 2)"
                    .to_string()
            },
            job.run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_cleanup_after_fail() -> Result<()> {
        let test_env = setup_test_env()?;
        let manifest = UploadManifest {
            slots: vec![],
            manifest_slot: Slot {
                key: "KeyM".to_string(),
                url: "url.m".to_string(),
            },
        };
        fs::write(&test_env.upload_parts_path, r#"["key"]"#)?;
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message:
                    "upload_job 'name' failed with: invalid upload manifest - no slots granted"
                        .to_string()
            },
            test_env
                .upload_job(manifest)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );
        assert!(!test_env.upload_parts_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_prepare_blueprint() -> Result<()> {
        let test_env = setup_test_env()?;
        let manifest = UploadManifest {
            slots: vec![
                Slot {
                    key: "KeyA".to_string(),
                    url: "url.a".to_string(),
                },
                Slot {
                    key: "KeyB".to_string(),
                    url: "url.b".to_string(),
                },
            ],
            manifest_slot: Slot {
                key: "KeyM".to_string(),
                url: "url.m".to_string(),
            },
        };
        let job = test_env.upload_job(manifest);
        let mut blueprint = job.uploader.prepare_blueprint()?;
        normalize_manifest(&mut blueprint);
        assert_eq!(
            DownloadManifest {
                total_size: 275,
                compression: None,
                chunks: vec![
                    Chunk {
                        key: "KeyA".to_string(),
                        url: "url.a".to_string(),
                        checksum: Checksum::Sha1([0u8; 20]),
                        size: 0,
                        destinations: vec![FileLocation {
                            path: test_env.tmp_dir.join("x"),
                            pos: 0,
                            size: 137,
                        }],
                    },
                    Chunk {
                        key: "KeyB".to_string(),
                        url: "url.b".to_string(),
                        checksum: Checksum::Sha1([0u8; 20]),
                        size: 0,
                        destinations: vec![
                            FileLocation {
                                path: test_env.tmp_dir.join("a"),
                                pos: 0,
                                size: 7,
                            },
                            FileLocation {
                                path: test_env.tmp_dir.join("d1/b"),
                                pos: 0,
                                size: 9,
                            },
                            FileLocation {
                                path: test_env.tmp_dir.join("d1/d2/c"),
                                pos: 0,
                                size: 3,
                            },
                            FileLocation {
                                path: test_env.tmp_dir.join("empty_file"),
                                pos: 0,
                                size: 0,
                            },
                            FileLocation {
                                path: test_env.tmp_dir.join("x"),
                                pos: 137,
                                size: 119,
                            }
                        ],
                    }
                ]
            },
            blueprint
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_sources_list() -> Result<()> {
        let tmp_dir = TempDir::new()?.to_path_buf();
        dummy_sources(&tmp_dir)?;
        let (total_size, mut sources) =
            sources_list(&tmp_dir, &[Pattern::new("*_ignored/ignored_*")?])?;
        assert_eq!(275, total_size);
        sources.sort_by(|a, b| a.path.file_stem().unwrap().cmp(b.path.file_stem().unwrap()));
        assert_eq!(
            vec![
                FileLocation {
                    path: tmp_dir.join("a"),
                    pos: 0,
                    size: 7,
                },
                FileLocation {
                    path: tmp_dir.join("d1").join("b"),
                    pos: 0,
                    size: 9,
                },
                FileLocation {
                    path: tmp_dir.join("d1").join("d2").join("c"),
                    pos: 0,
                    size: 3,
                },
                FileLocation {
                    path: tmp_dir.join("empty_file"),
                    pos: 0,
                    size: 0
                },
                FileLocation {
                    path: tmp_dir.join("x"),
                    pos: 0,
                    size: 256,
                },
            ],
            sources
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_build_slot_destinations() -> Result<()> {
        let mut sources = vec![
            FileLocation {
                path: PathBuf::from("f"),
                pos: 0,
                size: 0,
            },
            FileLocation {
                path: PathBuf::from("e"),
                pos: 0,
                size: 77,
            },
            FileLocation {
                path: PathBuf::from("d"),
                pos: 0,
                size: 2048,
            },
            FileLocation {
                path: PathBuf::from("c"),
                pos: 0,
                size: 512,
            },
            FileLocation {
                path: PathBuf::from("b"),
                pos: 0,
                size: 0,
            },
            FileLocation {
                path: PathBuf::from("a"),
                pos: 0,
                size: 0,
            },
        ];
        // split too big
        let destinations = build_destinations(1024, &mut sources);
        assert_eq!(
            vec![
                FileLocation {
                    path: PathBuf::from("f"),
                    pos: 0,
                    size: 0,
                },
                FileLocation {
                    path: PathBuf::from("e"),
                    pos: 0,
                    size: 77,
                },
                FileLocation {
                    path: PathBuf::from("d"),
                    pos: 512,
                    size: 1536,
                },
            ],
            sources
        );
        assert_eq!(
            vec![
                FileLocation {
                    path: PathBuf::from("a"),
                    pos: 0,
                    size: 0,
                },
                FileLocation {
                    path: PathBuf::from("b"),
                    pos: 0,
                    size: 0,
                },
                FileLocation {
                    path: PathBuf::from("c"),
                    pos: 0,
                    size: 512,
                },
                FileLocation {
                    path: PathBuf::from("d"),
                    pos: 0,
                    size: 512,
                },
            ],
            destinations
        );
        // continue split
        let destinations = build_destinations(1536, &mut sources);
        assert_eq!(
            vec![
                FileLocation {
                    path: PathBuf::from("f"),
                    pos: 0,
                    size: 0,
                },
                FileLocation {
                    path: PathBuf::from("e"),
                    pos: 0,
                    size: 77,
                },
            ],
            sources
        );
        // fill chunk as much as possible
        assert_eq!(
            vec![FileLocation {
                path: PathBuf::from("d"),
                pos: 512,
                size: 1536,
            },],
            destinations
        );

        let destinations = build_destinations(1024, &mut sources);
        assert!(sources.is_empty());
        assert_eq!(
            vec![
                FileLocation {
                    path: PathBuf::from("e"),
                    pos: 0,
                    size: 77,
                },
                FileLocation {
                    path: PathBuf::from("f"),
                    pos: 0,
                    size: 0,
                },
            ],
            destinations
        );
        Ok(())
    }
}

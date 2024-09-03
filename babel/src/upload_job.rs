/// This module implements job runner for uploading data. It uploads data according to given
/// manifest and source dir. In case of recoverable errors upload is retried according to given
/// `RestartPolicy`, with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset if upload continue without errors for at least `backoff_timeout_ms`.
use crate::{
    compression::{Coder, NoCoder, ZstdEncoder},
    job_runner::{ConnectionPool, Runner, TransferConfig},
    jobs::{load_chunks, load_job_data, save_chunk, save_job_data, RunnersState},
    pal::BabelEngineConnector,
    utils::sources_list,
    BabelEngineClient,
};
use async_trait::async_trait;
use babel_api::engine::{
    Checksum, Chunk, Compression, DownloadManifest, FileLocation, JobProgress, Slot, UploadSlots,
};
use bv_utils::{
    rpc::{with_timeout, RPC_REQUEST_TIMEOUT},
    {run_flag::RunFlag, with_retry},
};
use eyre::{anyhow, bail, ensure, Context, Result};
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use nu_glob::{Pattern, PatternError};
use serde::{Deserialize, Serialize};
use std::{
    cmp::min,
    fs,
    fs::File,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::Semaphore, task::JoinError};
use tracing::error;

// if uploading single chunk (about 500MB) takes more than 50min, it means that something
// is not ok
const UPLOAD_SINGLE_CHUNK_TIMEOUT: Duration = Duration::from_secs(50 * 60);
const BLUEPRINT_FILENAME: &str = "upload.blueprint";
const CHUNKS_FILENAME: &str = "upload.chunks";

pub fn cleanup_job(meta_dir: &Path) -> Result<()> {
    let blueprint_path = meta_dir.join(BLUEPRINT_FILENAME);
    if blueprint_path.exists() {
        fs::remove_file(&blueprint_path).with_context(|| {
            format!(
                "failed to cleanup upload blueprint file `{}`",
                blueprint_path.display()
            )
        })?;
    }
    let chunks_path = meta_dir.join(CHUNKS_FILENAME);
    if chunks_path.exists() {
        fs::remove_file(&chunks_path).with_context(|| {
            format!(
                "failed to cleanup upload chunks file `{}`",
                chunks_path.display()
            )
        })?;
    }
    Ok(())
}

pub struct Uploader<C> {
    connector: C,
    source_dir: PathBuf,
    exclude: Vec<Pattern>,
    config: TransferConfig,
    total_slots: u32,
    url_expires_secs: u32,
    data_version: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct Blueprint {
    manifest: DownloadManifest,
    data_version: u64,
}

#[async_trait]
impl<C: BabelEngineConnector + Send> Runner for Uploader<C> {
    async fn run(&mut self, mut run: RunFlag) -> Result<()> {
        let blueprint_path = self.config.archive_jobs_meta_dir.join(BLUEPRINT_FILENAME);
        let chunks_path = self.config.archive_jobs_meta_dir.join(CHUNKS_FILENAME);
        let (mut blueprint, uploaded) =
            if let Ok(blueprint) = load_job_data::<Blueprint>(&blueprint_path) {
                (blueprint, load_chunks(&chunks_path)?)
            } else {
                let manifest = self.prepare_manifest_blueprint()?;
                let slots = fetch_slots(
                    self.connector.connect(),
                    &manifest.chunks,
                    self.config.max_runners,
                    self.data_version,
                    self.url_expires_secs,
                )
                .await?;
                let mut blueprint = Blueprint {
                    manifest,
                    data_version: slots.data_version,
                };
                save_job_data(&blueprint_path, &blueprint)?;
                assign_slots(&mut blueprint.manifest.chunks, slots.slots);
                (blueprint, Default::default())
            };
        let mark_uploaded = |chunks: &mut Vec<Chunk>, chunk: Chunk| {
            let Some(blueprint) = chunks.iter_mut().find(|item| item.index == chunk.index) else {
                bail!("internal error - finished upload of chunk that doesn't exists in manifest");
            };
            *blueprint = chunk;
            Ok(())
        };
        let mut uploaded_chunks = uploaded.len() as u32;
        for chunk in uploaded {
            mark_uploaded(&mut blueprint.manifest.chunks, chunk)?;
        }
        let mut parallel_uploaders_run = run.child_flag();
        let mut uploaders = ParallelChunkUploaders::new(
            parallel_uploaders_run.clone(),
            blueprint.manifest.chunks.clone(),
            self.config.clone(),
            self.url_expires_secs,
            blueprint.data_version,
        );
        let mut save_uploaded = |chunk| {
            save_chunk(&chunks_path, &chunk)?;
            mark_uploaded(&mut blueprint.manifest.chunks, chunk)?;
            uploaded_chunks += 1;
            save_job_data(
                &self.config.progress_file_path,
                &JobProgress {
                    total: self.total_slots,
                    current: uploaded_chunks,
                    message: "chunks".to_string(),
                },
            )
        };
        let mut uploaders_state = RunnersState {
            result: Ok(()),
            run: parallel_uploaders_run,
        };
        loop {
            if uploaders_state.run.load() {
                if let Err(err) = uploaders.update_slots(self.connector.connect()).await {
                    uploaders_state.handle_error(err);
                } else {
                    uploaders.launch_more(&self.connector);
                }
            }
            match uploaders.wait_for_next().await {
                Some(Ok(chunk)) => {
                    if let Err(err) = save_uploaded(chunk) {
                        uploaders_state.handle_error(err);
                    }
                }
                Some(Err(err)) => uploaders_state.handle_error(err),
                None => break,
            }
        }
        uploaders_state.result?;
        if !run.load() {
            bail!("upload interrupted");
        }
        // make destinations paths relative to source_dir
        for chunk in &mut blueprint.manifest.chunks {
            for destination in &mut chunk.destinations {
                destination.path = destination
                    .path
                    .strip_prefix(&self.source_dir)?
                    .to_path_buf();
            }
        }
        // DownloadManifest may be pretty big, so better set longer timeout that depends on number of chunks
        let custom_timeout = RPC_REQUEST_TIMEOUT
            + bv_utils::rpc::estimate_put_download_manifest_request_timeout(
                blueprint.manifest.chunks.len(),
            );
        let mut client = self.connector.connect();
        client
            .put_download_manifest(with_timeout(blueprint.manifest, custom_timeout))
            .await
            .with_context(|| "failed to send DownloadManifest blueprint back to API")?;

        cleanup_job(&self.config.archive_jobs_meta_dir)?;
        Ok(())
    }
}

async fn fetch_slots(
    mut client: BabelEngineClient,
    chunks: &[Chunk],
    count: usize,
    data_version: Option<u64>,
    url_expires_secs: u32,
) -> Result<UploadSlots> {
    let chunks = chunks.iter().rev().take(count);
    Ok(client
        .get_upload_slots((
            data_version,
            chunks.map(|chunk| chunk.index).collect(),
            url_expires_secs,
        ))
        .await?
        .into_inner())
}

fn assign_slots(chunks: &mut [Chunk], slots: Vec<Slot>) {
    for slot in slots {
        if let Some(chunk) = chunks.iter_mut().find(|chunk| chunk.index == slot.index) {
            chunk.key = slot.key;
            chunk.url = Some(slot.url);
        }
    }
}

impl<C: BabelEngineConnector> Uploader<C> {
    pub fn new(
        connector: C,
        source_dir: PathBuf,
        exclude: Vec<String>,
        number_of_chunks: Option<u32>,
        url_expires_secs: Option<u32>,
        data_version: Option<u64>,
        config: TransferConfig,
    ) -> Result<Self> {
        let exclude = exclude
            .iter()
            .map(|pattern_str| Pattern::new(pattern_str))
            .collect::<Result<Vec<Pattern>, PatternError>>()?;
        let total_slots = if let Some(number_of_chunks) = number_of_chunks {
            if number_of_chunks == 0 {
                bail!("invalid number of chunks - need at leas one");
            }
            number_of_chunks
        } else {
            let (total_size, _) = sources_list(&source_dir, &exclude)?;
            // recommended size of chunk is around 500MB
            (1 + total_size / 500_000_000) as u32
        };
        let url_expires_secs = url_expires_secs.unwrap_or(3600 + (config.max_runners as u32) * 15);
        Ok(Self {
            connector,
            source_dir,
            exclude,
            config,
            total_slots,
            url_expires_secs,
            data_version,
        })
    }

    /// Prepare DownloadManifest blueprint with files to chunks mapping, based on provided slots.
    fn prepare_manifest_blueprint(&self) -> Result<DownloadManifest> {
        let (total_size, mut sources) = sources_list(&self.source_dir, &self.exclude)?;
        sources.sort_by(|a, b| a.path.cmp(&b.path));
        let chunk_size = total_size / self.total_slots as u64;
        let last_chunk_size = chunk_size + total_size % self.total_slots as u64;
        let mut chunks: Vec<_> = Default::default();
        let mut index = 0;
        while index < self.total_slots {
            let chunk_size = if index < self.total_slots - 1 {
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
                index,
                key: Default::default(),
                url: None,
                checksum: Checksum::Sha1(Default::default()), // unknown yet
                size: 0,                                      // unknown yet
                destinations,
            });
            index += 1;
        }
        Ok(DownloadManifest {
            total_size,
            compression: self.config.compression,
            chunks,
        })
    }
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
    futures: FuturesUnordered<BoxFuture<'a, Result<Result<Chunk>, JoinError>>>,
    chunks: Vec<Chunk>,
    connection_pool: ConnectionPool,
    url_expires_secs: u32,
    data_version: u64,
}

impl<'a> ParallelChunkUploaders<'a> {
    fn new(
        run: RunFlag,
        mut chunks: Vec<Chunk>,
        mut config: TransferConfig,
        url_expires_secs: u32,
        data_version: u64,
    ) -> Self {
        config.max_runners = min(config.max_runners, config.max_opened_files);
        let connection_pool = Arc::new(Semaphore::new(config.max_connections));
        chunks.retain(|chunk| chunk.size == 0);

        Self {
            run,
            config,
            futures: FuturesUnordered::new(),
            chunks,
            connection_pool,
            url_expires_secs,
            data_version,
        }
    }

    async fn update_slots(&mut self, client: BabelEngineClient) -> Result<()> {
        if let Some(Chunk { url: None, .. }) = self.chunks.last() {
            let slots = fetch_slots(
                client,
                &self.chunks,
                self.config.max_runners,
                Some(self.data_version),
                self.url_expires_secs,
            )
            .await?
            .slots;
            assign_slots(&mut self.chunks, slots);
        }
        Ok(())
    }

    fn launch_more(&mut self, connector: &impl BabelEngineConnector) {
        while self.futures.len() < self.config.max_runners {
            let Some(chunk) = self.chunks.pop() else {
                break;
            };
            let uploader =
                ChunkUploader::new(chunk, self.config.clone(), self.connection_pool.clone());
            let client = connector.connect();
            self.futures.push(Box::pin(tokio::spawn(
                uploader.run(self.run.clone(), client),
            )));
        }
    }

    async fn wait_for_next(&mut self) -> Option<Result<Chunk>> {
        self.futures
            .next()
            .await
            .map(|r| r.unwrap_or_else(|err| bail!("{err:#}")))
    }
}

struct ChunkUploader {
    chunk: Chunk,
    config: TransferConfig,
    connection_pool: ConnectionPool,
}

impl ChunkUploader {
    fn new(chunk: Chunk, config: TransferConfig, connection_pool: ConnectionPool) -> Self {
        Self {
            chunk,
            config,
            connection_pool,
        }
    }

    async fn run(self, run: RunFlag, client: BabelEngineClient) -> Result<Chunk> {
        let key = self.chunk.key.clone();
        self.upload_chunk(run, client)
            .await
            .with_context(|| format!("chunk '{key}' upload failed"))
    }

    async fn upload_chunk(mut self, mut run: RunFlag, client: BabelEngineClient) -> Result<Chunk> {
        self.chunk.size = self
            .chunk
            .destinations
            .iter()
            .fold(0, |acc, item| acc + item.size);
        let (checksum_tx, checksum_rx) =
            tokio::sync::watch::channel(Checksum::Blake3(blake3::Hasher::new().finalize().into()));
        if self.chunk.size > 0 {
            let body = match self.config.compression {
                None => reqwest::Body::wrap_stream(futures_util::stream::iter(
                    DestinationsReader::new(
                        client,
                        self.chunk.destinations.clone(),
                        self.config.clone(),
                        NoCoder::default(),
                        checksum_tx,
                    )
                    .await?,
                )),
                Some(Compression::ZSTD(level)) => {
                    let (parts, compressed_size) = consume_reader(
                        run.clone(),
                        DestinationsReader::new(
                            client,
                            self.chunk.destinations.clone(),
                            self.config.clone(),
                            ZstdEncoder::new(level)?,
                            checksum_tx,
                        )
                        .await?,
                    )
                    .await?;
                    self.chunk.size = compressed_size; // update chunk size after compression
                    reqwest::Body::wrap_stream(futures_util::stream::iter(parts))
                }
            };
            let connection_permit = self.connection_pool.acquire().await?;
            let client = reqwest::Client::new();
            if let Some(resp) = run
                .select(
                    client
                        .put(
                            self.chunk
                                .url
                                .as_ref()
                                .ok_or_else(|| anyhow!("missing chunk {} url", self.chunk.key))?
                                .clone(),
                        )
                        .header("Content-Length", format!("{}", self.chunk.size))
                        .timeout(UPLOAD_SINGLE_CHUNK_TIMEOUT)
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
            drop(connection_permit);
        }
        self.chunk.checksum = checksum_rx.borrow().clone();
        self.chunk.url = None;
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
        if !part.is_empty() {
            total_size += part.len();
            parts.push(Ok(part));
        }
    }
    Ok((parts, u64::try_from(total_size)?))
}

struct FileDescriptor {
    file: File,
    offset: u64,
    bytes_remaining: u64,
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
    async fn new(
        mut client: BabelEngineClient,
        mut iter: Vec<FileLocation>,
        config: TransferConfig,
        encoder: E,
        checksum_tx: tokio::sync::watch::Sender<Checksum>,
    ) -> Result<Self> {
        iter.reverse();
        let last = match iter.pop() {
            None => {
                let err_msg = "corrupted manifest - this is internal BV error, manifest shall be already validated";
                error!(err_msg);
                let _ = with_retry!(client.bv_error(err_msg.to_string()));
                Err(anyhow!(
                    "corrupted manifest - expected at least one destination file in chunk"
                ))
            }
            Some(first) => Ok(first),
        }?;
        let current = FileDescriptor {
            file: File::open(&last.path)
                .with_context(|| format!("can't open '{}'", last.path.display()))?,
            offset: last.pos,
            bytes_remaining: last.size,
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
                    // it is end of chunk, so we can take interim_buffer (won't be used anymore) and finalize its members
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
                    file: File::open(&next.path)
                        .with_context(|| format!("can't open '{}'", next.path.display()))?,
                    offset: next.pos,
                    bytes_remaining: next.size,
                };
            }
            let bytes_to_read = min(
                self.current.bytes_remaining,
                u64::try_from(buffer.capacity() - buffer.len())?,
            );
            let end = usize::try_from(bytes_to_read)?;
            let start = buffer.len();
            buffer.resize(start + end, 0);
            self.current
                .file
                .read_exact_at(&mut buffer[start..(start + end)], self.current.offset)?;
            self.current.bytes_remaining -= bytes_to_read;
            self.current.offset += bytes_to_read;
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
    use crate::job_runner::ArchiveJobRunner;
    use crate::utils;
    use crate::utils::tests::MockBabelEngine;
    use assert_fs::TempDir;
    use babel_api::engine::{JobStatus, RestartConfig, RestartPolicy, Slot};
    use bv_tests_utils::rpc::TestServer;
    use bv_tests_utils::start_test_server;
    use bv_utils::timer::SysTimer;
    use mockito::{Matcher, Server, ServerGuard};
    use std::fs;
    use tonic::Response;
    use url::Url;

    struct TestEnv {
        tmp_dir: PathBuf,
        blueprint_file_path: PathBuf,
        upload_progress_path: PathBuf,
        server: ServerGuard,
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_dir = TempDir::new()?.to_path_buf();
        let blueprint_file_path = tmp_dir.join(BLUEPRINT_FILENAME);
        dummy_sources(&tmp_dir)?;
        let server = Server::new();
        let upload_progress_path = tmp_dir.join("upload.progress");
        Ok(TestEnv {
            tmp_dir,
            blueprint_file_path,
            server,
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
        fn upload_job(
            &self,
            total_slots: usize,
        ) -> ArchiveJobRunner<SysTimer, Uploader<utils::tests::DummyConnector>> {
            ArchiveJobRunner {
                runner: Uploader {
                    connector: utils::tests::DummyConnector {
                        tmp_dir: self.tmp_dir.clone(),
                    },
                    source_dir: self.tmp_dir.clone(),
                    exclude: vec![Pattern::new("**/ignored_*").unwrap()],
                    config: TransferConfig {
                        max_opened_files: 1,
                        max_runners: 4,
                        max_connections: 2,
                        max_buffer_size: 50,
                        max_retries: 0,
                        backoff_base_ms: 1,
                        archive_jobs_meta_dir: self.tmp_dir.clone(),
                        progress_file_path: self.upload_progress_path.clone(),
                        compression: None,
                    },
                    total_slots: total_slots as u32,
                    url_expires_secs: 60,
                    data_version: None,
                },
                restart_policy: RestartPolicy::Never,
                timer: SysTimer,
            }
        }

        async fn start_server(&self, mock: MockBabelEngine) -> TestServer {
            start_test_server!(
                &self.tmp_dir,
                babel_api::babel::babel_engine_server::BabelEngineServer::new(mock)
            )
        }

        fn url(&self, path: &str) -> Url {
            Url::parse(&format!("{}/{}", self.server.url(), path)).unwrap()
        }

        fn dummy_upload_slots(&self) -> UploadSlots {
            UploadSlots {
                slots: vec![
                    Slot {
                        index: 0,
                        key: "KeyA".to_string(),
                        url: self.url("url.a"),
                    },
                    Slot {
                        index: 1,
                        key: "KeyB".to_string(),
                        url: self.url("url.b"),
                    },
                    Slot {
                        index: 2,
                        key: "KeyC".to_string(),
                        url: self.url("url.c"),
                    },
                ],
                data_version: 0,
            }
        }

        fn expected_download_manifest(&self) -> Result<DownloadManifest> {
            Ok(DownloadManifest {
                total_size: 275,
                compression: None,
                chunks: vec![
                    Chunk {
                        index: 0,
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
                        index: 1,
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
                        index: 2,
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
            })
        }
    }

    fn normalize_manifest(manifest: &mut DownloadManifest) {
        manifest.chunks.sort_by(|a, b| a.index.cmp(&b.index));
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
        let slots_count = expected_manifest.chunks.len();
        let mut mock = MockBabelEngine::new();
        let slots = test_env.dummy_upload_slots();
        mock.expect_get_upload_slots()
            .once()
            .returning(move |_| Ok(Response::new(slots.clone())));
        mock.expect_put_download_manifest()
            .once()
            .withf(move |req| *req.get_ref() == expected_manifest)
            .returning(|_| Ok(Response::new(())));
        let server = test_env.start_server(mock).await;
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: Default::default(),
            },
            test_env
                .upload_job(slots_count)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );

        assert!(!test_env.blueprint_file_path.exists());
        assert!(test_env.upload_progress_path.exists());
        let progress = fs::read_to_string(&test_env.upload_progress_path).unwrap();
        assert_eq!(&progress, r#"{"total":3,"current":3,"message":"chunks"}"#);
        server.assert().await;

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
        let expected_manifest = DownloadManifest {
            total_size: 275,
            compression: Some(Compression::ZSTD(5)),
            chunks: vec![
                Chunk {
                    index: 0,
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
                    index: 1,
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
                    index: 2,
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
        };
        let slots_count = expected_manifest.chunks.len();
        let mut mock = MockBabelEngine::new();
        let slots = test_env.dummy_upload_slots();
        mock.expect_get_upload_slots()
            .times(2)
            .returning(move |_| Ok(Response::new(slots.clone())));
        mock.expect_put_download_manifest()
            .once()
            .withf(move |req| *req.get_ref() == expected_manifest)
            .returning(|_| Ok(Response::new(())));
        let server = test_env.start_server(mock).await;

        let mut job = test_env.upload_job(slots_count);
        job.runner.config.compression = Some(Compression::ZSTD(5));
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
        server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_empty_upload_ok() -> Result<()> {
        let test_env = setup_test_env()?;
        fs::remove_dir_all(&test_env.tmp_dir)?;
        fs::create_dir_all(&test_env.tmp_dir)?;
        File::create(test_env.tmp_dir.join("empty_file_1"))?;
        File::create(test_env.tmp_dir.join("empty_file_2"))?;

        let expected_manifest = DownloadManifest {
            total_size: 0,
            compression: None,
            chunks: vec![Chunk {
                index: 0,
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
        };
        let slots_count = expected_manifest.chunks.len();
        let mut mock = MockBabelEngine::new();
        let slots = test_env.dummy_upload_slots();
        mock.expect_get_upload_slots()
            .once()
            .returning(move |_| Ok(Response::new(slots.clone())));
        mock.expect_put_download_manifest()
            .once()
            .withf(move |req| *req.get_ref() == expected_manifest)
            .returning(|_| Ok(Response::new(())));
        let server = test_env.start_server(mock).await;

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: Default::default(),
            },
            test_env
                .upload_job(slots_count)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );
        server.assert().await;
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
        let slots_count = expected_manifest.chunks.len();
        let mut mock = MockBabelEngine::new();
        let mut slots = test_env.dummy_upload_slots();
        slots.slots.retain(|slot| slot.index == 2);
        mock.expect_get_upload_slots()
            .once()
            .returning(move |_| Ok(Response::new(slots.clone())));
        mock.expect_put_download_manifest()
            .once()
            .withf(move |req| *req.get_ref() == expected_manifest)
            .returning(|_| Ok(Response::new(())));
        let server = test_env.start_server(mock).await;

        let job = test_env.upload_job(slots_count);
        // mark first two as uploaded
        let mut blueprint = job.runner.prepare_manifest_blueprint()?;
        save_job_data(
            &job.runner
                .config
                .archive_jobs_meta_dir
                .join(BLUEPRINT_FILENAME),
            &Blueprint {
                manifest: blueprint.clone(),
                data_version: 0,
            },
        )?;
        let chunks_path = job
            .runner
            .config
            .archive_jobs_meta_dir
            .join(CHUNKS_FILENAME);
        let chunk_a = blueprint
            .chunks
            .iter_mut()
            .find(|item| item.index == 0)
            .unwrap();
        chunk_a.key = "KeyA".to_string();
        chunk_a.size = 91;
        chunk_a.checksum = Checksum::Blake3([
            119, 175, 9, 4, 145, 218, 117, 139, 245, 72, 66, 12, 252, 244, 95, 29, 198, 151, 102,
            4, 20, 229, 205, 55, 90, 194, 137, 167, 103, 54, 187, 43,
        ]);
        save_chunk(&chunks_path, chunk_a).unwrap();
        let chunk_b = blueprint
            .chunks
            .iter_mut()
            .find(|item| item.index == 1)
            .unwrap();
        chunk_b.key = "KeyB".to_string();
        chunk_b.size = 91;
        chunk_b.checksum = Checksum::Blake3([
            119, 175, 9, 4, 145, 218, 117, 139, 245, 72, 66, 12, 252, 244, 95, 29, 198, 151, 102,
            4, 20, 229, 205, 55, 90, 194, 137, 167, 103, 54, 187, 43,
        ]);
        save_chunk(&chunks_path, chunk_b).unwrap();

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: Default::default(),
            },
            job.run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );
        assert!(!test_env.blueprint_file_path.exists());
        server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_server_errors() -> Result<()> {
        let test_env = setup_test_env()?;
        let mut mock = MockBabelEngine::new();
        let slots = test_env.dummy_upload_slots();
        let slots_count = slots.slots.len();
        mock.expect_get_upload_slots()
            .once()
            .returning(move |_| Ok(Response::new(slots.clone())));
        let server = test_env.start_server(mock).await;

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "job 'name' failed with: chunk 'KeyC' upload failed: server responded with 501 Not Implemented"
                    .to_string()
            },
            test_env
                .upload_job(slots_count)
                .run(RunFlag::default(), "name", &test_env.tmp_dir)
                .await
        );
        server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_invalid_source_dir() -> Result<()> {
        let test_env = setup_test_env()?;
        let mut job = test_env.upload_job(1);
        job.runner.source_dir = PathBuf::from("some/invalid/source");
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(-1),
                message: "job 'name' failed with: IO error for operation on some/invalid/source: No such file or directory (os error 2): No such file or directory (os error 2)"
                    .to_string()
            },
            job.run(RunFlag::default(), "name", &test_env.tmp_dir).await
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_no_cleanup_after_fail() -> Result<()> {
        let test_env = setup_test_env()?;
        fs::write(&test_env.blueprint_file_path, "anything")?;
        if let JobStatus::Finished {
            exit_code: Some(-1),
            ..
        } = test_env
            .upload_job(1)
            .run(RunFlag::default(), "name", &test_env.tmp_dir)
            .await
        {
            assert!(test_env.blueprint_file_path.exists());
        } else {
            bail!("unexpected success")
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_prepare_blueprint() -> Result<()> {
        let test_env = setup_test_env()?;
        let job = test_env.upload_job(2);
        let mut blueprint = job.runner.prepare_manifest_blueprint()?;
        normalize_manifest(&mut blueprint);
        assert_eq!(
            DownloadManifest {
                total_size: 275,
                compression: None,
                chunks: vec![
                    Chunk {
                        index: 0,
                        key: Default::default(),
                        url: None,
                        checksum: Checksum::Sha1([0u8; 20]),
                        size: 0,
                        destinations: vec![FileLocation {
                            path: test_env.tmp_dir.join("x"),
                            pos: 0,
                            size: 137,
                        }],
                    },
                    Chunk {
                        index: 1,
                        key: Default::default(),
                        url: None,
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

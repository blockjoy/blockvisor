use bv_utils::cmd::run_cmd;
use eyre::{bail, Result};
use rand::Rng;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};
use sysinfo::{Pid, ProcessExt, ProcessRefreshKind, RefreshKind, System, SystemExt};
use thiserror::Error;
use tokio::{
    fs::{self},
    io::AsyncWriteExt,
};
use tracing::{debug, warn};

use crate::nib::ImageVariant;

// image download should never take more than 15min
const ARCHIVE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Error, Debug)]
pub enum GetProcessIdError {
    #[error("process not found")]
    NotFound,
    #[error("found more than 1 matching process")]
    MoreThanOne,
}

/// Get the pid of the running VM process knowing its process name and part of command line.
pub fn get_process_pid(process_name: &str, cmd: &str) -> Result<Pid, GetProcessIdError> {
    let mut sys = System::new();
    debug!("Retrieving pid for process `{process_name}` and cmd like `{cmd}`");
    sys.refresh_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    let processes: Vec<_> = sys
        .processes_by_name(process_name)
        .filter(|&process| {
            process.cmd().contains(&cmd.to_string())
                && process.status() != sysinfo::ProcessStatus::Zombie
        })
        .collect();

    match processes.len() {
        0 => Err(GetProcessIdError::NotFound),
        1 => Ok(processes[0].pid()),
        _ => Err(GetProcessIdError::MoreThanOne),
    }
}

pub struct Archive(PathBuf);
impl Archive {
    pub async fn ungzip(self) -> Result<Self> {
        // pigz is parallel and fast
        // TODO: pigz is external dependency, we need a reliable way of delivering it to hosts
        run_cmd(
            "pigz",
            [
                OsStr::new("--decompress"),
                OsStr::new("--force"),
                self.0.as_os_str(),
            ],
        )
        .await?;
        if let (Some(parent), Some(name)) = (self.0.parent(), self.0.file_stem()) {
            Ok(Self(parent.join(name)))
        } else {
            bail!("invalid gzip file path {}", self.0.to_string_lossy())
        }
    }

    pub async fn untar(self) -> Result<Self> {
        let Some(parent_dir) = self.0.parent() else {
            bail!("invalid tar file path {}", self.0.to_string_lossy())
        };
        run_cmd(
            "tar",
            [
                OsStr::new("-C"),
                parent_dir.as_os_str(),
                OsStr::new("-xf"),
                self.0.as_os_str(),
            ],
        )
        .await?;
        let _ = fs::remove_file(&self.0).await;

        Ok(Self(parent_dir.into()))
    }
}

pub async fn download_archive(url: &str, path: PathBuf) -> Result<Archive> {
    download_file(url, &path).await?;
    Ok(Archive(path))
}

async fn download_file(url: &str, path: &PathBuf) -> Result<()> {
    debug!("Downloading url...");
    let _ = fs::remove_file(path).await;
    let mut file = fs::File::create(path).await?;

    let client = reqwest::Client::builder()
        .timeout(ARCHIVE_DOWNLOAD_TIMEOUT)
        .build()?;
    let mut resp = client.get(url).send().await?;

    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
    }

    file.flush().await?;
    debug!("Done downloading");

    Ok(())
}

/// Take base interval and add random amount of seconds to it
///
/// Do not add more than original seconds / 2
pub fn with_jitter(base: Duration) -> Duration {
    let mut rng = rand::rng();
    let jitter_max = base.as_secs() / 2;
    let jitter = Duration::from_secs(rng.random_range(0..jitter_max));
    base + jitter
}

pub fn render_template(template: &str, destination: &Path, params: &[(&str, &str)]) -> Result<()> {
    let mut context = tera::Context::new();
    for (key, value) in params {
        context.insert(key.to_string(), value);
    }
    let mut tera = tera::Tera::default();
    tera.add_raw_template("template", template)?;
    let destination_file = std::fs::File::create(destination)?;
    tera.render_to("template", &context, destination_file)?;
    Ok(())
}

pub fn verify_variant_sku(image_variant: &ImageVariant) -> eyre::Result<()> {
    Ok(eyre::ensure!(
        image_variant
            .sku_code
            .chars()
            .all(|character| character == '-'
                || character.is_ascii_digit()
                || character.is_ascii_uppercase())
            && image_variant.sku_code.split("-").count() == 3,
        "Invalid SKU format for variant '{}': '{}' (Should be formatted as 3 sections of uppercased ascii alphanumeric characters split by `-`, e.g.: `ETH-ERG-SF`)",
        image_variant.variant_key,
        image_variant.sku_code
    ))
}

pub async fn careful_save(path: &Path, content: &[u8]) -> Result<()> {
    let backup_path = path.with_extension(format!(
        "{}_bak",
        path.extension().unwrap_or_default().to_string_lossy()
    ));
    if path.exists() {
        if backup_path.exists() {
            fs::remove_file(&backup_path).await?;
        }
        fs::rename(path, &backup_path).await?;
    }
    let res = fs::write(&path, content).await;
    if backup_path.exists() {
        if res.is_err() {
            let _ = fs::remove_file(&path).await;
            if let Err(err) = fs::rename(&backup_path, path).await {
                warn!("careful_save can't roll back after failure: {err:#}");
            }
        } else if let Err(err) = fs::remove_file(&backup_path).await {
            warn!("failed to cleanup backup after careful_save: {err:#}");
        }
    }
    Ok(res?)
}

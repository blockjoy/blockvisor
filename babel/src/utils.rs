use eyre::Context;
use std::fs;
use std::path::Path;
use std::process::Output;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};

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

pub async fn mount_drive(drive: &str, dir: &str) -> eyre::Result<Output> {
    fs::create_dir_all(&dir)?;
    Ok(tokio::process::Command::new("mount")
        .args([drive, dir])
        .output()
        .await
        .with_context(|| "failed to mount drive".to_string())?)
}

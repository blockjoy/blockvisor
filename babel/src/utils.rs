use eyre::{bail, Context, ContextCompat};
use futures::StreamExt;
use std::fs;
use std::path::Path;
use std::process::Output;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio_stream::Stream;
use tonic::Status;

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

/// Write binary stream into the file.
pub async fn save_bin_stream<S: Stream<Item = Result<babel_api::Binary, Status>> + Unpin>(
    bin_path: &Path,
    stream: &mut S,
) -> eyre::Result<u32> {
    let _ = tokio::fs::remove_file(bin_path).await;
    let file = OpenOptions::new()
        .write(true)
        .mode(0o770)
        .append(false)
        .create(true)
        .open(bin_path)
        .await
        .with_context(|| "failed to open binary file")?;
    let mut writer = BufWriter::new(file);
    let mut expected_checksum = None;
    while let Some(part) = stream.next().await {
        match part? {
            babel_api::Binary::Bin(bin) => {
                writer
                    .write(&bin)
                    .await
                    .with_context(|| "failed to save binary")?;
            }
            babel_api::Binary::Checksum(checksum) => {
                expected_checksum = Some(checksum);
            }
        }
    }
    writer
        .flush()
        .await
        .with_context(|| "failed to save binary")?;
    let expected_checksum =
        expected_checksum.with_context(|| "incomplete binary stream - missing checksum")?;

    let checksum = file_checksum(bin_path)
        .await
        .with_context(|| "failed to calculate binary checksum")?;

    if expected_checksum != checksum {
        bail!(
            "received binary checksum ({checksum})\
                 doesn't match expected ({expected_checksum})"
        );
    }
    Ok(checksum)
}

pub async fn mount_drive(drive: &str, dir: &str) -> eyre::Result<Output> {
    fs::create_dir_all(dir)?;
    tokio::process::Command::new("mount")
        .args([drive, dir])
        .output()
        .await
        .with_context(|| "failed to mount drive")
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::TempDir;
    use eyre::Result;
    use std::fs;

    #[tokio::test]
    async fn test_save_bin_stream() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        fs::create_dir_all(&tmp_dir)?;
        let file_path = tmp_dir.join("test_file");

        let incomplete_bin = vec![
            Ok(babel_api::Binary::Bin(vec![1, 2, 3, 4, 6, 7, 8, 9, 10])),
            Ok(babel_api::Binary::Bin(vec![
                11, 12, 13, 14, 16, 17, 18, 19, 20,
            ])),
            Ok(babel_api::Binary::Bin(vec![
                21, 22, 23, 24, 26, 27, 28, 29, 30,
            ])),
        ];

        let _ = save_bin_stream(&file_path, &mut tokio_stream::iter(incomplete_bin.clone()))
            .await
            .unwrap_err();
        let mut invalid_bin = incomplete_bin.clone();
        invalid_bin.push(Ok(babel_api::Binary::Checksum(123)));
        let _ = save_bin_stream(&file_path, &mut tokio_stream::iter(invalid_bin.clone()))
            .await
            .unwrap_err();
        let mut correct_bin = incomplete_bin.clone();
        correct_bin.push(Ok(babel_api::Binary::Checksum(4135829304)));
        assert_eq!(
            4135829304,
            save_bin_stream(&file_path, &mut tokio_stream::iter(correct_bin.clone())).await?
        );
        assert_eq!(4135829304, file_checksum(&file_path).await.unwrap());
        Ok(())
    }

    #[tokio::test]
    async fn test_file_checksum() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        fs::create_dir_all(&tmp_dir)?;
        let file_path = tmp_dir.join("test_file");
        let _ = file_checksum(&file_path).await.unwrap_err();
        fs::write(&file_path, "dummy content")?;
        assert_eq!(2134916024, file_checksum(&file_path).await?);
        Ok(())
    }
}

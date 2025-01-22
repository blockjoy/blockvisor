use eyre::bail;
use std::path::{Path, PathBuf};

pub struct LockFile {
    lock_path: PathBuf,
}

impl LockFile {
    pub fn lock(path: &Path, name: &str) -> eyre::Result<Self> {
        let lock_path = path.join(name);
        if lock_path.exists() {
            bail!("{} already locked", lock_path.display())
        }
        std::fs::create_dir_all(path)?;
        std::fs::File::create(&lock_path)?;
        Ok(Self { lock_path })
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

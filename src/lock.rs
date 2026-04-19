use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct RunLock {
    file: File,
    path: PathBuf,
}

impl RunLock {
    pub fn acquire(path: &Path) -> Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create lock dir {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open lock file {}", path.display()))?;

        match file.try_lock_exclusive() {
            Ok(()) => {
                file.set_len(0).ok();
                writeln!(file, "{}", std::process::id()).ok();
                Ok(Some(Self {
                    file,
                    path: path.to_path_buf(),
                }))
            }
            Err(e) => {
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock) {
                    Ok(None)
                } else {
                    Err(e).with_context(|| format!("lock {}", path.display()))
                }
            }
        }
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
        let _ = std::fs::remove_file(&self.path);
    }
}

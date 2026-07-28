use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use fs2::FileExt;

pub(crate) struct InstallLock {
    file: File,
}

impl InstallLock {
    pub(crate) fn acquire(metadata_root: &Path) -> io::Result<Self> {
        let path = metadata_root.join("install.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.try_lock_exclusive()?;
        Ok(Self { file })
    }
}

pub(crate) fn is_contended(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock || cfg!(windows) && error.raw_os_error() == Some(33)
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        if let Err(error) = FileExt::unlock(&self.file) {
            eprintln!("failed to release installation lock: {error}");
        }
    }
}

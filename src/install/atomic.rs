use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use atomic_write_file::AtomicWriteFile;

pub(crate) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = parent(path)?;
    fs::create_dir_all(parent)?;
    let mut temporary = AtomicWriteFile::open(path)?;
    temporary.write_all(bytes)?;
    temporary.commit()?;
    sync_directory(parent)
}

fn parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path '{}' has no usable parent", path.display()),
            )
        })
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::metadata(path)?;
    Ok(())
}

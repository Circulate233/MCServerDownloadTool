use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::Dir;

use super::InstallError;

/// Canonical installation-root boundary used for every installer filesystem access.
#[derive(Debug, Clone)]
pub struct InstallRoot {
    canonical: PathBuf,
    directory: Arc<Dir>,
}

/// Open, root-anchored locations used while publishing one downloaded artifact.
///
/// Both directory handles remain open from staging-file creation through the
/// final rename. On Unix this makes operations relative to directory file
/// descriptors; on Windows the handles deny replacement of the directories.
#[derive(Debug, Clone)]
pub(crate) struct DownloadTarget {
    staging: Arc<Dir>,
    destination: Arc<Dir>,
    staging_name: OsString,
    destination_name: OsString,
    staging_path: PathBuf,
    final_path: PathBuf,
}

impl InstallRoot {
    /// Resolves the manifest parent into the canonical installation boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest has no parent, its parent cannot be
    /// canonicalized, or the manifest is not a regular unlinked file inside it.
    pub fn from_manifest(manifest_path: &Path) -> Result<Self, InstallError> {
        let parent = manifest_path
            .parent()
            .ok_or_else(|| InstallError::ManifestHasNoParent {
                path: manifest_path.to_path_buf(),
            })?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let canonical = fs::canonicalize(parent)
            .map_err(|source| InstallError::io("canonicalize installation root", parent, source))?;
        let directory =
            Dir::open_ambient_dir(&canonical, ambient_authority()).map_err(|source| {
                InstallError::io("open canonical installation root", &canonical, source)
            })?;
        let root = Self {
            canonical,
            directory: Arc::new(directory),
        };
        root.verify_existing_file(manifest_path)?;
        Ok(root)
    }

    /// Returns the canonical server root held by this boundary.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.canonical
    }

    /// Resolves a normalized relative path and rejects any existing linked component.
    ///
    /// # Errors
    ///
    /// Returns an error when `relative` is not normalized, leaves the canonical
    /// root, or traverses an existing symlink, junction, or reparse point.
    pub fn resolve(&self, relative: &Path) -> Result<PathBuf, InstallError> {
        validate_relative(relative)?;
        let path = self.canonical.join(relative);
        self.verify_absolute(&path, true)?;
        Ok(path)
    }

    /// Validates an existing regular file without following a linked leaf.
    ///
    /// # Errors
    ///
    /// Returns an error when `path` is outside this root, cannot be inspected,
    /// traverses link indirection, or is not an existing regular file.
    pub fn verify_existing_file(&self, path: &Path) -> Result<(), InstallError> {
        let absolute = if path.is_absolute() {
            let parent = path.parent().ok_or_else(|| InstallError::UnsafePath {
                path: path.to_path_buf(),
                reason: "file path has no parent".to_string(),
            })?;
            let canonical_parent = fs::canonicalize(parent)
                .map_err(|source| InstallError::io("canonicalize file parent", parent, source))?;
            canonical_parent.join(path.file_name().ok_or_else(|| InstallError::UnsafePath {
                path: path.to_path_buf(),
                reason: "file path has no name".to_string(),
            })?)
        } else {
            fs::canonicalize(path)
                .map_err(|source| InstallError::io("canonicalize file", path, source))?
        };
        self.verify_absolute(&absolute, false)?;
        let metadata = fs::symlink_metadata(path)
            .map_err(|source| InstallError::io("inspect file", path, source))?;
        if is_link_or_reparse(&metadata) || !metadata.file_type().is_file() {
            return Err(InstallError::UnsafePath {
                path: path.to_path_buf(),
                reason: "expected an existing regular file without symlink or reparse indirection"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// Revalidates a target whose leaf may not exist yet.
    ///
    /// # Errors
    ///
    /// Returns an error when `path` is outside this root, is not normalized, or
    /// any existing component is a symlink, junction, or reparse point.
    pub fn verify_target(&self, path: &Path) -> Result<(), InstallError> {
        self.verify_absolute(path, true)
    }

    /// Creates a relative directory one component at a time and verifies each component.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is invalid, a component cannot be created
    /// or inspected, or an existing component is linked or not a directory.
    pub fn create_directory(&self, relative: &Path) -> Result<PathBuf, InstallError> {
        validate_relative(relative)?;
        let mut current = self.canonical.clone();
        for component in relative.components() {
            let Component::Normal(component) = component else {
                return Err(InstallError::UnsafePath {
                    path: relative.to_path_buf(),
                    reason: "directory path is not normalized".to_string(),
                });
            };
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    reject_link(&current, &metadata)?;
                    if !metadata.is_dir() {
                        return Err(InstallError::UnsafePath {
                            path: current,
                            reason: "directory component is not a directory".to_string(),
                        });
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&current).map_err(|source| {
                        InstallError::io("create installation directory", &current, source)
                    })?;
                    let metadata = fs::symlink_metadata(&current).map_err(|source| {
                        InstallError::io("reinspect created directory", &current, source)
                    })?;
                    reject_link(&current, &metadata)?;
                    if !metadata.is_dir() {
                        return Err(InstallError::UnsafePath {
                            path: current,
                            reason: "created path is not a directory".to_string(),
                        });
                    }
                }
                Err(source) => {
                    return Err(InstallError::io(
                        "inspect installation directory",
                        &current,
                        source,
                    ));
                }
            }
        }
        self.verify_absolute(&current, false)?;
        Ok(current)
    }

    fn verify_absolute(&self, path: &Path, allow_missing: bool) -> Result<(), InstallError> {
        let relative =
            path.strip_prefix(&self.canonical)
                .map_err(|_| InstallError::UnsafePath {
                    path: path.to_path_buf(),
                    reason: format!(
                        "path is outside canonical installation root '{}'",
                        self.canonical.display()
                    ),
                })?;
        if relative.as_os_str().is_empty() {
            return Ok(());
        }
        validate_relative(relative)?;

        let mut current = self.canonical.clone();
        let components = relative.components().collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            let Component::Normal(component) = component else {
                return Err(InstallError::UnsafePath {
                    path: path.to_path_buf(),
                    reason: "path is not normalized".to_string(),
                });
            };
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    reject_link(&current, &metadata)?;
                    let canonical = fs::canonicalize(&current).map_err(|source| {
                        InstallError::io("canonicalize installation path", &current, source)
                    })?;
                    if !canonical.starts_with(&self.canonical) {
                        return Err(InstallError::UnsafePath {
                            path: current,
                            reason: "existing path resolves outside the installation root"
                                .to_string(),
                        });
                    }
                    if index + 1 < components.len() && !metadata.is_dir() {
                        return Err(InstallError::UnsafePath {
                            path: current,
                            reason: "non-directory component appears before the path leaf"
                                .to_string(),
                        });
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound && allow_missing => break,
                Err(source) => {
                    return Err(InstallError::io(
                        "inspect installation path",
                        &current,
                        source,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Allocates one downloader staging file and pins both its staging and
    /// destination directories to this installation root.
    ///
    /// The returned host path is only a transport endpoint. Publication uses
    /// the retained directory handles and therefore cannot be redirected by a
    /// later symlink, junction, or parent-directory replacement.
    pub(crate) fn download_target(
        &self,
        target: &Path,
        sequence: u64,
    ) -> Result<DownloadTarget, InstallError> {
        self.verify_target(target)?;
        let relative =
            target
                .strip_prefix(&self.canonical)
                .map_err(|_| InstallError::UnsafePath {
                    path: target.to_path_buf(),
                    reason: "artifact target is outside the installation root".to_string(),
                })?;
        validate_relative(relative)?;
        let destination_name = relative
            .file_name()
            .ok_or_else(|| InstallError::UnsafePath {
                path: target.to_path_buf(),
                reason: "artifact target has no file name".to_string(),
            })?
            .to_os_string();
        let destination_parent = relative
            .parent()
            .filter(|value| !value.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let destination = Arc::new(self.open_directory(destination_parent)?);

        let staging_relative = Path::new(".mcsdt").join("downloads");
        let staging = Arc::new(self.open_directory(&staging_relative)?);
        let staging_name = OsString::from(format!("artifact-{sequence}.part"));
        let staging_path = transport_path(
            &staging,
            &self.canonical.join(&staging_relative),
            &staging_name,
        );
        Ok(DownloadTarget {
            staging,
            destination,
            staging_name,
            destination_name,
            staging_path,
            final_path: target.to_path_buf(),
        })
    }

    fn open_directory(&self, relative: &Path) -> Result<Dir, InstallError> {
        if relative == Path::new(".") {
            return self.directory.try_clone().map_err(|source| {
                InstallError::io(
                    "clone installation root directory handle",
                    &self.canonical,
                    source,
                )
            });
        }
        validate_relative(relative)?;
        let mut current = self.directory.try_clone().map_err(|source| {
            InstallError::io(
                "clone installation root directory handle",
                &self.canonical,
                source,
            )
        })?;
        let mut display = self.canonical.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(InstallError::UnsafePath {
                    path: relative.to_path_buf(),
                    reason: "directory path is not normalized".to_string(),
                });
            };
            display.push(name);
            match current.symlink_metadata(name) {
                Ok(metadata) => {
                    reject_cap_link(&display, &metadata)?;
                    if !metadata.is_dir() {
                        return Err(InstallError::UnsafePath {
                            path: display,
                            reason: "directory component is not a directory".to_string(),
                        });
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    current.create_dir(name).map_err(|source| {
                        InstallError::io("create anchored installation directory", &display, source)
                    })?;
                    let metadata = current.symlink_metadata(name).map_err(|source| {
                        InstallError::io(
                            "reinspect anchored installation directory",
                            &display,
                            source,
                        )
                    })?;
                    reject_cap_link(&display, &metadata)?;
                    if !metadata.is_dir() {
                        return Err(InstallError::UnsafePath {
                            path: display,
                            reason: "created path is not a directory".to_string(),
                        });
                    }
                }
                Err(source) => {
                    return Err(InstallError::io(
                        "inspect anchored installation directory",
                        &display,
                        source,
                    ));
                }
            }
            current = current.open_dir(name).map_err(|source| {
                InstallError::io("open anchored installation directory", &display, source)
            })?;
        }
        Ok(current)
    }
}

impl DownloadTarget {
    /// Returns the transport path rooted in an already-open staging directory.
    #[must_use]
    pub(crate) fn staging_path(&self) -> &Path {
        &self.staging_path
    }

    /// Returns the user-visible final artifact location.
    #[must_use]
    pub(crate) fn final_path(&self) -> &Path {
        &self.final_path
    }

    /// Opens the completed staging file without reopening an ancestor by path.
    pub(crate) fn open_staging(&self) -> io::Result<cap_std::fs::File> {
        self.staging.open(&self.staging_name)
    }

    /// Atomically publishes the verified staging file through pinned directory handles.
    pub(crate) fn publish(&self) -> io::Result<()> {
        self.staging.rename(
            &self.staging_name,
            &self.destination,
            &self.destination_name,
        )?;
        #[cfg(unix)]
        sync_directory_handle(&self.destination)?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn transport_path(directory: &Dir, _fallback: &Path, name: &OsStr) -> PathBuf {
    use std::os::fd::AsRawFd;

    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name)
}

#[cfg(target_os = "macos")]
fn transport_path(directory: &Dir, _fallback: &Path, name: &OsStr) -> PathBuf {
    use std::os::fd::AsRawFd;

    PathBuf::from(format!("/dev/fd/{}", directory.as_raw_fd())).join(name)
}

#[cfg(windows)]
fn transport_path(_directory: &Dir, fallback: &Path, name: &OsStr) -> PathBuf {
    fallback.join(name)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn transport_path(_directory: &Dir, fallback: &Path, name: &OsStr) -> PathBuf {
    fallback.join(name)
}

#[cfg(unix)]
fn sync_directory_handle(directory: &Dir) -> io::Result<()> {
    directory.try_clone()?.into_std_file().sync_all()
}

fn reject_cap_link(path: &Path, metadata: &cap_std::fs::Metadata) -> Result<(), InstallError> {
    if metadata.file_type().is_symlink() {
        return Err(InstallError::UnsafePath {
            path: path.to_path_buf(),
            reason: "symlink, junction, or reparse-point traversal is forbidden".to_string(),
        });
    }
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(InstallError::UnsafePath {
                path: path.to_path_buf(),
                reason: "symlink, junction, or reparse-point traversal is forbidden".to_string(),
            });
        }
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<(), InstallError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            !matches!(component, Component::Normal(_))
                || component.as_os_str().to_string_lossy().contains('\0')
        })
    {
        return Err(InstallError::UnsafePath {
            path: path.to_path_buf(),
            reason: "path must be a normalized non-empty relative path".to_string(),
        });
    }
    Ok(())
}

fn reject_link(path: &Path, metadata: &fs::Metadata) -> Result<(), InstallError> {
    if is_link_or_reparse(metadata) {
        return Err(InstallError::UnsafePath {
            path: path.to_path_buf(),
            reason: "symlink, junction, or reparse-point traversal is forbidden".to_string(),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::InstallRoot;

    fn root(temp: &tempfile::TempDir) -> InstallRoot {
        let manifest = temp.path().join("server-install.json");
        fs::write(&manifest, b"{}").unwrap();
        InstallRoot::from_manifest(&manifest).unwrap()
    }

    #[test]
    fn anchored_download_refuses_staging_symlink_escape() {
        let installation = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = root(&installation);
        create_directory_link(outside.path(), &installation.path().join(".mcsdt"));

        let result = root.download_target(&root.path().join("mods/example.jar"), 1);

        assert!(result.is_err());
        assert!(!outside.path().join("downloads").exists());
    }

    #[test]
    fn anchored_download_publishes_through_pinned_directories() {
        let installation = tempfile::tempdir().unwrap();
        let root = root(&installation);
        let target = root.path().join("mods/example.jar");
        let download = root.download_target(&target, 2).unwrap();

        fs::write(download.staging_path(), b"verified bytes").unwrap();
        download.publish().unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"verified bytes");
        assert!(!download.staging_path().exists());
    }

    #[cfg(unix)]
    fn create_directory_link(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn create_directory_link(target: &Path, link: &Path) {
        let status = std::process::Command::new("cmd.exe")
            .args([
                "/d",
                "/q",
                "/c",
                "mklink",
                "/J",
                &link.display().to_string(),
                &target.display().to_string(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "failed to create test junction");
    }
}

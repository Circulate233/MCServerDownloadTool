use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use super::InstallError;

/// Canonical installation-root boundary used for every installer filesystem access.
#[derive(Debug, Clone)]
pub struct InstallRoot {
    canonical: PathBuf,
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
        let root = Self { canonical };
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

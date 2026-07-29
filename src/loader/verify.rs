use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use zip::ZipArchive;

use super::model::{LoaderError, LoaderOutputExpectation, VerifiedLaunch};

const MAX_JAR_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Verifies exact loader outputs without scanning for approximate jar names.
///
/// # Errors
///
/// Returns [`LoaderError`] when an expected file is absent, empty, unsafe, or a
/// Fabric jar has a different manifest `Main-Class`.
pub fn verify_loader_output(
    server_root: &Path,
    expected: &LoaderOutputExpectation,
) -> Result<VerifiedLaunch, LoaderError> {
    match expected {
        LoaderOutputExpectation::ModernArgs { windows, unix } => {
            verify_nonempty(server_root, windows)?;
            verify_nonempty(server_root, unix)?;
            Ok(VerifiedLaunch::ArgsFiles {
                windows: windows.clone(),
                unix: unix.clone(),
            })
        }
        LoaderOutputExpectation::ExactJar { path, main_class } => {
            verify_nonempty(server_root, path)?;
            if let Some(expected_main_class) = main_class {
                let actual = jar_main_class(&server_root.join(path))?;
                if actual.as_deref() != Some(expected_main_class.as_str()) {
                    return Err(LoaderError::InvalidOutput {
                        path: path.clone(),
                        reason: format!(
                            "manifest Main-Class is {actual:?}, expected '{expected_main_class}'"
                        ),
                    });
                }
            }
            Ok(VerifiedLaunch::Jar { path: path.clone() })
        }
    }
}

fn verify_nonempty(root: &Path, relative: &Path) -> Result<(), LoaderError> {
    super::model::validate_relative_path(relative)?;
    let full = root.join(relative);
    let metadata = fs::metadata(&full).map_err(|source| LoaderError::OutputIo {
        path: relative.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(LoaderError::InvalidOutput {
            path: relative.to_path_buf(),
            reason: "expected a non-empty regular file".to_string(),
        });
    }
    Ok(())
}

fn jar_main_class(path: &Path) -> Result<Option<String>, LoaderError> {
    let file = File::open(path).map_err(|source| LoaderError::OutputIo {
        path: path.to_path_buf(),
        source,
    })?;
    let mut archive = ZipArchive::new(file).map_err(|error| LoaderError::Jar {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    let mut manifest =
        archive
            .by_name("META-INF/MANIFEST.MF")
            .map_err(|error| LoaderError::Jar {
                path: path.to_path_buf(),
                reason: error.to_string(),
            })?;
    if manifest.size() > MAX_JAR_MANIFEST_BYTES {
        return Err(LoaderError::Jar {
            path: path.to_path_buf(),
            reason: format!(
                "META-INF/MANIFEST.MF expands to {} bytes, exceeding the {}-byte limit",
                manifest.size(),
                MAX_JAR_MANIFEST_BYTES
            ),
        });
    }
    let mut text = String::new();
    let manifest_size = usize::try_from(manifest.size()).map_err(|source| LoaderError::Jar {
        path: path.to_path_buf(),
        reason: format!("manifest size cannot be represented on this platform: {source}"),
    })?;
    text.try_reserve(manifest_size)
        .map_err(|source| LoaderError::Jar {
            path: path.to_path_buf(),
            reason: format!("could not reserve bounded manifest buffer: {source}"),
        })?;
    manifest
        .read_to_string(&mut text)
        .map_err(|source| LoaderError::OutputIo {
            path: path.to_path_buf(),
            source,
        })?;
    let unfolded = unfold_manifest(&text);
    Ok(unfolded.lines().find_map(|line| {
        line.strip_prefix("Main-Class:")
            .map(str::trim)
            .map(str::to_string)
    }))
}

fn unfold_manifest(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n");
    let mut output = String::with_capacity(normalized.len());
    for line in normalized.lines() {
        if let Some(continuation) = line.strip_prefix(' ') {
            output.push_str(continuation);
        } else {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(line);
        }
    }
    output
}

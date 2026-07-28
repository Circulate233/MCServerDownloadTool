use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::loader::VerifiedLaunch;

use super::atomic;

/// Durable inputs and verified outputs used for idempotent installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallState {
    /// SHA-256 of the exact selected manifest bytes.
    pub manifest_sha256: String,
    /// Java executable identity used for loader installation.
    pub java_executable: String,
    /// Hash of the exact loader plan, including expected output paths.
    pub loader_plan_sha256: String,
    /// Strictly verified launch output from the loader installer.
    pub loader_output: VerifiedLaunch,
    /// SHA-256 of the last start script published at its primary path.
    pub script_sha256: String,
}

impl InstallState {
    pub(crate) fn load(path: &Path) -> io::Result<Option<Self>> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn store(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        atomic::write(path, &bytes)
    }
}

//! Strict schema and integration boundaries for `MCServerDownloadTool`.

#![forbid(unsafe_code)]

use std::error::Error;
use std::path::{Path, PathBuf};

pub mod application;
pub mod cli;
pub mod error;
pub mod i18n;
pub mod install;
pub mod java;
pub mod loader;
pub mod manifest;
pub mod net;
pub mod scripts;
pub mod version;

use manifest::JavaConfig;

/// Java boundary that locates or provisions a runtime matching the manifest.
pub trait JavaRuntimeProvisioner {
    /// Concrete Java discovery or provisioning error.
    type Error: Error + Send + Sync + 'static;

    /// Returns the executable path for a runtime satisfying `config`.
    ///
    /// # Errors
    ///
    /// Returns the implementation's discovery or provisioning error when no
    /// matching Java runtime can be made available.
    fn provision(&self, config: &JavaConfig, server_root: &Path) -> Result<PathBuf, Self::Error>;
}

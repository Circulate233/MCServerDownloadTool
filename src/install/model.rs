use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::loader::{
    InstallerArtifact, InstallerSha1, LoaderError, LoaderFamily, LoaderPlan, VerifiedLaunch,
};
use crate::manifest::{LoaderKind, ManifestFile, SecretString, ValidatedManifest};
use crate::net::{NetworkError, TransferError, TransferEvent};
use crate::scripts::{ScriptError, ScriptPlatform};

/// Complete immutable input required by the installation state machine.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    /// Files in manifest order with explicit automatic or manual policy.
    pub files: Vec<ManifestFile>,
    /// Exact loader installer and expected launch output.
    pub loader: LoaderPlan,
    /// Already-selected Java executable verified by the Java layer.
    pub java_executable: PathBuf,
    /// Exact installer executable used for the Windows console ownership probe.
    pub console_helper_executable: PathBuf,
    /// Heap, JVM arguments, and server arguments for the start script.
    pub java: crate::manifest::JavaConfig,
    /// Optional validated network proxy shared by HTTP and installer JVM requests.
    pub proxy: Option<crate::cli::ProxyUrl>,
    /// Target script platform.
    pub script_platform: ScriptPlatform,
    /// Language used by reports generated inside the installation core.
    pub language: crate::i18n::Language,
    /// `CurseForge` API key, redacted from debug output.
    pub curseforge_api_key: Option<SecretString>,
}

impl InstallPlan {
    /// Converts every validated manifest field into the executable installation plan.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::InvalidPlan`] if the validated installer URL cannot
    /// produce a cache filename or the resulting loader plan is inconsistent.
    pub fn from_manifest(
        manifest: &ValidatedManifest,
        java_executable: PathBuf,
        console_helper_executable: PathBuf,
        proxy: Option<crate::cli::ProxyUrl>,
        script_platform: ScriptPlatform,
        language: crate::i18n::Language,
    ) -> Result<Self, InstallError> {
        let loader = manifest.loader();
        let installer = &loader.installer;
        let sha1 = match (&installer.sha1, &installer.sha1_sidecar) {
            (Some(value), None) => InstallerSha1::inline(value.clone()),
            (None, Some(url)) => InstallerSha1::sidecar(url.clone()),
            _ => {
                return Err(InstallError::InvalidPlan {
                    reason: "validated loader installer has no unique SHA-1 source".to_string(),
                });
            }
        };
        let plan = Self {
            files: manifest.files().to_vec(),
            loader: LoaderPlan {
                family: match loader.kind {
                    LoaderKind::Forge => LoaderFamily::Forge,
                    LoaderKind::Fabric => LoaderFamily::Fabric,
                    LoaderKind::NeoForge => LoaderFamily::NeoForge,
                    LoaderKind::Cleanroom => LoaderFamily::Cleanroom,
                },
                minecraft_version: manifest.minecraft().version.clone(),
                loader_version: loader.version.clone(),
                installer: InstallerArtifact {
                    url: installer.url.clone(),
                    file_name: installer.file_name().map_err(|error| {
                        InstallError::InvalidPlan {
                            reason: error.to_string(),
                        }
                    })?,
                    sha1,
                    size: installer.size,
                },
                output: loader.output.clone(),
            },
            java_executable,
            console_helper_executable,
            java: manifest.java().clone(),
            proxy,
            script_platform,
            language,
            curseforge_api_key: manifest.as_manifest().curseforge_api_key.clone(),
        };
        plan.loader.validate()?;
        Ok(plan)
    }
}

/// Coarse installation stages emitted in deterministic order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStage {
    /// The exclusive installation lock was acquired.
    Locked,
    /// Compatible Java runtimes are being discovered and selected.
    SelectingJava,
    /// Existing files and state are being validated.
    Inspecting,
    /// Loader installer and automatic files are transferred as one batch.
    Downloading,
    /// Manual files are checked together after automatic downloads.
    CheckingManualFiles,
    /// Loader installation or idempotent output verification is running.
    InstallingLoader,
    /// The start script and durable state are being published.
    WritingState,
    /// Every required operation completed.
    Completed,
}

/// Sanitized installation telemetry suitable for console output or structured logs.
#[derive(Debug, Clone)]
pub enum InstallEvent {
    /// The state machine entered a new stage.
    Stage(InstallStage),
    /// An existing verified target avoided network transfer.
    Reused { target: PathBuf },
    /// Shared network-engine progress without request headers or credentials.
    Transfer(TransferEvent),
    /// One upstream loader-installer stdout/stderr line, forwarded unchanged.
    LoaderOutput {
        /// Source output stream.
        stream: crate::loader::ProcessStream,
        /// Original upstream line.
        line: String,
    },
    /// Existing loader output allowed installer execution to be skipped.
    LoaderReused,
    /// A committed operation succeeded but best-effort cleanup failed.
    CleanupWarning {
        /// Path that could not be removed.
        path: PathBuf,
        /// Non-secret operating-system reason.
        reason: String,
    },
}

/// A typed failure returned by an installation observer.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum InstallObserverError {
    /// The persistent installation log could not accept or synchronize an event.
    #[error("persistent installation log '{path}' failed: {reason}")]
    PersistentLog {
        /// Durable log path associated with the failed operation.
        path: PathBuf,
        /// Stable operating-system error category retained for propagation.
        kind: io::ErrorKind,
        /// Non-secret error detail captured at the first failure.
        reason: String,
    },
    /// A caller-supplied terminal observer rejected an event.
    #[error("terminal installation observer failed: {reason}")]
    Terminal {
        /// Non-secret failure detail supplied by the terminal observer.
        reason: String,
    },
    /// Observer state synchronization was poisoned by an unexpected failure.
    #[error("installation observer synchronization was poisoned")]
    Synchronization,
}

impl InstallObserverError {
    /// Creates a typed error for a caller-supplied terminal observer failure.
    #[must_use]
    pub fn terminal(reason: impl Into<String>) -> Self {
        Self::Terminal {
            reason: reason.into(),
        }
    }
}

/// Receives progress and log events from one installation and exposes prior failures.
pub trait InstallObserver: Send + Sync {
    /// Handles one sanitized event; implementations should return promptly.
    ///
    /// # Errors
    ///
    /// Returns [`InstallObserverError`] when the event cannot be recorded or
    /// forwarded. The first error must remain visible through [`Self::check`].
    fn observe(&self, event: InstallEvent) -> Result<(), InstallObserverError>;

    /// Checks whether this observer has previously entered a failed state.
    ///
    /// # Errors
    ///
    /// Returns the first persistent observer failure. Stateless terminal
    /// observers use the default successful implementation.
    fn check(&self) -> Result<(), InstallObserverError> {
        Ok(())
    }
}

impl<F> InstallObserver for F
where
    F: Fn(InstallEvent) + Send + Sync,
{
    fn observe(&self, event: InstallEvent) -> Result<(), InstallObserverError> {
        self(event);
        Ok(())
    }
}

/// Observable successful installation result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    /// Root derived strictly from the selected manifest's parent directory.
    pub server_root: PathBuf,
    /// Verified exact launch description used by the generated script.
    pub launch: VerifiedLaunch,
    /// Whether valid state allowed loader execution to be skipped.
    pub loader_reused: bool,
}

/// Fail-fast errors from the installation state machine.
#[derive(Debug, Error)]
pub enum InstallError {
    /// The installation plan violates a path or cross-field invariant.
    #[error("invalid installation plan: {reason}")]
    InvalidPlan { reason: String },
    /// The manifest path has no parent directory that can be used as installation root.
    #[error("manifest path '{path}' has no installation root")]
    ManifestHasNoParent { path: PathBuf },
    /// A filesystem operation failed with its concrete target retained.
    #[error("failed to {operation} '{path}': {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// Another process already owns the installation lock.
    #[error("another installation already owns '{path}'")]
    Locked { path: PathBuf },
    /// A path would traverse a symlink, junction, reparse point, or root boundary.
    #[error("unsafe installation path '{path}': {reason}")]
    UnsafePath { path: PathBuf, reason: String },
    /// A session logging mutex was poisoned by an unexpected observer failure.
    #[error("persistent installation log synchronization was poisoned")]
    LogPoisoned,
    /// An observer rejected an event and cancelled the installation.
    #[error(transparent)]
    Observer(#[from] InstallObserverError),
    /// Network request construction or bounded metadata access failed.
    #[error(transparent)]
    Network(#[from] NetworkError),
    /// One or more artifacts failed in the shared batch.
    #[error("artifact batch failed: {failures:?}")]
    Transfer { failures: Vec<String> },
    /// Downloaded bytes did not match the manifest contract.
    #[error("downloaded artifact '{target}' failed verification: {reason}")]
    Verification { target: PathBuf, reason: String },
    /// The loader SHA-1 sidecar did not contain exactly one valid digest.
    #[error("loader SHA-1 sidecar '{url}' is invalid: {reason}")]
    InvalidInstallerSha1 { url: String, reason: String },
    /// Manual files are missing or invalid; the list was written before returning.
    #[error("{count} manual file(s) are missing or invalid; see '{list_path}'")]
    ManualFilesMissing { count: usize, list_path: PathBuf },
    /// Loader execution or strict output verification failed.
    #[error(transparent)]
    Loader(#[from] LoaderError),
    /// Script generation or publication failed.
    #[error(transparent)]
    Script(#[from] ScriptError),
    /// A user-modified start script was preserved and replacement content was written separately.
    #[error(
        "start script conflict: preserved '{existing}' and wrote generated script to '{generated}'"
    )]
    ScriptConflict {
        existing: PathBuf,
        generated: PathBuf,
    },
}

impl InstallError {
    pub(crate) fn io(operation: &'static str, path: impl AsRef<Path>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.as_ref().to_path_buf(),
            source,
        }
    }
}

impl From<TransferError> for InstallError {
    fn from(error: TransferError) -> Self {
        Self::Transfer {
            failures: vec![error.to_string()],
        }
    }
}

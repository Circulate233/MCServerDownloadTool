use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::cli::ProxyUrl;

/// Loader families whose official installer contracts are implemented here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoaderFamily {
    /// Minecraft Forge installer with `--installServer`.
    Forge,
    /// `NeoForge` installer with `--installServer`.
    NeoForge,
    /// Fabric installer with the official `server` arguments.
    Fabric,
    /// Cleanroom installer with `--installServer`.
    Cleanroom,
}

/// Verified loader-installer artifact downloaded before process execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallerArtifact {
    /// HTTPS artifact URL, or HTTP only in an explicitly controlled test environment.
    pub url: String,
    /// Safe cache filename beneath `.mcsdt/installers`.
    pub file_name: String,
    /// Vendor-published inline digest or same-origin sidecar.
    pub sha1: InstallerSha1,
    /// Optional exact artifact length.
    pub size: Option<u64>,
}

impl InstallerArtifact {
    /// Validates and creates one exact installer artifact declaration.
    ///
    /// # Errors
    ///
    /// Returns [`LoaderError::InvalidPlan`] for an unsafe filename or malformed SHA-1.
    pub fn new(
        url: impl Into<String>,
        file_name: impl Into<String>,
        sha1: impl Into<String>,
        size: Option<u64>,
    ) -> Result<Self, LoaderError> {
        let artifact = Self {
            url: url.into(),
            file_name: file_name.into(),
            sha1: InstallerSha1::inline(sha1),
            size,
        };
        artifact.validate()?;
        Ok(artifact)
    }

    pub(crate) fn validate(&self) -> Result<(), LoaderError> {
        validate_relative_path(Path::new(&self.file_name))?;
        if Path::new(&self.file_name).components().count() != 1 {
            return Err(LoaderError::InvalidPlan {
                reason: "installer file_name must contain exactly one path component".to_string(),
            });
        }
        self.sha1.validate()?;
        if self.size == Some(0) {
            return Err(LoaderError::InvalidPlan {
                reason: "installer size must be greater than zero when present".to_string(),
            });
        }
        Ok(())
    }
}

/// Source from which the exact loader installer SHA-1 is obtained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstallerSha1 {
    /// Digest embedded directly in the validated manifest.
    Inline {
        /// Exact forty-character hexadecimal SHA-1.
        value: String,
    },
    /// Small same-origin text response resolved before artifact transfer.
    Sidecar {
        /// Validated same-origin HTTPS sidecar URL.
        url: String,
    },
}

impl InstallerSha1 {
    /// Creates an inline digest source.
    #[must_use]
    pub fn inline(value: impl Into<String>) -> Self {
        Self::Inline {
            value: value.into(),
        }
    }

    /// Creates a sidecar digest source.
    #[must_use]
    pub fn sidecar(url: impl Into<String>) -> Self {
        Self::Sidecar { url: url.into() }
    }

    fn validate(&self) -> Result<(), LoaderError> {
        match self {
            Self::Inline { value } => {
                if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(LoaderError::InvalidPlan {
                        reason: "installer SHA-1 must contain exactly 40 hexadecimal characters"
                            .to_string(),
                    });
                }
            }
            Self::Sidecar { url } if url.trim().is_empty() => {
                return Err(LoaderError::InvalidPlan {
                    reason: "installer SHA-1 sidecar URL must not be blank".to_string(),
                });
            }
            Self::Sidecar { .. } => {}
        }
        Ok(())
    }
}

/// Exact post-install output that must exist before a start script is generated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LoaderOutputExpectation {
    /// Modern Forge and `NeoForge` publish platform-specific argument files.
    ModernArgs {
        /// Relative `win_args.txt` path.
        windows: PathBuf,
        /// Relative `unix_args.txt` path.
        unix: PathBuf,
    },
    /// A precise runnable jar path, optionally requiring an exact manifest Main-Class.
    ExactJar {
        /// Relative jar path determined from the exact loader version.
        path: PathBuf,
        /// Required manifest entry for Fabric; absent for legacy Forge and Cleanroom.
        main_class: Option<String>,
    },
}

/// Exact loader version, official installer, and expected launch output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoaderPlan {
    /// Installer family determining the exact Java arguments.
    pub family: LoaderFamily,
    /// Exact Minecraft version.
    pub minecraft_version: String,
    /// Exact loader version.
    pub loader_version: String,
    /// Installer artifact that joins the automatic-file batch.
    pub installer: InstallerArtifact,
    /// Strict output contract checked after successful process exit.
    pub output: LoaderOutputExpectation,
}

impl LoaderPlan {
    /// Validates all fields and output paths before filesystem or process activity.
    ///
    /// # Errors
    ///
    /// Returns [`LoaderError::InvalidPlan`] for a blank version or unsafe path.
    pub fn validate(&self) -> Result<(), LoaderError> {
        if self.minecraft_version.trim().is_empty() || self.loader_version.trim().is_empty() {
            return Err(LoaderError::InvalidPlan {
                reason: "Minecraft and loader versions must not be blank".to_string(),
            });
        }
        self.installer.validate()?;
        match &self.output {
            LoaderOutputExpectation::ModernArgs { windows, unix } => {
                if !matches!(self.family, LoaderFamily::Forge | LoaderFamily::NeoForge) {
                    return Err(LoaderError::InvalidPlan {
                        reason: "modern args output is only valid for Forge and NeoForge"
                            .to_string(),
                    });
                }
                validate_relative_path(windows)?;
                validate_relative_path(unix)?;
            }
            LoaderOutputExpectation::ExactJar { path, main_class } => {
                validate_relative_path(path)?;
                if path.extension().and_then(|value| value.to_str()) != Some("jar") {
                    return Err(LoaderError::InvalidPlan {
                        reason: "exact loader output must be a .jar path".to_string(),
                    });
                }
                if self.family == LoaderFamily::Fabric
                    && main_class.as_deref().is_none_or(str::is_empty)
                {
                    return Err(LoaderError::InvalidPlan {
                        reason: "Fabric output requires an exact Main-Class".to_string(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Strictly verified launch form persisted in install state and consumed by scripts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VerifiedLaunch {
    /// Modern Forge/NeoForge platform argument files.
    ArgsFiles { windows: PathBuf, unix: PathBuf },
    /// Exact runnable server jar.
    Jar { path: PathBuf },
}

/// Child-process output stream identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStream {
    /// Installer standard output.
    Stdout,
    /// Installer standard error.
    Stderr,
}

/// A typed output observer failure that requires installer process termination.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("loader process observer failed: {reason}")]
pub struct ProcessObserverError {
    reason: String,
}

impl ProcessObserverError {
    /// Creates an output observer failure from a non-secret reason.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Receives each complete UTF-8-lossy line while an installer is running.
pub trait ProcessObserver: Send + Sync {
    /// Forwards a line without buffering the whole installer output in memory.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessObserverError`] when forwarding fails. The process
    /// runner must terminate the child and reclaim both stream workers.
    fn line(&self, stream: ProcessStream, line: String) -> Result<(), ProcessObserverError>;
}

impl<F> ProcessObserver for F
where
    F: Fn(ProcessStream, String) + Send + Sync,
{
    fn line(&self, stream: ProcessStream, line: String) -> Result<(), ProcessObserverError> {
        self(stream, line);
        Ok(())
    }
}

/// Shell-free child process request used by real and fake Java runners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRequest {
    /// Executable passed directly to [`std::process::Command`].
    pub executable: PathBuf,
    /// Arguments passed as distinct operating-system strings.
    pub arguments: Vec<String>,
    /// Server root used as installer working directory.
    pub working_directory: PathBuf,
}

/// Injectable process boundary for loader execution tests and production Java.
pub trait ProcessRunner: Send + Sync {
    /// Executes one request and streams both output channels to `observer`.
    ///
    /// # Errors
    ///
    /// Returns a concrete I/O failure or unsuccessful exit status.
    fn run(
        &self,
        request: &ProcessRequest,
        observer: std::sync::Arc<dyn ProcessObserver>,
    ) -> Result<(), LoaderError>;
}

/// Boundary consumed by the installation state machine.
pub trait LoaderInstallation: Send + Sync {
    /// Runs the installer and verifies the exact expected output.
    ///
    /// # Errors
    ///
    /// Returns [`LoaderError`] when execution or output validation fails.
    fn install(
        &self,
        plan: &LoaderPlan,
        server_root: &Path,
        java_executable: &Path,
        installer_jar: &Path,
        proxy: Option<&ProxyUrl>,
        observer: std::sync::Arc<dyn ProcessObserver>,
    ) -> Result<VerifiedLaunch, LoaderError>;
}

/// Fail-fast loader plan, process, proxy, and output errors.
#[derive(Debug, Error)]
pub enum LoaderError {
    /// The caller supplied an internally inconsistent loader plan.
    #[error("invalid loader plan: {reason}")]
    InvalidPlan { reason: String },
    /// SOCKS proxy URLs cannot be represented by safe HTTP(S) JVM properties.
    #[error("installer JVM proxy scheme '{scheme}' is not supported")]
    UnsupportedProxy { scheme: String },
    /// Java process setup or streaming failed.
    #[error("failed to {operation} installer process: {source}")]
    ProcessIo {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    /// The installer returned a non-success status.
    #[error("loader installer exited unsuccessfully: {status}")]
    ProcessFailed { status: String },
    /// Output forwarding failed and the installer process was terminated.
    #[error("loader installer was terminated after output observation failed: {source}")]
    Observer {
        #[source]
        source: ProcessObserverError,
    },
    /// A required launch artifact is absent, empty, malformed, or unexpected.
    #[error("loader output '{path}' is invalid: {reason}")]
    InvalidOutput { path: PathBuf, reason: String },
    /// Reading a jar or argument file failed.
    #[error("failed to inspect loader output '{path}': {source}")]
    OutputIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// ZIP/JAR structure could not be decoded.
    #[error("failed to inspect jar '{path}': {reason}")]
    Jar { path: PathBuf, reason: String },
}

pub(crate) fn validate_relative_path(path: &Path) -> Result<(), LoaderError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            !matches!(component, Component::Normal(_))
                || component.as_os_str().to_string_lossy().contains('\0')
        })
    {
        return Err(LoaderError::InvalidPlan {
            reason: format!("'{}' is not a normalized relative path", path.display()),
        });
    }
    Ok(())
}

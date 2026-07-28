use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Stable process exit categories exposed by the command-line application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    /// The requested operation completed successfully.
    Success = 0,
    /// Command-line syntax or values are invalid.
    Usage = 2,
    /// The manifest could not be read.
    ManifestIo = 10,
    /// The manifest is not valid JSON or does not match the JSON shape.
    ManifestParse = 11,
    /// The manifest JSON shape is valid but violates a semantic rule.
    ManifestValidation = 12,
    /// Runtime configuration, such as the executable location, is invalid.
    Configuration = 20,
    /// A matching Java runtime could not be selected.
    Java = 30,
    /// Network setup or transfer failed.
    Network = 40,
    /// Downloaded or existing content failed integrity validation.
    Integrity = 41,
    /// The installation transaction, loader, script, or state failed.
    Installation = 50,
    /// An unexpected internal failure occurred.
    Internal = 70,
}

impl ExitCode {
    /// Returns the numeric process exit status.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

/// Errors produced while reading and validating a manifest.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// Reading the selected manifest failed.
    #[error("failed to read manifest {path}: {source}")]
    Read {
        /// Path selected by the caller.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// JSON parsing or strict schema deserialization failed.
    #[error("failed to parse manifest {origin}: {source}")]
    Parse {
        /// Human-readable source of the JSON bytes.
        origin: String,
        /// Underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// Cross-field or value validation failed.
    #[error(transparent)]
    Validation(#[from] ManifestValidationError),
}

impl ManifestError {
    /// Maps this manifest failure to its stable process exit category.
    #[must_use]
    pub const fn exit_code(&self) -> ExitCode {
        match self {
            Self::Read { .. } => ExitCode::ManifestIo,
            Self::Parse { .. } => ExitCode::ManifestParse,
            Self::Validation(_) => ExitCode::ManifestValidation,
        }
    }
}

/// Semantic violations in a schema-version-one manifest.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ManifestValidationError {
    /// Only the currently implemented schema can be accepted.
    #[error("schema_version must be 1, found {found}")]
    UnsupportedSchemaVersion {
        /// Version read from the manifest.
        found: u32,
    },
    /// A field violates its concrete value or cross-field contract.
    #[error("invalid {field}: {reason}")]
    InvalidField {
        /// JSON field path identifying the invalid value.
        field: String,
        /// Concrete validation rule that failed.
        reason: String,
    },
    /// Two file entries resolve to the same case-insensitive path.
    #[error("duplicate file path '{path}'")]
    DuplicatePath {
        /// Repeated path.
        path: String,
    },
    /// `CurseForge` access was requested without credentials.
    #[error("CurseForge API key is required when a CurseForge CDN download is present")]
    CurseForgeKeyRequired,
    /// Credentials were included although no CDN request can use them.
    #[error("curseforge_api_key is forbidden when there is no CurseForge CDN download")]
    UnusedCurseForgeKey,
}

/// Top-level application failures with stable exit-code mapping.
#[derive(Debug, Error)]
pub enum AppError {
    /// Manifest loading or validation failed.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// The current executable path could not be discovered.
    #[error("failed to determine the current executable: {0}")]
    CurrentExecutable(#[source] io::Error),
    /// The executable path has no parent directory for the default manifest.
    #[error("executable path has no parent directory: {0}")]
    ExecutableHasNoParent(PathBuf),
    /// A configured standard proxy variable contains an invalid URL.
    #[error("invalid proxy in {name}: {reason}")]
    InvalidEnvironmentProxy {
        /// Environment variable selected by precedence.
        name: &'static str,
        /// Concrete proxy parsing failure.
        reason: String,
    },
    /// The explicit proxy argument is invalid; the original value is never retained.
    #[error("invalid --proxy value: {reason}")]
    InvalidProxy { reason: String },
    /// Java discovery, probing, or interactive selection failed.
    #[error("Java runtime selection failed: {reason}")]
    Java { reason: String },
    /// Shared network-engine construction or metadata access failed.
    #[error(transparent)]
    Network(#[from] crate::net::NetworkError),
    /// Installation planning or execution failed.
    #[error(transparent)]
    Installation(#[from] crate::install::InstallError),
}

impl AppError {
    /// Maps an application failure to its stable process exit category.
    #[must_use]
    pub const fn exit_code(&self) -> ExitCode {
        match self {
            Self::Manifest(error) => error.exit_code(),
            Self::CurrentExecutable(_)
            | Self::ExecutableHasNoParent(_)
            | Self::InvalidEnvironmentProxy { .. }
            | Self::InvalidProxy { .. } => ExitCode::Configuration,
            Self::Java { .. } => ExitCode::Java,
            Self::Network(_) => ExitCode::Network,
            Self::Installation(error) => match error {
                crate::install::InstallError::Network(_)
                | crate::install::InstallError::Transfer { .. } => ExitCode::Network,
                crate::install::InstallError::Verification { .. }
                | crate::install::InstallError::InvalidInstallerSha1 { .. } => ExitCode::Integrity,
                _ => ExitCode::Installation,
            },
        }
    }
}

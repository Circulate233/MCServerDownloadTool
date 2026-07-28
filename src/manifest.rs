use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{ManifestError, ManifestValidationError};
use crate::loader::LoaderOutputExpectation;

/// The only manifest schema version understood by this release.
pub const SCHEMA_VERSION: u32 = 1;

/// Complete schema-version-one server installation manifest.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Schema discriminator. Version one is required.
    pub schema_version: u32,
    /// Exact Minecraft release installed by the selected loader.
    pub minecraft: MinecraftConfig,
    /// Java runtime and launch argument requirements.
    pub java: JavaConfig,
    /// Loader installer and expected output contract.
    pub loader: LoaderConfig,
    /// API key sent only to validated `CurseForge` CDN origins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curseforge_api_key: Option<SecretString>,
    /// Files materialized beneath the server root.
    pub files: Vec<ManifestFile>,
}

impl fmt::Debug for Manifest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Manifest")
            .field("schema_version", &self.schema_version)
            .field("minecraft", &self.minecraft)
            .field("java", &self.java)
            .field("loader", &self.loader)
            .field("curseforge_api_key", &self.curseforge_api_key)
            .field("files", &self.files)
            .finish()
    }
}

impl Manifest {
    /// Parses strict JSON and validates all semantic and cross-field rules.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Parse`] for malformed shape and
    /// [`ManifestError::Validation`] for the first semantic violation.
    pub fn from_slice(bytes: &[u8]) -> Result<ValidatedManifest, ManifestError> {
        let manifest: Self =
            serde_json::from_slice(bytes).map_err(|source| ManifestError::Parse {
                origin: "<memory>".to_string(),
                source,
            })?;
        manifest.into_validated().map_err(ManifestError::from)
    }

    /// Validates the complete schema without consuming it.
    ///
    /// # Errors
    ///
    /// Returns the first fail-fast [`ManifestValidationError`].
    pub fn validate(&self) -> Result<(), ManifestValidationError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ManifestValidationError::UnsupportedSchemaVersion {
                found: self.schema_version,
            });
        }
        validate_non_blank("minecraft.version", &self.minecraft.version)?;
        self.java.validate()?;
        self.loader.validate()?;

        let mut targets = FinalTargetSet::with_capacity(self.files.len() + 2);
        register_loader_output_targets(&self.loader.output, &mut targets)?;
        let mut needs_curseforge_key = false;
        for (index, file) in self.files.iter().enumerate() {
            file.validate(index)?;
            targets.register(&format!("files[{index}].path"), &file.path)?;
            needs_curseforge_key |= file.download.requires_curseforge_key();
        }
        if let Some(key) = &self.curseforge_api_key {
            validate_non_blank("curseforge_api_key", key.expose())?;
            if key.expose().contains(['\r', '\n', '\0']) {
                return invalid("curseforge_api_key", "NUL and line breaks are forbidden");
            }
        }
        match (needs_curseforge_key, self.curseforge_api_key.as_ref()) {
            (true, None) => Err(ManifestValidationError::CurseForgeKeyRequired),
            (false, Some(_)) => Err(ManifestValidationError::UnusedCurseForgeKey),
            _ => Ok(()),
        }
    }

    /// Validates and wraps this value for installation-facing APIs.
    ///
    /// # Errors
    ///
    /// Returns the first fail-fast [`ManifestValidationError`].
    pub fn into_validated(self) -> Result<ValidatedManifest, ManifestValidationError> {
        self.validate()?;
        Ok(ValidatedManifest(self))
    }
}

struct FinalTarget {
    field: String,
    path: String,
}

struct FinalTargetSet {
    targets: HashMap<String, FinalTarget>,
}

impl FinalTargetSet {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            targets: HashMap::with_capacity(capacity),
        }
    }

    fn register(&mut self, field: &str, path: &str) -> Result<(), ManifestValidationError> {
        let normalized = path.to_lowercase();
        if let Some(existing) = self.targets.get(&normalized) {
            return invalid(
                field,
                &format!(
                    "target path '{path}' conflicts case-insensitively with {} ('{}')",
                    existing.field, existing.path
                ),
            );
        }
        self.targets.insert(
            normalized,
            FinalTarget {
                field: field.to_string(),
                path: path.to_string(),
            },
        );
        Ok(())
    }
}

/// A strict manifest whose semantic invariants have already been checked.
#[derive(Clone, PartialEq, Eq)]
pub struct ValidatedManifest(Manifest);

impl fmt::Debug for ValidatedManifest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl ValidatedManifest {
    /// Returns the complete validated manifest.
    #[must_use]
    pub const fn as_manifest(&self) -> &Manifest {
        &self.0
    }

    /// Returns the exact Minecraft requirement.
    #[must_use]
    pub const fn minecraft(&self) -> &MinecraftConfig {
        &self.0.minecraft
    }

    /// Returns the Java runtime and argument requirements.
    #[must_use]
    pub const fn java(&self) -> &JavaConfig {
        &self.0.java
    }

    /// Returns the loader installation declaration.
    #[must_use]
    pub const fn loader(&self) -> &LoaderConfig {
        &self.0.loader
    }

    /// Returns file declarations in stable manifest order.
    #[must_use]
    pub fn files(&self) -> &[ManifestFile] {
        &self.0.files
    }

    /// Returns the validated `CurseForge` API key without formatting it.
    #[must_use]
    pub fn curseforge_api_key(&self) -> Option<&str> {
        self.0.curseforge_api_key.as_ref().map(SecretString::expose)
    }
}

/// Secret text that is redacted from all debug output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    /// Exposes the secret only to the authenticated request builder.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// Exact Minecraft release contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinecraftConfig {
    /// Minecraft release identifier, such as `1.21.1`.
    pub version: String,
}

/// Java runtime, heap, JVM argument, and server argument contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JavaConfig {
    /// Required Java feature-release number.
    pub major: u16,
    /// Initial heap size in mebibytes.
    pub min_memory_mb: u32,
    /// Maximum heap size in mebibytes.
    pub max_memory_mb: u32,
    /// Arguments placed before the loader launch target.
    #[serde(default)]
    pub jvm_args: Vec<String>,
    /// Arguments placed after the loader launch target.
    #[serde(default)]
    pub server_args: Vec<String>,
}

impl JavaConfig {
    fn validate(&self) -> Result<(), ManifestValidationError> {
        if self.major == 0 || self.major > 255 {
            return invalid("java.major", "must be between 1 and 255");
        }
        if self.min_memory_mb == 0 || self.min_memory_mb > self.max_memory_mb {
            return invalid(
                "java.min_memory_mb",
                "must be positive and no greater than java.max_memory_mb",
            );
        }
        validate_arguments("java.jvm_args", &self.jvm_args)?;
        validate_arguments("java.server_args", &self.server_args)
    }
}

/// Loader family, exact version, installer, and required output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoaderConfig {
    /// Loader family with an implemented installer command contract.
    pub kind: LoaderKind,
    /// Exact loader version.
    pub version: String,
    /// Installer artifact and integrity source.
    pub installer: LoaderInstaller,
    /// Exact output required after installer success.
    pub output: LoaderOutputExpectation,
}

impl LoaderConfig {
    fn validate(&self) -> Result<(), ManifestValidationError> {
        validate_non_blank("loader.version", &self.version)?;
        self.installer.validate()?;
        validate_loader_output(self.kind, &self.output)
    }
}

/// Loader families supported by the executable installer path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoaderKind {
    /// Minecraft Forge.
    Forge,
    /// Fabric loader.
    Fabric,
    /// `NeoForge`.
    NeoForge,
    /// Cleanroom.
    Cleanroom,
}

/// Loader installer URL, integrity source, and optional exact size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoaderInstaller {
    /// Credential-free HTTPS installer URL.
    pub url: String,
    /// Inline SHA-1; exactly one of this and `sha1_sidecar` is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
    /// Same-origin HTTPS text file containing the installer SHA-1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1_sidecar: Option<String>,
    /// Optional exact installer length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl LoaderInstaller {
    fn validate(&self) -> Result<(), ManifestValidationError> {
        let installer = validate_https_url("loader.installer.url", &self.url)?;
        match (&self.sha1, &self.sha1_sidecar) {
            (Some(sha1), None) => validate_sha1("loader.installer.sha1", sha1)?,
            (None, Some(sidecar)) => {
                let sidecar = validate_https_url("loader.installer.sha1_sidecar", sidecar)?;
                if origin(&installer) != origin(&sidecar) {
                    return invalid(
                        "loader.installer.sha1_sidecar",
                        "must use the same origin as loader.installer.url",
                    );
                }
            }
            _ => {
                return invalid(
                    "loader.installer",
                    "exactly one of sha1 or sha1_sidecar is required",
                );
            }
        }
        if self.size == Some(0) {
            return invalid("loader.installer.size", "must be greater than zero");
        }
        let file_name = self.file_name()?;
        if !file_name.to_ascii_lowercase().ends_with(".jar") {
            return invalid("loader.installer.url", "path must end in a .jar filename");
        }
        Ok(())
    }

    /// Derives the safe cache filename from the validated installer URL.
    ///
    /// # Errors
    ///
    /// Returns a manifest validation error when the URL has no safe final path segment.
    pub fn file_name(&self) -> Result<String, ManifestValidationError> {
        let url = validate_https_url("loader.installer.url", &self.url)?;
        let name = url
            .path_segments()
            .and_then(Iterator::last)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| ManifestValidationError::InvalidField {
                field: "loader.installer.url".to_string(),
                reason: "URL must end in an installer filename".to_string(),
            })?;
        validate_path("loader.installer.url", name, false)?;
        Ok(name.to_string())
    }
}

/// User-facing category associated with an installed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// Loader mod.
    Mod,
    /// Server resource pack.
    ResourcePack,
    /// Server shader pack.
    ShaderPack,
}

/// Automatic or explicit manual file acquisition mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum FileDownload {
    /// Download, verify, and atomically publish the URL.
    Automatic {
        /// Credential-free HTTPS artifact URL.
        url: String,
    },
    /// Require the user to place and verify the file before loader execution.
    Manual,
}

impl FileDownload {
    fn validate(&self, field: &str) -> Result<(), ManifestValidationError> {
        match self {
            Self::Automatic { url } => {
                validate_https_url(&format!("{field}.url"), url)?;
                Ok(())
            }
            Self::Manual => Ok(()),
        }
    }

    pub(crate) fn requires_curseforge_key(&self) -> bool {
        let Self::Automatic { url } = self else {
            return false;
        };
        Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
            .is_some_and(|host| host == "forgecdn.net" || host.ends_with(".forgecdn.net"))
    }
}

/// One server file and its complete installation metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    /// Human-readable project name used in manual reports.
    pub name: String,
    /// File category.
    #[serde(rename = "type")]
    pub kind: FileKind,
    /// Normalized slash-separated target beneath the server root.
    pub path: String,
    /// Automatic URL or manual acquisition mode.
    pub download: FileDownload,
    /// HTTPS page where a user can inspect or obtain the project file.
    pub project_page: String,
    /// Exact SHA-1 digest.
    pub sha1: String,
    /// Exact byte length.
    pub size: u64,
}

impl ManifestFile {
    fn validate(&self, index: usize) -> Result<(), ManifestValidationError> {
        validate_non_blank(&format!("files[{index}].name"), &self.name)?;
        validate_path(&format!("files[{index}].path"), &self.path, true)?;
        self.download
            .validate(&format!("files[{index}].download"))?;
        validate_https_url(&format!("files[{index}].project_page"), &self.project_page)?;
        validate_sha1(&format!("files[{index}].sha1"), &self.sha1)?;
        if self.size == 0 {
            return invalid(&format!("files[{index}].size"), "must be greater than zero");
        }
        Ok(())
    }
}

/// Reads, strictly deserializes, and semantically validates a manifest file.
///
/// # Errors
///
/// Returns a read, parse, or semantic validation error with the selected path.
pub fn load_manifest(path: &Path) -> Result<ValidatedManifest, ManifestError> {
    let bytes = fs::read(path).map_err(|source| ManifestError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|source| ManifestError::Parse {
            origin: path.display().to_string(),
            source,
        })?;
    manifest.into_validated().map_err(ManifestError::from)
}

fn validate_arguments(field: &str, arguments: &[String]) -> Result<(), ManifestValidationError> {
    for (index, argument) in arguments.iter().enumerate() {
        let item = format!("{field}[{index}]");
        validate_non_blank(&item, argument)?;
        if argument.contains(['\r', '\n', '\0']) {
            return invalid(&item, "NUL and line breaks are forbidden");
        }
    }
    Ok(())
}

fn register_loader_output_targets(
    output: &LoaderOutputExpectation,
    targets: &mut FinalTargetSet,
) -> Result<(), ManifestValidationError> {
    match output {
        LoaderOutputExpectation::ModernArgs { windows, unix } => {
            targets.register("loader.output.windows", windows.to_string_lossy().as_ref())?;
            targets.register("loader.output.unix", unix.to_string_lossy().as_ref())
        }
        LoaderOutputExpectation::ExactJar { path, .. } => {
            targets.register("loader.output.path", path.to_string_lossy().as_ref())
        }
    }
}

fn validate_loader_output(
    family: LoaderKind,
    output: &LoaderOutputExpectation,
) -> Result<(), ManifestValidationError> {
    match output {
        LoaderOutputExpectation::ModernArgs { windows, unix } => {
            if !matches!(family, LoaderKind::Forge | LoaderKind::NeoForge) {
                return invalid(
                    "loader.output",
                    "modern_args is supported only for forge and neoforge",
                );
            }
            validate_path(
                "loader.output.windows",
                windows.to_string_lossy().as_ref(),
                true,
            )?;
            validate_path("loader.output.unix", unix.to_string_lossy().as_ref(), true)
        }
        LoaderOutputExpectation::ExactJar { path, main_class } => {
            validate_path("loader.output.path", path.to_string_lossy().as_ref(), true)?;
            if path.extension().and_then(|value| value.to_str()) != Some("jar") {
                return invalid("loader.output.path", "must end in .jar");
            }
            if family == LoaderKind::Fabric {
                let value = main_class.as_deref().unwrap_or_default();
                validate_non_blank("loader.output.main_class", value)?;
            } else if main_class.as_deref().is_some_and(str::is_empty) {
                return invalid("loader.output.main_class", "must not be blank when present");
            }
            Ok(())
        }
    }
}

fn validate_non_blank(field: &str, value: &str) -> Result<(), ManifestValidationError> {
    if value.trim().is_empty() {
        return invalid(field, "must not be blank");
    }
    Ok(())
}

fn validate_sha1(field: &str, value: &str) -> Result<(), ManifestValidationError> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return invalid(
            field,
            "SHA-1 must contain exactly 40 hexadecimal characters",
        );
    }
    Ok(())
}

fn validate_https_url(field: &str, value: &str) -> Result<Url, ManifestValidationError> {
    if !value
        .split_once("://")
        .is_some_and(|(_, authority)| !authority.is_empty() && !authority.starts_with('/'))
    {
        return invalid(field, "URL must use scheme://host syntax");
    }
    let url = Url::parse(value).map_err(|error| ManifestValidationError::InvalidField {
        field: field.to_string(),
        reason: error.to_string(),
    })?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return invalid(field, "URL must be absolute HTTPS with a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return invalid(field, "URL credentials are forbidden");
    }
    if url.fragment().is_some() {
        return invalid(field, "URL fragments are forbidden");
    }
    Ok(url)
}

fn origin(url: &Url) -> (String, String, Option<u16>) {
    (
        url.scheme().to_ascii_lowercase(),
        url.host_str().unwrap_or_default().to_ascii_lowercase(),
        url.port_or_known_default(),
    )
}

fn validate_path(
    field: &str,
    value: &str,
    reject_tool_paths: bool,
) -> Result<(), ManifestValidationError> {
    if value.is_empty()
        || value.trim() != value
        || value.contains(['\\', '\0'])
        || value.starts_with('/')
        || has_windows_drive_prefix(value)
    {
        return invalid(
            field,
            "must be a normalized relative path using forward slashes",
        );
    }
    let path = Path::new(value);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
        || value.split('/').any(str::is_empty)
    {
        return invalid(field, "must not contain empty or dot path components");
    }
    for component in value.split('/') {
        if component.trim_end_matches([' ', '.']) != component {
            return invalid(field, "path components cannot end with a space or dot");
        }
        if is_windows_device_name(component) {
            return invalid(field, "contains a reserved Windows device name");
        }
    }
    if reject_tool_paths {
        let normalized = value.to_ascii_lowercase();
        let first = value
            .split('/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "server-install.json"
                | "mc-server-download-tool"
                | "mc-server-download-tool.exe"
                | "mcserverdownloadtool-windows-x86_64.exe"
                | "mcserverdownloadtool-linux-x86_64"
                | "mcserverdownloadtool-macos-aarch64"
                | "start.bat"
                | "start.sh"
                | "start.bat.new"
                | "start.sh.new"
                | "missing-files.txt"
        ) || first == ".mcsdt"
        {
            return invalid(field, "path is reserved by the installer");
        }
    }
    Ok(())
}

fn invalid<T>(field: &str, reason: &str) -> Result<T, ManifestValidationError> {
    Err(ManifestValidationError::InvalidField {
        field: field.to_string(),
        reason: reason.to_string(),
    })
}

fn has_windows_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn is_windows_device_name(component: &str) -> bool {
    let base = component.split('.').next().unwrap_or(component);
    let uppercase = base.to_ascii_uppercase();
    matches!(uppercase.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || uppercase
            .strip_prefix("COM")
            .or_else(|| uppercase.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

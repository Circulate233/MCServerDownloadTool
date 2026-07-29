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

const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_ARGUMENTS: usize = 512;
const MAX_ARGUMENT_BYTES: usize = 8 * 1024;
const MAX_NAME_BYTES: usize = 512;
const MAX_VERSION_BYTES: usize = 128;
const MAX_PATH_BYTES: usize = 1024;
const MAX_URL_BYTES: usize = 4096;
const MAX_API_KEY_BYTES: usize = 4096;
const MAX_LOADER_INSTALLER_BYTES: u64 = 512 * 1024 * 1024;
const MAX_MANIFEST_FILE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const FORGE_CDN_ORIGIN: &str = "edge.forgecdn.net";
const CURSEFORGE_ORIGIN: &str = "www.curseforge.com";

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
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES {
            return Err(ManifestValidationError::InvalidField {
                field: "manifest".to_string(),
                reason: format!("must not exceed {MAX_MANIFEST_BYTES} bytes"),
            }
            .into());
        }
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
        validate_minecraft_version("minecraft.version", &self.minecraft.version)?;
        self.java.validate()?;
        self.loader.validate()?;
        validate_loader_contract(&self.loader, &self.minecraft.version)?;

        if self.files.len() > MAX_FILES {
            return invalid(
                "files",
                &format!("must contain at most {MAX_FILES} entries"),
            );
        }

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
            validate_max_bytes("curseforge_api_key", key.expose(), MAX_API_KEY_BYTES)?;
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
        if self.jvm_args.len() > MAX_ARGUMENTS || self.server_args.len() > MAX_ARGUMENTS {
            return invalid(
                "java",
                &format!("each argument array may contain at most {MAX_ARGUMENTS} entries"),
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
        validate_maven_segment("loader.version", &self.version)?;
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
        validate_max_bytes("loader.installer.url", &self.url, MAX_URL_BYTES)?;
        let installer = validate_strict_https_url("loader.installer.url", &self.url)?;
        match (&self.sha1, &self.sha1_sidecar) {
            (Some(sha1), None) => validate_sha1("loader.installer.sha1", sha1)?,
            (None, Some(sidecar)) => {
                validate_max_bytes("loader.installer.sha1_sidecar", sidecar, MAX_URL_BYTES)?;
                let sidecar = validate_strict_https_url("loader.installer.sha1_sidecar", sidecar)?;
                if origin(&installer) != origin(&sidecar) {
                    return invalid(
                        "loader.installer.sha1_sidecar",
                        "must use the same origin as loader.installer.url",
                    );
                }
                if sidecar.as_str() != format!("{}.sha1", installer.as_str()) {
                    return invalid(
                        "loader.installer.sha1_sidecar",
                        "must be the exact installer URL with a .sha1 suffix",
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
        if self
            .size
            .is_some_and(|size| size == 0 || size > MAX_LOADER_INSTALLER_BYTES)
        {
            return invalid(
                "loader.installer.size",
                &format!("must be between 1 and {MAX_LOADER_INSTALLER_BYTES} bytes when present"),
            );
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
        let url = validate_strict_https_url("loader.installer.url", &self.url)?;
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
            Self::Automatic { url } => validate_forgecdn_url(&format!("{field}.url"), url),
            Self::Manual => Ok(()),
        }
    }

    pub(crate) fn requires_curseforge_key(&self) -> bool {
        let Self::Automatic { url } = self else {
            return false;
        };
        Url::parse(url)
            .ok()
            .is_some_and(|url| exact_https_origin(&url, FORGE_CDN_ORIGIN))
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
        let name_field = format!("files[{index}].name");
        validate_bounded_text(&name_field, &self.name, MAX_NAME_BYTES)?;
        validate_path(&format!("files[{index}].path"), &self.path, true)?;
        self.download
            .validate(&format!("files[{index}].download"))?;
        validate_project_page(
            &format!("files[{index}].project_page"),
            &self.project_page,
            self.kind,
        )?;
        validate_sha1(&format!("files[{index}].sha1"), &self.sha1)?;
        if self.size == 0 || self.size > MAX_MANIFEST_FILE_BYTES {
            return invalid(
                &format!("files[{index}].size"),
                &format!("must be between 1 and {MAX_MANIFEST_FILE_BYTES} bytes"),
            );
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
    let metadata = fs::metadata(path).map_err(|source| ManifestError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(ManifestValidationError::InvalidField {
            field: "manifest".to_string(),
            reason: "must be a regular file".to_string(),
        }
        .into());
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestValidationError::InvalidField {
            field: "manifest".to_string(),
            reason: format!("must not exceed {MAX_MANIFEST_BYTES} bytes"),
        }
        .into());
    }
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
        validate_max_bytes(&item, argument, MAX_ARGUMENT_BYTES)?;
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

fn validate_bounded_text(
    field: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ManifestValidationError> {
    validate_non_blank(field, value)?;
    validate_max_bytes(field, value, max_bytes)?;
    if value.chars().any(char::is_control) {
        return invalid(field, "control characters are forbidden");
    }
    Ok(())
}

fn validate_max_bytes(
    field: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ManifestValidationError> {
    if value.len() > max_bytes {
        return invalid(field, &format!("must not exceed {max_bytes} UTF-8 bytes"));
    }
    Ok(())
}

fn validate_minecraft_version(field: &str, value: &str) -> Result<(), ManifestValidationError> {
    validate_max_bytes(field, value, MAX_VERSION_BYTES)?;
    let components = value.split('.').collect::<Vec<_>>();
    if !(2..=3).contains(&components.len())
        || components.iter().any(|component| {
            component.is_empty()
                || !component.bytes().all(|byte| byte.is_ascii_digit())
                || (component.len() > 1 && component.starts_with('0'))
        })
        || components[0] != "1"
    {
        return invalid(
            field,
            "must use the numeric form 1.<minor> or 1.<minor>.<patch>",
        );
    }
    Ok(())
}

fn validate_maven_segment(field: &str, value: &str) -> Result<(), ManifestValidationError> {
    validate_max_bytes(field, value, MAX_VERSION_BYTES)?;
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains("..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
    {
        return invalid(
            field,
            "must be one safe Maven path segment containing only ASCII letters, digits, '.', '-', '_', or '+'",
        );
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

fn validate_strict_https_url(field: &str, value: &str) -> Result<Url, ManifestValidationError> {
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
    if url.port().is_some() || url.query().is_some() || url.fragment().is_some() {
        return invalid(
            field,
            "explicit ports, queries, and fragments are forbidden",
        );
    }
    Ok(url)
}

fn exact_https_origin(url: &Url, host: &str) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(host))
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn validate_forgecdn_url(field: &str, value: &str) -> Result<(), ManifestValidationError> {
    validate_max_bytes(field, value, MAX_URL_BYTES)?;
    let url = validate_strict_https_url(field, value)?;
    if !exact_https_origin(&url, FORGE_CDN_ORIGIN) {
        return invalid(field, "must use the exact https://edge.forgecdn.net origin");
    }
    let segments = url
        .path_segments()
        .map(Iterator::collect::<Vec<_>>)
        .unwrap_or_default();
    if segments.len() != 4
        || segments[0] != "files"
        || !is_decimal_id(segments[1])
        || !is_decimal_id(segments[2])
        || !is_safe_url_filename(segments[3])
    {
        return invalid(field, "must match /files/<id>/<subId>/<filename>");
    }
    Ok(())
}

fn validate_project_page(
    field: &str,
    value: &str,
    kind: FileKind,
) -> Result<(), ManifestValidationError> {
    validate_max_bytes(field, value, MAX_URL_BYTES)?;
    let url = validate_strict_https_url(field, value)?;
    if !exact_https_origin(&url, CURSEFORGE_ORIGIN) {
        return invalid(
            field,
            "must use the exact https://www.curseforge.com origin",
        );
    }
    let expected_kind = match kind {
        FileKind::Mod => "mc-mods",
        FileKind::ResourcePack => "texture-packs",
        FileKind::ShaderPack => "shaders",
    };
    let segments = url
        .path_segments()
        .map(Iterator::collect::<Vec<_>>)
        .unwrap_or_default();
    if segments.len() != 5
        || segments[0] != "minecraft"
        || segments[1] != expected_kind
        || !is_curseforge_slug(segments[2])
        || segments[3] != "files"
        || !is_decimal_id(segments[4])
    {
        return invalid(
            field,
            &format!("must match /minecraft/{expected_kind}/<slug>/files/<fileId>"),
        );
    }
    Ok(())
}

fn is_decimal_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_curseforge_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn is_safe_url_filename(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    !value.is_empty()
        && value.len() <= MAX_PATH_BYTES
        && value != "."
        && value != ".."
        && !lower.contains("%2f")
        && !lower.contains("%5c")
        && !value.chars().any(char::is_control)
}

fn validate_loader_contract(
    loader: &LoaderConfig,
    minecraft_version: &str,
) -> Result<(), ManifestValidationError> {
    let expected_url = match loader.kind {
        LoaderKind::Forge => format!(
            "https://maven.minecraftforge.net/net/minecraftforge/forge/{minecraft_version}-{0}/forge-{minecraft_version}-{0}-installer.jar",
            loader.version
        ),
        LoaderKind::NeoForge => format!(
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/{0}/neoforge-{0}-installer.jar",
            loader.version
        ),
        LoaderKind::Cleanroom => {
            let github = format!(
                "https://github.com/CleanroomMC/Cleanroom/releases/download/{0}/cleanroom-{0}-installer.jar",
                loader.version
            );
            let repository = if loader.version.to_ascii_lowercase().contains("snapshot") {
                "snapshots"
            } else {
                "releases"
            };
            let maven = format!(
                "https://repo.cleanroommc.com/{repository}/com/cleanroommc/cleanroom/{0}/cleanroom-{0}-installer.jar",
                loader.version
            );
            if loader.installer.url == github || loader.installer.url == maven {
                loader.installer.url.clone()
            } else {
                return invalid(
                    "loader.installer.url",
                    "must be the exact official Cleanroom GitHub Release or Maven installer URL",
                );
            }
        }
        LoaderKind::Fabric => validate_fabric_installer_url(&loader.installer.url)?,
    };
    if loader.installer.url != expected_url {
        return invalid(
            "loader.installer.url",
            &format!(
                "does not match the exact official {:?} installer coordinate",
                loader.kind
            ),
        );
    }

    let expected_output = match loader.kind {
        LoaderKind::Fabric => LoaderOutputExpectation::ExactJar {
            path: "fabric-server-launch.jar".into(),
            main_class: Some(
                "net.fabricmc.loader.impl.launch.server.FabricServerLauncher".to_string(),
            ),
        },
        LoaderKind::NeoForge => {
            let base = format!("libraries/net/neoforged/neoforge/{}", loader.version);
            LoaderOutputExpectation::ModernArgs {
                windows: format!("{base}/win_args.txt").into(),
                unix: format!("{base}/unix_args.txt").into(),
            }
        }
        LoaderKind::Cleanroom => LoaderOutputExpectation::ExactJar {
            path: format!("cleanroom-{minecraft_version}.jar").into(),
            main_class: None,
        },
        LoaderKind::Forge if is_modern_minecraft(minecraft_version)? => {
            let base = format!(
                "libraries/net/minecraftforge/forge/{minecraft_version}-{}",
                loader.version
            );
            LoaderOutputExpectation::ModernArgs {
                windows: format!("{base}/win_args.txt").into(),
                unix: format!("{base}/unix_args.txt").into(),
            }
        }
        LoaderKind::Forge => LoaderOutputExpectation::ExactJar {
            path: format!("forge-{minecraft_version}-{}.jar", loader.version).into(),
            main_class: None,
        },
    };
    if loader.output != expected_output {
        return invalid(
            "loader.output",
            "does not match the exact output contract for the selected loader and versions",
        );
    }
    Ok(())
}

fn validate_fabric_installer_url(value: &str) -> Result<String, ManifestValidationError> {
    let url = validate_strict_https_url("loader.installer.url", value)?;
    if !exact_https_origin(&url, "maven.fabricmc.net") {
        return invalid(
            "loader.installer.url",
            "Fabric installers must use the exact https://maven.fabricmc.net origin",
        );
    }
    let segments = url
        .path_segments()
        .map(Iterator::collect::<Vec<_>>)
        .unwrap_or_default();
    if segments.len() != 5
        || segments[..3] != ["net", "fabricmc", "fabric-installer"]
        || !validate_maven_segment_value(segments[3])
        || segments[4] != format!("fabric-installer-{}.jar", segments[3])
    {
        return invalid(
            "loader.installer.url",
            "must match /net/fabricmc/fabric-installer/<version>/fabric-installer-<version>.jar",
        );
    }
    Ok(value.to_string())
}

fn validate_maven_segment_value(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
}

fn is_modern_minecraft(value: &str) -> Result<bool, ManifestValidationError> {
    validate_minecraft_version("minecraft.version", value)?;
    let mut components = value.split('.');
    let major = components
        .next()
        .unwrap_or_default()
        .parse::<u32>()
        .map_err(|_| ManifestValidationError::InvalidField {
            field: "minecraft.version".to_string(),
            reason: "numeric component overflow".to_string(),
        })?;
    let minor = components
        .next()
        .unwrap_or_default()
        .parse::<u32>()
        .map_err(|_| ManifestValidationError::InvalidField {
            field: "minecraft.version".to_string(),
            reason: "numeric component overflow".to_string(),
        })?;
    Ok((major, minor) >= (1, 17))
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
        || value.len() > MAX_PATH_BYTES
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
        if component.chars().any(|character| {
            character.is_control() || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        }) {
            return invalid(
                field,
                "path components contain a Windows-forbidden character or ADS separator",
            );
        }
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
    matches!(
        uppercase.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || uppercase
        .strip_prefix("COM")
        .or_else(|| uppercase.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

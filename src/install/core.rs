use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::header::{HeaderName, HeaderValue};
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::loader::{
    InstallerSha1, LoaderInstallation, ProcessObserver, ProcessObserverError, ProcessStream,
    verify_loader_output,
};
use crate::manifest::{FileDownload, ManifestFile};
use crate::net::{
    ArtifactRequest, ArtifactTransfer, HttpRequest, HttpTransport, NetworkError, SensitiveHeaders,
    TransferEvent, TransferObserver, TransferObserverError,
};
use crate::scripts::{ScriptOutcome, ScriptRequest, WindowsFailureBehavior, write_start_script};

use super::atomic;
use super::model::{
    InstallError, InstallEvent, InstallObserver, InstallPlan, InstallResult, InstallStage,
};
use super::state::InstallState;
use super::{InstallRoot, InstallSession};

const MISSING_FILES: &str = "missing-files.txt";
const MAX_SHA1_SIDECAR_RESPONSE: usize = 1024;

/// Installation state-machine boundary used by the CLI integration layer.
pub trait InstallCore {
    /// Executes one installation rooted at the selected manifest's parent.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError`] immediately after a concrete lock, network,
    /// verification, loader, script, or durable-state failure.
    fn install(
        &self,
        manifest_path: &Path,
        plan: &InstallPlan,
        observer: Arc<dyn InstallObserver>,
    ) -> Result<InstallResult, InstallError>;

    /// Executes inside an already-acquired installation session.
    ///
    /// This entry point lets the application hold the same lock across Java
    /// selection and installation. Callers cannot construct an unlocked session.
    ///
    /// # Errors
    ///
    /// Returns the first plan, path-boundary, log, network, integrity, loader,
    /// script, or durable-state error encountered while using `session`.
    fn install_in_session(
        &self,
        manifest_path: &Path,
        plan: &InstallPlan,
        session: &InstallSession,
    ) -> Result<InstallResult, InstallError>;
}

/// Complete installer using one shared network engine and one loader boundary.
#[derive(Debug, Clone)]
pub struct Installer<E, L> {
    engine: E,
    loader: L,
}

impl<E, L> Installer<E, L> {
    /// Creates an installer whose engine is reused for metadata and the complete artifact batch.
    pub const fn new(engine: E, loader: L) -> Self {
        Self { engine, loader }
    }
}

impl<E, L> InstallCore for Installer<E, L>
where
    E: ArtifactTransfer + HttpTransport,
    L: LoaderInstallation,
{
    fn install(
        &self,
        manifest_path: &Path,
        plan: &InstallPlan,
        observer: Arc<dyn InstallObserver>,
    ) -> Result<InstallResult, InstallError> {
        let secrets = plan
            .curseforge_api_key
            .as_ref()
            .map(|key| key.expose().to_string())
            .into_iter();
        let session = InstallSession::acquire(manifest_path, plan.language, observer, secrets)?;
        let result = self.install_in_session(manifest_path, plan, &session);
        if let Err(error) = &result {
            session.record_failure(&error.to_string())?;
        }
        result
    }

    fn install_in_session(
        &self,
        manifest_path: &Path,
        plan: &InstallPlan,
        session: &InstallSession,
    ) -> Result<InstallResult, InstallError> {
        plan.loader.validate()?;
        validate_file_targets(&plan.files)?;
        session.root().verify_existing_file(manifest_path)?;
        validate_plan_paths(session.root(), plan)?;
        let manifest_bytes = fs::read(manifest_path)
            .map_err(|source| InstallError::io("read manifest", manifest_path, source))?;
        session.emit(InstallEvent::Stage(InstallStage::Inspecting))?;
        let observer = session.observer();

        let paths = InstallPaths::create(session.root().clone(), plan)?;
        paths.security.verify_target(&paths.state)?;
        let previous_state = InstallState::load(&paths.state)
            .map_err(|source| InstallError::io("read installation state", &paths.state, source))?;
        let identity = InstallIdentity {
            manifest_sha256: digest_sha256(&manifest_bytes),
            loader_plan_sha256: digest_serialized(&plan.loader)?,
            java_executable: plan.java_executable.to_string_lossy().into_owned(),
        };
        let reusable_loader = reusable_loader(
            previous_state.as_ref(),
            &identity.loader_plan_sha256,
            &identity.java_executable,
            &paths.security,
            &plan.loader.output,
        );

        let installer_sha1 =
            download_required(&self.engine, &paths, plan, reusable_loader, &observer)?;
        session.check_log()?;

        emit(
            &observer,
            InstallEvent::Stage(InstallStage::CheckingManualFiles),
        )?;
        enforce_manual_files(&paths.security, &plan.files, plan.language)?;

        emit(
            &observer,
            InstallEvent::Stage(InstallStage::InstallingLoader),
        )?;
        let (launch, loader_reused) = run_loader(
            &self.loader,
            &paths,
            plan,
            reusable_loader,
            installer_sha1.as_deref(),
            &observer,
        )?;
        session.check_log()?;

        emit(&observer, InstallEvent::Stage(InstallStage::WritingState))?;
        write_script_and_state(
            &paths,
            plan,
            previous_state.as_ref(),
            &identity,
            &launch,
            &observer,
        )?;
        session.emit(InstallEvent::Stage(InstallStage::Completed))?;
        Ok(InstallResult {
            server_root: paths.security.path().to_path_buf(),
            launch,
            loader_reused,
        })
    }
}

struct InstallPaths {
    security: InstallRoot,
    state: PathBuf,
    staging: PathBuf,
    installer: PathBuf,
}

impl InstallPaths {
    fn create(security: InstallRoot, plan: &InstallPlan) -> Result<Self, InstallError> {
        let staging = security.create_directory(Path::new(".mcsdt/staging"))?;
        let installers = security.create_directory(Path::new(".mcsdt/installers"))?;
        let state = security.resolve(Path::new(".mcsdt/install-state.json"))?;
        let installer = installers.join(&plan.loader.installer.file_name);
        security.verify_target(&installer)?;
        Ok(Self {
            security,
            state,
            staging,
            installer,
        })
    }
}

struct InstallIdentity {
    manifest_sha256: String,
    loader_plan_sha256: String,
    java_executable: String,
}

fn download_required<E: ArtifactTransfer + HttpTransport>(
    engine: &E,
    paths: &InstallPaths,
    plan: &InstallPlan,
    reusable_loader: bool,
    observer: &Arc<dyn InstallObserver>,
) -> Result<Option<String>, InstallError> {
    let mut pending = Vec::new();
    for (index, install_file) in plan.files.iter().enumerate() {
        let target = paths.security.resolve(Path::new(&install_file.path))?;
        if verify_manifest_file(&target, install_file).is_ok() {
            emit(observer, InstallEvent::Reused { target })?;
        } else if let FileDownload::Automatic { url } = &install_file.download {
            let curseforge_api_key =
                scoped_curseforge_key(install_file, plan.curseforge_api_key.as_ref());
            pending.push(PendingArtifact::manifest(
                index,
                &paths.staging,
                target,
                url,
                install_file,
                curseforge_api_key,
            )?);
        }
    }
    let installer_sha1 = if reusable_loader {
        None
    } else {
        Some(resolve_installer_sha1(engine, &plan.loader.installer.sha1)?)
    };
    if !reusable_loader
        && verify_sha1_size(
            &paths.installer,
            installer_sha1.as_deref().unwrap_or_default(),
            plan.loader.installer.size,
        )
        .is_err()
    {
        pending.push(PendingArtifact::installer(
            &paths.staging,
            paths.installer.clone(),
            &plan.loader.installer,
            installer_sha1.as_deref().unwrap_or_default(),
        )?);
    } else if !reusable_loader {
        emit(
            observer,
            InstallEvent::Reused {
                target: paths.installer.clone(),
            },
        )?;
    }
    transfer_and_publish(engine, &paths.security, &pending, observer)?;
    Ok(installer_sha1)
}

fn transfer_and_publish<E: ArtifactTransfer>(
    engine: &E,
    security: &InstallRoot,
    pending: &[PendingArtifact],
    observer: &Arc<dyn InstallObserver>,
) -> Result<(), InstallError> {
    if pending.is_empty() {
        return Ok(());
    }
    emit(observer, InstallEvent::Stage(InstallStage::Downloading))?;
    let transfer_observer: Arc<dyn TransferObserver> = Arc::new(TransferBridge {
        observer: Arc::clone(observer),
    });
    let requests = pending.iter().map(|item| item.request.clone()).collect();
    let results = engine.transfer_many(requests, transfer_observer);
    observer.check()?;
    let failures = results
        .into_iter()
        .filter_map(Result::err)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if !failures.is_empty() {
        return Err(InstallError::Transfer { failures });
    }
    for artifact in pending {
        observer.check()?;
        security.verify_existing_file(&artifact.staging)?;
        artifact.verify()?;
    }
    for artifact in pending {
        observer.check()?;
        security.verify_existing_file(&artifact.staging)?;
        security.verify_target(&artifact.target)?;
        atomic::copy(&artifact.staging, &artifact.target).map_err(|source| {
            InstallError::io(
                "atomically publish verified artifact",
                &artifact.target,
                source,
            )
        })?;
        security.verify_existing_file(&artifact.target)?;
        if let Err(error) = fs::remove_file(&artifact.staging) {
            emit(
                observer,
                InstallEvent::CleanupWarning {
                    path: artifact.staging.clone(),
                    reason: error.to_string(),
                },
            )?;
        }
    }
    Ok(())
}

fn run_loader<L: LoaderInstallation>(
    loader: &L,
    paths: &InstallPaths,
    plan: &InstallPlan,
    reusable: bool,
    installer_sha1: Option<&str>,
    observer: &Arc<dyn InstallObserver>,
) -> Result<(crate::loader::VerifiedLaunch, bool), InstallError> {
    if reusable {
        validate_loader_output_paths(&paths.security, &plan.loader.output, true)?;
        let launch = verify_loader_output(paths.security.path(), &plan.loader.output)?;
        emit(observer, InstallEvent::LoaderReused)?;
        return Ok((launch, true));
    }
    let installer_sha1 = installer_sha1.ok_or_else(|| InstallError::InvalidPlan {
        reason: "loader installation requires a resolved installer SHA-1".to_string(),
    })?;
    verify_sha1_size(&paths.installer, installer_sha1, plan.loader.installer.size).map_err(
        |reason| InstallError::Verification {
            target: paths.installer.clone(),
            reason,
        },
    )?;
    paths.security.verify_existing_file(&paths.installer)?;
    validate_loader_output_paths(&paths.security, &plan.loader.output, false)?;
    let process_observer: Arc<dyn ProcessObserver> = Arc::new(ProcessBridge {
        observer: Arc::clone(observer),
    });
    let launch = loader.install(
        &plan.loader,
        paths.security.path(),
        &plan.java_executable,
        &paths.installer,
        plan.proxy.as_ref(),
        process_observer,
    )?;
    observer.check()?;
    validate_loader_output_paths(&paths.security, &plan.loader.output, true)?;
    Ok((launch, false))
}

fn write_script_and_state(
    paths: &InstallPaths,
    plan: &InstallPlan,
    previous_state: Option<&InstallState>,
    identity: &InstallIdentity,
    launch: &crate::loader::VerifiedLaunch,
    observer: &Arc<dyn InstallObserver>,
) -> Result<(), InstallError> {
    observer.check()?;
    validate_script_paths(&paths.security, plan.script_platform)?;
    let script = write_start_script(
        paths.security.path(),
        &ScriptRequest {
            platform: plan.script_platform,
            java_executable: &plan.java_executable,
            java: &plan.java,
            launch,
            windows_failure: WindowsFailureBehavior::PauseOwnedConsole,
            previous_script_sha256: previous_state
                .map(|state| state.script_sha256.as_str())
                .filter(|hash| !hash.is_empty()),
        },
    )?;
    validate_script_paths(&paths.security, plan.script_platform)?;
    let (script_sha256, conflict) = match script {
        ScriptOutcome::Published { sha256, .. } => (sha256, None),
        ScriptOutcome::Conflict {
            existing,
            generated,
            ..
        } => (
            previous_state.map_or_else(String::new, |state| state.script_sha256.clone()),
            Some((existing, generated)),
        ),
    };
    observer.check()?;
    let state = InstallState {
        manifest_sha256: identity.manifest_sha256.clone(),
        java_executable: identity.java_executable.clone(),
        loader_plan_sha256: identity.loader_plan_sha256.clone(),
        loader_output: launch.clone(),
        script_sha256,
    };
    paths.security.verify_target(&paths.state)?;
    state.store(&paths.state).map_err(|source| {
        InstallError::io("atomically write installation state", &paths.state, source)
    })?;
    paths.security.verify_existing_file(&paths.state)?;
    if let Some((existing, generated)) = conflict {
        return Err(InstallError::ScriptConflict {
            existing,
            generated,
        });
    }
    Ok(())
}

fn validate_file_targets(files: &[ManifestFile]) -> Result<(), InstallError> {
    let mut targets = std::collections::HashSet::with_capacity(files.len());
    for entry in files {
        let target = Path::new(&entry.path);
        if entry.path.contains('\\')
            || entry.path.contains('\0')
            || target.as_os_str().is_empty()
            || target.is_absolute()
            || target
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(InstallError::InvalidPlan {
                reason: format!("'{}' is not a normalized relative target", entry.path),
            });
        }
        if !targets.insert(entry.path.to_lowercase()) {
            return Err(InstallError::InvalidPlan {
                reason: format!("duplicate target '{}'", entry.path),
            });
        }
    }
    Ok(())
}

fn validate_plan_paths(root: &InstallRoot, plan: &InstallPlan) -> Result<(), InstallError> {
    for file in &plan.files {
        root.resolve(Path::new(&file.path))?;
    }
    root.resolve(Path::new(MISSING_FILES))?;
    root.resolve(Path::new(".mcsdt/install-state.json"))?;
    root.resolve(&Path::new(".mcsdt/installers").join(&plan.loader.installer.file_name))?;
    validate_loader_output_paths(root, &plan.loader.output, false)?;
    validate_script_paths(root, plan.script_platform)
}

fn validate_loader_output_paths(
    root: &InstallRoot,
    output: &crate::loader::LoaderOutputExpectation,
    require_files: bool,
) -> Result<(), InstallError> {
    let paths = match output {
        crate::loader::LoaderOutputExpectation::ModernArgs { windows, unix } => {
            vec![windows.as_path(), unix.as_path()]
        }
        crate::loader::LoaderOutputExpectation::ExactJar { path, .. } => vec![path.as_path()],
    };
    for relative in paths {
        let path = root.resolve(relative)?;
        if require_files {
            root.verify_existing_file(&path)?;
        }
    }
    Ok(())
}

fn validate_script_paths(
    root: &InstallRoot,
    platform: crate::scripts::ScriptPlatform,
) -> Result<(), InstallError> {
    let name = match platform {
        crate::scripts::ScriptPlatform::Windows => "start.bat",
        crate::scripts::ScriptPlatform::Unix => "start.sh",
    };
    root.resolve(Path::new(name))?;
    root.resolve(Path::new(&format!("{name}.new")))?;
    Ok(())
}

fn reusable_loader(
    state: Option<&InstallState>,
    loader_plan_sha256: &str,
    java_identity: &str,
    root: &InstallRoot,
    expected: &crate::loader::LoaderOutputExpectation,
) -> bool {
    state.is_some_and(|state| {
        state.loader_plan_sha256 == loader_plan_sha256
            && state.java_executable == java_identity
            && validate_loader_output_paths(root, expected, true).is_ok()
            && verify_loader_output(root.path(), expected)
                .is_ok_and(|launch| launch == state.loader_output)
    })
}

fn enforce_manual_files(
    root: &InstallRoot,
    files: &[ManifestFile],
    language: crate::i18n::Language,
) -> Result<(), InstallError> {
    let missing = files
        .iter()
        .filter(|entry| matches!(entry.download, FileDownload::Manual))
        .filter(|entry| {
            root.resolve(Path::new(&entry.path))
                .and_then(|path| {
                    root.verify_existing_file(&path)?;
                    verify_manifest_file(&path, entry).map_err(|reason| {
                        InstallError::Verification {
                            target: path,
                            reason,
                        }
                    })
                })
                .is_err()
        })
        .map(Clone::clone)
        .collect::<Vec<_>>();
    let list_path = root.resolve(Path::new(MISSING_FILES))?;
    if missing.is_empty() {
        root.verify_target(&list_path)?;
        match fs::remove_file(&list_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(InstallError::io(
                    "remove stale missing-file list",
                    &list_path,
                    source,
                ));
            }
        }
        return Ok(());
    }
    let body = crate::i18n::Localizer::new(language).manual_file_report(&missing);
    root.verify_target(&list_path)?;
    atomic::write(&list_path, body.as_bytes()).map_err(|source| {
        InstallError::io("atomically write missing-file list", &list_path, source)
    })?;
    root.verify_existing_file(&list_path)?;
    Err(InstallError::ManualFilesMissing {
        count: missing.len(),
        list_path,
    })
}

fn scoped_curseforge_key<'a>(
    file: &ManifestFile,
    key: Option<&'a crate::manifest::SecretString>,
) -> Option<&'a crate::manifest::SecretString> {
    file.download
        .requires_curseforge_key()
        .then_some(key)
        .flatten()
}

#[derive(Debug)]
struct PendingArtifact {
    request: ArtifactRequest,
    staging: PathBuf,
    target: PathBuf,
    expected_size: Option<u64>,
    hash: ExpectedHash,
}

#[derive(Debug)]
enum ExpectedHash {
    Sha1(String),
}

impl PendingArtifact {
    fn manifest(
        index: usize,
        staging_root: &Path,
        target: PathBuf,
        url: &str,
        file: &ManifestFile,
        curseforge_api_key: Option<&crate::manifest::SecretString>,
    ) -> Result<Self, InstallError> {
        let staging = staging_root.join(format!("file-{index}.download"));
        let task_id = format!("manifest-file-{index}");
        let mut builder = ArtifactRequest::builder(&task_id, &staging, url);
        builder = builder.expected_size(file.size).expected_sha1(&file.sha1);
        if let Some(key) = curseforge_api_key {
            let sensitive = SensitiveHeaders::new().allow_origin(url)?.insert(
                HeaderName::from_static("x-api-key"),
                HeaderValue::from_str(key.expose()).map_err(|_| {
                    NetworkError::InvalidConfiguration {
                        reason: "CurseForge API key is not a valid HTTP header value".to_string(),
                    }
                })?,
            )?;
            builder = builder.sensitive_headers(sensitive);
        }
        Ok(Self {
            request: builder.build()?,
            staging,
            target,
            expected_size: Some(file.size),
            hash: ExpectedHash::Sha1(file.sha1.clone()),
        })
    }

    fn installer(
        staging_root: &Path,
        target: PathBuf,
        installer: &crate::loader::InstallerArtifact,
        sha1: &str,
    ) -> Result<Self, InstallError> {
        let staging = staging_root.join("loader-installer.download");
        let mut builder = ArtifactRequest::builder("loader-installer", &staging, &installer.url)
            .expected_sha1(sha1);
        if let Some(size) = installer.size {
            builder = builder.expected_size(size);
        }
        Ok(Self {
            request: builder.build()?,
            staging,
            target,
            expected_size: installer.size,
            hash: ExpectedHash::Sha1(sha1.to_string()),
        })
    }

    fn verify(&self) -> Result<(), InstallError> {
        verify_path(&self.staging, self.expected_size, &self.hash).map_err(|reason| {
            InstallError::Verification {
                target: self.target.clone(),
                reason,
            }
        })
    }
}

fn verify_manifest_file(path: &Path, file: &ManifestFile) -> Result<(), String> {
    verify_path(
        path,
        Some(file.size),
        &ExpectedHash::Sha1(file.sha1.clone()),
    )
}

fn verify_sha1_size(path: &Path, expected: &str, size: Option<u64>) -> Result<(), String> {
    verify_path(path, size, &ExpectedHash::Sha1(expected.to_string()))
}

fn verify_path(path: &Path, expected_size: Option<u64>, hash: &ExpectedHash) -> Result<(), String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("target is not a regular file".to_string());
    }
    if expected_size.is_some_and(|expected| expected != metadata.len()) {
        return Err(format!(
            "size is {}, expected {}",
            metadata.len(),
            expected_size.unwrap_or_default()
        ));
    }
    let (actual, expected) = match hash {
        ExpectedHash::Sha1(expected) => (stream_digest::<Sha1>(&mut file)?, expected),
    };
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!("digest is {actual}, expected {expected}"));
    }
    Ok(())
}

fn stream_digest<D: Digest + Default>(reader: &mut impl Read) -> Result<String, String> {
    let mut digest = D::default();
    let mut buffer = vec![0_u8; 256 * 1024].into_boxed_slice();
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let output = digest.finalize();
    let mut encoded = String::with_capacity(output.len() * 2);
    for byte in output {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(encoded)
}

fn resolve_installer_sha1<E: HttpTransport>(
    engine: &E,
    source: &InstallerSha1,
) -> Result<String, InstallError> {
    match source {
        InstallerSha1::Inline { value } => Ok(value.to_ascii_lowercase()),
        InstallerSha1::Sidecar { url } => {
            let response =
                engine.get_bytes(HttpRequest::get(url, MAX_SHA1_SIDECAR_RESPONSE).build()?)?;
            let body = std::str::from_utf8(&response.body).map_err(|error| {
                InstallError::InvalidInstallerSha1 {
                    url: redact_url(url),
                    reason: error.to_string(),
                }
            })?;
            let digest = body.trim();
            if digest.len() != 40 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(InstallError::InvalidInstallerSha1 {
                    url: redact_url(url),
                    reason: "response must contain only one 40-character hexadecimal SHA-1"
                        .to_string(),
                });
            }
            Ok(digest.to_ascii_lowercase())
        }
    }
}

fn redact_url(value: &str) -> String {
    reqwest::Url::parse(value).map_or_else(
        |_| "<invalid-url>".to_string(),
        |mut url| {
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        },
    )
}

fn digest_serialized(value: &impl serde::Serialize) -> Result<String, InstallError> {
    let bytes = serde_json::to_vec(value).map_err(|error| InstallError::Verification {
        target: PathBuf::from("<loader-plan>"),
        reason: error.to_string(),
    })?;
    Ok(digest_sha256(&bytes))
}

fn digest_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn emit(observer: &Arc<dyn InstallObserver>, event: InstallEvent) -> Result<(), InstallError> {
    observer.observe(event)?;
    Ok(())
}

struct TransferBridge {
    observer: Arc<dyn InstallObserver>,
}

impl TransferObserver for TransferBridge {
    fn observe(&self, event: TransferEvent) -> Result<(), TransferObserverError> {
        self.observer
            .observe(InstallEvent::Transfer(event))
            .map_err(|error| TransferObserverError::new(error.to_string()))
    }
}

struct ProcessBridge {
    observer: Arc<dyn InstallObserver>,
}

impl ProcessObserver for ProcessBridge {
    fn line(&self, stream: ProcessStream, line: String) -> Result<(), ProcessObserverError> {
        self.observer
            .observe(InstallEvent::LoaderOutput { stream, line })
            .map_err(|error| ProcessObserverError::new(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{FileKind, SecretString};

    fn file(url: &str) -> ManifestFile {
        ManifestFile {
            name: "credential scope".to_string(),
            kind: FileKind::Mod,
            path: "mods/scope.jar".to_string(),
            download: FileDownload::Automatic {
                url: url.to_string(),
            },
            project_page: "https://example.com/project".to_string(),
            sha1: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            size: 1,
        }
    }

    #[test]
    fn curseforge_key_is_scoped_only_to_curseforge_cdn_files() {
        let key: SecretString = serde_json::from_str("\"sensitive-key\"").unwrap();

        assert!(
            scoped_curseforge_key(
                &file("https://edge.forgecdn.net/files/1/2/mod.jar"),
                Some(&key),
            )
            .is_some()
        );
        assert!(
            scoped_curseforge_key(&file("https://downloads.example.com/mod.jar"), Some(&key),)
                .is_none()
        );
    }
}

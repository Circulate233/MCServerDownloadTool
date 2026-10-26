use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::loader::{
    InstallerSha1, LoaderInstallation, ProcessObserver, ProcessObserverError, ProcessStream,
    verify_loader_output,
};
use crate::manifest::{FileDownload, ManifestFile};
use crate::net::{
    ArtifactRequest, ArtifactTransfer, HttpRequest, HttpTransport, NetworkError, SensitiveHeaders,
    TransferError, TransferEvent, TransferObserver, TransferObserverError,
};
use crate::scripts::{ScriptOutcome, ScriptRequest, WindowsFailureBehavior, write_start_script};

use super::atomic;
use super::model::{
    InstallError, InstallEvent, InstallObserver, InstallPlan, InstallResult, InstallStage,
};
use super::state::{InstallState, InstalledArtifact};
use super::{DownloadTarget, InstallRoot, InstallSession};

const MISSING_FILES: &str = "missing-files.txt";
const MAX_SHA1_SIDECAR_RESPONSE: usize = 1024;
const MAX_LOADER_INSTALLER_BYTES: u64 = 512 * 1024 * 1024;
const MAX_INSTALLER_VERSION_JSON_BYTES: u64 = 16 * 1024 * 1024;
const MAX_INSTALLER_LIBRARY_COUNT: usize = 4096;
const MAX_INSTALLER_LIBRARY_BYTES: u64 = 512 * 1024 * 1024;
// Keep the batch at the transfer worker ceiling so a loader with hundreds of
// small Maven artifacts can fill the shared request budget. The network engine
// still applies its global, per-host, and per-file limits.
const INSTALLER_LIBRARY_BATCH_SIZE: usize = 32;
static DOWNLOAD_TARGET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

        let (installer_sha1, unavailable_files) =
            download_required(&self.engine, &paths, plan, reusable_loader, &observer)?;
        session.check_log()?;

        emit(
            &observer,
            InstallEvent::Stage(InstallStage::CheckingManualFiles),
        )?;
        enforce_manual_files(
            &paths.security,
            &plan.files,
            &unavailable_files,
            &observer,
            plan.language,
        )?;

        if !reusable_loader {
            predownload_installer_libraries(&self.engine, &paths, plan, &observer)?;
        }

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
        let loader_artifacts = loader_artifact_digests(&paths.security, &plan.loader.output)?;
        session.check_log()?;

        emit(&observer, InstallEvent::Stage(InstallStage::WritingState))?;
        write_script_and_state(
            &paths,
            plan,
            previous_state.as_ref(),
            &identity,
            &launch,
            &loader_artifacts,
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
    installer: PathBuf,
}

impl InstallPaths {
    fn create(security: InstallRoot, plan: &InstallPlan) -> Result<Self, InstallError> {
        let installers = security.create_directory(Path::new(".mcsdt/installers"))?;
        let state = security.resolve(Path::new(".mcsdt/install-state.json"))?;
        let installer = installers.join(&plan.loader.installer.file_name);
        security.verify_target(&installer)?;
        Ok(Self {
            security,
            state,
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
) -> Result<(Option<String>, Vec<usize>), InstallError> {
    let mut pending = Vec::new();
    for (index, install_file) in plan.files.iter().enumerate() {
        let target = paths.security.resolve(Path::new(&install_file.path))?;
        create_target_parent(&paths.security, &target)?;
        if verify_manifest_file(&target, install_file).is_ok() {
            emit(observer, InstallEvent::Reused { target })?;
        } else if let FileDownload::Automatic { url } = &install_file.download {
            let download_target = paths.security.download_target(
                &target,
                DOWNLOAD_TARGET_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            )?;
            let curseforge_api_key =
                scoped_curseforge_key(install_file, plan.curseforge_api_key.as_ref());
            pending.push(PendingArtifact::manifest(
                index,
                download_target,
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
        let download_target = paths.security.download_target(
            &paths.installer,
            DOWNLOAD_TARGET_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        )?;
        pending.push(PendingArtifact::installer(
            download_target,
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
    let unavailable_files = transfer_and_publish(engine, &pending, observer)?;
    Ok((installer_sha1, unavailable_files))
}

fn transfer_and_publish<E: ArtifactTransfer>(
    engine: &E,
    pending: &[PendingArtifact],
    observer: &Arc<dyn InstallObserver>,
) -> Result<Vec<usize>, InstallError> {
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    emit(observer, InstallEvent::Stage(InstallStage::Downloading))?;
    let transfer_observer: Arc<dyn TransferObserver> = Arc::new(TransferBridge {
        observer: Arc::clone(observer),
    });
    let mut round = pending.to_vec();
    let mut unavailable_files = Vec::new();
    let mut failures = Vec::new();
    while !round.is_empty() {
        let requests = round.iter().map(|item| item.request.clone()).collect();
        let results = engine.transfer_many(requests, Arc::clone(&transfer_observer));
        observer.check()?;
        let round_size = round.len();
        let mut retry = Vec::new();
        for (artifact, result) in round.into_iter().zip(results) {
            match result {
                Ok(_) => {
                    observer.check()?;
                    artifact.verify_staging()?;
                    artifact.publish()?;
                }
                Err(error) if is_curseforge_unavailable(&error) => {
                    if let Some(index) = artifact.manual_file_index {
                        unavailable_files.push(index);
                    } else {
                        failures.push(error.to_string());
                    }
                }
                Err(TransferError::Cancelled { .. }) => retry.push(artifact),
                Err(error) => failures.push(error.to_string()),
            }
        }
        if !retry.is_empty() && retry.len() == round_size {
            failures.extend(retry.into_iter().map(|artifact| {
                format!(
                    "artifact task '{}' was cancelled without a reported cause",
                    artifact.request.task_id()
                )
            }));
            break;
        }
        round = retry;
    }
    if !failures.is_empty() {
        return Err(InstallError::Transfer { failures });
    }
    Ok(unavailable_files)
}

/// Downloads dependencies declared by the loader's embedded `version.json` before
/// starting its Java installer. Most Forge-family installers then find these
/// verified files in `libraries/` and skip their own one-at-a-time downloads.
/// Unsupported installer formats and individual transfer failures are warnings:
/// the official installer remains the fallback for anything not prepared here.
fn predownload_installer_libraries<E: ArtifactTransfer>(
    engine: &E,
    paths: &InstallPaths,
    plan: &InstallPlan,
    observer: &Arc<dyn InstallObserver>,
) -> Result<(), InstallError> {
    paths.security.verify_existing_file(&paths.installer)?;
    let preseed =
        match read_installer_preseed_items(&paths.installer, &plan.loader.minecraft_version) {
            Ok(preseed) => preseed,
            Err(reason) => {
                emit(
                    observer,
                    InstallEvent::LoaderLibraryPreseedWarning { detail: reason },
                )?;
                return Ok(());
            }
        };
    if preseed.items.is_empty() {
        emit(
            observer,
            InstallEvent::LoaderLibraryPreseedWarning {
                detail: "the installer has no supported downloadable dependencies in version.json"
                    .to_string(),
            },
        )?;
        return Ok(());
    }

    let profile_skipped = preseed.skipped;
    let preparation = prepare_installer_libraries(paths, plan, preseed, observer)?;
    if !preparation.pending.is_empty() {
        emit(
            observer,
            InstallEvent::Stage(InstallStage::DownloadingLoaderLibraries),
        )?;
    }
    let download =
        transfer_installer_libraries(engine, observer, &preparation.pending, preparation.prepared)?;

    if download.failures > 0 || profile_skipped > 0 || preparation.skipped_conflicts > 0 {
        let remaining = preparation.total.saturating_sub(download.prepared);
        let mut detail = format!(
            "prepared {} of {} declared dependency artifact(s); {remaining} remain",
            download.prepared, preparation.total
        );
        if profile_skipped > 0 || preparation.skipped_conflicts > 0 {
            let _ = write!(
                detail,
                "; {} unsupported or conflicting entry/entries were left to the installer",
                profile_skipped + preparation.skipped_conflicts
            );
        }
        if let Some(reason) = download.first_failure {
            let _ = write!(detail, "; first transfer failure: {reason}");
        }
        emit(
            observer,
            InstallEvent::LoaderLibraryPreseedWarning { detail },
        )?;
    } else {
        emit(
            observer,
            InstallEvent::LoaderLibrariesPrepared {
                count: download.prepared,
            },
        )?;
    }
    Ok(())
}

struct InstallerLibraryPreparation {
    total: usize,
    prepared: usize,
    skipped_conflicts: usize,
    pending: Vec<PendingArtifact>,
}

fn prepare_installer_libraries(
    paths: &InstallPaths,
    plan: &InstallPlan,
    preseed: InstallerPreseedItems,
    observer: &Arc<dyn InstallObserver>,
) -> Result<InstallerLibraryPreparation, InstallError> {
    let total = preseed.items.len();
    let mut prepared = 0_usize;
    let mut skipped_conflicts = 0_usize;
    let mut pending = Vec::new();
    for item in preseed.items {
        let relative = Path::new(&item.path);
        if conflicts_with_manifest_or_output(plan, relative) {
            skipped_conflicts += 1;
            continue;
        }
        let target = paths.security.resolve(relative)?;
        create_target_parent(&paths.security, &target)?;
        match fs::symlink_metadata(&target) {
            Ok(_) => {
                paths.security.verify_existing_file(&target)?;
                if verify_sha1_size(&target, &item.sha1, item.size).is_ok() {
                    prepared += 1;
                    emit(
                        observer,
                        InstallEvent::Reused {
                            target: target.clone(),
                        },
                    )?;
                    continue;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(InstallError::io("inspect loader library", &target, source));
            }
        }
        let download_target = paths.security.download_target(
            &target,
            DOWNLOAD_TARGET_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        )?;
        pending.push(PendingArtifact::loader_library(download_target, &item)?);
    }
    Ok(InstallerLibraryPreparation {
        total,
        prepared,
        skipped_conflicts,
        pending,
    })
}

struct InstallerLibraryDownload {
    prepared: usize,
    failures: usize,
    first_failure: Option<String>,
}

fn transfer_installer_libraries<E: ArtifactTransfer>(
    engine: &E,
    observer: &Arc<dyn InstallObserver>,
    pending: &[PendingArtifact],
    mut prepared: usize,
) -> Result<InstallerLibraryDownload, InstallError> {
    let transfer_observer: Arc<dyn TransferObserver> = Arc::new(TransferBridge {
        observer: Arc::clone(observer),
    });
    let mut failures = 0_usize;
    let mut first_failure = None;
    for chunk in pending.chunks(INSTALLER_LIBRARY_BATCH_SIZE) {
        let mut round = chunk.to_vec();
        while !round.is_empty() {
            let round_size = round.len();
            let requests = round
                .iter()
                .map(|artifact| artifact.request.clone())
                .collect();
            let results = engine.transfer_many(requests, Arc::clone(&transfer_observer));
            observer.check()?;
            if results.len() != round_size {
                return Err(InstallError::Transfer {
                    failures: vec![format!(
                        "loader-library downloader returned {} result(s) for {round_size} request(s)",
                        results.len()
                    )],
                });
            }
            let mut retry = Vec::new();
            for (artifact, result) in round.into_iter().zip(results) {
                match result {
                    Ok(_) => {
                        observer.check()?;
                        match artifact.verify_staging() {
                            Ok(()) => {
                                artifact.publish()?;
                                prepared += 1;
                            }
                            Err(InstallError::Verification { reason, .. }) => {
                                failures += 1;
                                first_failure.get_or_insert(reason);
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    Err(TransferError::Cancelled { .. }) => retry.push(artifact),
                    Err(error) => {
                        failures += 1;
                        first_failure.get_or_insert_with(|| error.to_string());
                    }
                }
            }
            if !retry.is_empty() && retry.len() == round_size {
                failures += retry.len();
                first_failure.get_or_insert_with(|| {
                    "all loader-library requests were cancelled without a reported cause"
                        .to_string()
                });
                break;
            }
            round = retry;
        }
    }
    Ok(InstallerLibraryDownload {
        prepared,
        failures,
        first_failure,
    })
}

fn conflicts_with_manifest_or_output(plan: &InstallPlan, target: &Path) -> bool {
    let normalized = target
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    plan.files
        .iter()
        .any(|file| file.path.replace('\\', "/").to_ascii_lowercase() == normalized)
        || match &plan.loader.output {
            crate::loader::LoaderOutputExpectation::ModernArgs { windows, unix } => {
                windows == target || unix == target
            }
            crate::loader::LoaderOutputExpectation::ExactJar { path, .. } => path == target,
        }
}

#[derive(Debug)]
struct InstallerPreseedItems {
    items: Vec<InstallerLibrary>,
    skipped: usize,
}

#[derive(Debug, Clone)]
struct InstallerLibrary {
    task_id: String,
    path: String,
    url: String,
    sha1: String,
    size: Option<u64>,
}

/// Reads verified-download metadata from the same profile used by loader installers.
/// Paths and URLs are treated as untrusted even though the installer itself has
/// already passed its manifest checksum.
fn read_installer_preseed_items(
    installer: &Path,
    minecraft_version: &str,
) -> Result<InstallerPreseedItems, String> {
    let file = File::open(installer)
        .map_err(|error| format!("could not open installer archive: {error}"))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| format!("installer is not a readable ZIP archive: {error}"))?;
    let mut items = Vec::new();
    let mut skipped = 0_usize;
    let mut declared = 0_usize;
    let mut paths = BTreeMap::<String, String>::new();

    let version: InstallerVersionJson =
        read_installer_json(&mut archive, "version.json", true, "version.json")?
            .expect("required installer version.json must be present");
    append_installer_profile_libraries(
        version.libraries,
        &mut items,
        &mut skipped,
        &mut declared,
        &mut paths,
    )?;

    append_minecraft_download(
        version.downloads.client,
        format!("versions/{minecraft_version}/{minecraft_version}.jar"),
        "minecraft-client-jar",
        &mut items,
        &mut skipped,
        &mut declared,
        &mut paths,
    )?;
    append_minecraft_download(
        version.downloads.server,
        "server.jar".to_string(),
        "minecraft-server-jar",
        &mut items,
        &mut skipped,
        &mut declared,
        &mut paths,
    )?;

    // Forge-family installers keep processor-only dependencies (for example
    // Cleanroom's mcp_config ZIP) in install_profile.json rather than the
    // Minecraft version profile. Treat this file as optional so Fabric and
    // other installers that only ship version.json continue to work.
    if let Some(install_profile) = read_installer_json::<InstallerInstallProfile>(
        &mut archive,
        "install_profile.json",
        false,
        "install_profile.json",
    )? {
        append_installer_profile_libraries(
            install_profile.libraries,
            &mut items,
            &mut skipped,
            &mut declared,
            &mut paths,
        )?;
    }
    Ok(InstallerPreseedItems { items, skipped })
}

fn read_installer_json<T: DeserializeOwned>(
    archive: &mut zip::ZipArchive<File>,
    name: &str,
    required: bool,
    description: &str,
) -> Result<Option<T>, String> {
    let entry = match archive.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) if !required => return Ok(None),
        Err(error) => {
            return Err(if required {
                format!("installer does not contain a readable {description}: {error}")
            } else {
                format!("could not open optional {description}: {error}")
            });
        }
    };
    if entry.size() > MAX_INSTALLER_VERSION_JSON_BYTES {
        return Err(format!(
            "embedded {description} exceeds the {MAX_INSTALLER_VERSION_JSON_BYTES}-byte limit"
        ));
    }
    let mut bytes = Vec::new();
    entry
        .take(MAX_INSTALLER_VERSION_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read embedded {description}: {error}"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_INSTALLER_VERSION_JSON_BYTES {
        return Err(format!(
            "embedded {description} exceeds the {MAX_INSTALLER_VERSION_JSON_BYTES}-byte limit"
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("embedded {description} is invalid: {error}"))
}

fn append_installer_profile_libraries(
    libraries: Vec<InstallerVersionLibrary>,
    items: &mut Vec<InstallerLibrary>,
    skipped: &mut usize,
    declared: &mut usize,
    paths: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    for library in libraries {
        append_installer_library_artifact(
            &library.name,
            library.downloads.artifact,
            None,
            items,
            skipped,
            declared,
            paths,
        )?;
        for (classifier, artifact) in library.downloads.classifiers {
            append_installer_library_artifact(
                &library.name,
                Some(artifact),
                Some(&classifier),
                items,
                skipped,
                declared,
                paths,
            )?;
        }
    }
    Ok(())
}

fn append_installer_library_artifact(
    coordinate: &str,
    artifact: Option<InstallerArtifactEntry>,
    classifier: Option<&str>,
    items: &mut Vec<InstallerLibrary>,
    skipped: &mut usize,
    declared: &mut usize,
    paths: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    *declared = (*declared).saturating_add(1);
    if *declared > MAX_INSTALLER_LIBRARY_COUNT {
        return Err(format!(
            "installer profile declares more than {MAX_INSTALLER_LIBRARY_COUNT} library entries"
        ));
    }
    let Some(artifact) = artifact else {
        *skipped = skipped.saturating_add(1);
        return Ok(());
    };
    let coordinate = classifier.map_or_else(
        || coordinate.to_string(),
        |classifier| maven_coordinate_with_classifier(coordinate, classifier),
    );
    let Some(item) = installer_library_from_artifact(
        &coordinate,
        artifact,
        format!("loader-library-{}", items.len()),
        "libraries",
    ) else {
        *skipped = skipped.saturating_add(1);
        return Ok(());
    };
    if register_installer_library(paths, &item)? {
        items.push(item);
    }
    Ok(())
}

fn append_minecraft_download(
    artifact: Option<InstallerArtifactEntry>,
    path: String,
    task_id: &str,
    items: &mut Vec<InstallerLibrary>,
    skipped: &mut usize,
    declared: &mut usize,
    paths: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let Some(mut artifact) = artifact else {
        return Ok(());
    };
    *declared = (*declared).saturating_add(1);
    if *declared > MAX_INSTALLER_LIBRARY_COUNT {
        return Err(format!(
            "installer profile declares more than {MAX_INSTALLER_LIBRARY_COUNT} library entries"
        ));
    }
    artifact.path = Some(path);
    let Some(item) =
        installer_library_from_artifact("minecraft-artifact", artifact, task_id.to_string(), "")
    else {
        *skipped = skipped.saturating_add(1);
        return Ok(());
    };
    if register_installer_library(paths, &item)? {
        items.push(item);
    }
    Ok(())
}

fn register_installer_library(
    paths: &mut BTreeMap<String, String>,
    item: &InstallerLibrary,
) -> Result<bool, String> {
    let identity = item.path.to_ascii_lowercase();
    if let Some(previous_sha1) = paths.get(&identity) {
        if previous_sha1 == &item.sha1 {
            return Ok(false);
        }
        return Err(format!(
            "embedded profile assigns conflicting checksums to '{}'",
            item.path
        ));
    }
    paths.insert(identity, item.sha1.clone());
    Ok(true)
}

fn installer_library_from_artifact(
    coordinate: &str,
    artifact: InstallerArtifactEntry,
    task_id: String,
    target_prefix: &str,
) -> Option<InstallerLibrary> {
    let path = artifact
        .path
        .filter(|value| !value.trim().is_empty())
        .or_else(|| maven_path_from_coordinate(coordinate))?;
    let path = normalize_installer_library_path(&path)?;
    let url = artifact.url.filter(|value| !value.trim().is_empty())?;
    if url.len() > 8192 {
        return None;
    }
    let parsed = reqwest::Url::parse(&url).ok()?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let sha1 = artifact
        .sha1
        .filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))?;
    let size = artifact.size;
    if size.is_some_and(|size| size == 0 || size > MAX_INSTALLER_LIBRARY_BYTES) {
        return None;
    }
    let target_path = if target_prefix.is_empty() {
        path
    } else {
        format!("{target_prefix}/{path}")
    };
    Some(InstallerLibrary {
        task_id,
        path: target_path,
        url,
        sha1: sha1.to_ascii_lowercase(),
        size,
    })
}

fn maven_path_from_coordinate(coordinate: &str) -> Option<String> {
    let (coordinate, extension) = coordinate
        .rsplit_once('@')
        .map_or((coordinate, "jar"), |(left, right)| (left, right));
    let parts = coordinate.split(':').collect::<Vec<_>>();
    if !(3..=4).contains(&parts.len())
        || parts.iter().any(|part| !valid_maven_segment(part))
        || !valid_maven_segment(extension)
        || parts[0].split('.').any(str::is_empty)
    {
        return None;
    }
    let classifier = parts
        .get(3)
        .map_or_else(String::new, |value| format!("-{value}"));
    Some(format!(
        "{}/{}/{}/{}-{}{}.{}",
        parts[0].replace('.', "/"),
        parts[1],
        parts[2],
        parts[1],
        parts[2],
        classifier,
        extension
    ))
}

fn maven_coordinate_with_classifier(coordinate: &str, classifier: &str) -> String {
    coordinate.rsplit_once('@').map_or_else(
        || format!("{coordinate}:{classifier}"),
        |(base, extension)| format!("{base}:{classifier}@{extension}"),
    )
}

fn valid_maven_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
}

fn normalize_installer_library_path(path: &str) -> Option<String> {
    if path.len() > 4096 || path.contains('\0') {
        return None;
    }
    let normalized = path.replace('\\', "/");
    let relative = normalized.strip_prefix("libraries/").unwrap_or(&normalized);
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains(':')
        || relative
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return None;
    }
    Some(relative.to_string())
}

fn installer_library_mirror(url: &str) -> Option<String> {
    const REPOSITORY_MIRRORS: &[(&str, &str)] = &[
        (
            "https://repo.maven.apache.org/maven2",
            "https://maven.aliyun.com/repository/public",
        ),
        (
            "https://repo.cleanroommc.com/releases",
            "https://nexus.jsdu.cn/repository",
        ),
        (
            "https://maven.arcseekers.com/releases",
            "https://nexus.jsdu.cn/repository",
        ),
    ];
    REPOSITORY_MIRRORS
        .iter()
        .find_map(|(from, to)| url.strip_prefix(from).map(|suffix| format!("{to}{suffix}")))
}

#[derive(Debug, Deserialize)]
struct InstallerVersionJson {
    #[serde(default)]
    libraries: Vec<InstallerVersionLibrary>,
    #[serde(default)]
    downloads: InstallerVersionDownloads,
}

#[derive(Debug, Default, Deserialize)]
struct InstallerVersionDownloads {
    client: Option<InstallerArtifactEntry>,
    server: Option<InstallerArtifactEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct InstallerInstallProfile {
    #[serde(default)]
    libraries: Vec<InstallerVersionLibrary>,
}

#[derive(Debug, Deserialize)]
struct InstallerVersionLibrary {
    #[serde(default)]
    name: String,
    #[serde(default)]
    downloads: InstallerLibraryDownloads,
}

#[derive(Debug, Default, Deserialize)]
struct InstallerLibraryDownloads {
    artifact: Option<InstallerArtifactEntry>,
    #[serde(default)]
    classifiers: BTreeMap<String, InstallerArtifactEntry>,
}

#[derive(Debug, Deserialize)]
struct InstallerArtifactEntry {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    sha1: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

fn create_target_parent(root: &InstallRoot, target: &Path) -> Result<(), InstallError> {
    let relative = target
        .strip_prefix(root.path())
        .map_err(|_| InstallError::UnsafePath {
            path: target.to_path_buf(),
            reason: "artifact target is outside the installation root".to_string(),
        })?;
    if let Some(parent) = relative
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        root.create_directory(parent)?;
    }
    root.verify_target(target)
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
    loader_artifacts: &[InstalledArtifact],
    observer: &Arc<dyn InstallObserver>,
) -> Result<(), InstallError> {
    observer.check()?;
    validate_script_paths(&paths.security, plan.script_platform)?;
    let script = write_start_script(
        paths.security.path(),
        &ScriptRequest {
            platform: plan.script_platform,
            java_executable: &plan.java_executable,
            console_helper_executable: &plan.console_helper_executable,
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
        loader_artifacts: loader_artifacts.to_vec(),
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
            && loader_artifact_digests(root, expected)
                .is_ok_and(|artifacts| artifacts == state.loader_artifacts)
    })
}

fn loader_artifact_digests(
    root: &InstallRoot,
    expected: &crate::loader::LoaderOutputExpectation,
) -> Result<Vec<InstalledArtifact>, InstallError> {
    let relative_paths = match expected {
        crate::loader::LoaderOutputExpectation::ModernArgs { windows, unix } => {
            vec![windows.as_path(), unix.as_path()]
        }
        crate::loader::LoaderOutputExpectation::ExactJar { path, .. } => vec![path.as_path()],
    };
    let mut artifacts = Vec::with_capacity(relative_paths.len());
    for relative in relative_paths {
        let full = root.resolve(relative)?;
        root.verify_existing_file(&full)?;
        let mut file = File::open(&full)
            .map_err(|source| InstallError::io("open loader output for hashing", &full, source))?;
        let metadata = file.metadata().map_err(|source| {
            InstallError::io("inspect loader output for hashing", &full, source)
        })?;
        let mut hasher = Sha256::new();
        let size = std::io::copy(&mut file, &mut DigestWriter(&mut hasher))
            .map_err(|source| InstallError::io("hash loader output", &full, source))?;
        if size != metadata.len() {
            return Err(InstallError::Verification {
                target: full,
                reason: format!(
                    "file size changed while hashing: {} -> {size}",
                    metadata.len()
                ),
            });
        }
        artifacts.push(InstalledArtifact {
            path: relative.to_path_buf(),
            size,
            sha256: format!("{:x}", hasher.finalize()),
        });
    }
    Ok(artifacts)
}

struct DigestWriter<'a>(&'a mut Sha256);

impl std::io::Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn enforce_manual_files(
    root: &InstallRoot,
    files: &[ManifestFile],
    unavailable_files: &[usize],
    observer: &Arc<dyn InstallObserver>,
    language: crate::i18n::Language,
) -> Result<(), InstallError> {
    let unavailable_files = unavailable_files.iter().copied().collect::<HashSet<_>>();
    let missing = files
        .iter()
        .enumerate()
        .filter(|(index, entry)| {
            matches!(entry.download, FileDownload::Manual) || unavailable_files.contains(index)
        })
        .map(|(_, entry)| entry)
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
    for file in &missing {
        emit(
            observer,
            InstallEvent::ManualDownloadRequired {
                name: file.name.clone(),
                path: root.resolve(Path::new(&file.path))?,
                project_page: file.project_page.clone(),
            },
        )?;
    }
    emit(
        observer,
        InstallEvent::ManualFilesPending {
            count: missing.len(),
            list_path,
        },
    )?;
    Ok(())
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

#[derive(Debug, Clone)]
struct PendingArtifact {
    request: ArtifactRequest,
    target: DownloadTarget,
    expected_size: Option<u64>,
    hash: ExpectedHash,
    manual_file_index: Option<usize>,
}

#[derive(Debug, Clone)]
enum ExpectedHash {
    Sha1(String),
}

impl PendingArtifact {
    fn manifest(
        index: usize,
        target: DownloadTarget,
        url: &str,
        file: &ManifestFile,
        curseforge_api_key: Option<&crate::manifest::SecretString>,
    ) -> Result<Self, InstallError> {
        let task_id = format!("manifest-file-{index}");
        let mut builder = ArtifactRequest::builder(&task_id, target.staging_path(), url);
        let mirror_url = forgecdn_mirror_url(url);
        if let Some(mirror_url) = &mirror_url {
            builder = builder.candidate_url(mirror_url);
        }
        builder = builder.expected_size(file.size).expected_sha1(&file.sha1);
        if let Some(key) = curseforge_api_key {
            let mut sensitive = SensitiveHeaders::new().allow_origin(url)?;
            if let Some(mirror_url) = &mirror_url {
                sensitive = sensitive.allow_origin(mirror_url)?;
            }
            let sensitive = sensitive.insert(
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
            target,
            expected_size: Some(file.size),
            hash: ExpectedHash::Sha1(file.sha1.clone()),
            manual_file_index: Some(index),
        })
    }

    fn installer(
        target: DownloadTarget,
        installer: &crate::loader::InstallerArtifact,
        sha1: &str,
    ) -> Result<Self, InstallError> {
        let mut builder =
            ArtifactRequest::builder("loader-installer", target.staging_path(), &installer.url)
                .expected_sha1(sha1)
                .maximum_size(MAX_LOADER_INSTALLER_BYTES);
        if let Some(size) = installer.size {
            builder = builder.expected_size(size);
        }
        Ok(Self {
            request: builder.build()?,
            target,
            expected_size: installer.size,
            hash: ExpectedHash::Sha1(sha1.to_string()),
            manual_file_index: None,
        })
    }

    fn loader_library(
        target: DownloadTarget,
        library: &InstallerLibrary,
    ) -> Result<Self, InstallError> {
        let mut builder =
            ArtifactRequest::builder(&library.task_id, target.staging_path(), &library.url)
                .expected_sha1(&library.sha1)
                .maximum_size(MAX_INSTALLER_LIBRARY_BYTES);
        if let Some(size) = library.size {
            builder = builder.expected_size(size);
        }
        if let Some(mirror) = installer_library_mirror(&library.url) {
            builder = builder.candidate_url(mirror);
        }
        Ok(Self {
            request: builder.build()?,
            target,
            expected_size: library.size,
            hash: ExpectedHash::Sha1(library.sha1.clone()),
            manual_file_index: None,
        })
    }

    fn verify_staging(&self) -> Result<(), InstallError> {
        verify_reader(
            self.target.open_staging().map_err(|source| {
                InstallError::io(
                    "open anchored staged artifact",
                    self.target.final_path(),
                    source,
                )
            })?,
            self.expected_size,
            &self.hash,
        )
        .map_err(|reason| InstallError::Verification {
            target: self.target.final_path().to_path_buf(),
            reason,
        })
    }

    fn publish(&self) -> Result<(), InstallError> {
        self.target.publish().map_err(|source| {
            InstallError::io(
                "atomically publish anchored verified artifact",
                self.target.final_path(),
                source,
            )
        })
    }
}

fn forgecdn_mirror_url(url: &str) -> Option<String> {
    let mut parsed = reqwest::Url::parse(url).ok()?;
    if !parsed
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("edge.forgecdn.net"))
    {
        return None;
    }
    parsed.set_host(Some("mediafilez.forgecdn.net")).ok()?;
    Some(parsed.to_string())
}

fn is_curseforge_unavailable(error: &crate::net::TransferError) -> bool {
    const UNAVAILABLE_STATUSES: [u16; 2] = [403, 404];
    match error {
        crate::net::TransferError::Network {
            source: NetworkError::HttpStatus { status, .. },
            ..
        } => UNAVAILABLE_STATUSES.contains(status),
        crate::net::TransferError::CandidatesExhausted { failures, .. } => {
            !failures.is_empty()
                && failures.iter().all(|failure| {
                    UNAVAILABLE_STATUSES
                        .iter()
                        .any(|status| failure.contains(&format!("returned status {status}")))
                })
        }
        _ => false,
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
    verify_reader(&mut file, expected_size, hash)
}

fn verify_reader(
    mut file: impl Read,
    expected_size: Option<u64>,
    hash: &ExpectedHash,
) -> Result<(), String> {
    let (actual, expected) = match hash {
        ExpectedHash::Sha1(expected) => (stream_digest::<Sha1>(&mut file)?, expected),
    };
    if expected_size.is_some_and(|expected| expected != actual.1) {
        return Err(format!(
            "size is {}, expected {}",
            actual.1,
            expected_size.unwrap_or_default()
        ));
    }
    if !actual.0.eq_ignore_ascii_case(expected) {
        return Err(format!("digest is {}, expected {expected}", actual.0));
    }
    Ok(())
}

fn stream_digest<D: Digest + Default>(reader: &mut impl Read) -> Result<(String, u64), String> {
    let mut digest = D::default();
    let mut buffer = vec![0_u8; 256 * 1024].into_boxed_slice();
    let mut size = 0_u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        digest.update(&buffer[..read]);
    }
    let output = digest.finalize();
    let mut encoded = String::with_capacity(output.len() * 2);
    for byte in output {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok((encoded, size))
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
    use std::io::Write as _;

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

    #[test]
    fn forgecdn_fallback_uses_only_the_known_curseforge_mirror() {
        assert_eq!(
            forgecdn_mirror_url("https://edge.forgecdn.net/files/1234/56/mod.jar"),
            Some("https://mediafilez.forgecdn.net/files/1234/56/mod.jar".to_string())
        );
        assert_eq!(
            forgecdn_mirror_url("https://example.com/files/1234/56/mod.jar"),
            None
        );
    }

    #[test]
    fn installer_preseed_reads_version_and_install_profile_dependencies() {
        let temp = tempfile::tempdir().unwrap();
        let installer = temp.path().join("installer.jar");
        let mut archive = zip::ZipWriter::new(File::create(&installer).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        let version = serde_json::json!({
            "libraries": [{
                "name": "com.example:library:1.0",
                "downloads": {"artifact": {
                    "url": "https://repo.example/library.jar",
                    "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "size": 10
                }, "classifiers": {
                    "natives-windows": {
                        "url": "https://repo.example/library-natives-windows.jar",
                        "sha1": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                        "size": 11
                    }
                }}
            }],
            "downloads": {
                "client": {
                    "url": "https://repo.example/client.jar",
                    "sha1": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "size": 20
                },
                "server": {
                    "url": "https://repo.example/server.jar",
                    "sha1": "cccccccccccccccccccccccccccccccccccccccc",
                    "size": 30
                }
            }
        });
        let install_profile = serde_json::json!({
            "libraries": [{
                "name": "de.oceanlabs.mcp:mcp_config:1.0@zip",
                "downloads": {"artifact": {
                    "path": "de/oceanlabs/mcp/mcp_config/1.0/mcp_config-1.0.zip",
                    "url": "https://repo.example/mcp.zip",
                    "sha1": "dddddddddddddddddddddddddddddddddddddddd",
                    "size": 40
                }}
            }]
        });
        for (name, value) in [
            ("version.json", version),
            ("install_profile.json", install_profile),
        ] {
            archive.start_file(name, options).unwrap();
            archive
                .write_all(serde_json::to_string(&value).unwrap().as_bytes())
                .unwrap();
        }
        archive.finish().unwrap();

        let preseed = read_installer_preseed_items(&installer, "1.20.1").unwrap();
        assert_eq!(preseed.skipped, 0);
        assert_eq!(preseed.items.len(), 5);
        assert!(
            preseed
                .items
                .iter()
                .any(|item| item.path == "libraries/com/example/library/1.0/library-1.0.jar")
        );
        assert!(
            preseed.items.iter().any(|item| item.path
                == "libraries/com/example/library/1.0/library-1.0-natives-windows.jar")
        );
        assert!(
            preseed
                .items
                .iter()
                .any(|item| item.path == "versions/1.20.1/1.20.1.jar")
        );
        assert!(preseed.items.iter().any(|item| item.path == "server.jar"));
        assert!(preseed.items.iter().any(
            |item| item.path == "libraries/de/oceanlabs/mcp/mcp_config/1.0/mcp_config-1.0.zip"
        ));
    }

    #[test]
    fn only_explicit_curseforge_forbidden_or_missing_statuses_are_manual() {
        for status in [403, 404] {
            assert!(is_curseforge_unavailable(
                &crate::net::TransferError::Network {
                    task_id: "mod".to_string(),
                    source: NetworkError::HttpStatus {
                        url: "https://edge.forgecdn.net/files/1/2/mod.jar".to_string(),
                        status,
                        attempts: 1,
                    },
                }
            ));
        }
        assert!(!is_curseforge_unavailable(
            &crate::net::TransferError::Network {
                task_id: "mod".to_string(),
                source: NetworkError::HttpStatus {
                    url: "https://edge.forgecdn.net/files/1/2/mod.jar".to_string(),
                    status: 500,
                    attempts: 1,
                },
            }
        ));
        assert!(!is_curseforge_unavailable(
            &crate::net::TransferError::Cancelled {
                task_id: "mod".to_string(),
            }
        ));
    }
}

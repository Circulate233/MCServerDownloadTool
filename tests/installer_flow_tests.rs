use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use mc_server_download_tool::install::{
    InstallCore, InstallError, InstallEvent, InstallObserver, InstallObserverError, InstallPlan,
    InstallSession, Installer,
};
use mc_server_download_tool::loader::{
    InstallerArtifact, LoaderError, LoaderFamily, LoaderInstallation, LoaderOutputExpectation,
    LoaderPlan, ProcessObserver, ProcessStream, VerifiedLaunch,
};
use mc_server_download_tool::manifest::{FileDownload, FileKind, JavaConfig, ManifestFile};
use mc_server_download_tool::net::{
    ArtifactOutcome, ArtifactRequest, ArtifactTransfer, DownloadMode, HttpRequest, HttpResponse,
    HttpTransport, NetworkError, TransferError, TransferObserver,
};
use mc_server_download_tool::scripts::ScriptPlatform;
use sha1::{Digest, Sha1};
use url::Url;

#[derive(Clone)]
struct FakeEngine {
    transfers: Arc<AtomicUsize>,
}

impl HttpTransport for FakeEngine {
    fn get_bytes(&self, _request: HttpRequest) -> Result<HttpResponse, NetworkError> {
        Err(NetworkError::InvalidConfiguration {
            reason: "metadata access was not expected".to_string(),
        })
    }
}

impl ArtifactTransfer for FakeEngine {
    fn transfer_many(
        &self,
        requests: Vec<ArtifactRequest>,
        _observer: Arc<dyn TransferObserver>,
    ) -> Vec<Result<ArtifactOutcome, TransferError>> {
        self.transfers.fetch_add(1, Ordering::SeqCst);
        requests
            .into_iter()
            .map(|request| {
                let bytes = if request.task_id() == "loader-installer" {
                    b"installer".as_slice()
                } else {
                    b"automatic".as_slice()
                };
                fs::create_dir_all(request.target().parent().unwrap()).unwrap();
                fs::write(request.target(), bytes).unwrap();
                Ok(ArtifactOutcome {
                    task_id: request.task_id().to_string(),
                    source_url: Url::parse("https://example.invalid/artifact").unwrap(),
                    target: request.target().to_path_buf(),
                    bytes: bytes.len() as u64,
                    mode: DownloadMode::Single,
                })
            })
            .collect()
    }
}

#[derive(Clone)]
struct FakeLoader {
    runs: Arc<AtomicUsize>,
}

struct FailOnLoaderOutput {
    failed: AtomicBool,
}

impl InstallObserver for FailOnLoaderOutput {
    fn observe(&self, event: InstallEvent) -> Result<(), InstallObserverError> {
        if self.failed.load(Ordering::Acquire) {
            return Err(InstallObserverError::terminal(
                "loader output log is unavailable",
            ));
        }
        if matches!(event, InstallEvent::LoaderOutput { .. }) {
            self.failed.store(true, Ordering::Release);
            return Err(InstallObserverError::terminal(
                "loader output log is unavailable",
            ));
        }
        Ok(())
    }

    fn check(&self) -> Result<(), InstallObserverError> {
        if self.failed.load(Ordering::Acquire) {
            Err(InstallObserverError::terminal(
                "loader output log is unavailable",
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone)]
struct OutputFailingLoader {
    runs: Arc<AtomicUsize>,
    termination_triggered: Arc<AtomicBool>,
    continued_after_output: Arc<AtomicBool>,
}

impl LoaderInstallation for OutputFailingLoader {
    fn install(
        &self,
        plan: &LoaderPlan,
        server_root: &Path,
        _java_executable: &Path,
        _installer_jar: &Path,
        _proxy: Option<&mc_server_download_tool::cli::ProxyUrl>,
        observer: Arc<dyn ProcessObserver>,
    ) -> Result<VerifiedLaunch, LoaderError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if let Err(source) = observer.line(
            ProcessStream::Stdout,
            "loader emitted its first line".to_string(),
        ) {
            self.termination_triggered.store(true, Ordering::Release);
            return Err(LoaderError::Observer { source });
        }
        self.continued_after_output.store(true, Ordering::Release);
        let LoaderOutputExpectation::ExactJar { path, .. } = &plan.output else {
            return Err(LoaderError::InvalidPlan {
                reason: "test plan must use an exact jar".to_string(),
            });
        };
        fs::write(server_root.join(path), b"server").map_err(|source| LoaderError::ProcessIo {
            operation: "write controlled loader output for",
            source,
        })?;
        Ok(VerifiedLaunch::Jar { path: path.clone() })
    }
}

impl LoaderInstallation for FakeLoader {
    fn install(
        &self,
        plan: &LoaderPlan,
        server_root: &Path,
        _java_executable: &Path,
        _installer_jar: &Path,
        _proxy: Option<&mc_server_download_tool::cli::ProxyUrl>,
        _observer: Arc<dyn ProcessObserver>,
    ) -> Result<VerifiedLaunch, LoaderError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let LoaderOutputExpectation::ExactJar { path, .. } = &plan.output else {
            panic!("test plan must use an exact jar")
        };
        fs::write(server_root.join(path), b"server").unwrap();
        Ok(VerifiedLaunch::Jar { path: path.clone() })
    }
}

fn sha1(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

fn plan(files: Vec<ManifestFile>, runs: Arc<AtomicUsize>) -> (InstallPlan, FakeLoader) {
    let loader = FakeLoader { runs };
    let install_plan = InstallPlan {
        files,
        loader: LoaderPlan {
            family: LoaderFamily::Cleanroom,
            minecraft_version: "1.12.2".to_string(),
            loader_version: "0.3.0".to_string(),
            installer: InstallerArtifact::new(
                "https://example.invalid/installer.jar",
                "cleanroom-installer.jar",
                sha1(b"installer"),
                Some(9),
            )
            .unwrap(),
            output: LoaderOutputExpectation::ExactJar {
                path: "cleanroom-1.12.2.jar".into(),
                main_class: None,
            },
        },
        java_executable: "java".into(),
        java: JavaConfig {
            major: 8,
            min_memory_mb: 1024,
            max_memory_mb: 2048,
            jvm_args: vec!["-XX:+UseG1GC".to_string()],
            server_args: vec!["nogui".to_string()],
        },
        proxy: None,
        script_platform: ScriptPlatform::Unix,
        language: mc_server_download_tool::i18n::Language::EnUs,
        curseforge_api_key: None,
    };
    (install_plan, loader)
}

fn manifest_file(target: &str, automatic: bool, bytes: &[u8]) -> ManifestFile {
    ManifestFile {
        name: "Test file".to_string(),
        kind: FileKind::Mod,
        path: target.to_string(),
        download: if automatic {
            FileDownload::Automatic {
                url: "https://example.invalid/file".to_string(),
            }
        } else {
            FileDownload::Manual
        },
        project_page: "https://example.invalid/project".to_string(),
        sha1: sha1(bytes),
        size: bytes.len() as u64,
    }
}

#[test]
fn manual_missing_list_is_written_before_loader_execution() {
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("manifest.json");
    fs::write(&manifest_path, b"manifest").unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let (plan, loader) = plan(
        vec![manifest_file("mods/manual.jar", false, b"manual")],
        Arc::clone(&runs),
    );
    let engine = FakeEngine {
        transfers: Arc::new(AtomicUsize::new(0)),
    };
    let installer = Installer::new(engine, loader);

    let error = installer
        .install(&manifest_path, &plan, Arc::new(|_| {}))
        .unwrap_err();

    assert!(matches!(
        error,
        InstallError::ManualFilesMissing { count: 1, .. }
    ));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(
        fs::read_to_string(temp.path().join("missing-files.txt")).unwrap(),
        concat!(
            "Manual files require attention:\n",
            "\n- Test file\n",
            "  Path: mods/manual.jar\n",
            "  Project: https://example.invalid/project\n",
            "  Required: 6 bytes, SHA-1 b363713a938afcd3c74603827fab79e935b2b09b\n"
        )
    );
}

#[test]
fn automatic_file_and_loader_are_downloaded_once_and_valid_state_skips_reinstall() {
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("server-manifest.json");
    fs::write(&manifest_path, b"stable manifest bytes").unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let transfers = Arc::new(AtomicUsize::new(0));
    let (plan, loader) = plan(
        vec![manifest_file("mods/automatic.jar", true, b"automatic")],
        Arc::clone(&runs),
    );
    let installer = Installer::new(
        FakeEngine {
            transfers: Arc::clone(&transfers),
        },
        loader,
    );

    let first = installer
        .install(&manifest_path, &plan, Arc::new(|_| {}))
        .unwrap();
    fs::write(&manifest_path, b"changed non-loader manifest bytes").unwrap();
    let second = installer
        .install(&manifest_path, &plan, Arc::new(|_| {}))
        .unwrap();

    assert!(!first.loader_reused);
    assert!(second.loader_reused);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(transfers.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read(temp.path().join("mods/automatic.jar")).unwrap(),
        b"automatic"
    );
    assert!(temp.path().join(".mcsdt/install-state.json").is_file());
}

#[test]
fn direct_installer_api_cannot_bypass_an_existing_session_lock() {
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("server-install.json");
    fs::write(&manifest_path, b"stable manifest bytes").unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let (plan, loader) = plan(Vec::new(), Arc::clone(&runs));
    let held = InstallSession::acquire(
        &manifest_path,
        plan.language,
        Arc::new(|_| {}),
        std::iter::empty(),
    )
    .unwrap();
    let installer = Installer::new(
        FakeEngine {
            transfers: Arc::new(AtomicUsize::new(0)),
        },
        loader,
    );

    let error = installer
        .install(&manifest_path, &plan, Arc::new(|_| {}))
        .unwrap_err();

    assert!(matches!(error, InstallError::Locked { .. }));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    drop(held);
}

#[test]
fn loader_output_observer_failure_stops_before_scripts_and_state() {
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("server-install.json");
    fs::write(&manifest_path, b"stable manifest bytes").unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let termination_triggered = Arc::new(AtomicBool::new(false));
    let continued_after_output = Arc::new(AtomicBool::new(false));
    let (plan, _) = plan(
        vec![manifest_file("mods/automatic.jar", true, b"automatic")],
        Arc::clone(&runs),
    );
    let loader = OutputFailingLoader {
        runs: Arc::clone(&runs),
        termination_triggered: Arc::clone(&termination_triggered),
        continued_after_output: Arc::clone(&continued_after_output),
    };
    let installer = Installer::new(
        FakeEngine {
            transfers: Arc::new(AtomicUsize::new(0)),
        },
        loader,
    );
    let observer = Arc::new(FailOnLoaderOutput {
        failed: AtomicBool::new(false),
    });

    let error = installer
        .install(&manifest_path, &plan, observer)
        .unwrap_err();

    assert!(matches!(error, InstallError::Observer(_)));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert!(termination_triggered.load(Ordering::Acquire));
    assert!(!continued_after_output.load(Ordering::Acquire));
    for path in ["start.bat", "start.bat.new", "start.sh", "start.sh.new"] {
        assert!(!temp.path().join(path).exists(), "unexpected {path}");
    }
    assert!(!temp.path().join(".mcsdt/install-state.json").exists());

    assert_eq!(
        fs::read(temp.path().join("mods/automatic.jar")).unwrap(),
        b"automatic"
    );
    assert_eq!(
        fs::read(
            temp.path()
                .join(".mcsdt/installers/cleanroom-installer.jar"),
        )
        .unwrap(),
        b"installer"
    );
}

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use mc_server_download_tool::JavaRuntimeProvisioner;
use mc_server_download_tool::application::{
    Application, ApplicationImpl, Environment, InstallExecution,
};
use mc_server_download_tool::cli::Cli;
use mc_server_download_tool::error::{AppError, ExitCode};
use mc_server_download_tool::i18n::Language;
use mc_server_download_tool::install::{
    InstallError, InstallEvent, InstallPlan, InstallResult, InstallSession, InstallStage,
};
use mc_server_download_tool::loader::VerifiedLaunch;
use mc_server_download_tool::manifest::{FileDownload, LoaderKind, Manifest, SCHEMA_VERSION};
use mc_server_download_tool::net::{NetworkConfig, NetworkError};
use mc_server_download_tool::scripts::ScriptPlatform;
use serde_json::json;

struct EmptyEnvironment;

impl Environment for EmptyEnvironment {
    fn variable(&self, _name: &str) -> Option<String> {
        None
    }
}

fn manifest_json() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "curseforge_api_key": "test-key",
        "minecraft": { "version": "1.12.2" },
        "java": {
            "major": 8,
            "min_memory_mb": 1024,
            "max_memory_mb": 2048,
            "jvm_args": ["-XX:+UseG1GC"],
            "server_args": ["nogui"]
        },
        "loader": {
            "kind": "cleanroom",
            "version": "0.6.7-alpha",
            "installer": {
                "url": "https://github.com/CleanroomMC/Cleanroom/releases/download/0.6.7-alpha/cleanroom-0.6.7-alpha-installer.jar",
                "sha1": "171e1953354b93690bb1f2a0fc4a1b299f9b4188",
                "size": 6_968_390
            },
            "output": {
                "type": "exact_jar",
                "path": "cleanroom-1.12.2.jar",
                "main_class": null
            }
        },
        "files": [
            {
                "name": "Automatic mod",
                "type": "mod",
                "path": "mods/automatic.jar",
                "download": {
                    "mode": "automatic",
                    "url": "https://edge.forgecdn.net/files/1234/56/automatic.jar"
                },
                "project_page": "https://www.curseforge.com/minecraft/mc-mods/automatic/files/123456",
                "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 1024
            },
            {
                "name": "Manual mod",
                "type": "mod",
                "path": "mods/manual.jar",
                "download": { "mode": "manual" },
                "project_page": "https://www.curseforge.com/minecraft/mc-mods/manual/files/123457",
                "sha1": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "size": 2048
            }
        ]
    })
}

#[derive(Clone)]
struct FakeJava {
    calls: Arc<Mutex<Vec<String>>>,
    result: Result<PathBuf, io::ErrorKind>,
}

struct LockProbeJava {
    manifest_path: PathBuf,
    observed_lock: Arc<AtomicBool>,
}

#[derive(Clone)]
struct PreferredJava {
    received: Arc<Mutex<Option<PathBuf>>>,
}

impl JavaRuntimeProvisioner for PreferredJava {
    type Error = io::Error;

    fn provision(
        &self,
        _config: &mc_server_download_tool::manifest::JavaConfig,
        _server_root: &Path,
        preferred: Option<&Path>,
    ) -> Result<PathBuf, Self::Error> {
        *self.received.lock().unwrap() = preferred.map(Path::to_path_buf);
        Ok(PathBuf::from("C:/selected/java.exe"))
    }
}

impl JavaRuntimeProvisioner for LockProbeJava {
    type Error = io::Error;

    fn provision(
        &self,
        _config: &mc_server_download_tool::manifest::JavaConfig,
        _server_root: &Path,
        _preferred: Option<&Path>,
    ) -> Result<PathBuf, Self::Error> {
        let competing = InstallSession::acquire(
            &self.manifest_path,
            Language::EnUs,
            Arc::new(|_| {}),
            std::iter::empty(),
        );
        let locked = match competing {
            Err(InstallError::Locked { .. }) => true,
            Err(error) => panic!("competing session returned unexpected error: {error}"),
            Ok(_) => false,
        };
        self.observed_lock.store(locked, Ordering::SeqCst);
        Ok(PathBuf::from("java"))
    }
}

impl JavaRuntimeProvisioner for FakeJava {
    type Error = io::Error;

    fn provision(
        &self,
        config: &mc_server_download_tool::manifest::JavaConfig,
        server_root: &Path,
        _preferred: Option<&Path>,
    ) -> Result<PathBuf, Self::Error> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("java:{}:{}", config.major, server_root.display()));
        self.result
            .clone()
            .map_err(|kind| io::Error::new(kind, "injected Java failure"))
    }
}

#[derive(Clone)]
struct FakeInstall {
    calls: Arc<Mutex<Vec<String>>>,
    captured: Arc<Mutex<Option<(NetworkConfig, PathBuf, InstallPlan)>>>,
}

impl InstallExecution for FakeInstall {
    fn install(
        &self,
        network: NetworkConfig,
        manifest_path: &Path,
        plan: &InstallPlan,
        _session: &InstallSession,
    ) -> Result<InstallResult, AppError> {
        self.calls.lock().unwrap().push("install".to_string());
        *self.captured.lock().unwrap() = Some((network, manifest_path.to_path_buf(), plan.clone()));
        Ok(InstallResult {
            server_root: manifest_path.parent().unwrap().to_path_buf(),
            launch: VerifiedLaunch::Jar {
                path: "cleanroom-1.12.2.jar".into(),
            },
            loader_reused: false,
        })
    }
}

#[test]
fn application_orchestrates_final_manifest_into_one_install_execution() {
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("custom.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest_json()).unwrap(),
    )
    .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::new(Mutex::new(None));
    let application = ApplicationImpl::new(
        FakeJava {
            calls: Arc::clone(&calls),
            result: Ok(PathBuf::from("C:/Java/bin/java.exe")),
        },
        FakeInstall {
            calls: Arc::clone(&calls),
            captured: Arc::clone(&captured),
        },
        ScriptPlatform::Windows,
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_sink = Arc::clone(&events);

    let result = application
        .run(
            Cli {
                manifest: Some(manifest_path.clone()),
                lang: Some(Language::ZhCn),
                proxy: Some("http://127.0.0.1:7890".parse().unwrap()),
            },
            Path::new("C:/tool/mc-server-download-tool.exe"),
            Some("en-US"),
            &EmptyEnvironment,
            Arc::new(move |event| event_sink.lock().unwrap().push(event)),
        )
        .unwrap();

    assert_eq!(result.language, Language::ZhCn);
    assert_eq!(result.file_count, 2);
    assert_eq!(calls.lock().unwrap().as_slice()[1], "install");
    assert!(calls.lock().unwrap()[0].starts_with("java:8:"));
    assert!(matches!(
        events.lock().unwrap().as_slice(),
        [
            InstallEvent::Stage(InstallStage::Locked),
            InstallEvent::Stage(InstallStage::SelectingJava),
            ..
        ]
    ));

    let captured = captured.lock().unwrap();
    let (network, selected_manifest, plan) = captured.as_ref().unwrap();
    assert_eq!(network.proxy.as_deref(), Some("http://127.0.0.1:7890/"));
    assert_eq!(selected_manifest, &manifest_path);
    assert_eq!(plan.language, Language::ZhCn);
    assert_eq!(plan.script_platform, ScriptPlatform::Windows);
    assert_eq!(plan.java.major, 8);
    assert_eq!(plan.java.server_args, ["nogui"]);
    assert!(matches!(
        plan.files[0].download,
        FileDownload::Automatic { .. }
    ));
    assert!(matches!(plan.files[1].download, FileDownload::Manual));
}

#[test]
fn application_passes_saved_java_to_provisioner_before_installation() {
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("server-install.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest_json()).unwrap(),
    )
    .unwrap();
    std::fs::create_dir(temp.path().join(".mcsdt")).unwrap();
    let saved = temp.path().join("saved-java/bin/java.exe");
    std::fs::write(
        temp.path().join(".mcsdt/install-state.json"),
        serde_json::to_vec(&json!({
            "manifest_sha256": "00",
            "java_executable": saved,
            "loader_plan_sha256": "00",
            "loader_output": {"type": "jar", "path": "cleanroom-1.12.2.jar"},
            "loader_artifacts": [],
            "script_sha256": "00"
        }))
        .unwrap(),
    )
    .unwrap();
    let received = Arc::new(Mutex::new(None));
    let application = ApplicationImpl::new(
        PreferredJava {
            received: Arc::clone(&received),
        },
        FakeInstall {
            calls: Arc::new(Mutex::new(Vec::new())),
            captured: Arc::new(Mutex::new(None)),
        },
        ScriptPlatform::Windows,
    );

    application
        .run(
            Cli {
                manifest: Some(manifest_path),
                lang: Some(Language::EnUs),
                proxy: None,
            },
            Path::new("C:/tool.exe"),
            None,
            &EmptyEnvironment,
            Arc::new(|_| {}),
        )
        .unwrap();

    assert_eq!(received.lock().unwrap().as_deref(), Some(saved.as_path()));
}

#[test]
fn java_failure_stops_before_installation_and_has_a_stable_exit_code() {
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("server-install.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest_json()).unwrap(),
    )
    .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let application = ApplicationImpl::new(
        FakeJava {
            calls: Arc::clone(&calls),
            result: Err(io::ErrorKind::NotFound),
        },
        FakeInstall {
            calls: Arc::clone(&calls),
            captured: Arc::new(Mutex::new(None)),
        },
        ScriptPlatform::Unix,
    );

    let error = application
        .run(
            Cli {
                manifest: Some(manifest_path),
                lang: Some(Language::EnUs),
                proxy: None,
            },
            Path::new("/tool/mc-server-download-tool"),
            None,
            &EmptyEnvironment,
            Arc::new(|_| {}),
        )
        .unwrap_err();

    assert_eq!(error.exit_code(), ExitCode::Java);
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn application_holds_the_installation_lock_while_java_is_selected() {
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("server-install.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest_json()).unwrap(),
    )
    .unwrap();
    let observed_lock = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let application = ApplicationImpl::new(
        LockProbeJava {
            manifest_path: manifest_path.clone(),
            observed_lock: Arc::clone(&observed_lock),
        },
        FakeInstall {
            calls: Arc::clone(&calls),
            captured: Arc::new(Mutex::new(None)),
        },
        ScriptPlatform::Unix,
    );

    application
        .run(
            Cli {
                manifest: Some(manifest_path),
                lang: Some(Language::EnUs),
                proxy: None,
            },
            Path::new("/outside/mc-server-download-tool"),
            None,
            &EmptyEnvironment,
            Arc::new(|_| {}),
        )
        .unwrap();

    assert!(observed_lock.load(Ordering::SeqCst));
    assert_eq!(calls.lock().unwrap().as_slice(), ["install"]);
}

#[test]
fn repository_default_manifest_is_valid_cleanroom_v1() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("server-install.json");
    let manifest = Manifest::from_slice(&std::fs::read(path).unwrap()).unwrap();

    assert_eq!(manifest.as_manifest().schema_version, SCHEMA_VERSION);
    assert_eq!(manifest.minecraft().version, "1.12.2");
    assert_eq!(manifest.loader().kind, LoaderKind::Cleanroom);
    assert_eq!(manifest.loader().version, "0.6.7-alpha");
}

#[test]
fn application_errors_map_network_and_integrity_failures_to_distinct_exit_codes() {
    let network = AppError::Network(NetworkError::InvalidConfiguration {
        reason: "injected network failure".to_string(),
    });
    let integrity = AppError::Installation(InstallError::Verification {
        target: "mods/bad.jar".into(),
        reason: "injected digest mismatch".to_string(),
    });

    assert_eq!(network.exit_code(), ExitCode::Network);
    assert_eq!(integrity.exit_code(), ExitCode::Integrity);
}

#[test]
fn runtime_rejects_resources_and_loader_outputs_that_replace_selected_manifest() {
    let temp = tempfile::tempdir().unwrap();
    for (manifest_name, target_kind) in [
        ("custom-install.json", "resource"),
        ("custom-loader-manifest.jar", "loader"),
    ] {
        let manifest_path = temp.path().join(manifest_name);
        let mut value = manifest_json();
        match target_kind {
            "resource" => value["files"][0]["path"] = json!(manifest_name),
            "loader" => value["loader"]["output"]["path"] = json!(manifest_name),
            _ => unreachable!(),
        }
        std::fs::write(&manifest_path, serde_json::to_vec(&value).unwrap()).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let application = ApplicationImpl::new(
            FakeJava {
                calls: Arc::clone(&calls),
                result: Ok(PathBuf::from("java")),
            },
            FakeInstall {
                calls: Arc::clone(&calls),
                captured: Arc::new(Mutex::new(None)),
            },
            ScriptPlatform::Unix,
        );

        let error = application
            .run(
                Cli {
                    manifest: Some(manifest_path),
                    lang: Some(Language::EnUs),
                    proxy: None,
                },
                &temp.path().join("installer-outside-targets"),
                None,
                &EmptyEnvironment,
                Arc::new(|_| {}),
            )
            .unwrap_err();

        match target_kind {
            "resource" => assert!(matches!(
                error,
                AppError::Installation(InstallError::InvalidPlan { .. })
            )),
            "loader" => assert!(matches!(error, AppError::Manifest(_))),
            _ => unreachable!(),
        }
        assert!(calls.lock().unwrap().is_empty());
    }
}

#[test]
fn runtime_rejects_resource_or_script_replacing_actual_executable_in_root() {
    for executable_name in ["renamed-installer.bin", "start.sh"] {
        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("custom-install.json");
        let executable = temp.path().join(executable_name);
        std::fs::write(&executable, b"installer").unwrap();
        let mut value = manifest_json();
        if executable_name == "renamed-installer.bin" {
            value["files"][0]["path"] = json!(executable_name);
        }
        std::fs::write(&manifest_path, serde_json::to_vec(&value).unwrap()).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let application = ApplicationImpl::new(
            FakeJava {
                calls: Arc::clone(&calls),
                result: Ok(PathBuf::from("java")),
            },
            FakeInstall {
                calls: Arc::clone(&calls),
                captured: Arc::new(Mutex::new(None)),
            },
            ScriptPlatform::Unix,
        );

        let error = application
            .run(
                Cli {
                    manifest: Some(manifest_path),
                    lang: Some(Language::EnUs),
                    proxy: None,
                },
                &executable,
                None,
                &EmptyEnvironment,
                Arc::new(|_| {}),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            AppError::Installation(InstallError::InvalidPlan { .. })
        ));
        assert!(calls.lock().unwrap().is_empty());
    }
}

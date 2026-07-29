use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mc_server_download_tool::loader::{
    InstallerArtifact, LoaderError, LoaderExecutor, LoaderFamily, LoaderInstallation,
    LoaderOutputExpectation, LoaderPlan, ProcessObserver, ProcessObserverError, ProcessRequest,
    ProcessRunner, ProcessStream, SystemProcessRunner, VerifiedLaunch, verify_loader_output,
};
use sha1::{Digest, Sha1};
use zip::write::SimpleFileOptions;

#[derive(Clone)]
struct FakeJava {
    requests: Arc<Mutex<Vec<ProcessRequest>>>,
    output: LoaderOutputExpectation,
}

impl ProcessRunner for FakeJava {
    fn run(
        &self,
        request: &ProcessRequest,
        _observer: Arc<dyn ProcessObserver>,
    ) -> Result<(), mc_server_download_tool::loader::LoaderError> {
        self.requests.lock().unwrap().push(request.clone());
        match &self.output {
            LoaderOutputExpectation::ModernArgs { windows, unix } => {
                for path in [windows, unix] {
                    let full = request.working_directory.join(path);
                    fs::create_dir_all(full.parent().unwrap()).unwrap();
                    fs::write(full, b"args").unwrap();
                }
            }
            LoaderOutputExpectation::ExactJar { path, main_class } => {
                let full = request.working_directory.join(path);
                if let Some(main_class) = main_class {
                    write_jar(&full, main_class);
                } else {
                    fs::write(full, b"jar").unwrap();
                }
            }
        }
        Ok(())
    }
}

fn artifact() -> InstallerArtifact {
    InstallerArtifact::new(
        "https://example.invalid/installer.jar",
        "installer.jar",
        format!("{:x}", Sha1::digest(b"installer")),
        None,
    )
    .unwrap()
}

fn write_jar(path: &Path, main_class: &str) {
    let file = fs::File::create(path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    archive
        .start_file("META-INF/MANIFEST.MF", SimpleFileOptions::default())
        .unwrap();
    write!(
        archive,
        "Manifest-Version: 1.0\r\nMain-Class: {main_class}\r\n\r\n"
    )
    .unwrap();
    archive.finish().unwrap();
}

#[test]
fn forge_uses_install_server_and_requires_both_modern_argument_files() {
    let temp = tempfile::tempdir().unwrap();
    let output = LoaderOutputExpectation::ModernArgs {
        windows: "libraries/forge/win_args.txt".into(),
        unix: "libraries/forge/unix_args.txt".into(),
    };
    let requests = Arc::new(Mutex::new(Vec::new()));
    let executor = LoaderExecutor::new(FakeJava {
        requests: Arc::clone(&requests),
        output: output.clone(),
    });
    let plan = LoaderPlan {
        family: LoaderFamily::Forge,
        minecraft_version: "1.20.1".to_string(),
        loader_version: "47.3.0".to_string(),
        installer: artifact(),
        output,
    };

    let launch = executor
        .install(
            &plan,
            temp.path(),
            Path::new("java"),
            Path::new("installer.jar"),
            None,
            Arc::new(|_, _| {}),
        )
        .unwrap();

    assert!(matches!(launch, VerifiedLaunch::ArgsFiles { .. }));
    assert_eq!(
        requests.lock().unwrap()[0].arguments,
        ["-jar", "installer.jar", "--installServer"]
    );
}

#[test]
fn fabric_uses_official_server_parameters_and_checks_manifest_main_class() {
    let temp = tempfile::tempdir().unwrap();
    let output = LoaderOutputExpectation::ExactJar {
        path: "fabric-server-launch.jar".into(),
        main_class: Some("net.fabricmc.loader.impl.launch.server.FabricServerLauncher".to_string()),
    };
    let requests = Arc::new(Mutex::new(Vec::new()));
    let executor = LoaderExecutor::new(FakeJava {
        requests: Arc::clone(&requests),
        output: output.clone(),
    });
    let plan = LoaderPlan {
        family: LoaderFamily::Fabric,
        minecraft_version: "1.21.1".to_string(),
        loader_version: "0.16.10".to_string(),
        installer: artifact(),
        output,
    };

    executor
        .install(
            &plan,
            temp.path(),
            Path::new("java"),
            Path::new("installer.jar"),
            Some(&"http://127.0.0.1:7890".parse().unwrap()),
            Arc::new(|_, _| {}),
        )
        .unwrap();

    let arguments = &requests.lock().unwrap()[0].arguments;
    assert_eq!(
        &arguments[..4],
        [
            "-Dhttp.proxyHost=127.0.0.1",
            "-Dhttp.proxyPort=7890",
            "-Dhttps.proxyHost=127.0.0.1",
            "-Dhttps.proxyPort=7890",
        ]
    );
    assert_eq!(
        &arguments[4..],
        [
            "-jar",
            "installer.jar",
            "server",
            "-mcversion",
            "1.21.1",
            "-loader",
            "0.16.10",
            "-downloadMinecraft",
        ]
    );
}

#[test]
fn exact_jar_verification_does_not_accept_an_approximate_filename() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("forge-nearly-correct.jar"), b"jar").unwrap();
    let error = verify_loader_output(
        temp.path(),
        &LoaderOutputExpectation::ExactJar {
            path: "forge-1.12.2-14.23.5.2860.jar".into(),
            main_class: None,
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("forge-1.12.2-14.23.5.2860.jar"));
}

struct RejectReadyLine {
    observed: AtomicBool,
}

impl ProcessObserver for RejectReadyLine {
    fn line(&self, _stream: ProcessStream, line: String) -> Result<(), ProcessObserverError> {
        if line.ends_with("loader-child-ready") {
            self.observed.store(true, Ordering::Release);
            return Err(ProcessObserverError::new("persistent log unavailable"));
        }
        Ok(())
    }
}

#[test]
fn observer_failure_terminates_and_reaps_installer_process() {
    let temp = tempfile::tempdir().unwrap();
    let observer = Arc::new(RejectReadyLine {
        observed: AtomicBool::new(false),
    });
    let request = ProcessRequest {
        executable: std::env::current_exe().unwrap(),
        arguments: vec![
            "--ignored".to_string(),
            "--exact".to_string(),
            "loader_observer_child".to_string(),
            "--nocapture".to_string(),
        ],
        working_directory: temp.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        max_line_bytes: 64 * 1024,
        max_stream_bytes: 1024 * 1024,
    };
    let started = Instant::now();

    let result = SystemProcessRunner.run(&request, observer.clone());

    assert!(matches!(result, Err(LoaderError::Observer { .. })));
    assert!(observer.observed.load(Ordering::Acquire));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(temp.path().join("loader-child-started").exists());
    std::thread::sleep(Duration::from_millis(300));
    assert!(!temp.path().join("loader-child-completed").exists());
}

fn helper_request(temp: &Path, test: &str) -> ProcessRequest {
    ProcessRequest {
        executable: std::env::current_exe().unwrap(),
        arguments: vec![
            "--ignored".to_string(),
            "--exact".to_string(),
            test.to_string(),
            "--nocapture".to_string(),
        ],
        working_directory: temp.to_path_buf(),
        timeout: Duration::from_secs(5),
        max_line_bytes: 64 * 1024,
        max_stream_bytes: 1024 * 1024,
    }
}

#[test]
fn loader_output_line_limit_terminates_the_process() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = helper_request(temp.path(), "loader_oversized_line_child");
    request.max_line_bytes = 1024;

    let error = SystemProcessRunner
        .run(&request, Arc::new(|_, _| {}))
        .unwrap_err();

    assert!(matches!(
        error,
        LoaderError::OutputLimit {
            kind: "line",
            limit: 1024,
            ..
        }
    ));
}

#[test]
fn loader_total_timeout_terminates_the_process() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = helper_request(temp.path(), "loader_timeout_child");
    request.timeout = Duration::from_millis(100);
    let started = Instant::now();

    let error = SystemProcessRunner
        .run(&request, Arc::new(|_, _| {}))
        .unwrap_err();

    assert!(matches!(error, LoaderError::ProcessTimedOut { .. }));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn observer_failure_terminates_installer_descendants() {
    let temp = tempfile::tempdir().unwrap();
    let request = helper_request(temp.path(), "loader_descendant_parent");
    let observer = Arc::new(RejectReadyLine {
        observed: AtomicBool::new(false),
    });

    let error = SystemProcessRunner
        .run(&request, observer.clone())
        .unwrap_err();

    assert!(matches!(error, LoaderError::Observer { .. }));
    assert!(observer.observed.load(Ordering::Acquire));
    std::thread::sleep(Duration::from_secs(2));
    assert!(!temp.path().join("loader-grandchild-completed").exists());
}

#[test]
fn jar_manifest_uncompressed_size_is_bounded_before_parsing() {
    let temp = tempfile::tempdir().unwrap();
    let jar = temp.path().join("fabric-server-launch.jar");
    let file = fs::File::create(&jar).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    archive
        .start_file("META-INF/MANIFEST.MF", SimpleFileOptions::default())
        .unwrap();
    archive.write_all(&vec![b'A'; 1024 * 1024 + 1]).unwrap();
    archive.finish().unwrap();

    let error = verify_loader_output(
        temp.path(),
        &LoaderOutputExpectation::ExactJar {
            path: "fabric-server-launch.jar".into(),
            main_class: Some("example.Main".to_string()),
        },
    )
    .unwrap_err();

    assert!(matches!(error, LoaderError::Jar { .. }));
    assert!(error.to_string().contains("1048576-byte limit"));
}

#[test]
#[ignore = "executed as a child process by observer_failure_terminates_and_reaps_installer_process"]
fn loader_observer_child() {
    fs::write("loader-child-started", b"started").unwrap();
    println!("loader-child-ready");
    std::io::stdout().flush().unwrap();
    std::thread::sleep(Duration::from_secs(5));
    fs::write("loader-child-completed", b"completed").unwrap();
}

#[test]
#[ignore = "executed as a child process by loader_output_line_limit_terminates_the_process"]
fn loader_oversized_line_child() {
    std::io::stdout().write_all(&vec![b'x'; 4096]).unwrap();
    std::io::stdout().flush().unwrap();
    std::thread::sleep(Duration::from_secs(5));
}

#[test]
#[ignore = "executed as a child process by loader_total_timeout_terminates_the_process"]
fn loader_timeout_child() {
    std::thread::sleep(Duration::from_secs(5));
}

#[test]
#[ignore = "executed as a child process by observer_failure_terminates_installer_descendants"]
fn loader_descendant_parent() {
    let mut grandchild = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "loader_descendant_grandchild",
            "--nocapture",
        ])
        .spawn()
        .unwrap();
    println!("loader-child-ready");
    std::io::stdout().flush().unwrap();
    grandchild.wait().unwrap();
}

#[test]
#[ignore = "executed as a child process by loader_descendant_parent"]
fn loader_descendant_grandchild() {
    std::thread::sleep(Duration::from_millis(900));
    fs::write("loader-grandchild-completed", b"completed").unwrap();
}

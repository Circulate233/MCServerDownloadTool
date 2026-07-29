use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mc_server_download_tool::i18n::Language;
use mc_server_download_tool::java::{
    CandidatePlan, DiscoveryInputs, EnvironmentPolicy, InputEvent, InteractiveIo,
    JAVA_PROBE_TIMEOUT, JavaCommandProbe, JavaPlatform, JavaRuntime, ParallelProbeError,
    ParallelismProvider, ProbeError, ProcessError, ProcessOutput, ProcessRequest, ProcessRunner,
    RuntimeProbe, SearchRoot, SystemProcessRunner, build_candidate_plan, discover_from_plan,
    parse_java_properties, probe_candidates_parallel, select_runtime, sort_runtimes,
};

fn inputs(platform: JavaPlatform) -> DiscoveryInputs {
    let path_entry = if platform == JavaPlatform::Windows {
        PathBuf::from("C:/path-java")
    } else {
        PathBuf::from("/path-java")
    };
    DiscoveryInputs {
        platform,
        path_entries: vec![path_entry],
        java_home: Some(PathBuf::from("/env/jdk")),
        jre_home: Some(PathBuf::from("/env/jre")),
        user_home: Some(PathBuf::from("/users/alex")),
        app_data: Some(PathBuf::from("C:/Users/alex/AppData/Roaming")),
        local_app_data: Some(PathBuf::from("C:/Users/alex/AppData/Local")),
        program_files: vec![PathBuf::from("C:/Program Files")],
        registry_homes: vec![PathBuf::from("C:/Vendor/JDK")],
        platform_java_homes: vec![PathBuf::from("/Library/Java/Selected")],
        server_root: PathBuf::from("/server"),
    }
}

#[test]
fn windows_candidate_plan_covers_environment_registry_vendors_and_minecraft() {
    let plan = build_candidate_plan(&inputs(JavaPlatform::Windows));

    assert!(
        plan.direct_executables
            .contains(&PathBuf::from("C:/path-java/java.exe"))
    );
    assert!(
        plan.direct_executables
            .contains(&PathBuf::from("C:/Vendor/JDK/bin/java.exe"))
    );
    assert!(
        plan.search_roots
            .iter()
            .any(|root| root.path == Path::new("C:/Program Files/Eclipse Adoptium"))
    );
    assert!(plan.search_roots.iter().any(|root| {
        root.path == Path::new("C:/Users/alex/AppData/Roaming/.minecraft/runtime")
    }));
}

#[test]
fn linux_candidate_plan_covers_system_toolchains_and_minecraft() {
    let plan = build_candidate_plan(&inputs(JavaPlatform::Linux));

    for expected in [
        "/usr/java",
        "/usr/lib/jvm",
        "/usr/lib64/jvm",
        "/opt",
        "/users/alex/.sdkman/candidates/java",
        "/users/alex/.asdf/installs/java",
        "/users/alex/.gradle/jdks",
        "/users/alex/.jdks",
        "/users/alex/.minecraft/runtime",
    ] {
        assert!(
            plan.search_roots
                .iter()
                .any(|root| root.path == Path::new(expected)),
            "missing Linux search root {expected}"
        );
    }
    assert!(plan.search_patterns.iter().any(|pattern| {
        pattern.parent == Path::new("/usr")
            && pattern.child_prefix == "lib"
            && pattern.relative_tail == Path::new("jvm")
    }));
}

#[test]
fn macos_candidate_plan_covers_java_home_system_user_and_minecraft() {
    let plan = build_candidate_plan(&inputs(JavaPlatform::MacOs));

    assert!(
        plan.direct_executables
            .contains(&PathBuf::from("/Library/Java/Selected/bin/java"))
    );
    for expected in [
        "/Library/Java/JavaVirtualMachines",
        "/System/Library/Java/JavaVirtualMachines",
        "/users/alex/Library/Java/JavaVirtualMachines",
        "/users/alex/.sdkman/candidates/java",
        "/users/alex/.asdf/installs/java",
        "/users/alex/Library/Application Support/minecraft/runtime",
    ] {
        assert!(
            plan.search_roots
                .iter()
                .any(|root| root.path == Path::new(expected)),
            "missing macOS search root {expected}"
        );
    }
}

#[test]
fn materialized_candidates_are_canonical_and_deduplicated() {
    let temp = tempfile::tempdir().unwrap();
    let executable = temp.path().join(JavaPlatform::current().executable_name());
    std::fs::write(&executable, b"test launcher").unwrap();
    let plan = CandidatePlan {
        direct_executables: vec![
            executable.clone(),
            temp.path().join(".").join(executable.file_name().unwrap()),
        ],
        search_roots: vec![SearchRoot {
            path: temp.path().to_path_buf(),
            max_depth: 1,
        }],
        search_patterns: Vec::new(),
    };

    let report = discover_from_plan(&plan, JavaPlatform::current());

    assert!(report.warnings.is_empty());
    assert_eq!(
        report.candidates,
        vec![std::fs::canonicalize(executable).unwrap()]
    );
}

#[cfg(unix)]
#[test]
fn materialized_candidates_follow_java_executable_symlinks() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("jdk/bin");
    std::fs::create_dir_all(&home).unwrap();
    let target = temp.path().join("real-java");
    std::fs::write(&target, b"test launcher").unwrap();
    symlink(&target, home.join("java")).unwrap();
    let plan = CandidatePlan {
        direct_executables: Vec::new(),
        search_roots: vec![SearchRoot {
            path: temp.path().join("jdk"),
            max_depth: 2,
        }],
        search_patterns: Vec::new(),
    };

    let report = discover_from_plan(&plan, JavaPlatform::Linux);

    assert_eq!(
        report.candidates,
        vec![std::fs::canonicalize(target).unwrap()]
    );
}

#[cfg(unix)]
#[test]
fn materialized_candidates_do_not_traverse_linked_directories() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("jdk");
    let outside = temp.path().join("outside/bin");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("java"), b"test launcher").unwrap();
    symlink(temp.path().join("outside"), root.join("linked-runtime")).unwrap();
    let plan = CandidatePlan {
        direct_executables: Vec::new(),
        search_roots: vec![SearchRoot {
            path: root,
            max_depth: 3,
        }],
        search_patterns: Vec::new(),
    };

    let report = discover_from_plan(&plan, JavaPlatform::Linux);

    assert!(report.candidates.is_empty());
}

#[test]
fn parses_legacy_and_modern_java_properties() {
    let legacy = parse_java_properties(
        Path::new("java8"),
        br"
            java.version = 1.8.0_392
            java.vendor = Eclipse Adoptium
            os.arch = amd64
        ",
    )
    .unwrap();
    let modern = parse_java_properties(
        Path::new("java21"),
        br"
            java.version = 21.0.4+7-LTS
            java.vendor = Microsoft
            os.arch = aarch64
        ",
    )
    .unwrap();

    assert_eq!(legacy.major, 8);
    assert_eq!(modern.major, 21);
    assert_eq!(modern.vendor, "Microsoft");
    assert!(legacy.is_64_bit());
}

#[test]
fn parser_rejects_a_version_that_does_not_start_with_a_feature_number() {
    let error = parse_java_properties(
        Path::new("java"),
        br"
            java.version = invalid-21
            java.vendor = Test Vendor
            os.arch = amd64
        ",
    )
    .unwrap_err();

    assert!(matches!(error, ProbeError::InvalidVersion { .. }));
}

#[test]
fn sorting_prefers_64_bit_then_version_descending_then_path() {
    let mut runtimes = vec![
        runtime("C:/b/java.exe", "21.0.2", "x86"),
        runtime("C:/c/java.exe", "21.0.3", "amd64"),
        runtime("C:/a/java.exe", "21.0.3", "amd64"),
        runtime("C:/d/java.exe", "21.0.2", "amd64"),
    ];

    sort_runtimes(&mut runtimes);

    assert_eq!(
        runtimes
            .iter()
            .map(|runtime| runtime.executable.clone())
            .collect::<Vec<_>>(),
        [
            "C:/a/java.exe",
            "C:/c/java.exe",
            "C:/d/java.exe",
            "C:/b/java.exe"
        ]
        .map(PathBuf::from)
    );
}

#[test]
fn sorting_handles_version_components_larger_than_machine_integers() {
    let mut runtimes = vec![
        runtime("C:/small/java.exe", "21.0.999", "amd64"),
        runtime(
            "C:/large/java.exe",
            "21.0.1000000000000000000000000000000000000000",
            "amd64",
        ),
    ];

    sort_runtimes(&mut runtimes);

    assert_eq!(runtimes[0].executable, PathBuf::from("C:/large/java.exe"));
}

#[test]
fn java_probe_uses_fixed_timeout_and_clean_environment() {
    let runner = Arc::new(TimeoutRunner::default());
    let probe = JavaCommandProbe::new(Arc::clone(&runner));

    let error = probe.inspect(Path::new("java")).unwrap_err();

    assert!(matches!(
        error,
        ProbeError::Process(ProcessError::TimedOut { .. })
    ));
    let request = runner.request.lock().unwrap().clone().unwrap();
    assert_eq!(request.timeout, JAVA_PROBE_TIMEOUT);
    assert_eq!(request.environment, EnvironmentPolicy::CleanJava);
    assert_eq!(
        request.arguments,
        [
            OsString::from("-XshowSettings:properties"),
            OsString::from("-version")
        ]
    );
}

#[test]
fn system_process_runner_terminates_a_timed_out_child() {
    let (program, arguments) = timeout_command();
    let request = ProcessRequest::new(program, Duration::from_millis(75))
        .with_arguments(arguments.iter().map(String::as_str));
    let start = Instant::now();

    let error = SystemProcessRunner.run(&request).unwrap_err();

    assert!(matches!(error, ProcessError::TimedOut { .. }));
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[test]
fn unavailable_parallelism_is_an_explicit_error() {
    let error = probe_candidates_parallel(
        &[PathBuf::from("java")],
        21,
        &Arc::new(FixedProbe),
        &FailingParallelism,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        ParallelProbeError::AvailableParallelism { .. }
    ));
}

#[test]
fn parallel_probe_keeps_only_exact_major_matches() {
    let report = probe_candidates_parallel(
        &[
            PathBuf::from("java-8"),
            PathBuf::from("java-21"),
            PathBuf::from("java-17"),
        ],
        21,
        &Arc::new(NameProbe),
        &FixedParallelism(NonZeroUsize::new(2).unwrap()),
    )
    .unwrap();

    assert_eq!(report.matching.len(), 1);
    assert_eq!(report.matching[0].major, 21);
    assert_eq!(report.rejected.len(), 2);
}

#[test]
fn discovered_runtime_selection_retries_until_a_sequence_number_is_entered() {
    let io = MemoryIo::new([
        InputEvent::Line("C:/Java/bin/java.exe\n".to_string()),
        InputEvent::Line("2\n".to_string()),
    ]);
    let runtimes = vec![
        runtime("C:/first/java.exe", "21.0.2", "amd64"),
        runtime("C:/second/java.exe", "21.0.3", "amd64"),
    ];

    let selected = select_runtime(
        &runtimes,
        21,
        JavaPlatform::Windows,
        Language::EnUs,
        &io,
        &FixedProbe,
    )
    .unwrap();

    assert_eq!(selected.executable, PathBuf::from("C:/second/java.exe"));
    assert!(io.errors().contains("sequence numbers"));
}

#[test]
fn empty_discovery_accepts_java_home_and_retries_invalid_paths() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let executable = bin.join(JavaPlatform::current().executable_name());
    std::fs::write(&executable, b"test launcher").unwrap();
    let io = MemoryIo::new([
        InputEvent::Line("definitely-missing-java\n".to_string()),
        InputEvent::Line(format!("{}\n", temp.path().display())),
    ]);

    let selected = select_runtime(
        &[],
        21,
        JavaPlatform::current(),
        Language::EnUs,
        &io,
        &FixedProbe,
    )
    .unwrap();

    assert_eq!(
        selected.executable,
        std::fs::canonicalize(executable).unwrap()
    );
    assert!(io.errors().contains("does not exist"));
}

#[test]
fn chinese_manual_java_errors_explain_path_and_probe_failures() {
    let io = MemoryIo::new([
        InputEvent::Line("definitely-missing-java\n".to_string()),
        InputEvent::Line("still-not-java\n".to_string()),
        InputEvent::EndOfFile,
    ]);

    let error = select_runtime(
        &[],
        21,
        JavaPlatform::current(),
        Language::ZhCn,
        &io,
        &FixedProbe,
    )
    .unwrap_err();

    let errors = io.errors();
    assert!(errors.contains("Java 可执行文件不存在或不是文件"));
    assert!(!errors.contains("does not exist"));
    assert!(error.to_string().contains("输入已结束"));
}

#[test]
fn chinese_manual_java_probe_error_is_localized() {
    let temp = tempfile::tempdir().unwrap();
    let executable = temp.path().join(JavaPlatform::current().executable_name());
    std::fs::write(&executable, b"test launcher").unwrap();
    let io = MemoryIo::new([
        InputEvent::Line(format!("{}\n", executable.display())),
        InputEvent::EndOfFile,
    ]);

    let error = select_runtime(
        &[],
        21,
        JavaPlatform::current(),
        Language::ZhCn,
        &io,
        &MissingPropertyProbe,
    )
    .unwrap_err();

    let errors = io.errors();
    assert!(errors.contains("Java 元数据缺少必需属性“java.version”"));
    assert!(!errors.contains("did not report required property"));
    assert!(error.to_string().contains("输入已结束"));
}

#[test]
fn eof_during_selection_has_a_clear_error() {
    let io = MemoryIo::new([InputEvent::EndOfFile]);

    let error = select_runtime(
        &[runtime("java", "21", "amd64")],
        21,
        JavaPlatform::current(),
        Language::EnUs,
        &io,
        &FixedProbe,
    )
    .unwrap_err();

    assert!(error.to_string().contains("EOF"));
}

#[test]
fn interrupted_selection_reports_ctrl_c() {
    let error = select_runtime(
        &[runtime("java", "21", "amd64")],
        21,
        JavaPlatform::current(),
        Language::EnUs,
        &InterruptedIo,
        &FixedProbe,
    )
    .unwrap_err();

    assert!(error.to_string().contains("Ctrl+C"));
}

fn runtime(path: &str, version: &str, architecture: &str) -> JavaRuntime {
    JavaRuntime {
        executable: PathBuf::from(path),
        version: version.to_string(),
        major: 21,
        vendor: "Test Vendor".to_string(),
        architecture: architecture.to_string(),
    }
}

#[derive(Default)]
struct TimeoutRunner {
    request: Mutex<Option<ProcessRequest>>,
}

impl ProcessRunner for TimeoutRunner {
    fn run(&self, request: &ProcessRequest) -> Result<ProcessOutput, ProcessError> {
        *self.request.lock().unwrap() = Some(request.clone());
        Err(ProcessError::TimedOut {
            program: request.program.clone(),
            timeout: request.timeout,
            cleanup_error: None,
        })
    }
}

struct FixedProbe;

impl RuntimeProbe for FixedProbe {
    fn inspect(&self, executable: &Path) -> Result<JavaRuntime, ProbeError> {
        Ok(JavaRuntime {
            executable: executable.to_path_buf(),
            version: "21.0.3".to_string(),
            major: 21,
            vendor: "Test Vendor".to_string(),
            architecture: "amd64".to_string(),
        })
    }
}

struct NameProbe;

impl RuntimeProbe for NameProbe {
    fn inspect(&self, executable: &Path) -> Result<JavaRuntime, ProbeError> {
        let major: u16 = executable
            .to_string_lossy()
            .rsplit_once('-')
            .unwrap()
            .1
            .parse()
            .unwrap();
        Ok(JavaRuntime {
            executable: executable.to_path_buf(),
            version: major.to_string(),
            major,
            vendor: "Test Vendor".to_string(),
            architecture: "amd64".to_string(),
        })
    }
}

struct MissingPropertyProbe;

impl RuntimeProbe for MissingPropertyProbe {
    fn inspect(&self, _executable: &Path) -> Result<JavaRuntime, ProbeError> {
        Err(ProbeError::MissingProperty {
            property: "java.version",
        })
    }
}

struct FixedParallelism(NonZeroUsize);

impl ParallelismProvider for FixedParallelism {
    fn available_parallelism(&self) -> io::Result<NonZeroUsize> {
        Ok(self.0)
    }
}

struct FailingParallelism;

impl ParallelismProvider for FailingParallelism {
    fn available_parallelism(&self) -> io::Result<NonZeroUsize> {
        Err(io::Error::other("parallelism unavailable"))
    }
}

struct MemoryIo {
    inputs: Mutex<VecDeque<InputEvent>>,
    output: Mutex<String>,
    errors: Mutex<String>,
}

impl MemoryIo {
    fn new<const N: usize>(inputs: [InputEvent; N]) -> Self {
        Self {
            inputs: Mutex::new(VecDeque::from(inputs)),
            output: Mutex::new(String::new()),
            errors: Mutex::new(String::new()),
        }
    }

    fn errors(&self) -> String {
        self.errors.lock().unwrap().clone()
    }
}

impl InteractiveIo for MemoryIo {
    fn write_output(&self, message: &str) -> io::Result<()> {
        self.output.lock().unwrap().push_str(message);
        Ok(())
    }

    fn write_error(&self, message: &str) -> io::Result<()> {
        self.errors.lock().unwrap().push_str(message);
        Ok(())
    }

    fn read_line(&self) -> io::Result<InputEvent> {
        self.inputs
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| io::Error::other("test input exhausted"))
    }
}

struct InterruptedIo;

impl InteractiveIo for InterruptedIo {
    fn write_output(&self, _message: &str) -> io::Result<()> {
        Ok(())
    }

    fn write_error(&self, _message: &str) -> io::Result<()> {
        Ok(())
    }

    fn read_line(&self) -> io::Result<InputEvent> {
        Err(io::Error::new(io::ErrorKind::Interrupted, "Ctrl+C"))
    }
}

#[cfg(windows)]
fn timeout_command() -> (PathBuf, Vec<String>) {
    (
        PathBuf::from("ping.exe"),
        vec!["-n".to_string(), "6".to_string(), "127.0.0.1".to_string()],
    )
}

#[cfg(not(windows))]
fn timeout_command() -> (PathBuf, Vec<String>) {
    (PathBuf::from("/bin/sleep"), vec!["5".to_string()])
}

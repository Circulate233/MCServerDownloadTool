use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
use std::process::{Child, ExitStatus, Stdio};
#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use std::time::{Duration, Instant};

use mc_server_download_tool::loader::VerifiedLaunch;
use mc_server_download_tool::manifest::JavaConfig;
use mc_server_download_tool::scripts::{
    ScriptOutcome, ScriptPlatform, ScriptRequest, WindowsFailureBehavior, write_start_script,
};

fn java() -> JavaConfig {
    JavaConfig {
        major: 21,
        min_memory_mb: 16,
        max_memory_mb: 32,
        jvm_args: vec!["-Dserver.name=quoted value".to_string()],
        server_args: vec!["nogui".to_string()],
    }
}

fn compile_fake_java(directory: &Path, executable_name: &str) -> PathBuf {
    fs::create_dir_all(directory).unwrap();
    let source = directory.join("fake_java.rs");
    fs::write(
        &source,
        r#"
use std::{env, fs, process};

fn main() {
    let capture = env::var_os("MCSDT_FAKE_CAPTURE").expect("missing capture path");
    let mut values = vec![env::current_dir().unwrap().to_string_lossy().into_owned()];
    values.extend(env::args_os().skip(1).map(|value| value.to_string_lossy().into_owned()));
    fs::write(capture, values.join("\n")).unwrap();
    let code = env::var("MCSDT_FAKE_EXIT").unwrap().parse().unwrap();
    process::exit(code);
}
"#,
    )
    .unwrap();
    let executable = directory.join(executable_name);
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let status = Command::new(rustc)
        .args(["--edition=2024", "--crate-name", "mcsdt_fake_java"])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .status()
        .unwrap();
    assert!(status.success(), "failed to compile fake Java executable");
    executable
}

#[cfg(windows)]
fn wait_without_hanging(mut child: Child, timeout: Duration, scenario: &str) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("{scenario} did not exit within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(windows)]
fn spawn_interactive_windows(server_root: &Path, capture: &Path, exit_code: i32) -> Child {
    Command::new("cmd.exe")
        .args(["/d", "/q", "/k", "invoke-interactive.bat"])
        .current_dir(server_root)
        .env("MCSDT_FAKE_CAPTURE", capture)
        .env("MCSDT_FAKE_EXIT", exit_code.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[cfg(windows)]
fn spawn_cmd_windows(server_root: &Path, capture: &Path, exit_code: i32) -> Child {
    Command::new("cmd.exe")
        .args(["/d", "/q", "/c", "call start.bat"])
        .current_dir(server_root)
        .env("MCSDT_FAKE_CAPTURE", capture)
        .env("MCSDT_FAKE_EXIT", exit_code.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[cfg(windows)]
fn spawn_powershell_windows(server_root: &Path, capture: &Path, exit_code: i32) -> Child {
    Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "& '.\\start.bat'; exit $LASTEXITCODE",
        ])
        .current_dir(server_root)
        .env("MCSDT_FAKE_CAPTURE", capture)
        .env("MCSDT_FAKE_EXIT", exit_code.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[cfg(windows)]
fn assert_windows_capture(capture: &Path, server_root: &Path) {
    let captured = fs::read_to_string(capture).unwrap();
    let mut lines = captured.lines();
    assert_eq!(
        Path::new(lines.next().unwrap()).canonicalize().unwrap(),
        server_root.canonicalize().unwrap()
    );
    assert_eq!(
        lines.collect::<Vec<_>>(),
        [
            "-Xms16M",
            "-Xmx32M",
            "-D名称=值 空格!%PATH%&",
            "-jar",
            "核心 文件!%&.jar",
            "服务参数 空格!%TEMP%&"
        ]
    );
}

#[cfg(windows)]
fn compile_explorer_parent(directory: &Path) -> PathBuf {
    fs::create_dir_all(directory).unwrap();
    let source = directory.join("explorer_parent.rs");
    fs::write(
        &source,
        r#"
use std::process::{self, Command};

fn main() {
    let status = Command::new("cmd.exe")
        .args(["/d", "/q", "/c", "call start.bat"])
        .status()
        .unwrap();
    process::exit(status.code().unwrap_or(1));
}
"#,
    )
    .unwrap();
    let executable = directory.join("explorer.exe");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let status = Command::new(rustc)
        .args(["--edition=2024", "--crate-name", "mcsdt_explorer_parent"])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .status()
        .unwrap();
    assert!(status.success(), "failed to compile Explorer parent helper");
    executable
}

#[cfg(windows)]
fn assert_owned_windows_console_pauses(server_root: &Path, capture: &Path, explorer_parent: &Path) {
    use std::io::Write;

    let mut owned = Command::new(explorer_parent)
        .current_dir(server_root)
        .env("MCSDT_FAKE_CAPTURE", capture)
        .env("MCSDT_FAKE_EXIT", "41")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(400));
    let premature = owned.try_wait().unwrap();
    assert!(
        premature.is_none(),
        "an Explorer-owned cmd invocation must wait for a key; status={premature:?}"
    );
    owned.stdin.take().unwrap().write_all(b"x").unwrap();
    assert_eq!(
        wait_without_hanging(owned, Duration::from_secs(5), "Explorer-owned invocation").code(),
        Some(41)
    );
}

#[test]
fn modified_script_is_preserved_and_complete_replacement_is_published() {
    let temp = tempfile::tempdir().unwrap();
    let expected_root = temp.path().join("expected");
    let conflict_root = temp.path().join("conflict");
    fs::create_dir_all(&expected_root).unwrap();
    fs::create_dir_all(&conflict_root).unwrap();
    fs::write(conflict_root.join("start.bat"), b"user command\r\n").unwrap();
    let java = java();
    let launch = VerifiedLaunch::Jar {
        path: "fabric-server-launch.jar".into(),
    };
    let request = ScriptRequest {
        platform: ScriptPlatform::Windows,
        java_executable: Path::new("C:/Java/bin/java.exe"),
        java: &java,
        launch: &launch,
        windows_failure: WindowsFailureBehavior::PauseOwnedConsole,
        previous_script_sha256: None,
    };

    let expected = write_start_script(&expected_root, &request).unwrap();
    let conflict = write_start_script(&conflict_root, &request).unwrap();

    assert!(matches!(expected, ScriptOutcome::Published { .. }));
    assert!(matches!(conflict, ScriptOutcome::Conflict { .. }));
    assert_eq!(
        fs::read(conflict_root.join("start.bat")).unwrap(),
        b"user command\r\n"
    );
    assert_eq!(
        fs::read(conflict_root.join("start.bat.new")).unwrap(),
        fs::read(expected_root.join("start.bat")).unwrap()
    );
}

#[cfg(windows)]
#[test]
fn windows_script_executes_special_paths_and_applies_console_ownership_behavior() {
    let temp = tempfile::tempdir().unwrap();
    let server_root = temp.path().join("server 空格!%&");
    let java_root = temp.path().join("Java 运行时 空格!%&");
    fs::create_dir_all(&server_root).unwrap();
    let fake_java = compile_fake_java(&java_root, "fake Java 中文!%&.exe");
    let explorer_parent = compile_explorer_parent(&temp.path().join("explorer helper"));
    let capture = temp.path().join("captured arguments.txt");
    let java = JavaConfig {
        major: 21,
        min_memory_mb: 16,
        max_memory_mb: 32,
        jvm_args: vec!["-D名称=值 空格!%PATH%&".to_string()],
        server_args: vec!["服务参数 空格!%TEMP%&".to_string()],
    };
    let launch = VerifiedLaunch::Jar {
        path: "核心 文件!%&.jar".into(),
    };
    write_start_script(
        &server_root,
        &ScriptRequest {
            platform: ScriptPlatform::Windows,
            java_executable: &fake_java,
            java: &java,
            launch: &launch,
            windows_failure: WindowsFailureBehavior::PauseOwnedConsole,
            previous_script_sha256: None,
        },
    )
    .unwrap();

    fs::write(
        server_root.join("invoke-interactive.bat"),
        b"@echo off\r\ncall start.bat\r\nexit %ERRORLEVEL%\r\n",
    )
    .unwrap();
    let success = spawn_interactive_windows(&server_root, &capture, 0);
    assert_eq!(
        wait_without_hanging(
            success,
            Duration::from_secs(5),
            "successful interactive invocation"
        )
        .code(),
        Some(0)
    );
    assert_windows_capture(&capture, &server_root);

    let failure = spawn_interactive_windows(&server_root, &capture, 37);
    assert_eq!(
        wait_without_hanging(
            failure,
            Duration::from_secs(5),
            "failing interactive invocation"
        )
        .code(),
        Some(37),
        "an interactive terminal invocation must return without pausing"
    );

    let cmd_failure = spawn_cmd_windows(&server_root, &capture, 38);
    assert_eq!(
        wait_without_hanging(
            cmd_failure,
            Duration::from_secs(5),
            "ordinary cmd /c invocation"
        )
        .code(),
        Some(38),
        "a normal cmd /c invocation must return without pausing"
    );

    let powershell_failure = spawn_powershell_windows(&server_root, &capture, 39);
    assert_eq!(
        wait_without_hanging(
            powershell_failure,
            Duration::from_secs(5),
            "PowerShell invocation",
        )
        .code(),
        Some(39),
        "a PowerShell invocation must return without pausing"
    );

    assert_owned_windows_console_pauses(&server_root, &capture, &explorer_parent);
}

#[cfg(unix)]
#[test]
fn unix_script_preserves_shell_arguments_working_directory_and_exit_status() {
    let temp = tempfile::tempdir().unwrap();
    let server_root = temp.path().join("server 空格!%&'");
    let java_root = temp.path().join("Java runtime 空格!%&'");
    fs::create_dir_all(&server_root).unwrap();
    let fake_java = compile_fake_java(&java_root, "fake Java 中文!%&'");
    let capture = temp.path().join("captured arguments.txt");
    let java = JavaConfig {
        major: 21,
        min_memory_mb: 16,
        max_memory_mb: 32,
        jvm_args: vec!["-D名称=值 空格!%&'".to_string()],
        server_args: vec!["服务参数 空格!%&'".to_string()],
    };
    let launch = VerifiedLaunch::ArgsFiles {
        windows: "libraries/ignored win args.txt".into(),
        unix: "libraries/unix 参数!%&'.txt".into(),
    };
    write_start_script(
        &server_root,
        &ScriptRequest {
            platform: ScriptPlatform::Unix,
            java_executable: &fake_java,
            java: &java,
            launch: &launch,
            windows_failure: WindowsFailureBehavior::Return,
            previous_script_sha256: None,
        },
    )
    .unwrap();

    let status = Command::new("/bin/sh")
        .arg(server_root.join("start.sh"))
        .current_dir(temp.path())
        .env("MCSDT_FAKE_CAPTURE", &capture)
        .env("MCSDT_FAKE_EXIT", "29")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(29));
    let captured = fs::read_to_string(&capture).unwrap();
    let mut lines = captured.lines();
    assert_eq!(
        Path::new(lines.next().unwrap()).canonicalize().unwrap(),
        server_root.canonicalize().unwrap()
    );
    assert_eq!(
        lines.collect::<Vec<_>>(),
        [
            "-Xms16M",
            "-Xmx32M",
            "-D名称=值 空格!%&'",
            "@libraries/unix 参数!%&'.txt",
            "服务参数 空格!%&'"
        ]
    );
}

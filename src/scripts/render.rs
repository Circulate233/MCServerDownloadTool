use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::install::atomic;
use crate::loader::VerifiedLaunch;
use crate::manifest::JavaConfig;

/// Operating-system syntax selected for the generated start script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptPlatform {
    /// Windows `cmd.exe` batch syntax.
    Windows,
    /// POSIX-compatible shell syntax for Linux and macOS.
    Unix,
}

/// Explicit Windows console behavior after the Java process fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsFailureBehavior {
    /// Return Java's exit status without waiting for input.
    Return,
    /// Pause only when the batch file owns the interactive invocation.
    PauseOwnedConsole,
}

/// Inputs required to render a script containing only one exact Java launch.
#[derive(Debug, Clone)]
pub struct ScriptRequest<'a> {
    /// Selected script syntax.
    pub platform: ScriptPlatform,
    /// Exact Java executable path.
    pub java_executable: &'a Path,
    /// Heap sizes and additional JVM arguments.
    pub java: &'a JavaConfig,
    /// Strictly verified loader launch output.
    pub launch: &'a VerifiedLaunch,
    /// Windows-only failure behavior.
    pub windows_failure: WindowsFailureBehavior,
    /// Hash of the last primary script published by this tool, if any.
    pub previous_script_sha256: Option<&'a str>,
}

/// Result of ownership-aware script publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptOutcome {
    /// The primary script was created, updated, or already matched exactly.
    Published { path: PathBuf, sha256: String },
    /// A user-modified primary script was preserved and generated content was written to `.new`.
    Conflict {
        existing: PathBuf,
        generated: PathBuf,
        generated_sha256: String,
    },
}

/// Script rendering, path validation, and publication failures.
#[derive(Debug, Error)]
pub enum ScriptError {
    /// An argument cannot be represented safely in the selected shell syntax.
    #[error("unsafe script argument: {reason}")]
    UnsafeArgument { reason: String },
    /// A concrete script filesystem operation failed.
    #[error("failed to {operation} script '{path}': {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Renders and atomically publishes a start script while preserving user edits.
///
/// The script never downloads files, runs an installer, or writes an EULA. When
/// the primary path differs from the prior tool-owned hash, it is left untouched
/// and complete generated content is atomically published to `<name>.new`.
///
/// # Errors
///
/// Returns [`ScriptError`] when arguments are unsafe or filesystem publication fails.
pub fn write_start_script(
    server_root: &Path,
    request: &ScriptRequest<'_>,
) -> Result<ScriptOutcome, ScriptError> {
    let (name, bytes) = match request.platform {
        ScriptPlatform::Windows => ("start.bat", render_windows(request)?.into_bytes()),
        ScriptPlatform::Unix => ("start.sh", render_unix(request)?.into_bytes()),
    };
    let target = server_root.join(name);
    let generated_hash = sha256(&bytes);
    let existing = match fs::read(&target) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(ScriptError::Io {
                operation: "read",
                path: target,
                source,
            });
        }
    };
    if let Some(existing) = existing {
        let existing_hash = sha256(&existing);
        let tool_owned = existing_hash == generated_hash
            || request
                .previous_script_sha256
                .is_some_and(|previous| previous.eq_ignore_ascii_case(&existing_hash));
        if !tool_owned {
            let generated = server_root.join(format!("{name}.new"));
            atomic::write(&generated, &bytes).map_err(|source| ScriptError::Io {
                operation: "atomically write generated conflict",
                path: generated.clone(),
                source,
            })?;
            return Ok(ScriptOutcome::Conflict {
                existing: target,
                generated,
                generated_sha256: generated_hash,
            });
        }
        if existing_hash == generated_hash {
            return Ok(ScriptOutcome::Published {
                path: target,
                sha256: generated_hash,
            });
        }
    }
    atomic::write(&target, &bytes).map_err(|source| ScriptError::Io {
        operation: "atomically write",
        path: target.clone(),
        source,
    })?;
    set_unix_executable(&target, request.platform)?;
    Ok(ScriptOutcome::Published {
        path: target,
        sha256: generated_hash,
    })
}

fn render_windows(request: &ScriptRequest<'_>) -> Result<String, ScriptError> {
    let mut arguments = common_arguments(request);
    append_launch(&mut arguments, request.launch, ScriptPlatform::Windows);
    arguments.extend(request.java.server_args.iter().cloned());
    let mut command = quote_windows(
        request
            .java_executable
            .as_os_str()
            .to_string_lossy()
            .as_ref(),
    )?;
    for argument in arguments {
        command.push(' ');
        command.push_str(&quote_windows(&argument)?);
    }
    let mut script = String::from("@echo off\r\nsetlocal DisableDelayedExpansion\r\n");
    if request.windows_failure == WindowsFailureBehavior::PauseOwnedConsole {
        script.push_str(
            "set \"MCSDT_OWNED_CONSOLE=0\"\r\n\
for /f \"usebackq delims=\" %%# in (`powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"$ErrorActionPreference = 'Stop'; $probe = Start-Job -ArgumentList $PID -ScriptBlock { param($probePid) $self = Get-CimInstance Win32_Process -Filter ('ProcessId=' + $probePid); $cmd = Get-CimInstance Win32_Process -Filter ('ProcessId=' + $self.ParentProcessId); if ($cmd.Name -ine 'cmd.exe') { return }; $owner = Get-CimInstance Win32_Process -Filter ('ProcessId=' + $cmd.ParentProcessId); if ($owner.Name -ieq 'explorer.exe') { [Console]::Out.Write('1') } }; if ($null -ne (Wait-Job $probe -Timeout 3)) { Receive-Job $probe }; Stop-Job $probe -ErrorAction SilentlyContinue; Remove-Job $probe -Force -ErrorAction SilentlyContinue\" 2^>nul`) do if \"%%#\"==\"1\" set \"MCSDT_OWNED_CONSOLE=1\"\r\n",
        );
    }
    let execution = format!(
        "for /f \"tokens=2 delims=:\" %%# in ('chcp') do set \"MCSDT_CODEPAGE=%%#\"\r\n\
chcp 65001 >nul\r\n\
pushd \"%~dp0\" || exit /b 1\r\n\
{command}\r\n\
set \"MCSDT_EXIT=%ERRORLEVEL%\"\r\n\
popd\r\n\
if defined MCSDT_CODEPAGE chcp %MCSDT_CODEPAGE% >nul\r\n"
    );
    script.push_str(&execution);
    if request.windows_failure == WindowsFailureBehavior::PauseOwnedConsole {
        script.push_str(
            "if not \"%MCSDT_EXIT%\"==\"0\" if \"%MCSDT_OWNED_CONSOLE%\"==\"1\" pause\r\n",
        );
    }
    script.push_str("exit /b %MCSDT_EXIT%\r\n");
    Ok(script)
}

fn render_unix(request: &ScriptRequest<'_>) -> Result<String, ScriptError> {
    let mut arguments = common_arguments(request);
    append_launch(&mut arguments, request.launch, ScriptPlatform::Unix);
    arguments.extend(request.java.server_args.iter().cloned());
    let mut command = quote_unix(
        request
            .java_executable
            .as_os_str()
            .to_string_lossy()
            .as_ref(),
    )?;
    for argument in arguments {
        command.push(' ');
        command.push_str(&quote_unix(&argument)?);
    }
    Ok(format!(
        "#!/bin/sh\n\
case $0 in\n\
  */*) MCSDT_SCRIPT_DIR=${{0%/*}} ;;\n\
  *) MCSDT_SCRIPT_DIR=. ;;\n\
esac\n\
CDPATH= cd -P -- \"$MCSDT_SCRIPT_DIR\" || exit 1\n\
unset MCSDT_SCRIPT_DIR\n\
exec {command}\n"
    ))
}

fn common_arguments(request: &ScriptRequest<'_>) -> Vec<String> {
    let mut arguments = vec![
        format!("-Xms{}M", request.java.min_memory_mb),
        format!("-Xmx{}M", request.java.max_memory_mb),
    ];
    arguments.extend(request.java.jvm_args.iter().cloned());
    arguments
}

fn append_launch(arguments: &mut Vec<String>, launch: &VerifiedLaunch, platform: ScriptPlatform) {
    match launch {
        VerifiedLaunch::ArgsFiles { windows, unix } => {
            let path = match platform {
                ScriptPlatform::Windows => windows,
                ScriptPlatform::Unix => unix,
            };
            arguments.push(format!("@{}", slash_path(path)));
        }
        VerifiedLaunch::Jar { path } => {
            arguments.push("-jar".to_string());
            arguments.push(slash_path(path));
        }
    }
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn quote_windows(value: &str) -> Result<String, ScriptError> {
    reject_line_breaks(value)?;
    if value.contains('"') {
        return Err(ScriptError::UnsafeArgument {
            reason: "double quotes cannot be represented safely in a batch argument".to_string(),
        });
    }
    let escaped = value.replace('%', "%%");
    Ok(format!("\"{escaped}\""))
}

fn quote_unix(value: &str) -> Result<String, ScriptError> {
    reject_line_breaks(value)?;
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

fn reject_line_breaks(value: &str) -> Result<(), ScriptError> {
    if value.contains(['\r', '\n', '\0']) {
        return Err(ScriptError::UnsafeArgument {
            reason: "NUL and line breaks are forbidden".to_string(),
        });
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(unix)]
fn set_unix_executable(path: &Path, platform: ScriptPlatform) -> Result<(), ScriptError> {
    use std::os::unix::fs::PermissionsExt;
    if platform == ScriptPlatform::Unix {
        let mut permissions = fs::metadata(path)
            .map_err(|source| ScriptError::Io {
                operation: "read permissions for",
                path: path.to_path_buf(),
                source,
            })?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).map_err(|source| ScriptError::Io {
            operation: "set executable permissions on",
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_unix_executable(path: &Path, _platform: ScriptPlatform) -> Result<(), ScriptError> {
    fs::metadata(path).map_err(|source| ScriptError::Io {
        operation: "confirm script publication for",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

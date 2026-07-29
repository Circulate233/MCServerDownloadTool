use std::io::Read;
use std::process::{ChildStderr, ChildStdout, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use command_group::{CommandGroup, GroupChild};

use crate::cli::ProxyUrl;

use super::model::{
    LoaderError, LoaderFamily, LoaderInstallation, LoaderPlan, ProcessObserver,
    ProcessObserverError, ProcessRequest, ProcessRunner, ProcessStream, VerifiedLaunch,
};
use super::verify::verify_loader_output;

/// Production process runner using shell-free Java execution and concurrent stream forwarding.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProcessRunner;

const LOADER_TIMEOUT: Duration = Duration::from_mins(30);
const MAX_LOADER_LINE_BYTES: usize = 64 * 1024;
const MAX_LOADER_STREAM_BYTES: usize = 16 * 1024 * 1024;

impl ProcessRunner for SystemProcessRunner {
    fn run(
        &self,
        request: &ProcessRequest,
        observer: Arc<dyn ProcessObserver>,
    ) -> Result<(), LoaderError> {
        let deadline = Instant::now().checked_add(request.timeout).ok_or_else(|| {
            LoaderError::InvalidPlan {
                reason: format!("loader timeout {:?} is not representable", request.timeout),
            }
        })?;
        let max_line_bytes = request.max_line_bytes;
        let max_stream_bytes = request.max_stream_bytes;
        let (mut child, stdout, stderr) = spawn_process(request)?;

        let (failure_sender, failure_receiver) = mpsc::channel();
        let stdout_observer = Arc::clone(&observer);
        let stdout_sender = failure_sender.clone();
        let stdout_worker = thread::spawn(move || {
            forward_and_report(
                stdout,
                ProcessStream::Stdout,
                &stdout_observer,
                &stdout_sender,
                max_line_bytes,
                max_stream_bytes,
            )
        });
        let stderr_worker = thread::spawn(move || {
            forward_and_report(
                stderr,
                ProcessStream::Stderr,
                &observer,
                &failure_sender,
                max_line_bytes,
                max_stream_bytes,
            )
        });
        let status = loop {
            match failure_receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(failure) => {
                    let cleanup_error = terminate_child(&mut child);
                    let stdout_result = join_forwarder(stdout_worker);
                    let stderr_result = join_forwarder(stderr_worker);
                    let _ = stdout_result;
                    let _ = stderr_result;
                    return Err(failure.into_loader_error(cleanup_error));
                }
                Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
            }
            if let Some(status) = child.try_wait().map_err(|source| LoaderError::ProcessIo {
                operation: "query status of",
                source,
            })? {
                break status;
            }
            if Instant::now() >= deadline {
                let cleanup_error = terminate_child(&mut child);
                let _ = join_forwarder(stdout_worker);
                let _ = join_forwarder(stderr_worker);
                return Err(LoaderError::ProcessTimedOut {
                    timeout: request.timeout,
                    cleanup_error,
                });
            }
        };
        join_forwarder(stdout_worker)?;
        join_forwarder(stderr_worker)?;
        if !status.success() {
            return Err(LoaderError::ProcessFailed {
                status: status.to_string(),
            });
        }
        Ok(())
    }
}

fn spawn_process(
    request: &ProcessRequest,
) -> Result<(GroupChild, ChildStdout, ChildStderr), LoaderError> {
    let mut command = Command::new(&request.executable);
    command
        .args(&request.arguments)
        .current_dir(&request.working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for variable in [
        "_JAVA_OPTIONS",
        "JAVA_TOOL_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "CLASSPATH",
    ] {
        command.env_remove(variable);
    }
    let mut child = command
        .group_spawn()
        .map_err(|source| LoaderError::ProcessIo {
            operation: "spawn",
            source,
        })?;
    let stdout = child
        .inner()
        .stdout
        .take()
        .ok_or_else(|| LoaderError::ProcessIo {
            operation: "capture stdout",
            source: std::io::Error::other("spawned process did not expose stdout"),
        })?;
    let stderr = child
        .inner()
        .stderr
        .take()
        .ok_or_else(|| LoaderError::ProcessIo {
            operation: "capture stderr",
            source: std::io::Error::other("spawned process did not expose stderr"),
        })?;
    Ok((child, stdout, stderr))
}

fn forward_lines(
    mut stream: impl Read,
    kind: ProcessStream,
    observer: &Arc<dyn ProcessObserver>,
    max_line_bytes: usize,
    max_stream_bytes: usize,
) -> Result<(), ForwardFailure> {
    let mut chunk = [0_u8; 16 * 1024];
    let mut line = Vec::new();
    let mut total = 0_usize;
    loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|error| ForwardFailure::from_io(&error))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        if total > max_stream_bytes {
            return Err(ForwardFailure::Limit {
                kind: "stream",
                limit: max_stream_bytes,
            });
        }
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                observer
                    .line(kind, String::from_utf8_lossy(&line).into_owned())
                    .map_err(ForwardFailure::Observer)?;
                line.clear();
            } else {
                line.push(*byte);
                if line.len() > max_line_bytes {
                    return Err(ForwardFailure::Limit {
                        kind: "line",
                        limit: max_line_bytes,
                    });
                }
            }
        }
    }
    if !line.is_empty() {
        observer
            .line(kind, String::from_utf8_lossy(&line).into_owned())
            .map_err(ForwardFailure::Observer)?;
    }
    Ok(())
}

fn forward_and_report(
    stream: impl std::io::Read,
    kind: ProcessStream,
    observer: &Arc<dyn ProcessObserver>,
    failure_sender: &mpsc::Sender<ForwardFailure>,
    max_line_bytes: usize,
    max_stream_bytes: usize,
) -> Result<(), ForwardFailure> {
    let result = forward_lines(stream, kind, observer, max_line_bytes, max_stream_bytes);
    if let Err(error) = &result
        && failure_sender.send(error.clone()).is_err()
    {
        eprintln!("loader output failure receiver was dropped before notification");
    }
    result
}

fn join_forwarder(
    worker: thread::JoinHandle<Result<(), ForwardFailure>>,
) -> Result<(), LoaderError> {
    worker
        .join()
        .map_err(|_| LoaderError::ProcessIo {
            operation: "join output forwarding thread",
            source: std::io::Error::other("output forwarding thread panicked"),
        })?
        .map_err(|failure| failure.into_loader_error(None))
}

fn terminate_child(child: &mut GroupChild) -> Option<String> {
    let kill = child.kill().err().map(|error| error.to_string());
    let wait = child.wait().err().map(|error| error.to_string());
    match (kill, wait) {
        (None, None) => None,
        (Some(kill), None) => Some(format!("terminate failed: {kill}")),
        (None, Some(wait)) => Some(format!("reap failed: {wait}")),
        (Some(kill), Some(wait)) => Some(format!("terminate failed: {kill}; reap failed: {wait}")),
    }
}

#[derive(Debug, Clone)]
enum ForwardFailure {
    Read {
        kind: std::io::ErrorKind,
        reason: String,
    },
    Observer(ProcessObserverError),
    Limit {
        kind: &'static str,
        limit: usize,
    },
}

impl ForwardFailure {
    fn from_io(error: &std::io::Error) -> Self {
        Self::Read {
            kind: error.kind(),
            reason: error.to_string(),
        }
    }

    fn into_loader_error(self, cleanup_error: Option<String>) -> LoaderError {
        match self {
            Self::Read { kind, reason } => LoaderError::ProcessIo {
                operation: "stream output from",
                source: std::io::Error::new(kind, reason),
            },
            Self::Observer(source) => LoaderError::Observer { source },
            Self::Limit { kind, limit } => LoaderError::OutputLimit {
                stream: "stdout/stderr",
                kind,
                limit,
                cleanup_error,
            },
        }
    }
}

/// Loader installer backed by an injectable shell-free process runner.
#[derive(Debug, Clone)]
pub struct LoaderExecutor<R> {
    runner: R,
}

impl<R> LoaderExecutor<R> {
    /// Creates a loader executor using `runner` for Java process activity.
    pub const fn new(runner: R) -> Self {
        Self { runner }
    }
}

impl<R: ProcessRunner> LoaderInstallation for LoaderExecutor<R> {
    fn install(
        &self,
        plan: &LoaderPlan,
        server_root: &std::path::Path,
        java_executable: &std::path::Path,
        installer_jar: &std::path::Path,
        proxy: Option<&ProxyUrl>,
        observer: Arc<dyn ProcessObserver>,
    ) -> Result<VerifiedLaunch, LoaderError> {
        plan.validate()?;
        let mut arguments = proxy_arguments(proxy)?;
        arguments.push("-jar".to_string());
        arguments.push(installer_jar.as_os_str().to_string_lossy().into_owned());
        match plan.family {
            LoaderFamily::Forge | LoaderFamily::NeoForge | LoaderFamily::Cleanroom => {
                arguments.push("--installServer".to_string());
            }
            LoaderFamily::Fabric => {
                arguments.extend([
                    "server".to_string(),
                    "-mcversion".to_string(),
                    plan.minecraft_version.clone(),
                    "-loader".to_string(),
                    plan.loader_version.clone(),
                    "-downloadMinecraft".to_string(),
                ]);
            }
        }
        self.runner.run(
            &ProcessRequest {
                executable: java_executable.to_path_buf(),
                arguments,
                working_directory: server_root.to_path_buf(),
                timeout: LOADER_TIMEOUT,
                max_line_bytes: MAX_LOADER_LINE_BYTES,
                max_stream_bytes: MAX_LOADER_STREAM_BYTES,
            },
            observer,
        )?;
        verify_loader_output(server_root, &plan.output)
    }
}

fn proxy_arguments(proxy: Option<&ProxyUrl>) -> Result<Vec<String>, LoaderError> {
    let Some(proxy) = proxy else {
        return Ok(Vec::new());
    };
    let url = proxy.as_url();
    let host = url.host_str().ok_or_else(|| LoaderError::InvalidPlan {
        reason: "validated proxy URL unexpectedly has no host".to_string(),
    })?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| LoaderError::InvalidPlan {
            reason: "validated proxy URL unexpectedly has no effective port".to_string(),
        })?;
    match url.scheme() {
        "http" | "https" => Ok(vec![
            format!("-Dhttp.proxyHost={host}"),
            format!("-Dhttp.proxyPort={port}"),
            format!("-Dhttps.proxyHost={host}"),
            format!("-Dhttps.proxyPort={port}"),
        ]),
        "socks5" | "socks5h" => Ok(vec![
            format!("-DsocksProxyHost={host}"),
            format!("-DsocksProxyPort={port}"),
        ]),
        scheme => Err(LoaderError::UnsupportedProxy {
            scheme: scheme.to_string(),
        }),
    }
}

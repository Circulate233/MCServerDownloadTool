use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use crate::cli::ProxyUrl;

use super::model::{
    LoaderError, LoaderFamily, LoaderInstallation, LoaderPlan, ProcessObserver,
    ProcessObserverError, ProcessRequest, ProcessRunner, ProcessStream, VerifiedLaunch,
};
use super::verify::verify_loader_output;

/// Production process runner using shell-free Java execution and concurrent stream forwarding.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn run(
        &self,
        request: &ProcessRequest,
        observer: Arc<dyn ProcessObserver>,
    ) -> Result<(), LoaderError> {
        let mut child = Command::new(&request.executable)
            .args(&request.arguments)
            .current_dir(&request.working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| LoaderError::ProcessIo {
                operation: "spawn",
                source,
            })?;
        let stdout = child.stdout.take().ok_or_else(|| LoaderError::ProcessIo {
            operation: "capture stdout",
            source: std::io::Error::other("spawned process did not expose stdout"),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| LoaderError::ProcessIo {
            operation: "capture stderr",
            source: std::io::Error::other("spawned process did not expose stderr"),
        })?;

        let (failure_sender, failure_receiver) = mpsc::channel();
        let stdout_observer = Arc::clone(&observer);
        let stdout_sender = failure_sender.clone();
        let stdout_worker = thread::spawn(move || {
            forward_and_report(
                stdout,
                ProcessStream::Stdout,
                &stdout_observer,
                &stdout_sender,
            )
        });
        let stderr_worker = thread::spawn(move || {
            forward_and_report(stderr, ProcessStream::Stderr, &observer, &failure_sender)
        });
        let status = loop {
            match failure_receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(failure) => {
                    terminate_child(&mut child)?;
                    let stdout_result = join_forwarder(stdout_worker);
                    let stderr_result = join_forwarder(stderr_worker);
                    stdout_result?;
                    stderr_result?;
                    return Err(failure.into_loader_error());
                }
                Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
            }
            if let Some(status) = child.try_wait().map_err(|source| LoaderError::ProcessIo {
                operation: "query status of",
                source,
            })? {
                break status;
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

fn forward_lines(
    stream: impl std::io::Read,
    kind: ProcessStream,
    observer: &Arc<dyn ProcessObserver>,
) -> Result<(), ForwardFailure> {
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|error| ForwardFailure::from_io(&error))?;
        observer
            .line(kind, line)
            .map_err(ForwardFailure::Observer)?;
    }
    Ok(())
}

fn forward_and_report(
    stream: impl std::io::Read,
    kind: ProcessStream,
    observer: &Arc<dyn ProcessObserver>,
    failure_sender: &mpsc::Sender<ForwardFailure>,
) -> Result<(), ForwardFailure> {
    let result = forward_lines(stream, kind, observer);
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
        .map_err(ForwardFailure::into_loader_error)
}

fn terminate_child(child: &mut std::process::Child) -> Result<(), LoaderError> {
    match child.kill() {
        Ok(()) => child
            .wait()
            .map(|_| ())
            .map_err(|source| LoaderError::ProcessIo {
                operation: "reap terminated",
                source,
            }),
        Err(kill_error) => match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) | Err(_) => Err(LoaderError::ProcessIo {
                operation: "terminate after output observer failure for",
                source: kill_error,
            }),
        },
    }
}

#[derive(Debug, Clone)]
enum ForwardFailure {
    Read {
        kind: std::io::ErrorKind,
        reason: String,
    },
    Observer(ProcessObserverError),
}

impl ForwardFailure {
    fn from_io(error: &std::io::Error) -> Self {
        Self::Read {
            kind: error.kind(),
            reason: error.to_string(),
        }
    }

    fn into_loader_error(self) -> LoaderError {
        match self {
            Self::Read { kind, reason } => LoaderError::ProcessIo {
                operation: "stream output from",
                source: std::io::Error::new(kind, reason),
            },
            Self::Observer(source) => LoaderError::Observer { source },
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

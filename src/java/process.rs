use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

/// Environment handling applied before a child process starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentPolicy {
    /// Preserve the caller's environment unchanged.
    Inherit,
    /// Remove variables that can inject JVM options or class paths.
    CleanJava,
}

/// Complete, shell-free child-process request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRequest {
    /// Executable to launch directly.
    pub program: PathBuf,
    /// Arguments passed as individual operating-system strings.
    pub arguments: Vec<OsString>,
    /// Maximum wall-clock duration before the child is terminated.
    pub timeout: Duration,
    /// Environment policy for the child.
    pub environment: EnvironmentPolicy,
}

impl ProcessRequest {
    /// Creates a request with no arguments and an inherited environment.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            timeout,
            environment: EnvironmentPolicy::Inherit,
        }
    }

    /// Replaces the argument list without shell parsing.
    #[must_use]
    pub fn with_arguments<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.arguments = arguments
            .into_iter()
            .map(|argument| argument.as_ref().to_os_string())
            .collect();
        self
    }

    /// Selects how the child process environment is prepared.
    #[must_use]
    pub const fn with_environment(mut self, environment: EnvironmentPolicy) -> Self {
        self.environment = environment;
        self
    }
}

/// Captured output from one completed child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    /// Platform exit status code, or `None` when the platform cannot provide one.
    pub exit_code: Option<i32>,
    /// Bytes written to standard output.
    pub stdout: Vec<u8>,
    /// Bytes written to standard error.
    pub stderr: Vec<u8>,
}

/// Failures produced while starting, supervising, or collecting a child process.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// The executable could not be started.
    #[error("failed to start process '{}': {source}", program.display())]
    Spawn {
        /// Requested executable.
        program: PathBuf,
        /// Operating-system failure.
        #[source]
        source: io::Error,
    },
    /// The requested timeout cannot be represented by the monotonic clock.
    #[error("process timeout {timeout:?} is too large for '{}'", program.display())]
    InvalidTimeout {
        /// Requested executable.
        program: PathBuf,
        /// Unrepresentable timeout.
        timeout: Duration,
    },
    /// Process state could not be queried.
    #[error("failed to poll process '{}': {source}", program.display())]
    Poll {
        /// Requested executable.
        program: PathBuf,
        /// Operating-system failure.
        #[source]
        source: io::Error,
    },
    /// A pipe-reader thread could not be created.
    #[error("failed to start {stream} reader for '{}': {source}", program.display())]
    ReaderSpawn {
        /// Requested executable.
        program: PathBuf,
        /// Pipe name.
        stream: &'static str,
        /// Thread creation failure.
        #[source]
        source: io::Error,
    },
    /// Capturing a child stream failed.
    #[error("failed to read {stream} from '{}': {source}", program.display())]
    Read {
        /// Requested executable.
        program: PathBuf,
        /// Pipe name.
        stream: &'static str,
        /// Read failure.
        #[source]
        source: io::Error,
    },
    /// A pipe-reader thread panicked.
    #[error("{stream} reader panicked for process '{}'", program.display())]
    ReaderPanic {
        /// Requested executable.
        program: PathBuf,
        /// Pipe name.
        stream: &'static str,
    },
    /// A configured child pipe was unexpectedly unavailable after spawning.
    #[error(
        "{stream} pipe was unavailable for process '{}'{cleanup}",
        program.display(),
        cleanup = cleanup_error
            .as_deref()
            .map_or_else(String::new, |error| format!("; cleanup error: {error}"))
    )]
    MissingPipe {
        /// Requested executable.
        program: PathBuf,
        /// Pipe name.
        stream: &'static str,
        /// Failure observed while terminating or reaping the child, when any.
        cleanup_error: Option<String>,
    },
    /// The timeout elapsed and the child was terminated.
    #[error(
        "process '{}' exceeded {timeout:?} and was terminated{cleanup}",
        program.display(),
        cleanup = cleanup_error
            .as_deref()
            .map_or_else(String::new, |error| format!("; cleanup error: {error}"))
    )]
    TimedOut {
        /// Requested executable.
        program: PathBuf,
        /// Configured timeout.
        timeout: Duration,
        /// Failure observed while terminating or reaping the child, when any.
        cleanup_error: Option<String>,
    },
}

/// Process execution boundary used by discovery and Java inspection.
///
/// Implementations must execute requests without shell interpretation, honor
/// the timeout, capture both output streams, and terminate timed-out children.
pub trait ProcessRunner: Send + Sync {
    /// Executes one request and returns its captured output.
    ///
    /// # Errors
    ///
    /// Returns a [`ProcessError`] when the child cannot be started, supervised,
    /// terminated, or read completely.
    fn run(&self, request: &ProcessRequest) -> Result<ProcessOutput, ProcessError>;
}

/// Operating-system process runner backed by [`std::process::Command`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    #[allow(clippy::too_many_lines)]
    fn run(&self, request: &ProcessRequest) -> Result<ProcessOutput, ProcessError> {
        let deadline = Instant::now().checked_add(request.timeout).ok_or_else(|| {
            ProcessError::InvalidTimeout {
                program: request.program.clone(),
                timeout: request.timeout,
            }
        })?;
        let mut command = Command::new(&request.program);
        command
            .args(&request.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if request.environment == EnvironmentPolicy::CleanJava {
            for variable in [
                "_JAVA_OPTIONS",
                "JAVA_TOOL_OPTIONS",
                "JDK_JAVA_OPTIONS",
                "CLASSPATH",
            ] {
                command.env_remove(variable);
            }
        }

        let mut child = command.spawn().map_err(|source| ProcessError::Spawn {
            program: request.program.clone(),
            source,
        })?;
        let Some(stdout) = child.stdout.take() else {
            let cleanup_error = terminate_and_reap(&mut child);
            return Err(ProcessError::MissingPipe {
                program: request.program.clone(),
                stream: "stdout",
                cleanup_error,
            });
        };
        let Some(stderr) = child.stderr.take() else {
            let cleanup_error = terminate_and_reap(&mut child);
            return Err(ProcessError::MissingPipe {
                program: request.program.clone(),
                stream: "stderr",
                cleanup_error,
            });
        };

        let stdout_reader = spawn_reader(stdout, "stdout", &request.program).map_err(|error| {
            let cleanup_error = terminate_and_reap(&mut child);
            match error {
                ProcessError::ReaderSpawn { source, .. } => ProcessError::ReaderSpawn {
                    program: request.program.clone(),
                    stream: "stdout",
                    source: io::Error::new(
                        source.kind(),
                        append_cleanup(source.to_string(), cleanup_error),
                    ),
                },
                other => other,
            }
        })?;
        let stderr_reader = match spawn_reader(stderr, "stderr", &request.program) {
            Ok(reader) => reader,
            Err(error) => {
                let cleanup_error = terminate_and_reap(&mut child);
                let _ = stdout_reader.join();
                return Err(match error {
                    ProcessError::ReaderSpawn { source, .. } => ProcessError::ReaderSpawn {
                        program: request.program.clone(),
                        stream: "stderr",
                        source: io::Error::new(
                            source.kind(),
                            append_cleanup(source.to_string(), cleanup_error),
                        ),
                    },
                    other => other,
                });
            }
        };

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(source) => {
                    let cleanup_error = terminate_and_reap(&mut child);
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(ProcessError::Poll {
                        program: request.program.clone(),
                        source: io::Error::new(
                            source.kind(),
                            append_cleanup(source.to_string(), cleanup_error),
                        ),
                    });
                }
            }

            if Instant::now() >= deadline {
                let cleanup_error = terminate_and_reap(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(ProcessError::TimedOut {
                    program: request.program.clone(),
                    timeout: request.timeout,
                    cleanup_error,
                });
            }
            thread::sleep(Duration::from_millis(10));
        };

        let stdout = join_reader(stdout_reader, "stdout", &request.program)?;
        let stderr = join_reader(stderr_reader, "stderr", &request.program)?;
        Ok(ProcessOutput {
            exit_code: status.code(),
            stdout,
            stderr,
        })
    }
}

fn spawn_reader<R>(
    mut stream: R,
    name: &'static str,
    program: &std::path::Path,
) -> Result<thread::JoinHandle<io::Result<Vec<u8>>>, ProcessError>
where
    R: Read + Send + 'static,
{
    thread::Builder::new()
        .name(format!("java-{name}-reader"))
        .spawn(move || {
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .map_err(|source| ProcessError::ReaderSpawn {
            program: program.to_path_buf(),
            stream: name,
            source,
        })
}

fn join_reader(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    name: &'static str,
    program: &std::path::Path,
) -> Result<Vec<u8>, ProcessError> {
    reader
        .join()
        .map_err(|_| ProcessError::ReaderPanic {
            program: program.to_path_buf(),
            stream: name,
        })?
        .map_err(|source| ProcessError::Read {
            program: program.to_path_buf(),
            stream: name,
            source,
        })
}

fn terminate_and_reap(child: &mut std::process::Child) -> Option<String> {
    let kill_error = child.kill().err().map(|error| error.to_string());
    let wait_error = child.wait().err().map(|error| error.to_string());
    match (kill_error, wait_error) {
        (None, None) => None,
        (Some(kill), None) => Some(format!("terminate failed: {kill}")),
        (None, Some(wait)) => Some(format!("reap failed: {wait}")),
        (Some(kill), Some(wait)) => Some(format!("terminate failed: {kill}; reap failed: {wait}")),
    }
}

fn append_cleanup(message: String, cleanup_error: Option<String>) -> String {
    match cleanup_error {
        Some(cleanup) => format!("{message}; cleanup error: {cleanup}"),
        None => message,
    }
}

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use thiserror::Error;

use super::process::{EnvironmentPolicy, ProcessError, ProcessRequest, ProcessRunner};

/// Maximum duration of one Java metadata probe.
pub const JAVA_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Verified metadata reported by a Java executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaRuntime {
    /// Canonical or explicitly validated Java executable path.
    pub executable: PathBuf,
    /// Complete `java.version` property.
    pub version: String,
    /// Parsed Java feature-release number, with `1.8` normalized to `8`.
    pub major: u16,
    /// Complete `java.vendor` property.
    pub vendor: String,
    /// Complete `os.arch` property.
    pub architecture: String,
}

impl JavaRuntime {
    /// Returns whether the reported architecture is a recognized 64-bit JVM.
    #[must_use]
    pub fn is_64_bit(&self) -> bool {
        matches!(
            self.architecture.trim().to_ascii_lowercase().as_str(),
            "amd64"
                | "x86_64"
                | "aarch64"
                | "arm64"
                | "ppc64"
                | "ppc64le"
                | "s390x"
                | "sparcv9"
                | "riscv64"
        )
    }
}

/// Failure while executing or parsing one Java candidate.
#[derive(Debug, Error)]
pub enum ProbeError {
    /// The Java process could not complete under the process contract.
    #[error(transparent)]
    Process(#[from] ProcessError),
    /// Java returned a non-success exit status.
    #[error("Java metadata probe exited with status {status:?}: {stderr}")]
    ExitStatus {
        /// Platform exit status.
        status: Option<i32>,
        /// Trimmed standard error for diagnosis.
        stderr: String,
    },
    /// A required property was absent from `-XshowSettings:properties` output.
    #[error("Java metadata probe did not report required property '{property}'")]
    MissingProperty {
        /// Missing property name.
        property: &'static str,
    },
    /// The reported Java version cannot be mapped to a supported major number.
    #[error("invalid java.version property '{version}'")]
    InvalidVersion {
        /// Invalid property value.
        version: String,
    },
}

/// Java metadata inspection boundary used by parallel probing and manual input.
pub trait RuntimeProbe: Send + Sync {
    /// Executes and validates one Java executable.
    ///
    /// # Errors
    ///
    /// Returns [`ProbeError`] when the process fails, times out, returns a
    /// non-success status, or omits/mangles a required property.
    fn inspect(&self, executable: &Path) -> Result<JavaRuntime, ProbeError>;
}

/// Runtime probe that invokes Java through a reusable process runner.
#[derive(Debug)]
pub struct JavaCommandProbe<R> {
    process: Arc<R>,
}

impl<R> JavaCommandProbe<R> {
    /// Creates a Java metadata probe over the supplied process runner.
    #[must_use]
    pub fn new(process: Arc<R>) -> Self {
        Self { process }
    }
}

impl<R> RuntimeProbe for JavaCommandProbe<R>
where
    R: ProcessRunner,
{
    fn inspect(&self, executable: &Path) -> Result<JavaRuntime, ProbeError> {
        let request = ProcessRequest::new(executable, JAVA_PROBE_TIMEOUT)
            .with_arguments(["-XshowSettings:properties", "-version"])
            .with_environment(EnvironmentPolicy::CleanJava);
        let output = self.process.run(&request)?;
        if output.exit_code != Some(0) {
            return Err(ProbeError::ExitStatus {
                status: output.exit_code,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        let mut metadata = output.stderr;
        metadata.push(b'\n');
        metadata.extend(output.stdout);
        parse_java_properties(executable, &metadata)
    }
}

/// Rejected candidate and the probe or version reason that rejected it.
#[derive(Debug)]
pub struct ProbeRejection {
    /// Candidate executable path.
    pub executable: PathBuf,
    /// Human-readable rejection reason.
    pub reason: ProbeRejectionReason,
}

/// Structured reason that one Java candidate cannot satisfy the manifest.
#[derive(Debug)]
pub enum ProbeRejectionReason {
    /// Java ran successfully but reported a different feature release.
    FeatureReleaseMismatch {
        /// Java feature release reported by the candidate.
        found: u16,
        /// Exact feature release required by the manifest.
        required: u16,
    },
    /// Starting or inspecting the Java candidate failed.
    Probe(ProbeError),
}

/// Parallel-probe result separated into matching runtimes and rejections.
#[derive(Debug)]
pub struct ProbeReport {
    /// Runtimes whose feature release exactly matches the manifest request.
    pub matching: Vec<JavaRuntime>,
    /// Candidates rejected by process, metadata, or feature-release validation.
    pub rejected: Vec<ProbeRejection>,
}

/// Boundary for obtaining the process-level worker limit.
pub trait ParallelismProvider: Send + Sync {
    /// Returns the number of workers available to the current process.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error instead of silently substituting a
    /// worker count.
    fn available_parallelism(&self) -> io::Result<NonZeroUsize>;
}

/// Parallelism provider backed by [`thread::available_parallelism`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemParallelism;

impl ParallelismProvider for SystemParallelism {
    fn available_parallelism(&self) -> io::Result<NonZeroUsize> {
        thread::available_parallelism()
    }
}

/// Fatal infrastructure failure in the bounded parallel probe.
#[derive(Debug, Error)]
pub enum ParallelProbeError {
    /// The operating system could not report available parallelism.
    #[error("failed to determine available Java probe parallelism: {source}")]
    AvailableParallelism {
        /// Operating-system failure.
        #[source]
        source: io::Error,
    },
    /// A worker thread could not be created.
    #[error("failed to start Java probe worker {worker}: {source}")]
    WorkerSpawn {
        /// Zero-based worker index.
        worker: usize,
        /// Thread creation failure.
        #[source]
        source: io::Error,
    },
    /// The shared work queue was poisoned by a worker panic.
    #[error("Java probe work queue was poisoned")]
    QueuePoisoned,
    /// A worker exited without returning every candidate result.
    #[error("Java probe workers returned {received} of {expected} candidate results")]
    IncompleteResults {
        /// Number of queued candidates.
        expected: usize,
        /// Number of results received.
        received: usize,
    },
    /// A worker panicked and could not provide a trustworthy result.
    #[error("Java probe worker {worker} panicked")]
    WorkerPanic {
        /// Zero-based worker index.
        worker: usize,
    },
}

/// Probes candidates with a bounded worker pool and retains only the requested
/// Java feature release.
///
/// # Errors
///
/// Returns [`ParallelProbeError`] for worker infrastructure failures. Individual
/// candidate failures are retained in [`ProbeReport::rejected`].
pub fn probe_candidates_parallel<P, A>(
    candidates: &[PathBuf],
    required_major: u16,
    probe: &Arc<P>,
    parallelism: &A,
) -> Result<ProbeReport, ParallelProbeError>
where
    P: RuntimeProbe + 'static,
    A: ParallelismProvider,
{
    if candidates.is_empty() {
        return Ok(ProbeReport {
            matching: Vec::new(),
            rejected: Vec::new(),
        });
    }
    let workers = parallelism
        .available_parallelism()
        .map_err(|source| ParallelProbeError::AvailableParallelism { source })?
        .get()
        .min(candidates.len());
    let queue = Arc::new(Mutex::new(
        candidates.iter().cloned().collect::<VecDeque<_>>(),
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel();
    let mut handles = Vec::with_capacity(workers);
    for worker in 0..workers {
        let worker_queue = Arc::clone(&queue);
        let worker_probe = Arc::clone(probe);
        let worker_sender = sender.clone();
        let worker_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name(format!("java-probe-{worker}"))
            .spawn(move || -> Result<(), ParallelProbeError> {
                loop {
                    if worker_stop.load(AtomicOrdering::Acquire) {
                        return Ok(());
                    }
                    let candidate = worker_queue
                        .lock()
                        .map_err(|_| ParallelProbeError::QueuePoisoned)?
                        .pop_front();
                    let Some(candidate) = candidate else {
                        return Ok(());
                    };
                    let result = worker_probe.inspect(&candidate);
                    if worker_sender.send((candidate, result)).is_err() {
                        return Ok(());
                    }
                }
            });
        match handle {
            Ok(handle) => handles.push((worker, handle)),
            Err(source) => {
                stop.store(true, AtomicOrdering::Release);
                drop(sender);
                for (_, handle) in handles {
                    let _ = handle.join();
                }
                return Err(ParallelProbeError::WorkerSpawn { worker, source });
            }
        }
    }
    drop(sender);

    let results: Vec<_> = receiver.into_iter().collect();
    for (worker, handle) in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(ParallelProbeError::WorkerPanic { worker }),
        }
    }
    if results.len() != candidates.len() {
        return Err(ParallelProbeError::IncompleteResults {
            expected: candidates.len(),
            received: results.len(),
        });
    }

    let mut matching = Vec::new();
    let mut rejected = Vec::new();
    for (candidate, result) in results {
        match result {
            Ok(runtime) if runtime.major == required_major => matching.push(runtime),
            Ok(runtime) => rejected.push(ProbeRejection {
                executable: candidate,
                reason: ProbeRejectionReason::FeatureReleaseMismatch {
                    found: runtime.major,
                    required: required_major,
                },
            }),
            Err(error) => rejected.push(ProbeRejection {
                executable: candidate,
                reason: ProbeRejectionReason::Probe(error),
            }),
        }
    }
    sort_runtimes(&mut matching);
    rejected.sort_by(|left, right| left.executable.cmp(&right.executable));
    Ok(ProbeReport { matching, rejected })
}

/// Parses the three required properties emitted by
/// `java -XshowSettings:properties -version`.
///
/// # Errors
///
/// Returns [`ProbeError`] when a property is missing or `java.version` cannot
/// be normalized to a feature-release number.
pub fn parse_java_properties(executable: &Path, output: &[u8]) -> Result<JavaRuntime, ProbeError> {
    let output = String::from_utf8_lossy(output);
    let version = property(&output, "java.version").ok_or(ProbeError::MissingProperty {
        property: "java.version",
    })?;
    let vendor = property(&output, "java.vendor").ok_or(ProbeError::MissingProperty {
        property: "java.vendor",
    })?;
    let architecture = property(&output, "os.arch").ok_or(ProbeError::MissingProperty {
        property: "os.arch",
    })?;
    let major = parse_java_major(&version)?;
    Ok(JavaRuntime {
        executable: executable.to_path_buf(),
        version,
        major,
        vendor,
        architecture,
    })
}

/// Sorts verified runtimes by 64-bit preference, complete numeric Java version
/// descending, then executable path ascending.
pub fn sort_runtimes(runtimes: &mut [JavaRuntime]) {
    runtimes.sort_by(|left, right| {
        right
            .is_64_bit()
            .cmp(&left.is_64_bit())
            .then_with(|| compare_versions_descending(&left.version, &right.version))
            .then_with(|| left.executable.cmp(&right.executable))
    });
}

fn property(output: &str, expected: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (name, value) = line.trim().split_once('=')?;
        (name.trim() == expected)
            .then(|| value.trim().trim_matches('"').to_string())
            .filter(|value| !value.is_empty())
    })
}

fn parse_java_major(version: &str) -> Result<u16, ProbeError> {
    if !version.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return Err(ProbeError::InvalidVersion {
            version: version.to_string(),
        });
    }
    let parts = numeric_version_parts(version);
    let major = if parts.first() == Some(&"1") {
        parts.get(1).copied()
    } else {
        parts.first().copied()
    };
    major
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| (1..=255).contains(value))
        .ok_or_else(|| ProbeError::InvalidVersion {
            version: version.to_string(),
        })
}

fn compare_versions_descending(left: &str, right: &str) -> Ordering {
    compare_numeric_versions(right, left)
}

fn numeric_version_parts(version: &str) -> Vec<&str> {
    version
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .collect()
}

fn compare_numeric_versions(left: &str, right: &str) -> Ordering {
    let left = numeric_version_parts(left);
    let right = numeric_version_parts(right);
    let length = left.len().max(right.len());
    for index in 0..length {
        let left_part = match left.get(index) {
            Some(part) => *part,
            None => "0",
        };
        let right_part = match right.get(index) {
            Some(part) => *part,
            None => "0",
        };
        let ordering = compare_decimal(left_part, right_part);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn compare_decimal(left: &str, right: &str) -> Ordering {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    let left = if left.is_empty() { "0" } else { left };
    let right = if right.is_empty() { "0" } else { right };
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

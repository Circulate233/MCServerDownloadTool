//! Java runtime discovery, bounded metadata probing, and explicit selection.

mod discovery;
mod probe;
mod process;
mod selection;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::JavaRuntimeProvisioner;
use crate::i18n::{Language, Localizer};
use crate::manifest::JavaConfig;

pub use discovery::{
    CandidateDiscovery, CandidatePlan, DiscoveryError, DiscoveryInputs, DiscoveryReport,
    DiscoveryWarning, JavaPlatform, SearchRoot, SearchRootPattern, SystemCandidateDiscovery,
    build_candidate_plan, discover_from_plan,
};
pub use probe::{
    JAVA_PROBE_TIMEOUT, JavaCommandProbe, JavaRuntime, ParallelProbeError, ParallelismProvider,
    ProbeError, ProbeRejection, ProbeRejectionReason, ProbeReport, RuntimeProbe, SystemParallelism,
    parse_java_properties, probe_candidates_parallel, sort_runtimes,
};
pub use process::{
    EnvironmentPolicy, ProcessError, ProcessOutput, ProcessRequest, ProcessRunner,
    SystemProcessRunner,
};
pub use selection::{ConsoleIo, InputEvent, InteractiveIo, SelectionError, select_runtime};

/// End-to-end Java discovery and selection failure.
#[derive(Debug, Error)]
pub enum JavaDiscoveryError {
    /// Candidate source initialization failed.
    #[error("{message}")]
    Discovery {
        /// Localized user-facing description.
        message: String,
        /// Structured discovery failure retained for diagnostics.
        #[source]
        source: DiscoveryError,
    },
    /// Parallel probing infrastructure failed.
    #[error("{message}")]
    ParallelProbe {
        /// Localized user-facing description.
        message: String,
        /// Structured worker failure retained for diagnostics.
        #[source]
        source: ParallelProbeError,
    },
    /// Selection was interrupted or its terminal failed.
    #[error(transparent)]
    Selection(#[from] SelectionError),
    /// Discovery or probe diagnostics could not be written.
    #[error("{message}: {source}")]
    DiagnosticOutput {
        /// Localized operation description.
        message: &'static str,
        /// Error-output failure.
        #[source]
        source: io::Error,
    },
}

/// Immutable manifest and interaction context for one Java selection run.
#[derive(Debug, Clone, Copy)]
pub struct JavaSelectionRequest<'a> {
    /// Validated manifest Java requirements.
    pub config: &'a JavaConfig,
    /// Server directory whose local runtime folder is searched.
    pub server_root: &'a Path,
    /// Platform layout used for candidate and manual-path handling.
    pub platform: JavaPlatform,
    /// Existing CLI language selection used by diagnostics and prompts.
    pub language: Language,
}

/// Discovers, probes, filters, displays, and selects a Java runtime through
/// injected system boundaries.
///
/// # Errors
///
/// Returns [`JavaDiscoveryError`] for fatal discovery, worker, terminal, EOF, or
/// interruption failures. Individual candidate failures are logged before
/// selection and do not hide other valid candidates.
pub fn discover_and_select<D, P, I, A>(
    request: JavaSelectionRequest<'_>,
    discovery: &D,
    probe: &Arc<P>,
    io: &I,
    parallelism: &A,
) -> Result<PathBuf, JavaDiscoveryError>
where
    D: CandidateDiscovery,
    P: RuntimeProbe + 'static,
    I: InteractiveIo,
    A: ParallelismProvider,
{
    let localizer = Localizer::new(request.language);
    let discovery_report = discovery.discover(request.server_root).map_err(|source| {
        JavaDiscoveryError::Discovery {
            message: localizer.java_discovery_error(&source),
            source,
        }
    })?;
    for warning in discovery_report.warnings {
        write_diagnostic(
            io,
            request.language,
            &localizer.java_discovery_warning(&warning),
        )?;
    }
    let probe_report = probe_candidates_parallel(
        &discovery_report.candidates,
        request.config.major,
        probe,
        parallelism,
    )
    .map_err(|source| JavaDiscoveryError::ParallelProbe {
        message: localizer.java_parallel_probe_error(&source),
        source,
    })?;
    for rejection in probe_report.rejected {
        write_diagnostic(
            io,
            request.language,
            &localizer.java_probe_rejection(&rejection),
        )?;
    }
    let selected = select_runtime(
        &probe_report.matching,
        request.config.major,
        request.platform,
        request.language,
        io,
        probe.as_ref(),
    )?;
    Ok(selected.executable)
}

/// Production Java provisioner that searches the current operating system and
/// asks the user to make an explicit selection.
#[derive(Debug, Clone, Copy)]
pub struct InteractiveJavaProvisioner {
    language: Language,
}

impl InteractiveJavaProvisioner {
    /// Creates a provisioner using the existing CLI language selection.
    #[must_use]
    pub const fn new(language: Language) -> Self {
        Self { language }
    }
}

impl JavaRuntimeProvisioner for InteractiveJavaProvisioner {
    type Error = JavaDiscoveryError;

    fn provision(&self, config: &JavaConfig, server_root: &Path) -> Result<PathBuf, Self::Error> {
        let process = Arc::new(SystemProcessRunner);
        let discovery = SystemCandidateDiscovery::new(Arc::clone(&process));
        let probe = Arc::new(JavaCommandProbe::new(process));
        discover_and_select(
            JavaSelectionRequest {
                config,
                server_root,
                platform: JavaPlatform::current(),
                language: self.language,
            },
            &discovery,
            &probe,
            &ConsoleIo,
            &SystemParallelism,
        )
    }
}

fn write_diagnostic<I: InteractiveIo>(
    io: &I,
    language: Language,
    reason: &str,
) -> Result<(), JavaDiscoveryError> {
    let localizer = Localizer::new(language);
    io.write_error(&format!("{}: {reason}\n", localizer.error_prefix()))
        .map_err(|source| JavaDiscoveryError::DiagnosticOutput {
            message: localizer.java_diagnostic_output_error(),
            source,
        })
}

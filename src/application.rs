//! End-to-end application orchestration from resolved CLI input to durable installation.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::JavaRuntimeProvisioner;
use crate::cli::Cli;
use crate::error::AppError;
use crate::i18n::{Language, resolve_language};
use crate::install::{
    InstallCore, InstallEvent, InstallObserver, InstallPlan, InstallResult, InstallSession,
    InstallStage, Installer,
};
use crate::java::InteractiveJavaProvisioner;
use crate::loader::{LoaderExecutor, SystemProcessRunner};
use crate::manifest::{ValidatedManifest, load_manifest};
use crate::net::{NetworkConfig, NetworkEngine};
use crate::scripts::ScriptPlatform;

/// Process-environment boundary used only while resolving CLI configuration.
pub trait Environment: Send + Sync {
    /// Returns one Unicode environment variable, or `None` when it is absent.
    fn variable(&self, name: &str) -> Option<String>;
}

impl<F> Environment for F
where
    F: Fn(&str) -> Option<String> + Send + Sync,
{
    fn variable(&self, name: &str) -> Option<String> {
        self(name)
    }
}

/// Production environment reader.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEnvironment;

impl Environment for SystemEnvironment {
    fn variable(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// Installation boundary that owns creation of the one shared network engine.
pub trait InstallExecution: Send + Sync {
    /// Builds one network session and executes the complete immutable plan.
    ///
    /// # Errors
    ///
    /// Returns the concrete network or installation failure without retrying the
    /// whole installation or constructing a second engine.
    fn install(
        &self,
        network: NetworkConfig,
        manifest_path: &Path,
        plan: &InstallPlan,
        session: &InstallSession,
    ) -> Result<InstallResult, AppError>;
}

/// Production installation implementation using the shared network and loader boundaries.
#[derive(Debug, Clone, Copy, Default)]
pub struct InstallExecutionImpl;

impl InstallExecution for InstallExecutionImpl {
    fn install(
        &self,
        network: NetworkConfig,
        manifest_path: &Path,
        plan: &InstallPlan,
        session: &InstallSession,
    ) -> Result<InstallResult, AppError> {
        let engine = NetworkEngine::new(network)?;
        let loader = LoaderExecutor::new(SystemProcessRunner);
        Installer::new(engine, loader)
            .install_in_session(manifest_path, plan, session)
            .map_err(AppError::from)
    }
}

/// Successful result exposed to the command-line presentation layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationResult {
    /// Number of final-schema file declarations processed by the plan.
    pub file_count: usize,
    /// Language resolved from CLI, locale, and fallback precedence.
    pub language: Language,
    /// Durable installation result from the core state machine.
    pub installation: InstallResult,
}

/// Complete application behavior, separated from process I/O and exit handling.
pub trait Application {
    /// Resolves configuration, validates the manifest, selects Java, builds the
    /// immutable plan, and executes the installation exactly once.
    ///
    /// # Errors
    ///
    /// Returns the first configuration, manifest, Java, network, or installation failure.
    fn run(
        &self,
        cli: Cli,
        executable: &Path,
        system_locale: Option<&str>,
        environment: &dyn Environment,
        observer: Arc<dyn InstallObserver>,
    ) -> Result<ApplicationResult, AppError>;
}

/// Generic application implementation with injectable Java and installation boundaries.
#[derive(Debug, Clone)]
pub struct ApplicationImpl<J, I> {
    java: J,
    installation: I,
    script_platform: ScriptPlatform,
}

impl<J, I> ApplicationImpl<J, I> {
    /// Creates an application with explicit platform behavior and dependencies.
    pub const fn new(java: J, installation: I, script_platform: ScriptPlatform) -> Self {
        Self {
            java,
            installation,
            script_platform,
        }
    }
}

impl<J, I> Application for ApplicationImpl<J, I>
where
    J: JavaRuntimeProvisioner + Send + Sync,
    I: InstallExecution,
{
    fn run(
        &self,
        cli: Cli,
        executable: &Path,
        system_locale: Option<&str>,
        environment: &dyn Environment,
        observer: Arc<dyn InstallObserver>,
    ) -> Result<ApplicationResult, AppError> {
        let options = cli.resolve_with_environment(executable, system_locale, |name| {
            environment.variable(name)
        })?;
        let manifest = load_manifest(&options.manifest_path)?;
        let secrets = manifest
            .as_manifest()
            .curseforge_api_key
            .as_ref()
            .map(|key| key.expose().to_string())
            .into_iter();
        let session =
            InstallSession::acquire(&options.manifest_path, options.language, observer, secrets)?;
        let result: Result<ApplicationResult, AppError> = (|| {
            validate_runtime_reserved_targets(
                &manifest,
                &options.manifest_path,
                executable,
                session.root(),
                self.script_platform,
            )?;
            session.emit(InstallEvent::Stage(InstallStage::SelectingJava))?;
            let java_executable = self
                .java
                .provision(manifest.java(), session.root().path())
                .map_err(|error| AppError::Java {
                    reason: error.to_string(),
                })?;
            session.record_selected_java(&java_executable)?;

            let network = NetworkConfig {
                proxy: options.proxy.as_ref().map(ToString::to_string),
                ..NetworkConfig::default()
            };
            let plan = InstallPlan::from_manifest(
                &manifest,
                java_executable,
                options.proxy,
                self.script_platform,
                options.language,
            )?;
            let file_count = plan.files.len();
            let installation =
                self.installation
                    .install(network, &options.manifest_path, &plan, &session)?;
            Ok(ApplicationResult {
                file_count,
                language: options.language,
                installation,
            })
        })();
        if let Err(error) = &result {
            session.record_failure(&error.to_string())?;
        }
        result
    }
}

fn validate_runtime_reserved_targets(
    manifest: &ValidatedManifest,
    manifest_path: &Path,
    executable: &Path,
    root: &crate::install::InstallRoot,
    script_platform: ScriptPlatform,
) -> Result<(), crate::install::InstallError> {
    let manifest_identity = fs::canonicalize(manifest_path).map_err(|source| {
        crate::install::InstallError::io("canonicalize selected manifest", manifest_path, source)
    })?;
    let mut protected = vec![("selected manifest", manifest_identity)];
    if let Ok(executable_identity) = fs::canonicalize(executable)
        && executable_identity.starts_with(root.path())
    {
        protected.push(("running installer executable", executable_identity));
    }

    let mut targets = manifest
        .files()
        .iter()
        .map(|file| ("manifest file", PathBuf::from(&file.path)))
        .collect::<Vec<_>>();
    match &manifest.loader().output {
        crate::loader::LoaderOutputExpectation::ModernArgs { windows, unix } => {
            targets.push(("loader output", windows.clone()));
            targets.push(("loader output", unix.clone()));
        }
        crate::loader::LoaderOutputExpectation::ExactJar { path, .. } => {
            targets.push(("loader output", path.clone()));
        }
    }
    let script = match script_platform {
        ScriptPlatform::Windows => "start.bat",
        ScriptPlatform::Unix => "start.sh",
    };
    targets.push(("start script", PathBuf::from(script)));
    targets.push((
        "start script conflict output",
        PathBuf::from(format!("{script}.new")),
    ));

    for (kind, relative) in targets {
        let target = root.resolve(&relative)?;
        let identity = fs::canonicalize(&target).unwrap_or(target.clone());
        for (protected_kind, protected_path) in &protected {
            if paths_equal(&identity, protected_path) {
                return Err(crate::install::InstallError::InvalidPlan {
                    reason: format!(
                        "{kind} '{}' conflicts with the {protected_kind} '{}'",
                        relative.display(),
                        protected_path.display()
                    ),
                });
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn paths_equal(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .replace('/', "\\")
        .eq_ignore_ascii_case(&right.to_string_lossy().replace('/', "\\"))
}

#[cfg(not(windows))]
fn paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

/// Runs the production application with operating-system dependencies.
///
/// # Errors
///
/// Returns the first application failure with its stable exit-code category.
pub fn run(
    cli: Cli,
    executable: &Path,
    system_locale: Option<&str>,
    observer: Arc<dyn InstallObserver>,
) -> Result<ApplicationResult, AppError> {
    let language = resolve_language(cli.lang, system_locale);
    ApplicationImpl::new(
        InteractiveJavaProvisioner::new(language),
        InstallExecutionImpl,
        current_script_platform(),
    )
    .run(cli, executable, system_locale, &SystemEnvironment, observer)
}

const fn current_script_platform() -> ScriptPlatform {
    if cfg!(windows) {
        ScriptPlatform::Windows
    } else {
        ScriptPlatform::Unix
    }
}

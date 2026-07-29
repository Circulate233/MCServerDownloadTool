use std::collections::{HashSet, VecDeque};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use thiserror::Error;

use super::process::{ProcessRequest, ProcessRunner};

const DISCOVERY_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const SEARCH_DEPTH: usize = 6;
const MAX_SCAN_DIRECTORIES: usize = 4096;
const MAX_SCAN_ENTRIES: usize = 100_000;
const MAX_PATTERN_ENTRIES: usize = 1024;
const MAX_DISCOVERY_WARNINGS: usize = 256;
const MAX_REGISTRY_WORKERS: usize = 4;
const MAX_REGISTRY_OUTPUT: usize = 2 * 1024 * 1024;
const WINDOWS_REGISTRY_KEYS: &[&str] = &[
    r"HKLM\SOFTWARE\JavaSoft",
    r"HKLM\SOFTWARE\WOW6432Node\JavaSoft",
    r"HKLM\SOFTWARE\Eclipse Adoptium",
    r"HKLM\SOFTWARE\Adoptium",
    r"HKLM\SOFTWARE\Microsoft\JDK",
    r"HKLM\SOFTWARE\Azul Systems",
    r"HKLM\SOFTWARE\BellSoft",
    r"HKLM\SOFTWARE\Amazon Corretto",
    r"HKLM\SOFTWARE\IBM\Semeru",
    r"HKLM\SOFTWARE\Semeru",
    r"HKLM\SOFTWARE\SAP\SapMachine",
    r"HKLM\SOFTWARE\GraalVM",
    r"HKLM\SOFTWARE\JetBrains",
    r"HKLM\SOFTWARE\Red Hat",
    r"HKCU\SOFTWARE\JavaSoft",
    r"HKCU\SOFTWARE\Eclipse Adoptium",
    r"HKCU\SOFTWARE\Adoptium",
    r"HKCU\SOFTWARE\Microsoft\JDK",
    r"HKCU\SOFTWARE\Azul Systems",
    r"HKCU\SOFTWARE\BellSoft",
    r"HKCU\SOFTWARE\Amazon Corretto",
    r"HKCU\SOFTWARE\IBM\Semeru",
    r"HKCU\SOFTWARE\Semeru",
    r"HKCU\SOFTWARE\SAP\SapMachine",
    r"HKCU\SOFTWARE\GraalVM",
    r"HKCU\SOFTWARE\JetBrains",
    r"HKCU\SOFTWARE\Red Hat",
];

/// Operating-system layout whose Java installation conventions are generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavaPlatform {
    /// Microsoft Windows layouts and executable naming.
    Windows,
    /// Linux and other freedesktop-style layouts.
    Linux,
    /// Apple macOS framework and user-library layouts.
    MacOs,
}

impl JavaPlatform {
    /// Returns the platform selected at compile time.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }

    /// Returns the Java launcher filename for this platform.
    #[must_use]
    pub const fn executable_name(self) -> &'static str {
        match self {
            Self::Windows => "java.exe",
            Self::Linux | Self::MacOs => "java",
        }
    }
}

/// Immutable values used by the pure candidate-plan generator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryInputs {
    /// Platform whose directory conventions should be applied.
    pub platform: JavaPlatform,
    /// Directories from the process `PATH`, already split by the caller.
    pub path_entries: Vec<PathBuf>,
    /// `JAVA_HOME`, when present.
    pub java_home: Option<PathBuf>,
    /// `JRE_HOME`, when present.
    pub jre_home: Option<PathBuf>,
    /// Current user's home directory, when available.
    pub user_home: Option<PathBuf>,
    /// Windows roaming application-data directory, when available.
    pub app_data: Option<PathBuf>,
    /// Windows local application-data directory, when available.
    pub local_app_data: Option<PathBuf>,
    /// Windows Program Files roots, including the 32-bit root when distinct.
    pub program_files: Vec<PathBuf>,
    /// Java homes reported by supported Windows vendor registry keys.
    pub registry_homes: Vec<PathBuf>,
    /// Java homes reported by a platform facility such as `java_home`.
    pub platform_java_homes: Vec<PathBuf>,
    /// Server root retained for caller context; it is never searched for Java.
    pub server_root: PathBuf,
}

/// Directory tree to inspect for Java executables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRoot {
    /// Root directory of the bounded recursive search.
    pub path: PathBuf,
    /// Maximum number of child-directory levels visited.
    pub max_depth: usize,
}

/// Pattern that expands Java roots beneath one parent directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRootPattern {
    /// Parent whose immediate child directory names are matched.
    pub parent: PathBuf,
    /// Required child-directory name prefix.
    pub child_prefix: String,
    /// Relative path appended below each matching child.
    pub relative_tail: PathBuf,
    /// Maximum number of levels searched below each expanded root.
    pub max_depth: usize,
}

/// Pure discovery plan containing direct launchers and bounded search roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidatePlan {
    /// Launchers inferred directly from `PATH` or a Java home.
    pub direct_executables: Vec<PathBuf>,
    /// Vendor, toolchain, and Minecraft runtime directories to scan.
    pub search_roots: Vec<SearchRoot>,
    /// Platform wildcard roots expanded during filesystem discovery.
    pub search_patterns: Vec<SearchRootPattern>,
}

/// Non-fatal problem encountered while consulting an optional discovery source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryWarning {
    /// Source or path that failed.
    pub source: String,
    /// Concrete failure description suitable for an error log.
    pub reason: String,
}

/// Candidate discovery result with canonical, duplicate-free executable paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryReport {
    /// Canonical executable paths, sorted for deterministic probing.
    pub candidates: Vec<PathBuf>,
    /// Optional-source failures that did not prevent other sources from running.
    pub warnings: Vec<DiscoveryWarning>,
}

/// Fatal candidate discovery failure.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// The platform discovery contract could not be initialized.
    #[error("failed to initialize Java discovery: {reason}")]
    Initialization {
        /// Concrete initialization failure.
        reason: String,
    },
}

/// Candidate-source boundary used by Java runtime orchestration.
pub trait CandidateDiscovery: Send + Sync {
    /// Finds canonical Java executable candidates for one server root.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoveryError`] when discovery cannot be initialized. Failures
    /// from optional paths or platform facilities are returned as warnings.
    fn discover(&self, server_root: &Path) -> Result<DiscoveryReport, DiscoveryError>;
}

/// Builds all platform-specific direct candidates and bounded search roots.
///
/// This function performs no I/O, which keeps platform behavior testable on any
/// host operating system.
#[must_use]
pub fn build_candidate_plan(inputs: &DiscoveryInputs) -> CandidatePlan {
    let executable = inputs.platform.executable_name();
    let mut direct = Vec::new();
    for path_entry in &inputs.path_entries {
        if platform_absolute(path_entry, inputs.platform) && !path_entry.as_os_str().is_empty() {
            direct.push(path_entry.join(executable));
        }
    }
    for home in inputs
        .java_home
        .iter()
        .chain(inputs.jre_home.iter())
        .chain(inputs.registry_homes.iter())
        .chain(inputs.platform_java_homes.iter())
    {
        if platform_absolute(home, inputs.platform) && !home.as_os_str().is_empty() {
            direct.push(home.join("bin").join(executable));
        }
    }

    let mut roots = Vec::new();
    let mut patterns = Vec::new();
    match inputs.platform {
        JavaPlatform::Windows => add_windows_roots(inputs, &mut roots),
        JavaPlatform::Linux => add_linux_roots(inputs, &mut roots, &mut patterns),
        JavaPlatform::MacOs => add_macos_roots(inputs, &mut roots),
    }
    roots.retain(|root| platform_absolute(&root.path, inputs.platform));
    patterns.retain(|pattern| platform_absolute(&pattern.parent, inputs.platform));
    deduplicate_paths(&mut direct, inputs.platform);
    deduplicate_search_roots(&mut roots, inputs.platform);
    CandidatePlan {
        direct_executables: direct,
        search_roots: roots,
        search_patterns: patterns,
    }
}

fn platform_absolute(path: &Path, platform: JavaPlatform) -> bool {
    match platform {
        JavaPlatform::Windows => {
            if path.is_absolute() {
                return true;
            }

            // Candidate-plan tests deliberately model Windows paths on Unix
            // hosts. `Path::is_absolute` uses the host parser, therefore it
            // cannot recognize a Windows drive path there.
            let value = path.as_os_str().to_string_lossy();
            let bytes = value.as_bytes();
            bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && matches!(bytes[2], b'\\' | b'/')
        }
        JavaPlatform::Linux | JavaPlatform::MacOs => path.to_string_lossy().starts_with('/'),
    }
}

fn add_windows_roots(inputs: &DiscoveryInputs, roots: &mut Vec<SearchRoot>) {
    for program_files in &inputs.program_files {
        for relative in [
            "Java",
            "Eclipse Adoptium",
            "AdoptOpenJDK",
            "Microsoft",
            "Zulu",
            "BellSoft",
            "Amazon Corretto",
            "Semeru",
            "SapMachine",
            "GraalVM",
            "JetBrains",
            "RedHat",
            "Minecraft Launcher/runtime",
        ] {
            roots.push(SearchRoot {
                path: program_files.join(relative),
                max_depth: SEARCH_DEPTH,
            });
        }
    }
    if let Some(app_data) = &inputs.app_data {
        roots.push(SearchRoot {
            path: app_data.join(".minecraft/runtime"),
            max_depth: SEARCH_DEPTH,
        });
    }
    if let Some(local_app_data) = &inputs.local_app_data {
        roots.push(SearchRoot {
            path: local_app_data.join(".minecraft/runtime"),
            max_depth: SEARCH_DEPTH,
        });
    }
    if let Some(home) = &inputs.user_home {
        roots.push(SearchRoot {
            path: home.join(".jdks"),
            max_depth: SEARCH_DEPTH,
        });
        roots.push(SearchRoot {
            path: home.join(".gradle/jdks"),
            max_depth: SEARCH_DEPTH,
        });
        roots.push(SearchRoot {
            path: home.join(".minecraft/runtime"),
            max_depth: SEARCH_DEPTH,
        });
    }
}

fn add_linux_roots(
    inputs: &DiscoveryInputs,
    roots: &mut Vec<SearchRoot>,
    patterns: &mut Vec<SearchRootPattern>,
) {
    for path in [
        "/usr/java",
        "/usr/lib/jvm",
        "/usr/lib32/jvm",
        "/usr/lib64/jvm",
        "/opt",
    ] {
        roots.push(SearchRoot {
            path: PathBuf::from(path),
            max_depth: SEARCH_DEPTH,
        });
    }
    patterns.push(SearchRootPattern {
        parent: PathBuf::from("/usr"),
        child_prefix: "lib".to_string(),
        relative_tail: PathBuf::from("jvm"),
        max_depth: SEARCH_DEPTH,
    });
    if let Some(home) = &inputs.user_home {
        for relative in [
            ".sdkman/candidates/java",
            ".asdf/installs/java",
            ".gradle/jdks",
            ".jdks",
            ".minecraft/runtime",
        ] {
            roots.push(SearchRoot {
                path: home.join(relative),
                max_depth: SEARCH_DEPTH,
            });
        }
    }
}

fn add_macos_roots(inputs: &DiscoveryInputs, roots: &mut Vec<SearchRoot>) {
    roots.push(SearchRoot {
        path: PathBuf::from("/Library/Java/JavaVirtualMachines"),
        max_depth: SEARCH_DEPTH,
    });
    roots.push(SearchRoot {
        path: PathBuf::from("/System/Library/Java/JavaVirtualMachines"),
        max_depth: SEARCH_DEPTH,
    });
    if let Some(home) = &inputs.user_home {
        for relative in [
            "Library/Java/JavaVirtualMachines",
            ".sdkman/candidates/java",
            ".asdf/installs/java",
            "Library/Application Support/minecraft/runtime",
        ] {
            roots.push(SearchRoot {
                path: home.join(relative),
                max_depth: SEARCH_DEPTH,
            });
        }
    }
}

/// System-backed candidate discovery using environment variables, platform
/// commands, and bounded filesystem traversal.
#[derive(Debug)]
pub struct SystemCandidateDiscovery<R> {
    process: Arc<R>,
}

impl<R> SystemCandidateDiscovery<R> {
    /// Creates discovery backed by the supplied process runner.
    #[must_use]
    pub fn new(process: Arc<R>) -> Self {
        Self { process }
    }
}

impl<R> CandidateDiscovery for SystemCandidateDiscovery<R>
where
    R: ProcessRunner + 'static,
{
    fn discover(&self, server_root: &Path) -> Result<DiscoveryReport, DiscoveryError> {
        let platform = JavaPlatform::current();
        let mut warnings = Vec::new();
        let (registry_homes, platform_java_homes) = match platform {
            JavaPlatform::Windows => (
                collect_windows_registry_homes(&self.process, &mut warnings),
                Vec::new(),
            ),
            JavaPlatform::MacOs => (
                Vec::new(),
                collect_macos_java_homes(self.process.as_ref(), &mut warnings),
            ),
            JavaPlatform::Linux => (Vec::new(), Vec::new()),
        };
        let program_files = unique_environment_paths(["ProgramFiles", "ProgramFiles(x86)"]);
        let inputs = DiscoveryInputs {
            platform,
            path_entries: env::var_os("PATH")
                .map(|value| env::split_paths(&value).collect())
                .unwrap_or_default(),
            java_home: env_path("JAVA_HOME"),
            jre_home: env_path("JRE_HOME"),
            user_home: env_path(if platform == JavaPlatform::Windows {
                "USERPROFILE"
            } else {
                "HOME"
            }),
            app_data: env_path("APPDATA"),
            local_app_data: env_path("LOCALAPPDATA"),
            program_files,
            registry_homes,
            platform_java_homes,
            server_root: server_root.to_path_buf(),
        };
        let plan = build_candidate_plan(&inputs);
        let mut report = discover_from_plan(&plan, platform);
        warnings.append(&mut report.warnings);
        report.warnings = warnings;
        Ok(report)
    }
}

fn collect_windows_registry_homes<R>(
    process: &Arc<R>,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Vec<PathBuf>
where
    R: ProcessRunner + 'static,
{
    let Some(reg_executable) = trusted_system32_executable("reg.exe") else {
        push_warning(
            warnings,
            DiscoveryWarning {
                source: "Windows registry".to_string(),
                reason: "%SystemRoot%\\System32\\reg.exe is unavailable or unsafe".to_string(),
            },
        );
        return Vec::new();
    };
    let queue = Arc::new(Mutex::new(
        WINDOWS_REGISTRY_KEYS
            .iter()
            .copied()
            .enumerate()
            .collect::<VecDeque<_>>(),
    ));
    let results = Arc::new(Mutex::new(Vec::new()));
    thread::scope(|scope| {
        for _ in 0..MAX_REGISTRY_WORKERS {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            let process = Arc::clone(process);
            let reg_executable = reg_executable.clone();
            scope.spawn(move || {
                loop {
                    let task = queue.lock().ok().and_then(|mut queue| queue.pop_front());
                    let Some((index, key)) = task else {
                        break;
                    };
                    let request = ProcessRequest::new(&reg_executable, DISCOVERY_COMMAND_TIMEOUT)
                        .with_arguments(["query", key, "/s"])
                        .with_output_limit(MAX_REGISTRY_OUTPUT);
                    let result = process.run(&request);
                    if let Ok(mut results) = results.lock() {
                        results.push((index, key, result));
                    }
                }
            });
        }
    });
    let Some(mut results) = take_registry_results(results, warnings) else {
        return Vec::new();
    };
    results.sort_by_key(|(index, _, _)| *index);
    let mut homes = Vec::new();
    for (_, key, result) in results {
        match result {
            Ok(output) if output.exit_code == Some(0) => {
                homes.extend(parse_registry_homes(&output.stdout));
            }
            Ok(_) => {}
            Err(error) => push_warning(
                warnings,
                DiscoveryWarning {
                    source: key.to_string(),
                    reason: error.to_string(),
                },
            ),
        }
    }
    homes
}

fn take_registry_results<T>(
    results: Arc<Mutex<Vec<T>>>,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Option<Vec<T>> {
    let Ok(results) = Arc::try_unwrap(results) else {
        push_warning(
            warnings,
            DiscoveryWarning {
                source: "Windows registry".to_string(),
                reason: "registry workers retained result ownership unexpectedly".to_string(),
            },
        );
        return None;
    };
    let Ok(results) = results.into_inner() else {
        push_warning(
            warnings,
            DiscoveryWarning {
                source: "Windows registry".to_string(),
                reason: "registry result mutex was poisoned".to_string(),
            },
        );
        return None;
    };
    Some(results)
}

fn trusted_system32_executable(name: &str) -> Option<PathBuf> {
    let root = env::var_os("SystemRoot").map(PathBuf::from)?;
    if !root.is_absolute() {
        return None;
    }
    let executable = root.join("System32").join(name);
    trusted_regular_file(&executable).then_some(executable)
}

fn parse_registry_homes(output: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (name, value) = line
                .split_once("REG_EXPAND_SZ")
                .or_else(|| line.split_once("REG_SZ"))?;
            let value_name = name.split_whitespace().last()?;
            if !["JavaHome", "Path", "InstallationPath"]
                .iter()
                .any(|expected| value_name.eq_ignore_ascii_case(expected))
            {
                return None;
            }
            let value = value.trim();
            (!value.is_empty()).then(|| PathBuf::from(value))
        })
        .collect()
}

fn collect_macos_java_homes<R: ProcessRunner>(
    process: &R,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Vec<PathBuf> {
    let request = ProcessRequest::new("/usr/libexec/java_home", DISCOVERY_COMMAND_TIMEOUT)
        .with_arguments(["-V"]);
    match process.run(&request) {
        Ok(output) if output.exit_code == Some(0) => {
            let mut bytes = output.stdout;
            bytes.push(b'\n');
            bytes.extend(output.stderr);
            parse_macos_java_homes(&bytes)
        }
        Ok(output) => {
            warnings.push(DiscoveryWarning {
                source: "/usr/libexec/java_home".to_string(),
                reason: format!("command exited with status {:?}", output.exit_code),
            });
            Vec::new()
        }
        Err(error) => {
            warnings.push(DiscoveryWarning {
                source: "/usr/libexec/java_home".to_string(),
                reason: error.to_string(),
            });
            Vec::new()
        }
    }
}

fn parse_macos_java_homes(output: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let start = line.find('/')?;
            let path = line[start..].trim();
            if path.ends_with("/Contents/Home") || path == "/Library/Java/Home" {
                Some(PathBuf::from(path))
            } else {
                None
            }
        })
        .collect()
}

/// Traverses a pure candidate plan, canonicalizes executable files, and removes
/// aliases that resolve to the same path.
#[must_use]
pub fn discover_from_plan(plan: &CandidatePlan, platform: JavaPlatform) -> DiscoveryReport {
    let mut warnings = Vec::new();
    let candidates = materialize_candidates(plan, platform, &mut warnings);
    DiscoveryReport {
        candidates,
        warnings,
    }
}

fn materialize_candidates(
    plan: &CandidatePlan,
    platform: JavaPlatform,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Vec<PathBuf> {
    let executable_name = platform.executable_name();
    let mut candidates = Vec::new();
    let mut canonical_keys = HashSet::new();
    let mut budget = ScanBudget::default();
    for executable in &plan.direct_executables {
        add_canonical_executable(
            executable,
            platform,
            &mut canonical_keys,
            &mut candidates,
            warnings,
            &mut budget,
        );
    }
    for root in &plan.search_roots {
        scan_root(
            root,
            executable_name,
            platform,
            &mut canonical_keys,
            &mut candidates,
            warnings,
            &mut budget,
        );
    }
    for pattern in &plan.search_patterns {
        scan_pattern(
            pattern,
            executable_name,
            platform,
            &mut canonical_keys,
            &mut candidates,
            warnings,
            &mut budget,
        );
    }
    candidates.sort_by_key(|candidate| path_key(candidate, platform));
    candidates
}

fn scan_pattern(
    pattern: &SearchRootPattern,
    executable_name: &str,
    platform: JavaPlatform,
    canonical_keys: &mut HashSet<String>,
    candidates: &mut Vec<PathBuf>,
    warnings: &mut Vec<DiscoveryWarning>,
    budget: &mut ScanBudget,
) {
    if !trusted_directory(&pattern.parent) {
        return;
    }
    let entries = match fs::read_dir(&pattern.parent) {
        Ok(entries) => entries,
        Err(source) => {
            warnings.push(DiscoveryWarning {
                source: pattern.parent.display().to_string(),
                reason: source.to_string(),
            });
            return;
        }
    };
    for entry in entries {
        if !budget.take_pattern_entry(warnings) {
            return;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                warnings.push(DiscoveryWarning {
                    source: pattern.parent.display().to_string(),
                    reason: source.to_string(),
                });
                continue;
            }
        };
        let name = entry.file_name();
        if !name
            .to_string_lossy()
            .starts_with(pattern.child_prefix.as_str())
        {
            continue;
        }
        let root = SearchRoot {
            path: entry.path().join(&pattern.relative_tail),
            max_depth: pattern.max_depth,
        };
        scan_root(
            &root,
            executable_name,
            platform,
            canonical_keys,
            candidates,
            warnings,
            budget,
        );
    }
}

fn scan_root(
    root: &SearchRoot,
    executable_name: &str,
    platform: JavaPlatform,
    canonical_keys: &mut HashSet<String>,
    candidates: &mut Vec<PathBuf>,
    warnings: &mut Vec<DiscoveryWarning>,
    budget: &mut ScanBudget,
) {
    if !trusted_directory(&root.path) {
        return;
    }
    let mut directories = VecDeque::from([(root.path.clone(), 0_usize)]);
    let mut visited = HashSet::new();
    while let Some((directory, depth)) = directories.pop_front() {
        if !budget.take_directory(warnings) {
            return;
        }
        if !trusted_directory(&directory) {
            continue;
        }
        let canonical_directory = match fs::canonicalize(&directory) {
            Ok(path) => path,
            Err(source) => {
                warnings.push(DiscoveryWarning {
                    source: directory.display().to_string(),
                    reason: source.to_string(),
                });
                continue;
            }
        };
        if !visited.insert(path_key(&canonical_directory, platform)) {
            continue;
        }
        let entries = match fs::read_dir(&canonical_directory) {
            Ok(entries) => entries,
            Err(source) => {
                warnings.push(DiscoveryWarning {
                    source: canonical_directory.display().to_string(),
                    reason: source.to_string(),
                });
                continue;
            }
        };
        for entry in entries {
            if !budget.take_entry(warnings) {
                return;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(source) => {
                    warnings.push(DiscoveryWarning {
                        source: canonical_directory.display().to_string(),
                        reason: source.to_string(),
                    });
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(source) => {
                    warnings.push(DiscoveryWarning {
                        source: path.display().to_string(),
                        reason: source.to_string(),
                    });
                    continue;
                }
            };
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(source) => {
                    push_warning(
                        warnings,
                        DiscoveryWarning {
                            source: path.display().to_string(),
                            reason: source.to_string(),
                        },
                    );
                    continue;
                }
            };
            let is_file = metadata.is_file() || file_type.is_symlink();
            let is_directory = metadata.is_dir();
            if is_file && filename_matches(&entry.file_name(), executable_name, platform) {
                add_canonical_executable(
                    &path,
                    platform,
                    canonical_keys,
                    candidates,
                    warnings,
                    budget,
                );
            } else if depth < root.max_depth && is_directory {
                directories.push_back((path, depth + 1));
            }
        }
    }
}

fn add_canonical_executable(
    executable: &Path,
    platform: JavaPlatform,
    canonical_keys: &mut HashSet<String>,
    candidates: &mut Vec<PathBuf>,
    warnings: &mut Vec<DiscoveryWarning>,
    budget: &mut ScanBudget,
) {
    if !executable.is_absolute() {
        return;
    }
    let metadata = match fs::symlink_metadata(executable) {
        Ok(metadata) => metadata,
        Err(source) => {
            push_warning(
                warnings,
                DiscoveryWarning {
                    source: executable.display().to_string(),
                    reason: source.to_string(),
                },
            );
            return;
        }
    };
    let is_regular_file = metadata.is_file() && !is_link_or_reparse(&metadata);
    let is_final_link =
        metadata.file_type().is_symlink() && executable.parent().is_some_and(trusted_directory);
    if !is_regular_file && !is_final_link {
        return;
    }
    match fs::canonicalize(executable) {
        Ok(canonical) if trusted_regular_file(&canonical) => {
            if canonical_keys.insert(path_key(&canonical, platform)) {
                if !budget.take_candidate(warnings) {
                    return;
                }
                candidates.push(canonical);
            }
        }
        Ok(_) => {}
        Err(source) => warnings.push(DiscoveryWarning {
            source: executable.display().to_string(),
            reason: source.to_string(),
        }),
    }
}

#[derive(Default)]
struct ScanBudget {
    directories: usize,
    entries: usize,
    pattern_entries: usize,
    candidates: usize,
    limit_reported: bool,
}

impl ScanBudget {
    fn take_directory(&mut self, warnings: &mut Vec<DiscoveryWarning>) -> bool {
        self.directories += 1;
        self.check(self.directories <= MAX_SCAN_DIRECTORIES, warnings)
    }

    fn take_entry(&mut self, warnings: &mut Vec<DiscoveryWarning>) -> bool {
        self.entries += 1;
        self.check(self.entries <= MAX_SCAN_ENTRIES, warnings)
    }

    fn take_pattern_entry(&mut self, warnings: &mut Vec<DiscoveryWarning>) -> bool {
        self.pattern_entries += 1;
        self.check(self.pattern_entries <= MAX_PATTERN_ENTRIES, warnings)
    }

    fn take_candidate(&mut self, warnings: &mut Vec<DiscoveryWarning>) -> bool {
        self.candidates += 1;
        self.check(
            self.candidates <= super::probe::MAX_JAVA_CANDIDATES,
            warnings,
        )
    }

    fn check(&mut self, allowed: bool, warnings: &mut Vec<DiscoveryWarning>) -> bool {
        if !allowed && !self.limit_reported {
            self.limit_reported = true;
            push_warning(
                warnings,
                DiscoveryWarning {
                    source: "Java discovery".to_string(),
                    reason:
                        "filesystem discovery safety limit reached; remaining entries were skipped"
                            .to_string(),
                },
            );
        }
        allowed
    }
}

fn push_warning(warnings: &mut Vec<DiscoveryWarning>, warning: DiscoveryWarning) {
    if warnings.len() < MAX_DISCOVERY_WARNINGS {
        warnings.push(warning);
    }
}

fn trusted_regular_file(path: &Path) -> bool {
    path.is_absolute()
        && path.ancestors().all(|component| {
            fs::symlink_metadata(component).is_ok_and(|metadata| !is_link_or_reparse(&metadata))
        })
        && fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && !is_link_or_reparse(&metadata))
}

fn trusted_directory(path: &Path) -> bool {
    path.is_absolute()
        && fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_dir() && !is_link_or_reparse(&metadata))
}

fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn filename_matches(name: &OsString, expected: &str, platform: JavaPlatform) -> bool {
    if platform == JavaPlatform::Windows {
        name.to_string_lossy().eq_ignore_ascii_case(expected)
    } else {
        name == expected
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn unique_environment_paths<const N: usize>(names: [&str; N]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = names.into_iter().filter_map(env_path).collect();
    deduplicate_paths(&mut paths, JavaPlatform::Windows);
    paths
}

fn deduplicate_search_roots(roots: &mut Vec<SearchRoot>, platform: JavaPlatform) {
    let mut seen = HashSet::new();
    roots.retain(|root| seen.insert(path_key(&root.path, platform)));
}

fn deduplicate_paths(paths: &mut Vec<PathBuf>, platform: JavaPlatform) {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path_key(path, platform)));
}

fn path_key(path: &Path, platform: JavaPlatform) -> String {
    let value = path.to_string_lossy().to_string();
    if platform == JavaPlatform::Windows {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_macos_java_homes, parse_registry_homes};
    use std::path::PathBuf;

    #[test]
    fn parses_supported_registry_value_names() {
        let output = br"
            JavaHome    REG_SZ    C:\Program Files\Java\jdk-21
            InstallationPath REG_SZ D:\JDKs\corretto-17
            path REG_EXPAND_SZ E:\Portable\jdk
            Ignored REG_SZ C:\not-java-home
        ";

        assert_eq!(
            parse_registry_homes(output),
            vec![
                PathBuf::from(r"C:\Program Files\Java\jdk-21"),
                PathBuf::from(r"D:\JDKs\corretto-17"),
                PathBuf::from(r"E:\Portable\jdk")
            ]
        );
    }

    #[test]
    fn parses_java_home_verbose_output() {
        let output = br#"
            21.0.3 (arm64) "Eclipse Adoptium" - "Temurin 21" /Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home
            /Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home
        "#;

        assert_eq!(
            parse_macos_java_homes(output),
            vec![
                PathBuf::from("/Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home"),
                PathBuf::from("/Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home")
            ]
        );
    }
}

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

const RELEASE_OVERRIDE_ENV: &str = "MCSDT_RELEASE_VERSION";

/// A version that has passed the build-version grammar and can be embedded safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildVersion(String);

impl BuildVersion {
    /// Creates a version after enforcing three decimal components without leading zeroes.
    pub fn parse(value: &str) -> Result<Self, BuildVersionError> {
        if !is_release_version(value) {
            return Err(BuildVersionError::InvalidVersion {
                source: value.to_string(),
            });
        }
        Ok(Self(value.to_string()))
    }

    /// Creates the ordinary build version from a release baseline and a seven-digit hash.
    pub fn with_hash(baseline: &str, hash: &str) -> Result<Self, BuildVersionError> {
        let baseline = Self::parse(baseline)?;
        if hash.len() != 7
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            return Err(BuildVersionError::InvalidHash {
                source: hash.to_string(),
            });
        }
        Ok(Self(format!("{}+{hash}", baseline.0)))
    }

    /// Returns the validated string for the `rustc-env` directive.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BuildVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Build-time failures that must stop a versionless binary from being produced.
#[derive(Debug)]
pub enum BuildVersionError {
    InvalidVersion {
        source: String,
    },
    InvalidHash {
        source: String,
    },
    Git {
        executable: String,
        operation: String,
        detail: String,
    },
    InvalidGitOutput {
        operation: String,
        detail: String,
    },
}

impl fmt::Display for BuildVersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVersion { source } => write!(
                formatter,
                "version '{source}' must match X.Y.Z with decimal components and no leading zeroes"
            ),
            Self::InvalidHash { source } => {
                write!(
                    formatter,
                    "git hash '{source}' is not exactly seven hexadecimal digits"
                )
            }
            Self::Git {
                executable,
                operation,
                detail,
            } => {
                write!(
                    formatter,
                    "git executable '{executable}' failed while running '{operation}': {detail}"
                )
            }
            Self::InvalidGitOutput { operation, detail } => {
                write!(
                    formatter,
                    "git {operation} returned invalid output: {detail}"
                )
            }
        }
    }
}

impl std::error::Error for BuildVersionError {}

/// Resolves the version used by all Rust runtime identity surfaces.
pub fn resolve_build_version(
    repository_root: &Path,
    release_override: Option<OsString>,
) -> Result<BuildVersion, BuildVersionError> {
    resolve_build_version_with_git(repository_root, release_override, OsStr::new("git"))
}

/// Resolves a build version with the supplied Git executable.
///
/// The build script uses the Git executable from `PATH`; accepting it here
/// makes the missing-Git failure path directly testable without changing
/// process-global environment state.
pub fn resolve_build_version_with_git(
    repository_root: &Path,
    release_override: Option<OsString>,
    git_executable: &OsStr,
) -> Result<BuildVersion, BuildVersionError> {
    if let Some(value) = release_override {
        let value = value
            .to_str()
            .ok_or_else(|| BuildVersionError::InvalidVersion {
                source: value.to_string_lossy().into_owned(),
            })?;
        return BuildVersion::parse(value);
    }

    let head = git_output(
        repository_root,
        git_executable,
        ["rev-parse", "--verify", "HEAD"],
    )?;
    if head.len() < 7
        || !head
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(BuildVersionError::InvalidHash { source: head });
    }
    let hash = &head[..7];
    let tags = git_output(
        repository_root,
        git_executable,
        ["tag", "--merged", "HEAD", "--list"],
    )?;
    let valid_tags = tags
        .lines()
        .filter_map(|tag| {
            let version = tag.strip_prefix('v')?;
            is_release_version(version).then_some(tag.to_string())
        })
        .collect::<Vec<_>>();
    let baseline = if valid_tags.is_empty() {
        "0.0.0".to_string()
    } else {
        let mut arguments = vec![
            "describe".to_string(),
            "--tags".to_string(),
            "--abbrev=0".to_string(),
            "HEAD".to_string(),
        ];
        arguments.extend(valid_tags.iter().map(|tag| format!("--match={tag}")));
        let tag = git_output(repository_root, git_executable, arguments)?;
        let version = tag
            .strip_prefix('v')
            .ok_or_else(|| BuildVersionError::InvalidGitOutput {
                operation: "describe nearest release tag".to_string(),
                detail: tag.clone(),
            })?;
        BuildVersion::parse(version)?;
        version.to_string()
    };
    BuildVersion::with_hash(&baseline, hash)
}

/// Finds the Git files that affect an ordinary build's version.
pub fn git_dependency_paths(repository_root: &Path) -> Result<Vec<PathBuf>, BuildVersionError> {
    let mut paths = vec![repository_root.join(".git")];
    for git_path in ["HEAD", "packed-refs", "refs/tags"] {
        let path = git_output(
            repository_root,
            OsStr::new("git"),
            ["rev-parse", "--git-path", git_path],
        )?;
        paths.push(resolve_git_path(repository_root, &path));
    }
    if let Some(reference) = git_symbolic_head(repository_root)? {
        let path = git_output(
            repository_root,
            OsStr::new("git"),
            ["rev-parse", "--git-path", &reference],
        )?;
        paths.push(resolve_git_path(repository_root, &path));
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn git_symbolic_head(repository_root: &Path) -> Result<Option<String>, BuildVersionError> {
    let output = Command::new("git")
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .current_dir(repository_root)
        .output()
        .map_err(|source| BuildVersionError::Git {
            executable: "git".to_string(),
            operation: "symbolic-ref HEAD".to_string(),
            detail: source.to_string(),
        })?;
    if output.status.success() {
        let reference = utf8_output("symbolic-ref HEAD", &output.stdout)?;
        if !reference.starts_with("refs/") {
            return Err(BuildVersionError::InvalidGitOutput {
                operation: "symbolic-ref HEAD".to_string(),
                detail: reference,
            });
        }
        return Ok(Some(reference));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    Err(BuildVersionError::Git {
        executable: "git".to_string(),
        operation: "symbolic-ref HEAD".to_string(),
        detail: command_error(&output.stderr, output.status.code()),
    })
}

fn git_output<I, S>(
    repository_root: &Path,
    git_executable: &OsStr,
    arguments: I,
) -> Result<String, BuildVersionError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_os_string())
        .collect::<Vec<_>>();
    let operation = arguments
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let output = Command::new(git_executable)
        .args(&arguments)
        .current_dir(repository_root)
        .output()
        .map_err(|source| BuildVersionError::Git {
            executable: git_executable.to_string_lossy().into_owned(),
            operation: operation.clone(),
            detail: source.to_string(),
        })?;
    if !output.status.success() {
        return Err(BuildVersionError::Git {
            executable: git_executable.to_string_lossy().into_owned(),
            operation,
            detail: command_error(&output.stderr, output.status.code()),
        });
    }
    utf8_output(&operation, &output.stdout)
}

fn utf8_output(operation: &str, output: &[u8]) -> Result<String, BuildVersionError> {
    String::from_utf8(output.to_vec())
        .map(|value| value.trim().to_string())
        .map_err(|error| BuildVersionError::InvalidGitOutput {
            operation: operation.to_string(),
            detail: error.to_string(),
        })
}

fn command_error(stderr: &[u8], status: Option<i32>) -> String {
    let detail = String::from_utf8_lossy(stderr).trim().to_string();
    if detail.is_empty() {
        format!("exit status {status:?}")
    } else {
        detail
    }
}

fn resolve_git_path(repository_root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repository_root.join(path)
    }
}

fn is_release_version(value: &str) -> bool {
    let components = value.split('.').collect::<Vec<_>>();
    components.len() == 3
        && components.iter().all(|component| {
            !component.is_empty()
                && (component == &"0" || !component.starts_with('0'))
                && component.bytes().all(|byte| byte.is_ascii_digit())
        })
}

pub fn release_override_from_environment() -> Option<OsString> {
    env::var_os(RELEASE_OVERRIDE_ENV)
}

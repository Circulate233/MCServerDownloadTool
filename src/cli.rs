use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use clap::{Arg, ArgAction, CommandFactory, FromArgMatches, Parser, ValueEnum};
use url::Url;

use crate::error::AppError;
use crate::i18n::{Language, Localizer, resolve_language};

/// Strict Minecraft server installation entry point.
#[derive(Debug, Clone, Parser)]
#[command(name = "mc-server-download-tool", about)]
pub struct Cli {
    /// JSON manifest path; defaults to server-install.json beside the executable.
    #[arg(long, value_name = "PATH")]
    pub manifest: Option<PathBuf>,
    /// Output language. Overrides the system locale.
    #[arg(long, value_enum, value_name = "LANG")]
    pub lang: Option<Language>,
    /// HTTP(S) or SOCKS proxy shared by downloads and the loader installer.
    #[arg(long, value_name = "URL")]
    pub proxy: Option<ProxyArgument>,
}

/// Rendered early CLI result, including help, version output, and syntax errors.
///
/// The process entry point uses this type instead of calling [`clap::Error::exit`]
/// so language selection is resolved before Clap can terminate the process.
#[derive(Debug)]
pub struct CliParseFailure {
    rendered: String,
    use_stderr: bool,
    exit_code: i32,
}

impl CliParseFailure {
    pub(crate) fn new(rendered: String, use_stderr: bool, exit_code: i32) -> Self {
        Self {
            rendered,
            use_stderr,
            exit_code,
        }
    }

    /// Returns the complete user-facing output, including its trailing newline.
    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }

    /// Returns whether the output belongs on standard error rather than standard output.
    #[must_use]
    pub const fn use_stderr(&self) -> bool {
        self.use_stderr
    }

    /// Returns Clap's stable process status (`0` for help/version, `2` for misuse).
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

/// Parses one command line after resolving the presentation language from the
/// same arguments and system locale.
///
/// The language pre-scan delegates accepted values to [`Language`]'s Clap value
/// parser. It does not decide whether the command line itself is valid; the one
/// canonical [`Cli`] command definition performs that validation exactly once.
///
/// # Errors
///
/// Returns [`CliParseFailure`] for localized help/version output and every Clap
/// syntax or value error. Invalid `--lang` values remain errors and are rendered
/// in the system-locale language because no valid explicit override exists.
pub fn try_parse_localized_from<I, T>(
    arguments: I,
    system_locale: Option<&str>,
) -> Result<Cli, CliParseFailure>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
    let language = language_from_arguments(&arguments, system_locale);
    let mut command = localized_command(language);
    let usage = command.render_usage().to_string();
    match command.try_get_matches_from_mut(arguments) {
        Ok(matches) => Cli::from_arg_matches(&matches)
            .map_err(|error| Localizer::new(language).cli_parse_failure(&error, &usage)),
        Err(error) => Err(Localizer::new(language).cli_parse_failure(&error, &usage)),
    }
}

/// Resolves the language needed to render help or syntax errors before normal
/// argument parsing can return a [`Cli`].
#[must_use]
pub fn language_from_arguments(arguments: &[OsString], system_locale: Option<&str>) -> Language {
    let mut explicit = None;
    let mut values = arguments.iter().skip(1);
    while let Some(argument) = values.next() {
        if argument == OsStr::new("--") {
            break;
        }
        if argument == OsStr::new("--lang") {
            explicit = values.next().and_then(|value| parse_language(value));
            continue;
        }
        if let Some(argument) = argument.to_str()
            && let Some(value) = argument.strip_prefix("--lang=")
        {
            explicit = Language::from_str(value, true).ok();
        }
    }
    resolve_language(explicit, system_locale)
}

fn parse_language(value: &OsStr) -> Option<Language> {
    value
        .to_str()
        .and_then(|value| Language::from_str(value, true).ok())
}

fn localized_command(language: Language) -> clap::Command {
    let localizer = Localizer::new(language);
    let heading = localizer.cli_options_heading();
    Cli::command()
        .disable_help_flag(true)
        .disable_version_flag(true)
        .version(crate::version::BUILD_VERSION)
        .about(localizer.cli_about())
        .help_template(localizer.cli_help_template())
        .mut_arg("manifest", |argument| {
            argument
                .help(localizer.cli_manifest_help())
                .help_heading(heading)
        })
        .mut_arg("lang", |argument| {
            argument
                .help(localizer.cli_language_help())
                .help_heading(heading)
                .hide_possible_values(language == Language::ZhCn)
        })
        .mut_arg("proxy", |argument| {
            argument
                .help(localizer.cli_proxy_help())
                .help_heading(heading)
        })
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::Help)
                .help(localizer.cli_help_flag_help())
                .help_heading(heading),
        )
        .arg(
            Arg::new("version")
                .short('V')
                .long("version")
                .action(ArgAction::Version)
                .help(localizer.cli_version_flag_help())
                .help_heading(heading),
        )
}

impl Cli {
    /// Resolves defaults that depend on the current process environment.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::ExecutableHasNoParent`] when no explicit manifest is
    /// supplied and `executable` has no parent directory.
    pub fn resolve(
        self,
        executable: &Path,
        system_locale: Option<&str>,
    ) -> Result<ResolvedOptions, AppError> {
        Ok(ResolvedOptions {
            manifest_path: resolve_manifest_path(self.manifest, executable)?,
            language: resolve_language(self.lang, system_locale),
            proxy: parse_explicit_proxy(self.proxy)?,
        })
    }

    /// Resolves defaults including the documented proxy environment precedence.
    ///
    /// # Errors
    ///
    /// Returns an [`AppError`] for an unusable executable path or the first
    /// configured proxy environment variable whose URL is invalid.
    pub fn resolve_with_environment<F>(
        self,
        executable: &Path,
        system_locale: Option<&str>,
        environment: F,
    ) -> Result<ResolvedOptions, AppError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        Ok(ResolvedOptions {
            manifest_path: resolve_manifest_path(self.manifest, executable)?,
            language: resolve_language(self.lang, system_locale),
            proxy: resolve_proxy(parse_explicit_proxy(self.proxy)?, environment)?,
        })
    }
}

/// Raw proxy CLI argument retained only until sanitized validation after Clap parsing.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyArgument(String);

impl fmt::Debug for ProxyArgument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl FromStr for ProxyArgument {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(value.to_string()))
    }
}

fn parse_explicit_proxy(value: Option<ProxyArgument>) -> Result<Option<ProxyUrl>, AppError> {
    value
        .map(|value| {
            value
                .0
                .parse()
                .map_err(|reason| AppError::InvalidProxy { reason })
        })
        .transpose()
}

/// Parsed and validated proxy endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyUrl(Url);

impl ProxyUrl {
    /// Exposes the parsed URL to the network and loader execution layers.
    #[must_use]
    pub const fn as_url(&self) -> &Url {
        &self.0
    }
}

impl fmt::Display for ProxyUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ProxyUrl {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !has_explicit_authority(value) {
            return Err("proxy URL must use scheme://host syntax".to_string());
        }
        let url = Url::parse(value).map_err(|error| format!("invalid proxy URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h") {
            return Err("proxy URL scheme must be http, https, socks5, or socks5h".to_string());
        }
        if url.host_str().is_none() {
            return Err("proxy URL must include a host".to_string());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("proxy URL must not embed credentials".to_string());
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err("proxy URL must not include a query or fragment".to_string());
        }
        Ok(Self(url))
    }
}

fn has_explicit_authority(value: &str) -> bool {
    value
        .split_once("://")
        .is_some_and(|(_, authority)| !authority.is_empty() && !authority.starts_with('/'))
}

/// Fully resolved command options consumed by application execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOptions {
    /// Explicit or executable-adjacent manifest path.
    pub manifest_path: PathBuf,
    /// Language selected using the documented precedence.
    pub language: Language,
    /// Validated optional proxy endpoint.
    pub proxy: Option<ProxyUrl>,
}

/// Uses an explicit path or returns server-install.json beside the executable.
///
/// # Errors
///
/// Returns [`AppError::ExecutableHasNoParent`] when no explicit path is supplied
/// and `executable` has no parent directory.
pub fn resolve_manifest_path(
    explicit: Option<PathBuf>,
    executable: &Path,
) -> Result<PathBuf, AppError> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    executable
        .parent()
        .map(|parent| parent.join("server-install.json"))
        .ok_or_else(|| AppError::ExecutableHasNoParent(executable.to_path_buf()))
}

/// Resolves the proxy from CLI then uppercase and lowercase standard variables.
///
/// # Errors
///
/// Returns [`AppError::InvalidEnvironmentProxy`] when the first configured
/// environment value is not a supported proxy URL.
pub fn resolve_proxy<F>(
    explicit: Option<ProxyUrl>,
    mut environment: F,
) -> Result<Option<ProxyUrl>, AppError>
where
    F: FnMut(&str) -> Option<String>,
{
    if explicit.is_some() {
        return Ok(explicit);
    }
    for name in [
        "HTTPS_PROXY",
        "ALL_PROXY",
        "HTTP_PROXY",
        "https_proxy",
        "all_proxy",
        "http_proxy",
    ] {
        if let Some(value) = environment(name) {
            let proxy = value
                .parse()
                .map_err(|reason| AppError::InvalidEnvironmentProxy { name, reason })?;
            return Ok(Some(proxy));
        }
    }
    Ok(None)
}

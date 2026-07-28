use std::fmt::Write as _;

use clap::ValueEnum;
use clap::error::{ContextKind, ErrorKind};

use crate::cli::CliParseFailure;
use crate::error::{AppError, ManifestError, ManifestValidationError};
use crate::install::{InstallEvent, InstallStage};
use crate::java::{
    DiscoveryError, DiscoveryWarning, ParallelProbeError, ProbeError, ProbeRejection,
    ProbeRejectionReason, ProcessError,
};
use crate::loader::ProcessStream;
use crate::manifest::ManifestFile;
use crate::net::TransferPhase;

/// User-facing languages supported by the command-line surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Language {
    /// English (United States), also the deterministic fallback language.
    #[value(name = "en-US", alias = "en", alias = "en_US")]
    EnUs,
    /// Simplified Chinese (China).
    #[value(name = "zh-CN", alias = "zh", alias = "zh_CN")]
    ZhCn,
}

/// Resolves language using CLI override, system locale, then English.
#[must_use]
pub fn resolve_language(explicit: Option<Language>, system_locale: Option<&str>) -> Language {
    explicit.unwrap_or_else(|| {
        system_locale.map_or(Language::EnUs, |locale| {
            if locale.to_ascii_lowercase().starts_with("zh") {
                Language::ZhCn
            } else {
                Language::EnUs
            }
        })
    })
}

/// Small message catalog for current command output.
#[derive(Debug, Clone, Copy)]
pub struct Localizer {
    language: Language,
}

impl Localizer {
    /// Creates a catalog bound to a resolved language.
    #[must_use]
    pub const fn new(language: Language) -> Self {
        Self { language }
    }

    /// Returns the localized prefix for a fatal error log line.
    #[must_use]
    pub const fn error_prefix(self) -> &'static str {
        match self.language {
            Language::EnUs => "error",
            Language::ZhCn => "错误",
        }
    }

    /// Returns the localized one-line program description used by Clap help.
    #[must_use]
    pub const fn cli_about(self) -> &'static str {
        match self.language {
            Language::EnUs => "Install a Minecraft server from server-install.json",
            Language::ZhCn => "Minecraft 服务端安装器：根据 server-install.json 完成安装",
        }
    }

    /// Returns the localized Clap help template.
    #[must_use]
    pub const fn cli_help_template(self) -> &'static str {
        match self.language {
            Language::EnUs => "{about-with-newline}\nUsage: {usage}\n\n{all-args}{after-help}",
            Language::ZhCn => "{about-with-newline}\n用法：{usage}\n\n{all-args}{after-help}",
        }
    }

    /// Returns the localized heading for command options.
    #[must_use]
    pub const fn cli_options_heading(self) -> &'static str {
        match self.language {
            Language::EnUs => "Options",
            Language::ZhCn => "选项",
        }
    }

    /// Returns localized help for `--manifest`.
    #[must_use]
    pub const fn cli_manifest_help(self) -> &'static str {
        match self.language {
            Language::EnUs => {
                "Manifest path; defaults to server-install.json beside this executable"
            }
            Language::ZhCn => "清单文件路径；默认读取本程序同目录的 server-install.json",
        }
    }

    /// Returns localized help for `--lang`.
    #[must_use]
    pub const fn cli_language_help(self) -> &'static str {
        match self.language {
            Language::EnUs => "Interface language; overrides the system locale",
            Language::ZhCn => "界面语言（en-US 或 zh-CN）；优先于系统语言",
        }
    }

    /// Returns localized help for `--proxy` without exposing a configured URL.
    #[must_use]
    pub const fn cli_proxy_help(self) -> &'static str {
        match self.language {
            Language::EnUs => "HTTP(S) or SOCKS proxy used for all downloads",
            Language::ZhCn => "所有下载共用的 HTTP(S) 或 SOCKS 代理",
        }
    }

    /// Returns localized help for the generated help flag.
    #[must_use]
    pub const fn cli_help_flag_help(self) -> &'static str {
        match self.language {
            Language::EnUs => "Print help",
            Language::ZhCn => "显示帮助",
        }
    }

    /// Returns localized help for the generated version flag.
    #[must_use]
    pub const fn cli_version_flag_help(self) -> &'static str {
        match self.language {
            Language::EnUs => "Print version",
            Language::ZhCn => "显示版本",
        }
    }

    /// Converts a Clap result into localized process output while preserving
    /// Clap's help/version and usage exit statuses.
    #[must_use]
    pub fn cli_parse_failure(self, error: &clap::Error, fallback_usage: &str) -> CliParseFailure {
        if matches!(
            error.kind(),
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
        ) {
            let rendered = match (self.language, error.kind()) {
                (Language::ZhCn, ErrorKind::DisplayHelp) => {
                    error.to_string().replace("选项:", "选项：")
                }
                _ => error.to_string(),
            };
            return CliParseFailure::new(rendered, error.use_stderr(), error.exit_code());
        }

        let detail = self.cli_error_detail(error);
        let usage = error
            .get(ContextKind::Usage)
            .map_or_else(|| fallback_usage.to_string(), ToString::to_string);
        let usage = usage.strip_prefix("Usage: ").unwrap_or(&usage).to_string();
        let mut rendered = match self.language {
            Language::EnUs => format!("command-line error: {detail}\n"),
            Language::ZhCn => format!("命令行参数错误：{detail}\n"),
        };
        rendered.push('\n');
        rendered.push_str(match self.language {
            Language::EnUs => "Usage: ",
            Language::ZhCn => "用法：",
        });
        rendered.push_str(&usage);
        rendered.push('\n');
        rendered.push_str(match self.language {
            Language::EnUs => "\nRun '--help' for more information.\n",
            Language::ZhCn => "\n请运行“--help”查看完整帮助。\n",
        });
        CliParseFailure::new(rendered, true, error.exit_code())
    }

    fn cli_error_detail(self, error: &clap::Error) -> String {
        let invalid_arg = clap_context(error, ContextKind::InvalidArg);
        let invalid_value = clap_context(error, ContextKind::InvalidValue);
        let valid_values = clap_context(error, ContextKind::ValidValue);
        let suggested = clap_context(error, ContextKind::SuggestedArg)
            .or_else(|| clap_context(error, ContextKind::SuggestedValue));
        match (self.language, error.kind()) {
            (Language::EnUs, ErrorKind::InvalidValue) => format!(
                "value '{}' is not valid for {}{}",
                invalid_value.as_deref().unwrap_or(""),
                invalid_arg.as_deref().unwrap_or("the selected option"),
                valid_values.map_or_else(String::new, |values| format!("; valid values: {values}"))
            ),
            (Language::ZhCn, ErrorKind::InvalidValue) => format!(
                "参数“{}”不接受值“{}”{}",
                invalid_arg.as_deref().unwrap_or("未知参数"),
                invalid_value.as_deref().unwrap_or(""),
                valid_values.map_or_else(String::new, |values| format!("；可用值：{values}"))
            ),
            (Language::EnUs, ErrorKind::UnknownArgument) => format!(
                "unrecognized argument '{}'{}",
                invalid_arg.as_deref().unwrap_or(""),
                suggested.map_or_else(String::new, |value| format!("; did you mean '{value}'?"))
            ),
            (Language::ZhCn, ErrorKind::UnknownArgument) => format!(
                "无法识别参数“{}”{}",
                invalid_arg.as_deref().unwrap_or(""),
                suggested.map_or_else(String::new, |value| format!("；是否想使用“{value}”？"))
            ),
            (Language::EnUs, ErrorKind::MissingRequiredArgument) => format!(
                "required argument is missing: {}",
                invalid_arg.as_deref().unwrap_or("see usage below")
            ),
            (Language::ZhCn, ErrorKind::MissingRequiredArgument) => format!(
                "缺少必需参数：{}",
                invalid_arg.as_deref().unwrap_or("请查看下方用法")
            ),
            (Language::EnUs, ErrorKind::TooFewValues) => format!(
                "{} requires a value",
                invalid_arg.as_deref().unwrap_or("the selected option")
            ),
            (Language::ZhCn, ErrorKind::TooFewValues) => format!(
                "参数“{}”缺少值",
                invalid_arg.as_deref().unwrap_or("未知参数")
            ),
            (Language::EnUs, ErrorKind::ArgumentConflict) => format!(
                "{} conflicts with another supplied argument",
                invalid_arg.as_deref().unwrap_or("the selected option")
            ),
            (Language::ZhCn, ErrorKind::ArgumentConflict) => format!(
                "参数“{}”与另一个已提供参数冲突",
                invalid_arg.as_deref().unwrap_or("未知参数")
            ),
            (Language::EnUs, _) => "the supplied arguments are invalid".to_string(),
            (Language::ZhCn, _) => "提供的参数或参数值无效".to_string(),
        }
    }

    /// Formats a non-fatal Java discovery-source warning.
    #[must_use]
    pub fn java_discovery_warning(self, warning: &DiscoveryWarning) -> String {
        match self.language {
            Language::EnUs => format!(
                "could not inspect Java source '{}': {}; continuing with other sources",
                warning.source, warning.reason
            ),
            Language::ZhCn => format!(
                "无法检查 Java 来源“{}”：{}；将继续检查其他来源",
                warning.source, warning.reason
            ),
        }
    }

    /// Formats a fatal Java discovery initialization failure.
    #[must_use]
    pub fn java_discovery_error(self, error: &DiscoveryError) -> String {
        match (self.language, error) {
            (Language::EnUs, DiscoveryError::Initialization { reason }) => format!(
                "Java discovery could not be initialized: {reason}. Check the process environment and try again"
            ),
            (Language::ZhCn, DiscoveryError::Initialization { reason }) => {
                format!("无法初始化 Java 发现流程：{reason}。请检查当前进程环境后重试")
            }
        }
    }

    /// Formats a fatal bounded-worker failure during Java probing.
    #[must_use]
    pub fn java_parallel_probe_error(self, error: &ParallelProbeError) -> String {
        match (self.language, error) {
            (Language::EnUs, ParallelProbeError::AvailableParallelism { source }) => format!(
                "could not determine Java probe parallelism: {source}. Check operating-system resource limits"
            ),
            (Language::ZhCn, ParallelProbeError::AvailableParallelism { source }) => {
                format!("无法确定 Java 探测并发数：{source}。请检查操作系统资源限制")
            }
            (Language::EnUs, ParallelProbeError::WorkerSpawn { worker, source }) => format!(
                "could not start Java probe worker {worker}: {source}. Check process and thread limits"
            ),
            (Language::ZhCn, ParallelProbeError::WorkerSpawn { worker, source }) => {
                format!("无法启动 Java 探测线程 {worker}：{source}。请检查进程和线程资源限制")
            }
            (Language::EnUs, ParallelProbeError::QueuePoisoned) => {
                "Java probe work queue became unavailable; restart the installer".to_string()
            }
            (Language::ZhCn, ParallelProbeError::QueuePoisoned) => {
                "Java 探测任务队列已不可用，请重新启动安装器".to_string()
            }
            (Language::EnUs, ParallelProbeError::IncompleteResults { expected, received }) => {
                format!(
                    "Java probing returned {received} of {expected} results; restart the installer"
                )
            }
            (Language::ZhCn, ParallelProbeError::IncompleteResults { expected, received }) => {
                format!(
                    "Java 探测应返回 {expected} 项结果，实际仅收到 {received} 项；请重新启动安装器"
                )
            }
            (Language::EnUs, ParallelProbeError::WorkerPanic { worker }) => {
                format!("Java probe worker {worker} stopped unexpectedly; restart the installer")
            }
            (Language::ZhCn, ParallelProbeError::WorkerPanic { worker }) => {
                format!("Java 探测线程 {worker} 异常停止，请重新启动安装器")
            }
        }
    }

    /// Formats one rejected Java candidate without losing its structured cause.
    #[must_use]
    pub fn java_probe_rejection(self, rejection: &ProbeRejection) -> String {
        let reason = match &rejection.reason {
            ProbeRejectionReason::FeatureReleaseMismatch { found, required } => {
                match self.language {
                    Language::EnUs => format!(
                        "Java feature release {found} does not match required release {required}"
                    ),
                    Language::ZhCn => {
                        format!("Java 主版本 {found} 与要求的主版本 {required} 不匹配")
                    }
                }
            }
            ProbeRejectionReason::Probe(error) => self.java_probe_error(error),
        };
        format!("{}: {reason}", rejection.executable.display())
    }

    /// Formats one Java metadata probe failure.
    #[must_use]
    pub fn java_probe_error(self, error: &ProbeError) -> String {
        match (self.language, error) {
            (language, ProbeError::Process(error)) => Self::java_process_error(language, error),
            (Language::EnUs, ProbeError::ExitStatus { status, stderr }) => {
                format!("Java metadata probe exited with status {status:?}: {stderr}")
            }
            (Language::ZhCn, ProbeError::ExitStatus { status, stderr }) => {
                format!("Java 元数据探测进程以状态 {status:?} 退出：{stderr}")
            }
            (Language::EnUs, ProbeError::MissingProperty { property }) => {
                format!("Java metadata did not contain required property '{property}'")
            }
            (Language::ZhCn, ProbeError::MissingProperty { property }) => {
                format!("Java 元数据缺少必需属性“{property}”")
            }
            (Language::EnUs, ProbeError::InvalidVersion { version }) => {
                format!("java.version '{version}' is invalid")
            }
            (Language::ZhCn, ProbeError::InvalidVersion { version }) => {
                format!("java.version“{version}”无法解析为有效主版本")
            }
        }
    }

    fn java_process_error(language: Language, error: &ProcessError) -> String {
        match error {
            ProcessError::Spawn { program, source } => Self::java_process_io_error(
                language,
                JavaProcessIoOperation::Spawn,
                program,
                source,
            ),
            ProcessError::InvalidTimeout { program, timeout } => match language {
                Language::EnUs => format!(
                    "Java probe timeout {timeout:?} is invalid for '{}'",
                    program.display()
                ),
                Language::ZhCn => {
                    format!("Java“{}”的探测超时值 {timeout:?} 无效", program.display())
                }
            },
            ProcessError::Poll { program, source } => {
                Self::java_process_io_error(language, JavaProcessIoOperation::Poll, program, source)
            }
            ProcessError::ReaderSpawn {
                program,
                stream,
                source,
            } => Self::java_process_io_error(
                language,
                JavaProcessIoOperation::ReaderSpawn(stream),
                program,
                source,
            ),
            ProcessError::Read {
                program,
                stream,
                source,
            } => Self::java_process_io_error(
                language,
                JavaProcessIoOperation::Read(stream),
                program,
                source,
            ),
            ProcessError::ReaderPanic { program, stream } => match language {
                Language::EnUs => format!(
                    "{stream} reader stopped unexpectedly for Java '{}'",
                    program.display()
                ),
                Language::ZhCn => {
                    format!("Java“{}”的 {stream} 读取线程异常停止", program.display())
                }
            },
            ProcessError::MissingPipe {
                program,
                stream,
                cleanup_error,
            } => match language {
                Language::EnUs => format!(
                    "Java '{}' did not provide its {stream} pipe{}",
                    program.display(),
                    cleanup_suffix(language, cleanup_error.as_deref())
                ),
                Language::ZhCn => format!(
                    "Java“{}”未提供 {stream} 管道{}",
                    program.display(),
                    cleanup_suffix(language, cleanup_error.as_deref())
                ),
            },
            ProcessError::TimedOut {
                program,
                timeout,
                cleanup_error,
            } => match language {
                Language::EnUs => format!(
                    "Java '{}' did not respond within {timeout:?} and was terminated{}",
                    program.display(),
                    cleanup_suffix(language, cleanup_error.as_deref())
                ),
                Language::ZhCn => format!(
                    "Java“{}”在 {timeout:?} 内未响应，已终止{}",
                    program.display(),
                    cleanup_suffix(language, cleanup_error.as_deref())
                ),
            },
        }
    }

    fn java_process_io_error(
        language: Language,
        operation: JavaProcessIoOperation<'_>,
        program: &std::path::Path,
        source: &std::io::Error,
    ) -> String {
        match (language, operation) {
            (Language::EnUs, JavaProcessIoOperation::Spawn) => format!(
                "could not start Java executable '{}': {source}",
                program.display()
            ),
            (Language::ZhCn, JavaProcessIoOperation::Spawn) => {
                format!("无法启动 Java 可执行文件“{}”：{source}", program.display())
            }
            (Language::EnUs, JavaProcessIoOperation::Poll) => format!(
                "could not query Java process '{}': {source}",
                program.display()
            ),
            (Language::ZhCn, JavaProcessIoOperation::Poll) => {
                format!("无法查询 Java 进程“{}”的状态：{source}", program.display())
            }
            (Language::EnUs, JavaProcessIoOperation::ReaderSpawn(stream)) => format!(
                "could not start {stream} reader for Java '{}': {source}",
                program.display()
            ),
            (Language::ZhCn, JavaProcessIoOperation::ReaderSpawn(stream)) => format!(
                "无法为 Java“{}”启动 {stream} 读取线程：{source}",
                program.display()
            ),
            (Language::EnUs, JavaProcessIoOperation::Read(stream)) => format!(
                "could not read {stream} from Java '{}': {source}",
                program.display()
            ),
            (Language::ZhCn, JavaProcessIoOperation::Read(stream)) => {
                format!("无法读取 Java“{}”的 {stream}：{source}", program.display())
            }
        }
    }

    /// Returns the localized operation text used when Java diagnostics cannot be written.
    #[must_use]
    pub const fn java_diagnostic_output_error(self) -> &'static str {
        match self.language {
            Language::EnUs => "failed to write Java discovery diagnostics",
            Language::ZhCn => "无法写入 Java 发现诊断信息",
        }
    }

    /// Formats a fatal application error with a localized category and sanitized detail.
    #[must_use]
    pub fn fatal_error(self, error: &AppError) -> String {
        let category = match (self.language, error.exit_code()) {
            (Language::EnUs, crate::error::ExitCode::ManifestIo) => "manifest I/O",
            (Language::EnUs, crate::error::ExitCode::ManifestParse) => "manifest format",
            (Language::EnUs, crate::error::ExitCode::ManifestValidation) => "manifest validation",
            (Language::EnUs, crate::error::ExitCode::Configuration) => "configuration",
            (Language::EnUs, crate::error::ExitCode::Java) => "Java selection",
            (Language::EnUs, crate::error::ExitCode::Network) => "network",
            (Language::EnUs, crate::error::ExitCode::Integrity) => "integrity",
            (Language::EnUs, crate::error::ExitCode::Installation) => "installation",
            (Language::EnUs, _) => "internal",
            (Language::ZhCn, crate::error::ExitCode::ManifestIo) => "清单读取",
            (Language::ZhCn, crate::error::ExitCode::ManifestParse) => "清单格式",
            (Language::ZhCn, crate::error::ExitCode::ManifestValidation) => "清单校验",
            (Language::ZhCn, crate::error::ExitCode::Configuration) => "配置",
            (Language::ZhCn, crate::error::ExitCode::Java) => "Java 选择",
            (Language::ZhCn, crate::error::ExitCode::Network) => "网络",
            (Language::ZhCn, crate::error::ExitCode::Integrity) => "完整性校验",
            (Language::ZhCn, crate::error::ExitCode::Installation) => "安装",
            (Language::ZhCn, _) => "内部错误",
        };
        format!(
            "{} ({category}): {}",
            self.error_prefix(),
            self.app_error_detail(error)
        )
    }

    fn app_error_detail(self, error: &AppError) -> String {
        match (self.language, error) {
            (language, AppError::Manifest(error)) => Self::manifest_error(language, error),
            (Language::EnUs, AppError::CurrentExecutable(source)) => format!(
                "could not determine the running executable: {source}. Start the installer from a normal filesystem location"
            ),
            (Language::ZhCn, AppError::CurrentExecutable(source)) => format!(
                "无法确定当前安装器的可执行文件路径：{source}。请从普通文件系统目录启动安装器"
            ),
            (Language::EnUs, AppError::ExecutableHasNoParent(path)) => format!(
                "executable path '{}' has no parent directory. Supply --manifest explicitly",
                path.display()
            ),
            (Language::ZhCn, AppError::ExecutableHasNoParent(path)) => format!(
                "可执行文件路径“{}”没有父目录。请显式提供 --manifest",
                path.display()
            ),
            (Language::EnUs, AppError::InvalidEnvironmentProxy { name, reason }) => format!(
                "proxy from {name} is invalid: {}. Correct or remove that environment variable",
                proxy_reason(Language::EnUs, reason)
            ),
            (Language::ZhCn, AppError::InvalidEnvironmentProxy { name, reason }) => format!(
                "环境变量 {name} 中的代理无效：{}。请修正或删除该环境变量",
                proxy_reason(Language::ZhCn, reason)
            ),
            (Language::EnUs, AppError::InvalidProxy { reason }) => format!(
                "--proxy is invalid: {}. Use scheme://host without embedded credentials",
                proxy_reason(Language::EnUs, reason)
            ),
            (Language::ZhCn, AppError::InvalidProxy { reason }) => format!(
                "--proxy 无效：{}。请使用不含账号密码的 scheme://host 格式",
                proxy_reason(Language::ZhCn, reason)
            ),
            (Language::EnUs, AppError::Java { reason }) => format!(
                "Java runtime selection failed: {reason}. Select a Java executable with the exact required major version"
            ),
            (Language::ZhCn, AppError::Java { reason }) => format!(
                "Java 运行时选择失败：{reason}。请选择与清单要求主版本完全一致的 Java 可执行文件"
            ),
            (Language::EnUs, AppError::Network(error)) => format!(
                "network operation failed: {error}. Check connectivity, proxy settings, and the source URL before retrying"
            ),
            (Language::ZhCn, AppError::Network(error)) => {
                format!("网络操作失败：{error}。请检查网络连接、代理设置和来源 URL 后重试")
            }
            (Language::EnUs, AppError::Installation(error)) => format!(
                "server installation failed: {error}. Review .mcsdt/install.log, correct the reported file or loader problem, then run the installer again"
            ),
            (Language::ZhCn, AppError::Installation(error)) => format!(
                "服务端安装失败：{error}。请查看 .mcsdt/install.log，修正提示的文件或 Loader 问题后重新运行安装器"
            ),
        }
    }

    fn manifest_error(language: Language, error: &ManifestError) -> String {
        match (language, error) {
            (Language::EnUs, ManifestError::Read { path, source }) => format!(
                "failed to read manifest {}: {source}. Confirm the file exists and is readable",
                path.display()
            ),
            (Language::ZhCn, ManifestError::Read { path, source }) => format!(
                "无法读取清单“{}”：{source}。请确认文件存在且当前用户有读取权限",
                path.display()
            ),
            (Language::EnUs, ManifestError::Parse { origin, source }) => format!(
                "failed to parse manifest {origin}: {source}. Correct the JSON syntax and schema fields"
            ),
            (Language::ZhCn, ManifestError::Parse { origin, source }) => {
                format!("无法解析清单“{origin}”：{source}。请修正 JSON 语法和字段结构")
            }
            (language, ManifestError::Validation(error)) => {
                Self::manifest_validation_error(language, error)
            }
        }
    }

    fn manifest_validation_error(language: Language, error: &ManifestValidationError) -> String {
        match (language, error) {
            (
                Language::EnUs,
                ManifestValidationError::UnsupportedSchemaVersion { found },
            ) => format!(
                "schema_version must be 1, but {found} was provided. Regenerate the manifest with a compatible mcmpu release"
            ),
            (
                Language::ZhCn,
                ManifestValidationError::UnsupportedSchemaVersion { found },
            ) => format!(
                "schema_version 必须为 1，当前为 {found}。请使用兼容版本的 mcmpu 重新生成清单"
            ),
            (
                Language::EnUs,
                ManifestValidationError::InvalidField { field, reason },
            ) => format!(
                "manifest field '{field}' is invalid: {reason}. Correct that field and retry"
            ),
            (
                Language::ZhCn,
                ManifestValidationError::InvalidField { field, reason },
            ) => format!("清单字段“{field}”无效：{reason}。请修正该字段后重试"),
            (Language::EnUs, ManifestValidationError::DuplicatePath { path }) => format!(
                "manifest target path '{path}' is declared more than once. Keep exactly one entry for that path"
            ),
            (Language::ZhCn, ManifestValidationError::DuplicatePath { path }) => format!(
                "清单目标路径“{path}”被重复声明。该路径只能保留一个文件项"
            ),
            (Language::EnUs, ManifestValidationError::CurseForgeKeyRequired) => {
                "curseforge_api_key is required because the manifest contains a CurseForge CDN download. Regenerate the manifest with the release build key"
                    .to_string()
            }
            (Language::ZhCn, ManifestValidationError::CurseForgeKeyRequired) => {
                "清单包含 CurseForge CDN 下载，因此必须提供 curseforge_api_key。请使用带发布密钥的构建重新生成清单"
                    .to_string()
            }
            (Language::EnUs, ManifestValidationError::UnusedCurseForgeKey) => {
                "curseforge_api_key is forbidden because no CurseForge CDN download uses it. Remove the unused key"
                    .to_string()
            }
            (Language::ZhCn, ManifestValidationError::UnusedCurseForgeKey) => {
                "清单没有 CurseForge CDN 下载，禁止保留未使用的 curseforge_api_key。请删除该字段"
                    .to_string()
            }
        }
    }

    /// Formats one structured installation event without exposing credentials.
    #[must_use]
    pub fn install_event(self, event: &InstallEvent) -> String {
        match event {
            InstallEvent::Stage(stage) => self.install_stage(*stage).to_string(),
            InstallEvent::Reused { target } => match self.language {
                Language::EnUs => format!("reused verified file: {}", target.display()),
                Language::ZhCn => format!("复用已验证文件：{}", target.display()),
            },
            InstallEvent::Transfer(event) => match self.language {
                Language::EnUs => format!(
                    "download {}: {} ({} bytes)",
                    event.task_id,
                    transfer_phase(event.phase, Language::EnUs),
                    event.transferred_bytes
                ),
                Language::ZhCn => format!(
                    "下载 {}：{}（{} 字节）",
                    event.task_id,
                    transfer_phase(event.phase, Language::ZhCn),
                    event.transferred_bytes
                ),
            },
            InstallEvent::LoaderOutput { stream, line } => match stream {
                ProcessStream::Stdout | ProcessStream::Stderr => line.clone(),
            },
            InstallEvent::LoaderReused => match self.language {
                Language::EnUs => "verified loader output; installer execution skipped".to_string(),
                Language::ZhCn => "Loader 输出验证通过，已跳过安装器执行".to_string(),
            },
            InstallEvent::CleanupWarning { path, reason } => match self.language {
                Language::EnUs => {
                    format!(
                        "could not remove temporary file '{}': {reason}",
                        path.display()
                    )
                }
                Language::ZhCn => {
                    format!("无法删除临时文件“{}”：{reason}", path.display())
                }
            },
        }
    }

    /// Builds the complete manual-file gate report.
    #[must_use]
    pub fn manual_file_report(self, files: &[ManifestFile]) -> String {
        let mut report = match self.language {
            Language::EnUs => "Manual files require attention:\n".to_string(),
            Language::ZhCn => "以下文件需要手动处理：\n".to_string(),
        };
        for file in files {
            match self.language {
                Language::EnUs => write!(
                    report,
                    "\n- {}\n  Path: {}\n  Project: {}\n  Required: {} bytes, SHA-1 {}\n",
                    file.name, file.path, file.project_page, file.size, file.sha1
                )
                .expect("writing to String cannot fail"),
                Language::ZhCn => write!(
                    report,
                    "\n- {}\n  路径：{}\n  项目页：{}\n  要求：{} 字节，SHA-1 {}\n",
                    file.name, file.path, file.project_page, file.size, file.sha1
                )
                .expect("writing to String cannot fail"),
            }
        }
        report
    }

    /// Formats the final successful installation summary.
    #[must_use]
    pub fn installation_complete(self, root: &std::path::Path) -> String {
        match self.language {
            Language::EnUs => format!("server installation completed: {}", root.display()),
            Language::ZhCn => format!("服务端安装完成：{}", root.display()),
        }
    }

    const fn install_stage(self, stage: InstallStage) -> &'static str {
        match (self.language, stage) {
            (Language::EnUs, InstallStage::Locked) => "installation lock acquired",
            (Language::EnUs, InstallStage::SelectingJava) => "discovering compatible Java runtimes",
            (Language::EnUs, InstallStage::Inspecting) => "validating existing installation state",
            (Language::EnUs, InstallStage::Downloading) => "downloading required artifacts",
            (Language::EnUs, InstallStage::CheckingManualFiles) => "checking manual files",
            (Language::EnUs, InstallStage::InstallingLoader) => "installing and verifying loader",
            (Language::EnUs, InstallStage::WritingState) => {
                "writing start script and installation state"
            }
            (Language::EnUs, InstallStage::Completed) => "installation completed",
            (Language::ZhCn, InstallStage::Locked) => "已获取安装锁",
            (Language::ZhCn, InstallStage::SelectingJava) => "正在发现兼容的 Java 运行时",
            (Language::ZhCn, InstallStage::Inspecting) => "正在验证已有安装状态",
            (Language::ZhCn, InstallStage::Downloading) => "正在下载所需文件",
            (Language::ZhCn, InstallStage::CheckingManualFiles) => "正在检查手动文件",
            (Language::ZhCn, InstallStage::InstallingLoader) => "正在安装并验证 Loader",
            (Language::ZhCn, InstallStage::WritingState) => "正在写入启动脚本和安装状态",
            (Language::ZhCn, InstallStage::Completed) => "安装已完成",
        }
    }
}

enum JavaProcessIoOperation<'a> {
    Spawn,
    Poll,
    ReaderSpawn(&'a str),
    Read(&'a str),
}

const fn transfer_phase(phase: TransferPhase, language: Language) -> &'static str {
    match (language, phase) {
        (Language::EnUs, TransferPhase::Queued) => "queued",
        (Language::EnUs, TransferPhase::Probing) => "probing",
        (Language::EnUs, TransferPhase::Single) => "receiving",
        (Language::EnUs, TransferPhase::Segmented) => "receiving segments",
        (Language::EnUs, TransferPhase::Retrying) => "retrying",
        (Language::EnUs, TransferPhase::Verifying) => "verifying",
        (Language::EnUs, TransferPhase::Completed) => "completed",
        (Language::EnUs, TransferPhase::Failed) => "failed",
        (Language::EnUs, TransferPhase::Cancelled) => "cancelled",
        (Language::ZhCn, TransferPhase::Queued) => "等待中",
        (Language::ZhCn, TransferPhase::Probing) => "正在探测",
        (Language::ZhCn, TransferPhase::Single) => "正在接收",
        (Language::ZhCn, TransferPhase::Segmented) => "正在分段接收",
        (Language::ZhCn, TransferPhase::Retrying) => "正在重试",
        (Language::ZhCn, TransferPhase::Verifying) => "正在校验",
        (Language::ZhCn, TransferPhase::Completed) => "已完成",
        (Language::ZhCn, TransferPhase::Failed) => "失败",
        (Language::ZhCn, TransferPhase::Cancelled) => "已取消",
    }
}

fn clap_context(error: &clap::Error, kind: ContextKind) -> Option<String> {
    error.get(kind).map(ToString::to_string)
}

fn cleanup_suffix(language: Language, cleanup_error: Option<&str>) -> String {
    cleanup_error.map_or_else(String::new, |error| match language {
        Language::EnUs => format!("; cleanup also failed: {error}"),
        Language::ZhCn => format!("；清理进程时也失败：{error}"),
    })
}

fn proxy_reason(language: Language, reason: &str) -> String {
    if language == Language::EnUs {
        return reason.to_string();
    }
    match reason {
        "proxy URL must use scheme://host syntax" => {
            "代理 URL 必须使用 scheme://host 格式".to_string()
        }
        "proxy URL scheme must be http, https, socks5, or socks5h" => {
            "代理协议必须为 http、https、socks5 或 socks5h".to_string()
        }
        "proxy URL must include a host" => "代理 URL 必须包含主机名".to_string(),
        "proxy URL must not embed credentials" => "代理 URL 不得内嵌账号或密码".to_string(),
        "proxy URL must not include a query or fragment" => {
            "代理 URL 不得包含查询参数或片段".to_string()
        }
        _ if reason.starts_with("invalid proxy URL:") => {
            reason.replacen("invalid proxy URL:", "代理 URL 无法解析：", 1)
        }
        _ => reason.to_string(),
    }
}

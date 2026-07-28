use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::i18n::{Language, Localizer};

use super::discovery::JavaPlatform;
use super::probe::{JavaRuntime, RuntimeProbe};

/// One input event returned by an interactive terminal implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// One complete line without interpretation by the selector.
    Line(String),
    /// The input stream reached EOF.
    EndOfFile,
}

/// Input/output boundary for deterministic, testable Java selection.
pub trait InteractiveIo: Send + Sync {
    /// Writes user-facing choices or prompts to standard output.
    ///
    /// # Errors
    ///
    /// Returns the underlying output failure.
    fn write_output(&self, message: &str) -> io::Result<()>;

    /// Writes rejected input and candidate diagnostics to standard error.
    ///
    /// # Errors
    ///
    /// Returns the underlying error-output failure.
    fn write_error(&self, message: &str) -> io::Result<()>;

    /// Reads one line or explicitly reports EOF.
    ///
    /// # Errors
    ///
    /// Returns the underlying input failure. Implementations must preserve
    /// [`io::ErrorKind::Interrupted`] so Ctrl+C can produce a clear exit.
    fn read_line(&self) -> io::Result<InputEvent>;
}

/// Terminal I/O implementation using the process standard streams.
#[derive(Debug, Default, Clone, Copy)]
pub struct ConsoleIo;

impl InteractiveIo for ConsoleIo {
    fn write_output(&self, message: &str) -> io::Result<()> {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        output.write_all(message.as_bytes())?;
        output.flush()
    }

    fn write_error(&self, message: &str) -> io::Result<()> {
        let stderr = io::stderr();
        let mut output = stderr.lock();
        output.write_all(message.as_bytes())?;
        output.flush()
    }

    fn read_line(&self) -> io::Result<InputEvent> {
        let mut line = String::new();
        let read = io::stdin().read_line(&mut line)?;
        if read == 0 {
            Ok(InputEvent::EndOfFile)
        } else {
            Ok(InputEvent::Line(line))
        }
    }
}

/// Fatal interruption or terminal failure during Java selection.
#[derive(Debug, Error)]
pub enum SelectionError {
    /// A standard-stream operation failed.
    #[error("{message}: {source}")]
    Io {
        /// Localized operation description.
        message: &'static str,
        /// Standard-stream failure.
        #[source]
        source: io::Error,
    },
    /// The user closed the input stream without selecting Java.
    #[error("{message}")]
    EndOfInput {
        /// Localized exit description.
        message: &'static str,
    },
    /// Interactive input was interrupted, normally by Ctrl+C.
    #[error("{message}")]
    Interrupted {
        /// Localized exit description.
        message: &'static str,
    },
}

/// Selects one verified runtime. A non-empty discovered list accepts only a
/// displayed sequence number. An empty list repeatedly accepts an executable or
/// Java home path and validates it by executing Java.
///
/// # Errors
///
/// Returns [`SelectionError`] only for I/O failure, EOF, or interruption. Invalid
/// choices, paths, probes, and Java versions are logged and retried.
pub fn select_runtime<I, P>(
    runtimes: &[JavaRuntime],
    required_major: u16,
    platform: JavaPlatform,
    language: Language,
    io: &I,
    probe: &P,
) -> Result<JavaRuntime, SelectionError>
where
    I: InteractiveIo,
    P: RuntimeProbe,
{
    if runtimes.is_empty() {
        select_manual_runtime(required_major, platform, language, io, probe)
    } else {
        select_numbered_runtime(runtimes, language, io)
    }
}

fn select_numbered_runtime<I: InteractiveIo>(
    runtimes: &[JavaRuntime],
    language: Language,
    io: &I,
) -> Result<JavaRuntime, SelectionError> {
    write_output(io, language, catalog(language).available_header)?;
    for (index, runtime) in runtimes.iter().enumerate() {
        write_output(
            io,
            language,
            &format!(
                "  {}. Java {} | {} | {} | {}\n",
                index + 1,
                runtime.version,
                runtime.vendor,
                runtime.architecture,
                runtime.executable.display()
            ),
        )?;
    }
    loop {
        write_output(io, language, catalog(language).number_prompt)?;
        let line = read_line(io, language)?;
        let selection = line.trim().parse::<usize>();
        if let Ok(index) = selection
            && (1..=runtimes.len()).contains(&index)
        {
            return Ok(runtimes[index - 1].clone());
        }
        write_retry_error(io, language, catalog(language).invalid_number)?;
    }
}

fn select_manual_runtime<I, P>(
    required_major: u16,
    platform: JavaPlatform,
    language: Language,
    io: &I,
    probe: &P,
) -> Result<JavaRuntime, SelectionError>
where
    I: InteractiveIo,
    P: RuntimeProbe,
{
    write_output(io, language, catalog(language).none_found)?;
    loop {
        write_output(io, language, catalog(language).path_prompt)?;
        let line = read_line(io, language)?;
        let value = line.trim();
        if value.is_empty() {
            write_retry_error(io, language, catalog(language).empty_path)?;
            continue;
        }
        let executable = match resolve_manual_executable(Path::new(value), platform, language) {
            Ok(path) => path,
            Err(reason) => {
                write_retry_error(io, language, &reason)?;
                continue;
            }
        };
        match probe.inspect(&executable) {
            Ok(runtime) if runtime.major == required_major => return Ok(runtime),
            Ok(runtime) => {
                let reason = match language {
                    Language::EnUs => format!(
                        "Java {} does not match required feature release {required_major}",
                        runtime.major
                    ),
                    Language::ZhCn => format!(
                        "Java {} 与要求的主版本 {required_major} 不匹配",
                        runtime.major
                    ),
                };
                write_retry_error(io, language, &reason)?;
            }
            Err(error) => write_retry_error(
                io,
                language,
                &Localizer::new(language).java_probe_error(&error),
            )?,
        }
    }
}

fn resolve_manual_executable(
    input: &Path,
    platform: JavaPlatform,
    language: Language,
) -> Result<PathBuf, String> {
    let executable = if input.is_dir() {
        input.join("bin").join(platform.executable_name())
    } else {
        input.to_path_buf()
    };
    if !executable.is_file() {
        return Err(match language {
            Language::EnUs => format!(
                "Java executable does not exist or is not a file: {}",
                executable.display()
            ),
            Language::ZhCn => format!("Java 可执行文件不存在或不是文件：{}", executable.display()),
        });
    }
    fs::canonicalize(&executable).map_err(|source| match language {
        Language::EnUs => format!(
            "failed to canonicalize Java executable '{}': {source}",
            executable.display()
        ),
        Language::ZhCn => format!(
            "无法解析 Java 可执行文件“{}”的实际路径：{source}",
            executable.display()
        ),
    })
}

fn read_line<I: InteractiveIo>(io: &I, language: Language) -> Result<String, SelectionError> {
    match io.read_line() {
        Ok(InputEvent::Line(line)) => Ok(line),
        Ok(InputEvent::EndOfFile) => Err(SelectionError::EndOfInput {
            message: catalog(language).eof,
        }),
        Err(source) if source.kind() == io::ErrorKind::Interrupted => {
            Err(SelectionError::Interrupted {
                message: catalog(language).interrupted,
            })
        }
        Err(source) => Err(SelectionError::Io {
            message: catalog(language).io_failure,
            source,
        }),
    }
}

fn write_output<I: InteractiveIo>(
    io: &I,
    language: Language,
    message: &str,
) -> Result<(), SelectionError> {
    io.write_output(message)
        .map_err(|source| SelectionError::Io {
            message: catalog(language).io_failure,
            source,
        })
}

fn write_retry_error<I: InteractiveIo>(
    io: &I,
    language: Language,
    reason: &str,
) -> Result<(), SelectionError> {
    let localizer = Localizer::new(language);
    io.write_error(&format!("{}: {reason}\n", localizer.error_prefix()))
        .map_err(|source| SelectionError::Io {
            message: catalog(language).io_failure,
            source,
        })
}

struct SelectionCatalog {
    available_header: &'static str,
    number_prompt: &'static str,
    invalid_number: &'static str,
    none_found: &'static str,
    path_prompt: &'static str,
    empty_path: &'static str,
    eof: &'static str,
    interrupted: &'static str,
    io_failure: &'static str,
}

const fn catalog(language: Language) -> SelectionCatalog {
    match language {
        Language::EnUs => SelectionCatalog {
            available_header: "Compatible Java runtimes:\n",
            number_prompt: "Select Java by number: ",
            invalid_number: "enter one of the displayed sequence numbers",
            none_found: "No compatible Java runtime was discovered.\n",
            path_prompt: "Java executable or Java home: ",
            empty_path: "Java path must not be empty",
            eof: "Java selection ended because input reached EOF",
            interrupted: "Java selection was cancelled by Ctrl+C",
            io_failure: "Java selection input/output failed",
        },
        Language::ZhCn => SelectionCatalog {
            available_header: "可用的 Java 运行时：\n",
            number_prompt: "请输入 Java 序号：",
            invalid_number: "请输入列表中显示的序号",
            none_found: "未发现符合要求的 Java 运行时。\n",
            path_prompt: "请输入 Java 可执行文件或 Java Home：",
            empty_path: "Java 路径不能为空",
            eof: "输入已结束，Java 选择退出",
            interrupted: "已通过 Ctrl+C 取消 Java 选择",
            io_failure: "Java 选择期间读写终端失败",
        },
    }
}

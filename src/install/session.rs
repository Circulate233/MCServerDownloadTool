use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::i18n::{Language, Localizer};

use super::filesystem::InstallRoot;
use super::lock::InstallLock;
use super::{InstallError, InstallEvent, InstallObserver, InstallObserverError, InstallStage};

const METADATA_DIRECTORY: &str = ".mcsdt";
const INSTALL_LOG: &str = "install.log";

/// Exclusive, logged installation lifetime shared by application and installer layers.
pub struct InstallSession {
    root: InstallRoot,
    observer: Arc<SessionObserver>,
    _lock: InstallLock,
}

impl InstallSession {
    /// Acquires the root lock and creates a freshly truncated durable installation log.
    ///
    /// # Errors
    ///
    /// Returns an error when the root or metadata boundary is unsafe, another
    /// process owns the lock, or the durable log cannot be created and synced.
    pub fn acquire(
        manifest_path: &Path,
        language: Language,
        terminal: Arc<dyn InstallObserver>,
        secrets: impl IntoIterator<Item = String>,
    ) -> Result<Self, InstallError> {
        let root = InstallRoot::from_manifest(manifest_path)?;
        let metadata_root = root.create_directory(Path::new(METADATA_DIRECTORY))?;
        root.verify_target(&metadata_root.join("install.lock"))?;
        let lock = InstallLock::acquire(&metadata_root).map_err(|source| {
            if super::lock::is_contended(&source) {
                InstallError::Locked {
                    path: metadata_root.join("install.lock"),
                }
            } else {
                InstallError::io(
                    "acquire installation lock",
                    metadata_root.join("install.lock"),
                    source,
                )
            }
        })?;
        root.verify_target(&metadata_root)?;
        let log_path = metadata_root.join(INSTALL_LOG);
        root.verify_target(&log_path)?;
        match fs::symlink_metadata(&log_path) {
            Ok(metadata) => {
                if is_link_or_reparse(&metadata)
                    || has_multiple_links(&log_path, &metadata).map_err(|source| {
                        InstallError::io("inspect installation log link count", &log_path, source)
                    })?
                    || !metadata.is_file()
                {
                    return Err(InstallError::UnsafePath {
                        path: log_path,
                        reason: "installation log must be a regular file with one link".to_string(),
                    });
                }
                fs::remove_file(&log_path).map_err(|source| {
                    InstallError::io("remove previous installation log", &log_path, source)
                })?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(InstallError::io(
                    "inspect installation log",
                    &log_path,
                    source,
                ));
            }
        }
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&log_path)
            .map_err(|source| {
                InstallError::io("create persistent installation log", &log_path, source)
            })?;
        let logger = InstallLog::new(file, log_path, secrets)?;
        let observer = Arc::new(SessionObserver {
            language,
            terminal,
            logger: Mutex::new(logger),
            failure: Mutex::new(None),
        });
        let session = Self {
            root,
            observer,
            _lock: lock,
        };
        session.emit(InstallEvent::Stage(InstallStage::Locked))?;
        Ok(session)
    }

    /// Returns the canonical filesystem boundary for this locked session.
    #[must_use]
    pub fn root(&self) -> &InstallRoot {
        &self.root
    }

    /// Returns the observer that mirrors sanitized events to terminal and durable log.
    #[must_use]
    pub fn observer(&self) -> Arc<dyn InstallObserver> {
        self.observer.clone()
    }

    /// Emits one event and immediately surfaces any persistent-log failure.
    ///
    /// # Errors
    ///
    /// Returns an error when writing or synchronizing the persistent log fails,
    /// including a prior failure recorded by a concurrent event observer.
    pub fn emit(&self, event: InstallEvent) -> Result<(), InstallError> {
        self.observer.observe(event)?;
        Ok(())
    }

    /// Records the selected Java executable in the durable session log.
    ///
    /// # Errors
    ///
    /// Returns an error when the log mutex is unavailable or the durable log
    /// cannot be written, flushed, and synchronized.
    pub fn record_selected_java(&self, executable: &Path) -> Result<(), InstallError> {
        let message = match self.observer.language {
            Language::EnUs => format!("selected Java executable: {}", executable.display()),
            Language::ZhCn => format!("已选择 Java 可执行文件：{}", executable.display()),
        };
        self.observer.record_line("java", &message)?;
        Ok(())
    }

    /// Records the final failure before releasing the session lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the log mutex is unavailable or the sanitized
    /// failure record cannot be durably written and synchronized.
    pub fn record_failure(&self, error: &str) -> Result<(), InstallError> {
        self.observer.record_line("failed", error)?;
        Ok(())
    }

    /// Fails when a concurrent observer encountered a durable logging error.
    ///
    /// # Errors
    ///
    /// Returns the stored log I/O failure, or a synchronization error when the
    /// failure-state mutex was poisoned.
    pub fn check_log(&self) -> Result<(), InstallError> {
        self.observer.check()?;
        Ok(())
    }
}

struct SessionObserver {
    language: Language,
    terminal: Arc<dyn InstallObserver>,
    logger: Mutex<InstallLog>,
    failure: Mutex<Option<InstallObserverError>>,
}

impl SessionObserver {
    fn record_line(&self, category: &str, message: &str) -> Result<(), InstallObserverError> {
        self.check_failure()?;
        let mut logger = self
            .logger
            .lock()
            .map_err(|_| InstallObserverError::Synchronization)?;
        if let Err(error) = logger.write_line(category, message, true) {
            let failure = InstallObserverError::PersistentLog {
                path: logger.path.clone(),
                kind: error.kind(),
                reason: error.to_string(),
            };
            drop(logger);
            self.store_failure(failure.clone())?;
            return Err(failure);
        }
        Ok(())
    }

    fn store_failure(&self, failure: InstallObserverError) -> Result<(), InstallObserverError> {
        let mut stored = self
            .failure
            .lock()
            .map_err(|_| InstallObserverError::Synchronization)?;
        if stored.is_none() {
            *stored = Some(failure);
        }
        Ok(())
    }

    fn check_failure(&self) -> Result<(), InstallObserverError> {
        let failure = self
            .failure
            .lock()
            .map_err(|_| InstallObserverError::Synchronization)?;
        failure.clone().map_or(Ok(()), Err)
    }
}

impl InstallObserver for SessionObserver {
    fn observe(&self, event: InstallEvent) -> Result<(), InstallObserverError> {
        let message = Localizer::new(self.language).install_event(&event);
        self.check_failure()?;
        let mut logger = self
            .logger
            .lock()
            .map_err(|_| InstallObserverError::Synchronization)?;
        if let Err(error) = logger.write_event(&event, &message) {
            let failure = InstallObserverError::PersistentLog {
                path: logger.path.clone(),
                kind: error.kind(),
                reason: error.to_string(),
            };
            drop(logger);
            self.store_failure(failure.clone())?;
            return Err(failure);
        }
        drop(logger);
        if let Err(error) = self.terminal.observe(event) {
            self.store_failure(error.clone())?;
            return Err(error);
        }
        Ok(())
    }

    fn check(&self) -> Result<(), InstallObserverError> {
        self.check_failure()?;
        let mut logger = self
            .logger
            .lock()
            .map_err(|_| InstallObserverError::Synchronization)?;
        logger
            .file
            .flush()
            .map_err(|error| InstallObserverError::PersistentLog {
                path: logger.path.clone(),
                kind: error.kind(),
                reason: error.to_string(),
            })
    }
}

struct InstallLog {
    file: BufWriter<File>,
    path: PathBuf,
    secrets: Vec<String>,
    progress: HashMap<String, (crate::net::TransferPhase, Instant)>,
}

impl InstallLog {
    fn new(
        file: File,
        path: PathBuf,
        secrets: impl IntoIterator<Item = String>,
    ) -> Result<Self, InstallError> {
        let mut log = Self {
            file: BufWriter::with_capacity(64 * 1024, file),
            path,
            secrets: secrets
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect(),
            progress: HashMap::new(),
        };
        log.write_line("session", "installation session started", true)
            .map_err(|source| {
                InstallError::io("initialize persistent installation log", &log.path, source)
            })?;
        Ok(log)
    }

    fn write_event(&mut self, event: &InstallEvent, message: &str) -> io::Result<()> {
        let durable = matches!(event, InstallEvent::Stage(_));
        if let InstallEvent::Transfer(transfer) = event {
            let now = Instant::now();
            let should_write = self
                .progress
                .get(&transfer.task_id)
                .is_none_or(|(phase, last)| {
                    *phase != transfer.phase
                        || transfer.phase.terminal()
                        || now.saturating_duration_since(*last) >= Duration::from_secs(1)
                });
            if !should_write {
                return Ok(());
            }
            self.progress
                .insert(transfer.task_id.clone(), (transfer.phase, now));
        }
        self.write_line("event", message, durable)
    }

    fn write_line(&mut self, category: &str, message: &str, durable: bool) -> io::Result<()> {
        let sanitized = sanitize(message, &self.secrets);
        writeln!(self.file, "[{category}] {sanitized}")?;
        if durable {
            self.file.flush()?;
            self.file.get_ref().sync_data()?;
        }
        Ok(())
    }
}

impl Drop for InstallLog {
    fn drop(&mut self) {
        let _ = self.file.flush();
        let _ = self.file.get_ref().sync_data();
    }
}

#[cfg(windows)]
fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn has_multiple_links(_path: &Path, metadata: &fs::Metadata) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(metadata.nlink() > 1)
}

#[cfg(windows)]
fn has_multiple_links(path: &Path, _metadata: &fs::Metadata) -> io::Result<bool> {
    use std::time::Duration;

    use crate::java::{EnvironmentPolicy, ProcessRequest, ProcessRunner, SystemProcessRunner};

    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "SystemRoot is not absolute"))?;
    let fsutil = system_root.join("System32").join("fsutil.exe");
    let metadata = fs::symlink_metadata(&fsutil)?;
    if !metadata.is_file() || is_link_or_reparse(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "System32 fsutil.exe is not a trusted regular file",
        ));
    }
    let output = SystemProcessRunner
        .run(
            &ProcessRequest::new(fsutil, Duration::from_secs(5))
                .with_arguments([
                    std::ffi::OsString::from("hardlink"),
                    std::ffi::OsString::from("list"),
                    path.as_os_str().to_os_string(),
                ])
                .with_environment(EnvironmentPolicy::Inherit)
                .with_output_limit(256 * 1024),
        )
        .map_err(io::Error::other)?;
    if output.exit_code != Some(0) {
        return Err(io::Error::other(format!(
            "fsutil hardlink list exited with {:?}",
            output.exit_code
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(2)
        .count()
        > 1)
}

#[cfg(not(any(windows, unix)))]
fn has_multiple_links(_path: &Path, _metadata: &fs::Metadata) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "hard-link inspection is unavailable on this platform",
    ))
}

fn sanitize(value: &str, secrets: &[String]) -> String {
    let mut sanitized = value.to_string();
    for secret in secrets {
        sanitized = sanitized.replace(secret, "[REDACTED]");
    }
    sanitized
        .split_whitespace()
        .map(sanitize_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn sanitize_token(token: &str) -> String {
    let (prefix, body, suffix) = trim_token_punctuation(token);
    let Ok(mut url) = url::Url::parse(body) else {
        return token.to_string();
    };
    if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h") {
        return token.to_string();
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    format!("{prefix}{url}{suffix}")
}

fn trim_token_punctuation(token: &str) -> (&str, &str, &str) {
    let start = ["https://", "http://", "socks5://", "socks5h://"]
        .into_iter()
        .filter_map(|scheme| token.find(scheme))
        .min()
        .unwrap_or(0);
    let end = token
        .char_indices()
        .rev()
        .find(|(_, character)| {
            !matches!(
                character,
                ')' | ']' | '}' | ',' | ';' | ':' | '\'' | '"' | '。' | '，'
            )
        })
        .map_or(token.len(), |(index, character)| {
            index + character.len_utf8()
        });
    (&token[..start], &token[start..end], &token[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizer_removes_secrets_credentials_query_and_fragment() {
        let text = sanitize(
            "key SECRET request to 'https://user:pass@example.com/file?a=1#fragment': failed",
            &["SECRET".to_string()],
        );
        assert_eq!(
            text,
            "key [REDACTED] request to 'https://example.com/file': failed"
        );
    }
}

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
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
        self.check()?;
        let mut logger = self
            .logger
            .lock()
            .map_err(|_| InstallObserverError::Synchronization)?;
        if let Err(error) = logger.write_line(category, message) {
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
}

impl InstallObserver for SessionObserver {
    fn observe(&self, event: InstallEvent) -> Result<(), InstallObserverError> {
        let message = Localizer::new(self.language).install_event(&event);
        self.record_line("event", &message)?;
        if let Err(error) = self.terminal.observe(event) {
            self.store_failure(error.clone())?;
            return Err(error);
        }
        Ok(())
    }

    fn check(&self) -> Result<(), InstallObserverError> {
        let failure = self
            .failure
            .lock()
            .map_err(|_| InstallObserverError::Synchronization)?;
        failure.clone().map_or(Ok(()), Err)
    }
}

struct InstallLog {
    file: File,
    path: PathBuf,
    secrets: Vec<String>,
}

impl InstallLog {
    fn new(
        file: File,
        path: PathBuf,
        secrets: impl IntoIterator<Item = String>,
    ) -> Result<Self, InstallError> {
        let mut log = Self {
            file,
            path,
            secrets: secrets
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect(),
        };
        log.write_line("session", "installation session started")
            .map_err(|source| {
                InstallError::io("initialize persistent installation log", &log.path, source)
            })?;
        Ok(log)
    }

    fn write_line(&mut self, category: &str, message: &str) -> io::Result<()> {
        let sanitized = sanitize(message, &self.secrets);
        writeln!(self.file, "[{category}] {sanitized}")?;
        self.file.flush()?;
        self.file.sync_data()
    }
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

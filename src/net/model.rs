use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::de::DeserializeOwned;
use thiserror::Error;

pub(crate) const MAX_GLOBAL_REQUESTS: usize = 128;
pub(crate) const MAX_QUEUED_JOBS: usize = 1024;
pub(crate) const MAX_TRANSFER_WORKERS: usize = 32;
pub(crate) const MAX_REDIRECTS: usize = 32;
pub(crate) const MAX_REQUEST_ATTEMPTS: usize = 16;
pub(crate) const MAX_IDLE_CONNECTIONS_PER_HOST: usize = 128;

/// Connection, timeout, retry, and identity settings shared by one network session.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// User-Agent sent by every request from this session.
    pub user_agent: String,
    /// Optional HTTP/HTTPS/SOCKS proxy accepted by `reqwest`.
    pub proxy: Option<String>,
    /// Maximum duration spent establishing a connection.
    pub connect_timeout: Duration,
    /// Timeout applied to blocking connect, read, and write operations.
    pub read_timeout: Duration,
    /// Maximum number of redirects followed by the explicit redirect loop.
    pub max_redirects: usize,
    /// Maximum request attempts for retryable transport and HTTP failures.
    pub max_attempts: usize,
    /// Initial exponential retry delay when `Retry-After` is absent.
    pub retry_base_delay: Duration,
    /// Upper bound for an exponential retry delay.
    pub retry_max_delay: Duration,
    /// Number of idle connections retained per origin.
    pub idle_connections_per_host: usize,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            user_agent: format!("mc-server-download-tool/{}", crate::version::BUILD_VERSION),
            proxy: None,
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_mins(1),
            max_redirects: 10,
            max_attempts: 4,
            retry_base_delay: Duration::from_millis(200),
            retry_max_delay: Duration::from_secs(5),
            idle_connections_per_host: 24,
        }
    }
}

impl NetworkConfig {
    pub(crate) fn validate(&self) -> Result<(), NetworkError> {
        if self.user_agent.trim().is_empty() {
            return Err(NetworkError::InvalidConfiguration {
                reason: "user_agent must not be empty".to_string(),
            });
        }
        if self.connect_timeout.is_zero() || self.read_timeout.is_zero() {
            return Err(NetworkError::InvalidConfiguration {
                reason: "connect_timeout and read_timeout must be greater than zero".to_string(),
            });
        }
        if self.max_redirects == 0 || self.max_attempts == 0 {
            return Err(NetworkError::InvalidConfiguration {
                reason: "max_redirects and max_attempts must be greater than zero".to_string(),
            });
        }
        if self.max_redirects > MAX_REDIRECTS || self.max_attempts > MAX_REQUEST_ATTEMPTS {
            return Err(NetworkError::InvalidConfiguration {
                reason: format!(
                    "max_redirects must not exceed {MAX_REDIRECTS} and max_attempts must not exceed {MAX_REQUEST_ATTEMPTS}"
                ),
            });
        }
        if self.retry_base_delay > self.retry_max_delay {
            return Err(NetworkError::InvalidConfiguration {
                reason: "retry_base_delay must not exceed retry_max_delay".to_string(),
            });
        }
        if self.idle_connections_per_host == 0 {
            return Err(NetworkError::InvalidConfiguration {
                reason: "idle_connections_per_host must be greater than zero".to_string(),
            });
        }
        if self.idle_connections_per_host > MAX_IDLE_CONNECTIONS_PER_HOST {
            return Err(NetworkError::InvalidConfiguration {
                reason: format!(
                    "idle_connections_per_host must not exceed {MAX_IDLE_CONNECTIONS_PER_HOST}"
                ),
            });
        }
        Ok(())
    }
}

/// Hard concurrency limits enforced across all transfers sharing an engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkLimits {
    /// Maximum simultaneous HTTP requests across all hosts and files.
    pub global_requests: usize,
    /// Maximum simultaneous HTTP requests to one host.
    pub requests_per_host: usize,
    /// Maximum simultaneous HTTP requests belonging to one artifact.
    pub requests_per_file: usize,
    /// File size at which strict Range probing may enable segmentation.
    pub segment_threshold: u64,
    /// Preferred bytes per Range segment before applying the per-file cap.
    pub target_segment_size: u64,
}

impl NetworkLimits {
    /// Detects logical CPU availability and derives all concurrency budgets.
    ///
    /// Detection failures are returned to the caller because silently choosing
    /// a fallback would hide an invalid runtime environment.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] when logical parallelism cannot be detected.
    pub fn automatic() -> Result<Self, NetworkError> {
        let parallelism = std::thread::available_parallelism().map_err(|source| {
            NetworkError::ParallelismDetection {
                reason: source.to_string(),
            }
        })?;
        Self::for_parallelism(parallelism.get())
    }

    /// Derives deterministic request budgets from a logical CPU count.
    ///
    /// This constructor is public so callers can inspect planned budgets before
    /// creating worker pools and tests can validate the scheduling formula.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] when `logical_cpus` is zero.
    pub fn for_parallelism(logical_cpus: usize) -> Result<Self, NetworkError> {
        if logical_cpus == 0 {
            return Err(NetworkError::InvalidConfiguration {
                reason: "logical CPU count must be greater than zero".to_string(),
            });
        }
        let global_requests = logical_cpus.saturating_mul(4).clamp(8, 64);
        let requests_per_host = global_requests.min(logical_cpus.saturating_mul(2).clamp(4, 32));
        let requests_per_file = requests_per_host.min(logical_cpus.clamp(2, 16));
        Ok(Self {
            global_requests,
            requests_per_host,
            requests_per_file,
            segment_threshold: 16 * 1024 * 1024,
            target_segment_size: 8 * 1024 * 1024,
        })
    }

    /// Returns the number of Range segments justified by file size and all
    /// applicable request budgets. Empty files do not require a segment.
    #[must_use]
    pub fn range_segment_count(&self, file_size: u64) -> usize {
        if file_size == 0 {
            return 0;
        }
        let required = file_size.div_ceil(self.target_segment_size);
        let required = usize::try_from(required).unwrap_or(usize::MAX);
        let budget = self
            .global_requests
            .min(self.requests_per_host)
            .min(self.requests_per_file);
        required.min(budget).max(1)
    }

    pub(crate) fn validate(&self) -> Result<(), NetworkError> {
        if self.global_requests == 0 || self.requests_per_host == 0 || self.requests_per_file == 0 {
            return Err(NetworkError::InvalidConfiguration {
                reason: "all request concurrency limits must be greater than zero".to_string(),
            });
        }
        if self.requests_per_host > self.global_requests
            || self.requests_per_file > self.global_requests
        {
            return Err(NetworkError::InvalidConfiguration {
                reason: "per-host and per-file limits must not exceed the global limit".to_string(),
            });
        }
        if self.global_requests > MAX_GLOBAL_REQUESTS {
            return Err(NetworkError::InvalidConfiguration {
                reason: format!(
                    "global_requests must not exceed the hard limit of {MAX_GLOBAL_REQUESTS}"
                ),
            });
        }
        if self.segment_threshold == 0 || self.target_segment_size == 0 {
            return Err(NetworkError::InvalidConfiguration {
                reason: "segment sizes must be greater than zero".to_string(),
            });
        }
        Ok(())
    }
}

/// Headers that may only be transmitted to an explicit set of HTTP origins.
///
/// Redirect handling is performed one hop at a time. These headers are rebuilt
/// for every hop and omitted unless the destination scheme, hostname, and
/// effective port match an entry in `allowed_origins`.
#[derive(Debug, Clone, Default)]
pub struct SensitiveHeaders {
    headers: HeaderMap,
    allowed_origins: BTreeSet<Origin>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Origin {
    scheme: String,
    host: String,
    port: u16,
}

impl SensitiveHeaders {
    /// Creates an empty sensitive header set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the effective origin of one HTTP URL allowed to receive credentials.
    /// Paths, queries, and fragments are ignored; URL user information is rejected.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] when `url` is not an absolute HTTP(S) URL.
    pub fn allow_origin(mut self, url: impl AsRef<str>) -> Result<Self, NetworkError> {
        let url = parse_http_url(url.as_ref())?;
        self.allowed_origins.insert(Origin::from_url(&url)?);
        Ok(self)
    }

    /// Marks and adds any explicit credential header. At least one allowed
    /// origin must be configured before the request is built.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] if credential policy initialization fails.
    pub fn insert(
        mut self,
        name: HeaderName,
        mut value: HeaderValue,
    ) -> Result<Self, NetworkError> {
        value.set_sensitive(true);
        self.headers.insert(name, value);
        Ok(self)
    }

    pub(crate) fn validate(&self) -> Result<(), NetworkError> {
        if !self.headers.is_empty() && self.allowed_origins.is_empty() {
            return Err(NetworkError::InvalidConfiguration {
                reason: "sensitive headers require at least one allowed origin".to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn for_url(&self, url: &Url) -> HeaderMap {
        if Origin::from_url(url)
            .ok()
            .is_some_and(|origin| self.allowed_origins.contains(&origin))
        {
            self.headers.clone()
        } else {
            HeaderMap::new()
        }
    }
}

impl Origin {
    fn from_url(url: &Url) -> Result<Self, NetworkError> {
        let host = url
            .host_str()
            .ok_or_else(|| NetworkError::InvalidConfiguration {
                reason: "credential origin must contain a hostname".to_string(),
            })?;
        let port =
            url.port_or_known_default()
                .ok_or_else(|| NetworkError::InvalidConfiguration {
                    reason: "credential origin must have an effective port".to_string(),
                })?;
        Ok(Self {
            scheme: url.scheme().to_ascii_lowercase(),
            host: host.trim_end_matches('.').to_ascii_lowercase(),
            port,
        })
    }
}

/// A bounded in-memory GET request.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub(crate) url: Url,
    pub(crate) headers: HeaderMap,
    pub(crate) sensitive_headers: SensitiveHeaders,
    pub(crate) max_response_bytes: usize,
}

impl HttpRequest {
    /// Starts building a GET request whose body cannot exceed `max_response_bytes`.
    pub fn get(url: impl AsRef<str>, max_response_bytes: usize) -> HttpRequestBuilder {
        HttpRequestBuilder {
            url: url.as_ref().to_string(),
            headers: HeaderMap::new(),
            sensitive_headers: SensitiveHeaders::new(),
            max_response_bytes,
        }
    }

    /// Returns the initial URL before redirects.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }
}

/// Builder for [`HttpRequest`].
pub struct HttpRequestBuilder {
    url: String,
    headers: HeaderMap,
    sensitive_headers: SensitiveHeaders,
    max_response_bytes: usize,
}

impl HttpRequestBuilder {
    /// Adds a non-credential request header.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] for credential or engine-controlled headers.
    pub fn header(mut self, name: HeaderName, value: HeaderValue) -> Result<Self, NetworkError> {
        reject_sensitive_regular_header(&name)?;
        if matches!(
            name,
            reqwest::header::RANGE | reqwest::header::IF_RANGE | reqwest::header::ACCEPT_ENCODING
        ) {
            return Err(NetworkError::InvalidConfiguration {
                reason: format!(
                    "artifact header '{}' is controlled by the transfer engine",
                    name.as_str()
                ),
            });
        }
        self.headers.insert(name, value);
        Ok(self)
    }

    /// Sets host-scoped credentials for this request.
    #[must_use]
    pub fn sensitive_headers(mut self, headers: SensitiveHeaders) -> Self {
        self.sensitive_headers = headers;
        self
    }

    /// Validates and creates the request.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] for an invalid URL, response limit, or credential policy.
    pub fn build(self) -> Result<HttpRequest, NetworkError> {
        if self.max_response_bytes == 0 {
            return Err(NetworkError::InvalidConfiguration {
                reason: "max_response_bytes must be greater than zero".to_string(),
            });
        }
        self.sensitive_headers.validate()?;
        let url = parse_http_url(&self.url)?;
        Ok(HttpRequest {
            url,
            headers: self.headers,
            sensitive_headers: self.sensitive_headers,
            max_response_bytes: self.max_response_bytes,
        })
    }
}

/// Complete bounded response returned by [`crate::net::HttpTransport`].
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// Final URL after explicit redirect handling.
    pub final_url: Url,
    /// HTTP response status.
    pub status: reqwest::StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Response body, bounded by the originating request.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Parses the bounded response as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] when the response body is not valid JSON for `T`.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, NetworkError> {
        serde_json::from_slice(&self.body).map_err(|source| NetworkError::Json {
            url: redact_url(&self.final_url),
            source,
        })
    }
}

/// Immutable request for one artifact published to one filesystem target.
#[derive(Debug, Clone)]
pub struct ArtifactRequest {
    pub(crate) task_id: String,
    pub(crate) urls: Vec<Url>,
    pub(crate) target: PathBuf,
    pub(crate) headers: HeaderMap,
    pub(crate) sensitive_headers: SensitiveHeaders,
    pub(crate) expected_size: Option<u64>,
    pub(crate) expected_sha1: Option<String>,
    pub(crate) expected_sha256: Option<String>,
}

impl ArtifactRequest {
    /// Starts a request with its stable task ID, destination, and primary URL.
    pub fn builder(
        task_id: impl Into<String>,
        target: impl Into<PathBuf>,
        primary_url: impl AsRef<str>,
    ) -> ArtifactRequestBuilder {
        ArtifactRequestBuilder {
            task_id: task_id.into(),
            urls: vec![primary_url.as_ref().to_string()],
            target: target.into(),
            headers: HeaderMap::new(),
            sensitive_headers: SensitiveHeaders::new(),
            expected_size: None,
            expected_sha1: None,
            expected_sha256: None,
        }
    }

    /// Stable identifier copied into progress events and outcomes.
    #[must_use]
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// Final path replaced only after complete verification.
    #[must_use]
    pub fn target(&self) -> &Path {
        &self.target
    }
}

/// Builder for [`ArtifactRequest`].
pub struct ArtifactRequestBuilder {
    task_id: String,
    urls: Vec<String>,
    target: PathBuf,
    headers: HeaderMap,
    sensitive_headers: SensitiveHeaders,
    expected_size: Option<u64>,
    expected_sha1: Option<String>,
    expected_sha256: Option<String>,
}

impl ArtifactRequestBuilder {
    /// Appends a fallback URL tried after earlier candidates fail.
    #[must_use]
    pub fn candidate_url(mut self, url: impl AsRef<str>) -> Self {
        self.urls.push(url.as_ref().to_string());
        self
    }

    /// Adds a non-credential header to every candidate and redirect hop.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] when `name` is a credential header.
    pub fn header(mut self, name: HeaderName, value: HeaderValue) -> Result<Self, NetworkError> {
        reject_sensitive_regular_header(&name)?;
        self.headers.insert(name, value);
        Ok(self)
    }

    /// Sets host-scoped credentials for candidate downloads.
    #[must_use]
    pub fn sensitive_headers(mut self, headers: SensitiveHeaders) -> Self {
        self.sensitive_headers = headers;
        self
    }

    /// Requires the completed artifact to have exactly this many bytes.
    #[must_use]
    pub fn expected_size(mut self, expected_size: u64) -> Self {
        self.expected_size = Some(expected_size);
        self
    }

    /// Requires the completed artifact to match this hexadecimal SHA-1.
    #[must_use]
    pub fn expected_sha1(mut self, expected_sha1: impl Into<String>) -> Self {
        self.expected_sha1 = Some(expected_sha1.into());
        self
    }

    /// Requires the completed artifact to match this hexadecimal SHA-256.
    #[must_use]
    pub fn expected_sha256(mut self, expected_sha256: impl Into<String>) -> Self {
        self.expected_sha256 = Some(expected_sha256.into());
        self
    }

    /// Validates the complete request.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] for invalid identifiers, paths, URLs, hashes, or credentials.
    pub fn build(self) -> Result<ArtifactRequest, NetworkError> {
        if self.task_id.trim().is_empty() {
            return Err(NetworkError::InvalidConfiguration {
                reason: "artifact task_id must not be empty".to_string(),
            });
        }
        if self.target.as_os_str().is_empty() {
            return Err(NetworkError::InvalidConfiguration {
                reason: "artifact target path must not be empty".to_string(),
            });
        }
        self.sensitive_headers.validate()?;
        let urls = self
            .urls
            .iter()
            .map(|url| parse_http_url(url))
            .collect::<Result<Vec<_>, _>>()?;
        let expected_sha1 = normalize_digest(self.expected_sha1, 40, "SHA-1")?;
        let expected_sha256 = normalize_digest(self.expected_sha256, 64, "SHA-256")?;
        Ok(ArtifactRequest {
            task_id: self.task_id,
            urls,
            target: self.target,
            headers: self.headers,
            sensitive_headers: self.sensitive_headers,
            expected_size: self.expected_size,
            expected_sha1,
            expected_sha256,
        })
    }
}

/// Transfer method selected after strict Range probing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadMode {
    /// One streaming HTTP response supplied the artifact.
    Single,
    /// Multiple verified Range responses were merged in order.
    Segmented,
}

/// Successful artifact publication details.
///
/// Success means the verified file replaced the target namespace entry. If a
/// post-replacement directory sync cannot confirm crash durability, the engine
/// logs that condition but still returns this outcome because publication has
/// already committed and cannot be reported as an ordinary pre-commit failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactOutcome {
    /// Stable task identifier.
    pub task_id: String,
    /// Candidate URL that supplied the artifact.
    pub source_url: Url,
    /// Published target path.
    pub target: PathBuf,
    /// Number of verified bytes published.
    pub bytes: u64,
    /// Selected transfer method.
    pub mode: DownloadMode,
}

/// Progress phases emitted by the artifact engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferPhase {
    /// Accepted by the batch scheduler but not yet started.
    Queued,
    /// Determining whether strict Range segmentation is safe.
    Probing,
    /// Streaming one complete response.
    Single,
    /// Streaming verified Range segments.
    Segmented,
    /// Waiting before a bounded retry.
    Retrying,
    /// Computing final size and digests.
    Verifying,
    /// Verified data was atomically published.
    Completed,
    /// The task ended with a concrete error.
    Failed,
    /// The task was not started or was stopped after another batch error.
    Cancelled,
}

impl TransferPhase {
    pub(crate) fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Throttled progress snapshot for one artifact.
#[derive(Debug, Clone)]
pub struct TransferEvent {
    /// Stable artifact task identifier.
    pub task_id: String,
    /// Current phase.
    pub phase: TransferPhase,
    /// Monotonic cumulative network bytes received for this task. Retries and
    /// fallback candidates may make this value exceed `total_bytes`.
    pub transferred_bytes: u64,
    /// Expected length of the artifact currently being attempted, if known.
    pub total_bytes: Option<u64>,
    /// Number of active HTTP requests across the shared engine.
    pub active_requests: usize,
    /// Average cumulative network throughput since the task was queued.
    pub bytes_per_second: f64,
}

/// A typed observer failure that cancels the current transfer batch.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("transfer observer failed: {reason}")]
pub struct TransferObserverError {
    reason: String,
}

impl TransferObserverError {
    /// Creates a transfer observer failure from a non-secret reason.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Receives artifact progress without blocking or owning transfer behavior.
pub trait TransferObserver: Send + Sync {
    /// Handles one progress snapshot. Implementations should return promptly.
    ///
    /// # Errors
    ///
    /// Returns [`TransferObserverError`] to cancel the complete current batch.
    fn observe(&self, event: TransferEvent) -> Result<(), TransferObserverError>;
}

impl<F> TransferObserver for F
where
    F: Fn(TransferEvent) + Send + Sync,
{
    fn observe(&self, event: TransferEvent) -> Result<(), TransferObserverError> {
        self(event);
        Ok(())
    }
}

/// Errors from bounded metadata and API requests.
#[derive(Debug, Error)]
pub enum NetworkError {
    /// Configuration violates an invariant required by the engine.
    #[error("invalid network configuration: {reason}")]
    InvalidConfiguration { reason: String },
    /// The operating system could not report available logical parallelism.
    #[error("failed to detect available logical parallelism: {reason}")]
    ParallelismDetection { reason: String },
    /// URL parsing or scheme validation failed.
    #[error("invalid HTTP URL '{url}': {reason}")]
    InvalidUrl { url: String, reason: String },
    /// Shared HTTP client construction failed.
    #[error("failed to build shared HTTP client: {reason}")]
    ClientBuild { reason: String },
    /// A connection, timeout, or request operation failed after bounded retries.
    #[error("HTTP request to '{url}' failed after {attempts} attempt(s): {reason}")]
    Request {
        url: String,
        attempts: usize,
        reason: String,
    },
    /// A non-success response was returned after applying retry policy.
    #[error("HTTP request to '{url}' returned status {status} after {attempts} attempt(s)")]
    HttpStatus {
        url: String,
        status: u16,
        attempts: usize,
    },
    /// Redirect resolution failed or exceeded the configured bound.
    #[error("redirect handling failed for '{url}': {reason}")]
    Redirect { url: String, reason: String },
    /// A bounded in-memory response exceeded its declared limit.
    #[error("response from '{url}' exceeded the {limit}-byte limit")]
    ResponseTooLarge { url: String, limit: usize },
    /// Reading a response body failed.
    #[error("failed reading response from '{url}': {reason}")]
    ResponseRead { url: String, reason: String },
    /// Request work was cancelled before acquiring or while waiting for its budget.
    #[error("HTTP request to '{url}' was cancelled")]
    Cancelled { url: String },
    /// JSON decoding failed without exposing response credentials.
    #[error("failed to parse JSON response from '{url}': {source}")]
    Json {
        url: String,
        #[source]
        source: serde_json::Error,
    },
    /// A fixed network worker unexpectedly terminated.
    #[error("network worker failed: {reason}")]
    Worker { reason: String },
}

/// Errors from streaming, segmenting, verifying, or publishing an artifact.
#[derive(Debug, Error)]
pub enum TransferError {
    /// A batch contains duplicate task IDs, duplicate targets, or another unsafe request shape.
    #[error("invalid artifact task '{task_id}': {reason}")]
    InvalidRequest { task_id: String, reason: String },
    /// All candidate URLs failed; each sanitized cause is retained.
    #[error("artifact task '{task_id}' exhausted all candidate URLs: {failures:?}")]
    CandidatesExhausted {
        task_id: String,
        failures: Vec<String>,
    },
    /// HTTP transport failed for a specific artifact operation.
    #[error("artifact task '{task_id}' network failure: {source}")]
    Network {
        task_id: String,
        #[source]
        source: NetworkError,
    },
    /// Filesystem streaming or publication failed.
    #[error("artifact task '{task_id}' failed to {operation} '{path}': {source}")]
    Io {
        task_id: String,
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Range response semantics did not match the requested interval.
    #[error("artifact task '{task_id}' received an invalid Range response from '{url}': {reason}")]
    RangeProtocol {
        task_id: String,
        url: String,
        reason: String,
    },
    /// A Range request was ignored with a full `200` response.
    #[error("artifact task '{task_id}' server ignored a Range request")]
    RangeIgnored { task_id: String },
    /// A strong `ETag` changed while segments were being fetched.
    #[error("artifact task '{task_id}' changed identity during segmented transfer")]
    ResourceChanged { task_id: String },
    /// Final byte count differs from trusted metadata.
    #[error("artifact task '{task_id}' has size {actual}, expected {expected}")]
    SizeMismatch {
        task_id: String,
        expected: u64,
        actual: u64,
    },
    /// Final SHA-1 differs from trusted metadata.
    #[error("artifact task '{task_id}' has SHA-1 {actual}, expected {expected}")]
    Sha1Mismatch {
        task_id: String,
        expected: String,
        actual: String,
    },
    /// Final SHA-256 differs from trusted metadata.
    #[error("artifact task '{task_id}' has SHA-256 {actual}, expected {expected}")]
    Sha256Mismatch {
        task_id: String,
        expected: String,
        actual: String,
    },
    /// Work stopped after another task failed or the same task became invalid.
    #[error("artifact task '{task_id}' was cancelled")]
    Cancelled { task_id: String },
    /// Progress observation failed and cancelled the complete batch.
    #[error("artifact task '{task_id}' was cancelled by its observer: {source}")]
    Observer {
        task_id: String,
        #[source]
        source: TransferObserverError,
    },
    /// A fixed worker pool could not complete submitted work.
    #[error("artifact task '{task_id}' worker failure: {reason}")]
    Worker { task_id: String, reason: String },
}

pub(crate) fn reject_sensitive_regular_header(name: &HeaderName) -> Result<(), NetworkError> {
    if is_sensitive_name(name) {
        return Err(NetworkError::InvalidConfiguration {
            reason: format!(
                "credential header '{}' must use SensitiveHeaders with an allowed-origin list",
                name.as_str()
            ),
        });
    }
    Ok(())
}

pub(crate) fn is_sensitive_name(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "x-api-key"
            | "api-key"
            | "x-auth-token"
    )
}

fn parse_http_url(value: &str) -> Result<Url, NetworkError> {
    let url = Url::parse(value).map_err(|source| NetworkError::InvalidUrl {
        url: "<invalid-url>".to_string(),
        reason: source.to_string(),
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(NetworkError::InvalidUrl {
            url: redact_url(&url),
            reason: "only absolute http:// and https:// URLs are accepted".to_string(),
        });
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(NetworkError::InvalidUrl {
            url: redact_url(&url),
            reason: "URL user information is not accepted; use SensitiveHeaders".to_string(),
        });
    }
    Ok(url)
}

pub(crate) fn redact_url(url: &Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    redacted.set_query(None);
    redacted.set_fragment(None);
    redacted.to_string()
}

fn normalize_digest(
    digest: Option<String>,
    expected_len: usize,
    label: &str,
) -> Result<Option<String>, NetworkError> {
    let Some(digest) = digest else {
        return Ok(None);
    };
    let digest = digest.trim().to_ascii_lowercase();
    if digest.len() != expected_len || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(NetworkError::InvalidConfiguration {
            reason: format!(
                "expected {label} must contain exactly {expected_len} hexadecimal digits"
            ),
        });
    }
    Ok(Some(digest))
}

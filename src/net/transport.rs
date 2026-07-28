use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, LOCATION, RETRY_AFTER};
use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;

use super::model::{
    HttpRequest, HttpResponse, MAX_QUEUED_JOBS, MAX_TRANSFER_WORKERS, NetworkConfig, NetworkError,
    NetworkLimits, SensitiveHeaders, redact_url,
};
use super::pool::{
    BudgetAcquireError, CancellationToken, ObserverDispatcher, PoolError, RequestBudget,
    RequestPermit, TargetRegistry, WorkerPool, global_target_registry,
};

/// Bounded in-memory HTTP access sharing the engine connection pool and retry policy.
pub trait HttpTransport: Send + Sync {
    /// Performs one bounded GET. API and metadata responses never use Range segmentation.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] for request, response, redirect, or worker failures.
    fn get_bytes(&self, request: HttpRequest) -> Result<HttpResponse, NetworkError>;

    /// Performs one bounded GET and decodes its JSON body.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] for transport failures or invalid JSON.
    fn get_json<T: DeserializeOwned>(&self, request: HttpRequest) -> Result<T, NetworkError>
    where
        Self: Sized,
    {
        self.get_bytes(request)?.json()
    }
}

pub(crate) struct SharedTransport {
    client: Client,
    config: NetworkConfig,
    pub(crate) http_pool: WorkerPool,
    pub(crate) transfer_pool: WorkerPool,
    pub(crate) observers: Arc<ObserverDispatcher>,
    pub(crate) targets: Arc<TargetRegistry>,
    pub(crate) budget: Arc<RequestBudget>,
}

impl SharedTransport {
    pub(crate) fn new(
        config: NetworkConfig,
        limits: NetworkLimits,
    ) -> Result<Arc<Self>, NetworkError> {
        config.validate()?;
        limits.validate()?;
        let mut builder = Client::builder()
            .user_agent(&config.user_agent)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.connect_timeout)
            .timeout(config.read_timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(config.idle_connections_per_host)
            .http2_adaptive_window(true);
        if let Some(proxy) = config.proxy.as_deref() {
            let proxy = reqwest::Proxy::all(proxy).map_err(|source| NetworkError::ClientBuild {
                reason: format!("invalid proxy URL: {source}"),
            })?;
            builder = builder.proxy(proxy);
        }
        let client = builder
            .build()
            .map_err(|source| NetworkError::ClientBuild {
                reason: source.to_string(),
            })?;
        let queue_capacity = limits
            .global_requests
            .checked_mul(8)
            .ok_or_else(|| NetworkError::InvalidConfiguration {
                reason: "network worker queue capacity overflowed".to_string(),
            })?
            .min(MAX_QUEUED_JOBS);
        let http_pool = WorkerPool::new("http", limits.global_requests, queue_capacity)
            .map_err(pool_network_error)?;
        let transfer_pool = WorkerPool::new(
            "transfer",
            limits.global_requests.min(MAX_TRANSFER_WORKERS),
            queue_capacity,
        )
        .map_err(pool_network_error)?;
        let observers = ObserverDispatcher::new().map_err(pool_network_error)?;
        Ok(Arc::new(Self {
            client,
            config,
            http_pool,
            transfer_pool,
            observers,
            targets: global_target_registry(),
            budget: Arc::new(RequestBudget::new(limits)),
        }))
    }

    pub(crate) fn get_bytes(
        self: &Arc<Self>,
        request: HttpRequest,
    ) -> Result<HttpResponse, NetworkError> {
        let transport = Arc::clone(self);
        let handle = self
            .http_pool
            .submit(move || transport.get_bytes_direct(&request))
            .map_err(pool_network_error)?;
        handle.wait().map_err(pool_network_error)?
    }

    fn get_bytes_direct(&self, request: &HttpRequest) -> Result<HttpResponse, NetworkError> {
        for body_attempt in 1..=self.config.max_attempts {
            let mut no_retry_notice = |_: usize, _: Duration| {};
            let open = self.send_with_redirects(
                Method::GET,
                request.url.clone(),
                request.headers.clone(),
                request.sensitive_headers.clone(),
                None,
                None,
                &mut no_retry_notice,
            )?;
            let status = open.response.status();
            if !status.is_success() {
                return Err(NetworkError::HttpStatus {
                    url: redact_url(&open.final_url),
                    status: status.as_u16(),
                    attempts: open.attempts,
                });
            }
            match read_bounded_response(open, request.max_response_bytes) {
                Ok(response) => return Ok(response),
                Err(error @ NetworkError::ResponseRead { .. })
                    if body_attempt < self.config.max_attempts =>
                {
                    let delay = self.body_retry_delay(body_attempt);
                    thread::sleep(delay);
                    drop(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(NetworkError::Request {
            url: redact_url(&request.url),
            attempts: self.config.max_attempts,
            reason: "bounded response retry loop ended unexpectedly".to_string(),
        })
    }

    pub(crate) fn max_attempts(&self) -> usize {
        self.config.max_attempts
    }

    pub(crate) fn body_retry_delay(&self, attempt: usize) -> Duration {
        exponential_delay(
            attempt,
            self.config.retry_base_delay,
            self.config.retry_max_delay,
        )
    }

    pub(crate) fn flush_observers(&self) {
        self.observers.flush();
    }

    pub(crate) fn dispatch_event(
        &self,
        observer: Arc<dyn super::model::TransferObserver>,
        event: super::model::TransferEvent,
        cancelled: Arc<CancellationToken>,
        failure: Arc<std::sync::Mutex<Option<super::model::TransferObserverError>>>,
    ) {
        self.observers.emit(
            observer,
            event,
            cancelled,
            failure,
            Arc::clone(&self.budget),
        );
    }
}

fn read_bounded_response(
    mut open: OpenResponse,
    max_response_bytes: usize,
) -> Result<HttpResponse, NetworkError> {
    let final_url = open.final_url.clone();
    let status = open.response.status();
    let headers = open.response.headers().clone();
    if headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > max_response_bytes as u64)
    {
        return Err(NetworkError::ResponseTooLarge {
            url: redact_url(&final_url),
            limit: max_response_bytes,
        });
    }
    let capacity = open
        .response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .map_or(max_response_bytes, |length| length.min(max_response_bytes));
    let mut body = Vec::with_capacity(capacity);
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read =
            open.response
                .read(&mut buffer)
                .map_err(|source| NetworkError::ResponseRead {
                    url: redact_url(&final_url),
                    reason: source.to_string(),
                })?;
        if read == 0 {
            break;
        }
        if body.len().saturating_add(read) > max_response_bytes {
            return Err(NetworkError::ResponseTooLarge {
                url: redact_url(&final_url),
                limit: max_response_bytes,
            });
        }
        body.extend_from_slice(&buffer[..read]);
    }
    Ok(HttpResponse {
        final_url,
        status,
        headers,
        body,
    })
}

impl SharedTransport {
    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    pub(crate) fn send_with_redirects(
        &self,
        method: Method,
        initial_url: Url,
        headers: HeaderMap,
        sensitive_headers: SensitiveHeaders,
        task_id: Option<&str>,
        cancelled: Option<&CancellationToken>,
        on_retry: &mut dyn FnMut(usize, Duration),
    ) -> Result<OpenResponse, NetworkError> {
        let mut current = initial_url;
        let mut visited = HashSet::new();
        for redirect_count in 0..=self.config.max_redirects {
            if !visited.insert(current.as_str().to_string()) {
                return Err(NetworkError::Redirect {
                    url: redact_url(&current),
                    reason: "redirect loop detected".to_string(),
                });
            }
            let open = self.send_one_hop(
                method.clone(),
                current.clone(),
                &headers,
                &sensitive_headers,
                task_id,
                cancelled,
                on_retry,
            )?;
            if !is_redirect(open.response.status()) {
                return Ok(open);
            }
            if redirect_count == self.config.max_redirects {
                return Err(NetworkError::Redirect {
                    url: redact_url(&current),
                    reason: format!("exceeded {} redirects", self.config.max_redirects),
                });
            }
            let location = open
                .response
                .headers()
                .get(LOCATION)
                .ok_or_else(|| NetworkError::Redirect {
                    url: redact_url(&current),
                    reason: "redirect response omitted Location".to_string(),
                })?
                .to_str()
                .map_err(|source| NetworkError::Redirect {
                    url: redact_url(&current),
                    reason: format!("Location is not valid text: {source}"),
                })?;
            let next = current
                .join(location)
                .map_err(|source| NetworkError::Redirect {
                    url: redact_url(&current),
                    reason: format!("invalid Location header: {source}"),
                })?;
            if !matches!(next.scheme(), "http" | "https") {
                return Err(NetworkError::Redirect {
                    url: redact_url(&current),
                    reason: format!("unsupported redirect scheme '{}'", next.scheme()),
                });
            }
            if current.scheme() == "https" && next.scheme() == "http" {
                return Err(NetworkError::Redirect {
                    url: redact_url(&current),
                    reason: "HTTPS to HTTP downgrade is not allowed".to_string(),
                });
            }
            drop(open);
            current = next;
        }
        Err(NetworkError::Redirect {
            url: redact_url(&current),
            reason: "redirect state became inconsistent".to_string(),
        })
    }

    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    fn send_one_hop(
        &self,
        method: Method,
        url: Url,
        headers: &HeaderMap,
        sensitive_headers: &SensitiveHeaders,
        task_id: Option<&str>,
        cancelled: Option<&CancellationToken>,
        on_retry: &mut dyn FnMut(usize, Duration),
    ) -> Result<OpenResponse, NetworkError> {
        let host = url
            .host_str()
            .expect("validated HTTP URL always has a host")
            .to_ascii_lowercase();
        let mut last_reason = None;
        for attempt in 1..=self.config.max_attempts {
            let permit =
                self.budget
                    .acquire(&host, task_id, cancelled)
                    .map_err(|error| match error {
                        BudgetAcquireError::Cancelled => NetworkError::Cancelled {
                            url: redact_url(&url),
                        },
                        BudgetAcquireError::Unavailable => NetworkError::Worker {
                            reason: "request concurrency budget is unavailable".to_string(),
                        },
                    })?;
            let mut hop_headers = headers.clone();
            for (name, value) in sensitive_headers.for_url(&url) {
                if let Some(name) = name {
                    hop_headers.insert(name, value);
                }
            }
            match self
                .client
                .request(method.clone(), url.clone())
                .headers(hop_headers)
                .send()
            {
                Ok(response) => {
                    let status = response.status();
                    if is_redirect(status) || status.is_success() {
                        return Ok(OpenResponse {
                            response,
                            final_url: url,
                            attempts: attempt,
                            _permit: permit,
                        });
                    }
                    if retryable_status(status) && attempt < self.config.max_attempts {
                        let delay = retry_delay(
                            response.headers(),
                            attempt,
                            self.config.retry_base_delay,
                            self.config.retry_max_delay,
                        );
                        drop(response);
                        drop(permit);
                        on_retry(attempt + 1, delay);
                        wait_for_retry(cancelled, &url, delay)?;
                        continue;
                    }
                    return Ok(OpenResponse {
                        response,
                        final_url: url,
                        attempts: attempt,
                        _permit: permit,
                    });
                }
                Err(source) => {
                    let retryable =
                        source.is_connect() || source.is_timeout() || source.is_request();
                    last_reason = Some(request_error_reason(&source));
                    drop(permit);
                    if retryable && attempt < self.config.max_attempts {
                        let delay = exponential_delay(
                            attempt,
                            self.config.retry_base_delay,
                            self.config.retry_max_delay,
                        );
                        on_retry(attempt + 1, delay);
                        wait_for_retry(cancelled, &url, delay)?;
                        continue;
                    }
                    return Err(NetworkError::Request {
                        url: redact_url(&url),
                        attempts: attempt,
                        reason: last_reason.unwrap_or_else(|| "unknown request error".to_string()),
                    });
                }
            }
        }
        Err(NetworkError::Request {
            url: redact_url(&url),
            attempts: self.config.max_attempts,
            reason: last_reason.unwrap_or_else(|| "retry loop ended unexpectedly".to_string()),
        })
    }
}

fn wait_for_retry(
    cancelled: Option<&CancellationToken>,
    url: &Url,
    delay: Duration,
) -> Result<(), NetworkError> {
    if let Some(cancelled) = cancelled {
        if cancelled.wait(delay) {
            return Err(NetworkError::Cancelled {
                url: redact_url(url),
            });
        }
    } else {
        thread::sleep(delay);
    }
    Ok(())
}

pub(crate) struct OpenResponse {
    pub(crate) response: Response,
    pub(crate) final_url: Url,
    pub(crate) attempts: usize,
    _permit: RequestPermit,
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn retry_delay(headers: &HeaderMap, attempt: usize, base: Duration, maximum: Duration) -> Duration {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after)
        .map_or_else(
            || exponential_delay(attempt, base, maximum),
            |delay| delay.min(maximum),
        )
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline = httpdate::parse_http_date(value).ok()?;
    Some(
        deadline
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

fn exponential_delay(attempt: usize, base: Duration, maximum: Duration) -> Duration {
    let shift =
        u32::try_from(attempt.saturating_sub(1).min(20)).expect("the retry shift is bounded to 20");
    base.saturating_mul(1_u32 << shift).min(maximum)
}

fn request_error_reason(source: &reqwest::Error) -> String {
    let category = if source.is_timeout() {
        "timeout"
    } else if source.is_connect() {
        "connection"
    } else if source.is_request() {
        "request"
    } else if source.is_body() {
        "response body"
    } else {
        "transport"
    };
    format!("{category} error")
}

#[allow(clippy::needless_pass_by_value)]
fn pool_network_error(error: PoolError) -> NetworkError {
    NetworkError::Worker {
        reason: error.to_string(),
    }
}

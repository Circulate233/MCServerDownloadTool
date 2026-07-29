use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG, HeaderMap, HeaderValue,
    IF_RANGE, RANGE,
};
use reqwest::{Method, StatusCode, Url};
use sha1::{Digest as Sha1Digest, Sha1};
use sha2::Sha256;

use super::model::{
    ArtifactOutcome, ArtifactRequest, DownloadMode, HttpRequest, HttpResponse, NetworkConfig,
    NetworkError, NetworkLimits, TransferError, TransferEvent, TransferObserver,
    TransferObserverError, TransferPhase, redact_url,
};
use super::pool::{CancellationToken, JobHandle, PoolError, TargetLease};
use super::transport::{HttpTransport, OpenResponse, SharedTransport};

const COPY_BUFFER_SIZE: usize = 256 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

type IndexedTransferResult = (usize, Result<ArtifactOutcome, TransferError>);

struct SubmittedTransferJob {
    submitted_index: usize,
    handle: JobHandle<IndexedTransferResult>,
}

/// The sole filesystem download interface for one shared network engine.
pub trait ArtifactTransfer: Send + Sync {
    /// Transfers a batch under one global request budget.
    ///
    /// Results preserve request order. After the first concrete task failure, no
    /// new task is started; in-flight work is collected and remaining entries are
    /// returned as [`TransferError::Cancelled`].
    fn transfer_many(
        &self,
        requests: Vec<ArtifactRequest>,
        observer: Arc<dyn TransferObserver>,
    ) -> Vec<Result<ArtifactOutcome, TransferError>>;

    /// Transfers one file through the same fixed worker pool and limits as a batch.
    ///
    /// # Errors
    ///
    /// Returns [`TransferError`] when request validation, transfer, verification,
    /// or publication fails.
    fn transfer_one(
        &self,
        request: ArtifactRequest,
        observer: Arc<dyn TransferObserver>,
    ) -> Result<ArtifactOutcome, TransferError> {
        self.transfer_many(vec![request], observer)
            .into_iter()
            .next()
            .expect("a one-element transfer batch always returns one result")
    }
}

/// Shared blocking network context implementing bounded API access and artifact transfer.
#[derive(Clone)]
pub struct NetworkEngine {
    transport: Arc<SharedTransport>,
    limits: NetworkLimits,
}

impl NetworkEngine {
    /// Creates an engine with request limits derived from available logical CPUs.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] when configuration, parallelism detection, or
    /// worker initialization fails.
    pub fn new(config: NetworkConfig) -> Result<Self, NetworkError> {
        Self::with_limits(config, NetworkLimits::automatic()?)
    }

    /// Creates an engine with explicitly validated concurrency and segmentation limits.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError`] when configuration or worker initialization fails.
    pub fn with_limits(config: NetworkConfig, limits: NetworkLimits) -> Result<Self, NetworkError> {
        let transport = SharedTransport::new(config, limits)?;
        Ok(Self { transport, limits })
    }

    /// Returns the number of requests currently holding the shared global budget.
    #[must_use]
    pub fn active_requests(&self) -> usize {
        self.transport.budget.active()
    }
}

impl HttpTransport for NetworkEngine {
    fn get_bytes(&self, request: HttpRequest) -> Result<HttpResponse, NetworkError> {
        self.transport.get_bytes(request)
    }
}

impl ArtifactTransfer for NetworkEngine {
    fn transfer_many(
        &self,
        requests: Vec<ArtifactRequest>,
        observer: Arc<dyn TransferObserver>,
    ) -> Vec<Result<ArtifactOutcome, TransferError>> {
        if requests.is_empty() {
            return Vec::new();
        }
        let mut requests = requests;
        let _target_lease = match validate_and_reserve_batch(&mut requests, &self.transport) {
            Ok(lease) => lease,
            Err(results) => return results,
        };
        let requests = Arc::new(requests);
        let cancelled = CancellationToken::new();
        let observer_failure = Arc::new(Mutex::new(None));
        let progress = Arc::new(
            requests
                .iter()
                .map(|request| {
                    Arc::new(TaskProgress::new(
                        request.task_id.clone(),
                        Arc::clone(&observer),
                        Arc::clone(&self.transport),
                        Arc::clone(&cancelled),
                        Arc::clone(&observer_failure),
                        request.expected_size,
                    ))
                })
                .collect::<Vec<_>>(),
        );
        for task in progress.iter() {
            task.emit(TransferPhase::Queued, 0, task.total(), true);
        }
        let mut handles = Vec::with_capacity(requests.len());
        let mut results = (0..requests.len()).map(|_| None).collect::<Vec<_>>();
        for index in 0..requests.len() {
            if cancelled.is_cancelled() {
                break;
            }
            let job_requests = Arc::clone(&requests);
            let task_progress = Arc::clone(&progress[index]);
            let engine = self.clone();
            let job_cancelled = Arc::clone(&cancelled);
            match self.transport.transfer_pool.submit(move || {
                let request = &job_requests[index];
                let result = if job_cancelled.is_cancelled() {
                    Err(task_progress.cancellation_error())
                } else {
                    engine.transfer_request(request, &task_progress)
                };
                finish_task(&task_progress, &job_cancelled, &result);
                (index, result)
            }) {
                Ok(handle) => handles.push(SubmittedTransferJob {
                    submitted_index: index,
                    handle,
                }),
                Err(source) => {
                    cancelled.cancel();
                    let error = pool_transfer_error(&requests[index].task_id, source);
                    progress[index].emit(
                        TransferPhase::Failed,
                        progress[index].transferred(),
                        progress[index].total(),
                        true,
                    );
                    results[index] = Some(Err(error));
                    break;
                }
            }
        }
        wait_for_transfer_jobs(handles, &mut results, &requests, &progress, &cancelled);
        finalize_batch_results(
            &self.transport,
            &requests,
            &progress,
            &observer_failure,
            results,
        )
    }
}

fn wait_for_transfer_jobs(
    handles: Vec<SubmittedTransferJob>,
    results: &mut [Option<Result<ArtifactOutcome, TransferError>>],
    requests: &[ArtifactRequest],
    progress: &[Arc<TaskProgress>],
    cancelled: &CancellationToken,
) {
    for SubmittedTransferJob {
        submitted_index,
        handle,
    } in handles
    {
        match handle.wait() {
            Ok((index, result)) => results[index] = Some(result),
            Err(source) => {
                cancelled.cancel();
                progress[submitted_index].emit(
                    TransferPhase::Failed,
                    progress[submitted_index].transferred(),
                    progress[submitted_index].total(),
                    true,
                );
                results[submitted_index] = Some(Err(pool_transfer_error(
                    &requests[submitted_index].task_id,
                    source,
                )));
            }
        }
    }
}

fn finalize_batch_results(
    transport: &SharedTransport,
    requests: &[ArtifactRequest],
    progress: &[Arc<TaskProgress>],
    observer_failure: &Mutex<Option<TransferObserverError>>,
    mut results: Vec<Option<Result<ArtifactOutcome, TransferError>>>,
) -> Vec<Result<ArtifactOutcome, TransferError>> {
    for (index, result) in results.iter_mut().enumerate() {
        if result.is_none() {
            progress[index].emit(
                TransferPhase::Cancelled,
                progress[index].transferred(),
                progress[index].total(),
                true,
            );
            *result = Some(Err(TransferError::Cancelled {
                task_id: requests[index].task_id.clone(),
            }));
        }
    }
    transport.flush_observers();
    if let Some(error) = observer_failure
        .lock()
        .unwrap_or_else(|poisoned| {
            eprintln!("network observer failure lock was poisoned while collecting results");
            poisoned.into_inner()
        })
        .clone()
        && !results
            .iter()
            .any(|result| matches!(result, Some(Err(TransferError::Observer { .. }))))
        && let Some((index, result)) = results
            .iter_mut()
            .enumerate()
            .find(|(_, result)| matches!(result, Some(Err(TransferError::Cancelled { .. }))))
    {
        *result = Some(Err(TransferError::Observer {
            task_id: requests[index].task_id.clone(),
            source: error,
        }));
    }
    results
        .into_iter()
        .map(|result| result.expect("every transfer result is populated"))
        .collect()
}

fn finish_task(
    progress: &TaskProgress,
    cancelled: &CancellationToken,
    result: &Result<ArtifactOutcome, TransferError>,
) {
    match result {
        Ok(outcome) => progress.emit(
            TransferPhase::Completed,
            outcome.bytes,
            Some(outcome.bytes),
            true,
        ),
        Err(TransferError::Cancelled { .. }) => progress.emit(
            TransferPhase::Cancelled,
            progress.transferred(),
            progress.total(),
            true,
        ),
        Err(_) => {
            cancelled.cancel();
            progress.emit(
                TransferPhase::Failed,
                progress.transferred(),
                progress.total(),
                true,
            );
        }
    }
}

impl NetworkEngine {
    fn transfer_request(
        &self,
        request: &ArtifactRequest,
        progress: &Arc<TaskProgress>,
    ) -> Result<ArtifactOutcome, TransferError> {
        progress.ensure_active()?;
        ensure_parent(request)?;
        let mut failures = Vec::new();
        for url in &request.urls {
            progress.ensure_active()?;
            let staging = unique_temp_path(&request.target, "part");
            match self.transfer_candidate(request, url, &staging, progress) {
                Ok(mode) => {
                    progress.emit(
                        TransferPhase::Verifying,
                        progress.transferred(),
                        progress.total(),
                        true,
                    );
                    let bytes = match verify_file(request, &staging) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            cleanup_file(&staging, "discard failed verification staging file");
                            if request.urls.len() == 1 {
                                return Err(error);
                            }
                            failures.push(format!("{}: {error}", redact_url(url)));
                            progress.reset_candidate();
                            continue;
                        }
                    };
                    progress.ensure_active()?;
                    if let Err(error) = atomic_publish(request, &staging) {
                        cleanup_file(&staging, "discard failed publication staging file");
                        return Err(error);
                    }
                    return Ok(ArtifactOutcome {
                        task_id: request.task_id.clone(),
                        source_url: url.clone(),
                        target: request.target.clone(),
                        bytes,
                        mode,
                    });
                }
                Err(error) => {
                    cleanup_file(&staging, "discard failed candidate staging file");
                    if matches!(error, TransferError::Cancelled { .. }) {
                        return Err(error);
                    }
                    if request.urls.len() == 1 {
                        return Err(error);
                    }
                    failures.push(format!("{}: {error}", redact_url(url)));
                    progress.reset_candidate();
                }
            }
        }
        Err(TransferError::CandidatesExhausted {
            task_id: request.task_id.clone(),
            failures,
        })
    }

    fn transfer_candidate(
        &self,
        request: &ArtifactRequest,
        url: &Url,
        staging: &Path,
        progress: &Arc<TaskProgress>,
    ) -> Result<DownloadMode, TransferError> {
        progress.ensure_active()?;
        if request
            .expected_size
            .is_some_and(|size| size < self.limits.segment_threshold)
        {
            self.download_single(request, url, staging, progress)?;
            return Ok(DownloadMode::Single);
        }
        progress.emit(
            TransferPhase::Probing,
            progress.transferred(),
            request.expected_size,
            true,
        );
        match self.probe_range(request, url, staging, progress)? {
            ProbeOutcome::SingleWritten => Ok(DownloadMode::Single),
            ProbeOutcome::SingleRequired => {
                self.download_single(request, url, staging, progress)?;
                Ok(DownloadMode::Single)
            }
            ProbeOutcome::Range(range) => {
                if request
                    .maximum_size
                    .is_some_and(|maximum| range.total > maximum)
                {
                    return Err(TransferError::SizeMismatch {
                        task_id: request.task_id.clone(),
                        expected: request.maximum_size.unwrap_or_default(),
                        actual: range.total,
                    });
                }
                progress.set_total(Some(range.total));
                if range.total < self.limits.segment_threshold
                    || (range.validator.is_none() && request.expected_sha256.is_none())
                {
                    self.download_single(request, url, staging, progress)?;
                    return Ok(DownloadMode::Single);
                }
                match self.download_segmented(request, url, staging, &range, progress) {
                    Ok(()) => Ok(DownloadMode::Segmented),
                    Err(TransferError::RangeIgnored { .. }) => {
                        progress.reset_candidate();
                        self.download_single(request, url, staging, progress)?;
                        Ok(DownloadMode::Single)
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }

    fn probe_range(
        &self,
        request: &ArtifactRequest,
        url: &Url,
        staging: &Path,
        progress: &Arc<TaskProgress>,
    ) -> Result<ProbeOutcome, TransferError> {
        progress.ensure_active()?;
        let transport = Arc::clone(&self.transport);
        let request = request.clone();
        let url = url.clone();
        let staging = staging.to_path_buf();
        let progress = Arc::clone(progress);
        let task_id = request.task_id.clone();
        let handle = self
            .transport
            .http_pool
            .submit(move || {
                let mut headers = request.headers.clone();
                headers.insert(RANGE, HeaderValue::from_static("bytes=0-0"));
                headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
                for body_attempt in 1..=transport.max_attempts() {
                    let mut retry = |_: usize, _: Duration| emit_retry(&progress);
                    let open = transport
                        .send_with_redirects(
                            Method::GET,
                            url.clone(),
                            headers.clone(),
                            request.sensitive_headers.clone(),
                            Some(&request.task_id),
                            Some(&progress.cancelled),
                            &mut retry,
                        )
                        .map_err(|source| TransferError::Network {
                            task_id: request.task_id.clone(),
                            source,
                        })?;
                    validate_identity_encoding(
                        &request.task_id,
                        &open.final_url,
                        open.response.headers(),
                    )?;
                    let result = match open.response.status() {
                        StatusCode::OK => {
                            progress.emit(
                                TransferPhase::Single,
                                progress.transferred(),
                                response_total(&open),
                                true,
                            );
                            stream_full_response(&request, open, &staging, &progress)
                                .map(|()| ProbeOutcome::SingleWritten)
                        }
                        StatusCode::PARTIAL_CONTENT => parse_probe_response(&request, &url, open),
                        StatusCode::RANGE_NOT_SATISFIABLE => {
                            parse_empty_probe_response(&request, &url, &open)
                        }
                        status => Err(http_status_transfer_error(
                            &request,
                            &open.final_url,
                            status,
                            open.attempts,
                        )),
                    };
                    match result {
                        Ok(outcome) => return Ok(outcome),
                        Err(error)
                            if retryable_body_error(&error)
                                && body_attempt < transport.max_attempts() =>
                        {
                            cleanup_file(&staging, "retry Range probe body");
                            wait_for_body_retry(&transport, &progress, body_attempt)?;
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(unexpected_retry_end(
                    &request,
                    &url,
                    transport.max_attempts(),
                ))
            })
            .map_err(|source| pool_transfer_error(&task_id, source))?;
        handle
            .wait()
            .map_err(|source| pool_transfer_error(&task_id, source))?
    }

    fn download_single(
        &self,
        request: &ArtifactRequest,
        url: &Url,
        staging: &Path,
        progress: &Arc<TaskProgress>,
    ) -> Result<(), TransferError> {
        progress.ensure_active()?;
        cleanup_file(staging, "prepare full download staging file");
        progress.emit(
            TransferPhase::Single,
            progress.transferred(),
            request.expected_size,
            true,
        );
        let transport = Arc::clone(&self.transport);
        let request = request.clone();
        let url = url.clone();
        let staging = staging.to_path_buf();
        let progress = Arc::clone(progress);
        let task_id = request.task_id.clone();
        let handle = self
            .transport
            .http_pool
            .submit(move || {
                let mut headers = request.headers.clone();
                headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
                for body_attempt in 1..=transport.max_attempts() {
                    let mut retry = |_: usize, _: Duration| emit_retry(&progress);
                    let open = transport
                        .send_with_redirects(
                            Method::GET,
                            url.clone(),
                            headers.clone(),
                            request.sensitive_headers.clone(),
                            Some(&request.task_id),
                            Some(&progress.cancelled),
                            &mut retry,
                        )
                        .map_err(|source| TransferError::Network {
                            task_id: request.task_id.clone(),
                            source,
                        })?;
                    if open.response.status() != StatusCode::OK {
                        return Err(http_status_transfer_error(
                            &request,
                            &open.final_url,
                            open.response.status(),
                            open.attempts,
                        ));
                    }
                    validate_identity_encoding(
                        &request.task_id,
                        &open.final_url,
                        open.response.headers(),
                    )?;
                    match stream_full_response(&request, open, &staging, &progress) {
                        Ok(()) => return Ok(()),
                        Err(error)
                            if retryable_body_error(&error)
                                && body_attempt < transport.max_attempts() =>
                        {
                            cleanup_file(&staging, "retry full response body");
                            wait_for_body_retry(&transport, &progress, body_attempt)?;
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(unexpected_retry_end(
                    &request,
                    &url,
                    transport.max_attempts(),
                ))
            })
            .map_err(|source| pool_transfer_error(&task_id, source))?;
        handle
            .wait()
            .map_err(|source| pool_transfer_error(&task_id, source))?
    }

    fn download_segmented(
        &self,
        request: &ArtifactRequest,
        url: &Url,
        staging: &Path,
        range: &RangeInfo,
        progress: &Arc<TaskProgress>,
    ) -> Result<(), TransferError> {
        cleanup_file(staging, "prepare segmented download staging file");
        progress.emit(
            TransferPhase::Segmented,
            progress.transferred(),
            Some(range.total),
            true,
        );
        let total = range.total;
        let ranges = segment_ranges(total, self.limits.range_segment_count(total));
        let local_cancel = CancellationToken::child(&progress.cancelled);
        let mut jobs: Vec<(PathBuf, JobHandle<Result<(), TransferError>>)> =
            Vec::with_capacity(ranges.len());
        for (index, (start, end)) in ranges.iter().copied().enumerate() {
            let part_path = segment_path(staging, index);
            let transport = Arc::clone(&self.transport);
            let request = request.clone();
            let url = url.clone();
            let progress = Arc::clone(progress);
            let validator = range.validator.clone();
            let cancel = Arc::clone(&local_cancel);
            let task_id = request.task_id.clone();
            let worker_path = part_path.clone();
            let handle = match self.transport.http_pool.submit(move || {
                download_segment(
                    &transport,
                    &request,
                    &url,
                    &worker_path,
                    start,
                    end,
                    total,
                    validator.as_ref(),
                    &progress,
                    &cancel,
                )
            }) {
                Ok(handle) => handle,
                Err(source) => {
                    local_cancel.cancel();
                    for (submitted_path, submitted) in jobs {
                        let _ = submitted.wait();
                        cleanup_file(&submitted_path, "cancel partially submitted Range transfer");
                    }
                    cleanup_file(&part_path, "cancel unsubmitted Range segment");
                    return Err(pool_transfer_error(&task_id, source));
                }
            };
            jobs.push((part_path, handle));
        }

        let segment_paths = jobs
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let mut errors = Vec::new();
        for (_, job) in jobs {
            match job.wait() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    local_cancel.cancel();
                    errors.push(error);
                }
                Err(source) => {
                    local_cancel.cancel();
                    errors.push(pool_transfer_error(&request.task_id, source));
                }
            }
        }
        if !errors.is_empty() {
            for path in &segment_paths {
                cleanup_file(path, "discard failed Range segment");
            }
            if errors
                .iter()
                .any(|error| matches!(error, TransferError::RangeIgnored { .. }))
                && errors.iter().all(|error| {
                    matches!(
                        error,
                        TransferError::RangeIgnored { .. } | TransferError::Cancelled { .. }
                    )
                })
            {
                return Err(TransferError::RangeIgnored {
                    task_id: request.task_id.clone(),
                });
            }
            return Err(errors
                .into_iter()
                .find(|error| !matches!(error, TransferError::Cancelled { .. }))
                .unwrap_or_else(|| TransferError::Cancelled {
                    task_id: request.task_id.clone(),
                }));
        }
        progress.ensure_active()?;
        let merge_result = merge_segments(request, &segment_paths, staging);
        for path in &segment_paths {
            cleanup_file(path, "discard merged Range segment");
        }
        merge_result
    }
}

fn validate_and_reserve_batch(
    requests: &mut [ArtifactRequest],
    transport: &Arc<SharedTransport>,
) -> Result<TargetLease, Vec<Result<ArtifactOutcome, TransferError>>> {
    let mut task_ids = HashSet::new();
    let mut targets = HashSet::new();
    let mut problem = None;
    let mut identities = Vec::with_capacity(requests.len());
    for request in requests.iter_mut() {
        if !task_ids.insert(request.task_id.clone()) {
            problem = Some((
                request.task_id.clone(),
                "task_id is duplicated within the batch".to_string(),
            ));
            break;
        }
        let absolute = match absolute_lexical_path(&request.target) {
            Ok(path) => path,
            Err(source) => {
                problem = Some((
                    request.task_id.clone(),
                    format!("failed to resolve target path: {source}"),
                ));
                break;
            }
        };
        let identity = match target_identity(&absolute) {
            Ok(identity) => identity,
            Err(source) => {
                problem = Some((
                    request.task_id.clone(),
                    format!("failed to establish target identity: {source}"),
                ));
                break;
            }
        };
        request.target = absolute;
        if !targets.insert(identity.clone()) {
            problem = Some((
                request.task_id.clone(),
                "target path is duplicated within the batch".to_string(),
            ));
            break;
        }
        identities.push(identity);
    }
    if let Some((task_id, reason)) = problem {
        return Err(invalid_batch_results(requests, &task_id, &reason));
    }
    transport.targets.acquire(identities).map_err(|reason| {
        let task_id = requests
            .first()
            .map_or("batch", |request| request.task_id.as_str());
        invalid_batch_results(requests, task_id, &reason)
    })
}

fn invalid_batch_results(
    requests: &[ArtifactRequest],
    invalid_task_id: &str,
    reason: &str,
) -> Vec<Result<ArtifactOutcome, TransferError>> {
    requests
        .iter()
        .map(|request| {
            Err(TransferError::InvalidRequest {
                task_id: request.task_id.clone(),
                reason: if request.task_id == invalid_task_id {
                    reason.to_string()
                } else {
                    format!("batch rejected because task '{invalid_task_id}' is invalid")
                },
            })
        })
        .collect()
}

fn absolute_lexical_path(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "target path escapes its filesystem root",
                    ));
                }
            }
            Component::Normal(component) => normalized.push(component),
        }
    }
    if normalized.file_name().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact target must name a file",
        ));
    }
    Ok(normalized)
}

fn target_identity(target: &Path) -> std::io::Result<PathBuf> {
    let file_name = target.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact target must name a file",
        )
    })?;
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let mut cursor = parent;
    let mut missing = Vec::new();
    let canonical_parent = loop {
        match fs::canonicalize(cursor) {
            Ok(canonical) => break canonical,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                let component = cursor.file_name().ok_or(source)?;
                missing.push(component.to_os_string());
                cursor = cursor.parent().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "no existing target ancestor could be canonicalized",
                    )
                })?;
            }
            Err(source) => return Err(source),
        }
    };
    let mut identity = canonical_parent;
    for component in missing.into_iter().rev() {
        identity.push(component);
    }
    identity.push(file_name);
    Ok(platform_identity(&identity))
}

#[cfg(windows)]
fn platform_identity(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

#[cfg(not(windows))]
fn platform_identity(path: &Path) -> PathBuf {
    path.to_path_buf()
}

enum ProbeOutcome {
    SingleWritten,
    SingleRequired,
    Range(RangeInfo),
}

#[derive(Clone)]
struct RangeInfo {
    total: u64,
    validator: Option<Validator>,
}

#[derive(Clone)]
enum Validator {
    StrongEtag(HeaderValue),
}

fn parse_probe_response(
    request: &ArtifactRequest,
    url: &Url,
    mut open: OpenResponse,
) -> Result<ProbeOutcome, TransferError> {
    let (start, end, total) =
        parse_content_range(request, url, open.response.headers().get(CONTENT_RANGE))?;
    if start != 0 || end != 0 {
        return Err(range_error(
            request,
            url,
            format!("probe Content-Range was {start}-{end}, expected 0-0"),
        ));
    }
    if let Some(expected) = request.expected_size
        && expected != total
    {
        return Err(TransferError::SizeMismatch {
            task_id: request.task_id.clone(),
            expected,
            actual: total,
        });
    }
    let mut byte = [0_u8; 2];
    let first = open
        .response
        .read(&mut byte)
        .map_err(|source| response_io_error(request, url, source))?;
    let second = open
        .response
        .read(&mut byte[first..])
        .map_err(|source| response_io_error(request, url, source))?;
    if first != 1 || second != 0 {
        return Err(range_error(
            request,
            url,
            format!(
                "probe body had {} byte(s), expected exactly 1",
                first + second
            ),
        ));
    }
    let validator = strong_etag(open.response.headers()).map(Validator::StrongEtag);
    Ok(ProbeOutcome::Range(RangeInfo { total, validator }))
}

fn parse_empty_probe_response(
    request: &ArtifactRequest,
    url: &Url,
    open: &OpenResponse,
) -> Result<ProbeOutcome, TransferError> {
    let value = open
        .response
        .headers()
        .get(CONTENT_RANGE)
        .ok_or_else(|| {
            range_error(
                request,
                url,
                "416 response omitted Content-Range".to_string(),
            )
        })?
        .to_str()
        .map_err(|source| {
            range_error(
                request,
                url,
                format!("416 Content-Range is not valid text: {source}"),
            )
        })?;
    if value.trim() != "bytes */0" {
        return Err(range_error(
            request,
            url,
            format!("unexpected unsatisfied Content-Range '{value}'"),
        ));
    }
    if let Some(expected) = request.expected_size
        && expected != 0
    {
        return Err(TransferError::SizeMismatch {
            task_id: request.task_id.clone(),
            expected,
            actual: 0,
        });
    }
    Ok(ProbeOutcome::SingleRequired)
}

fn emit_retry(progress: &TaskProgress) {
    progress.emit(
        TransferPhase::Retrying,
        progress.transferred(),
        progress.total(),
        true,
    );
}

fn wait_for_body_retry(
    transport: &SharedTransport,
    progress: &TaskProgress,
    attempt: usize,
) -> Result<(), TransferError> {
    emit_retry(progress);
    if progress.cancelled.wait(transport.body_retry_delay(attempt)) {
        return Err(progress.cancellation_error());
    }
    Ok(())
}

fn retryable_body_error(error: &TransferError) -> bool {
    match error {
        TransferError::Io { operation, .. } => *operation == "read HTTP response body",
        TransferError::SizeMismatch {
            expected, actual, ..
        } => actual < expected,
        TransferError::RangeProtocol { reason, .. } => {
            reason.starts_with("segment body had") || reason.starts_with("probe body had")
        }
        _ => false,
    }
}

fn http_status_transfer_error(
    request: &ArtifactRequest,
    url: &Url,
    status: StatusCode,
    attempts: usize,
) -> TransferError {
    TransferError::Network {
        task_id: request.task_id.clone(),
        source: NetworkError::HttpStatus {
            url: redact_url(url),
            status: status.as_u16(),
            attempts,
        },
    }
}

fn unexpected_retry_end(
    request: &ArtifactRequest,
    url: &Url,
    max_attempts: usize,
) -> TransferError {
    TransferError::Network {
        task_id: request.task_id.clone(),
        source: NetworkError::Request {
            url: redact_url(url),
            attempts: max_attempts,
            reason: "body retry loop ended unexpectedly".to_string(),
        },
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn download_segment(
    transport: &Arc<SharedTransport>,
    request: &ArtifactRequest,
    url: &Url,
    path: &Path,
    start: u64,
    end: u64,
    total: u64,
    validator: Option<&Validator>,
    progress: &Arc<TaskProgress>,
    cancelled: &Arc<CancellationToken>,
) -> Result<(), TransferError> {
    let mut headers = request.headers.clone();
    let range_value = HeaderValue::from_str(&format!("bytes={start}-{end}")).map_err(|source| {
        TransferError::InvalidRequest {
            task_id: request.task_id.clone(),
            reason: format!("failed to create Range header: {source}"),
        }
    })?;
    headers.insert(RANGE, range_value);
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    if let Some(validator) = validator {
        headers.insert(IF_RANGE, validator.value().clone());
    }
    for body_attempt in 1..=transport.max_attempts() {
        if cancelled.is_cancelled() {
            return Err(progress.cancellation_error());
        }
        cleanup_file(path, "prepare Range segment");
        let mut retry = |_: usize, _: Duration| emit_retry(progress);
        let mut open = transport
            .send_with_redirects(
                Method::GET,
                url.clone(),
                headers.clone(),
                request.sensitive_headers.clone(),
                Some(&request.task_id),
                Some(cancelled),
                &mut retry,
            )
            .map_err(|source| match source {
                NetworkError::Cancelled { .. } => TransferError::Cancelled {
                    task_id: request.task_id.clone(),
                },
                source => TransferError::Network {
                    task_id: request.task_id.clone(),
                    source,
                },
            })?;
        if open.response.status() == StatusCode::OK {
            cancelled.cancel();
            return Err(TransferError::RangeIgnored {
                task_id: request.task_id.clone(),
            });
        }
        if open.response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(http_status_transfer_error(
                request,
                &open.final_url,
                open.response.status(),
                open.attempts,
            ));
        }
        validate_identity_encoding(&request.task_id, &open.final_url, open.response.headers())?;
        let (actual_start, actual_end, actual_total) =
            parse_content_range(request, url, open.response.headers().get(CONTENT_RANGE))?;
        if (actual_start, actual_end, actual_total) != (start, end, total) {
            return Err(range_error(
                request,
                url,
                format!(
                    "Content-Range was {actual_start}-{actual_end}/{actual_total}, expected {start}-{end}/{total}"
                ),
            ));
        }
        if let Some(validator) = validator
            && !validator.matches(open.response.headers())
        {
            cancelled.cancel();
            return Err(TransferError::ResourceChanged {
                task_id: request.task_id.clone(),
            });
        }
        let expected = end - start + 1;
        let mut file = create_new_file(request, path, "create Range segment")?;
        match stream_exact(
            request,
            &open.final_url,
            &mut open.response,
            &mut file,
            expected,
            progress,
            TransferPhase::Segmented,
            Some(cancelled),
        ) {
            Ok(()) => {
                file.sync_all().map_err(|source| TransferError::Io {
                    task_id: request.task_id.clone(),
                    operation: "synchronize Range segment",
                    path: path.to_path_buf(),
                    source,
                })?;
                return Ok(());
            }
            Err(error)
                if retryable_body_error(&error) && body_attempt < transport.max_attempts() =>
            {
                drop(file);
                cleanup_file(path, "retry Range segment body");
                if cancelled.wait(transport.body_retry_delay(body_attempt)) {
                    return Err(progress.cancellation_error());
                }
            }
            Err(error) => {
                drop(file);
                cleanup_file(path, "failed Range segment");
                return Err(error);
            }
        }
    }
    Err(unexpected_retry_end(request, url, transport.max_attempts()))
}

impl Validator {
    fn value(&self) -> &HeaderValue {
        match self {
            Self::StrongEtag(value) => value,
        }
    }

    fn matches(&self, headers: &HeaderMap) -> bool {
        match self {
            Self::StrongEtag(expected) => strong_etag(headers).as_ref() == Some(expected),
        }
    }
}

fn strong_etag(headers: &HeaderMap) -> Option<HeaderValue> {
    let value = headers.get(ETAG)?;
    let text = value.to_str().ok()?.trim();
    if text.starts_with("W/") || !text.starts_with('"') || !text.ends_with('"') {
        return None;
    }
    Some(value.clone())
}

fn parse_content_range(
    request: &ArtifactRequest,
    url: &Url,
    value: Option<&HeaderValue>,
) -> Result<(u64, u64, u64), TransferError> {
    let value = value
        .ok_or_else(|| range_error(request, url, "Content-Range is missing".to_string()))?
        .to_str()
        .map_err(|source| {
            range_error(
                request,
                url,
                format!("Content-Range is not valid text: {source}"),
            )
        })?;
    let value = value
        .strip_prefix("bytes ")
        .ok_or_else(|| range_error(request, url, format!("invalid Content-Range '{value}'")))?;
    let (interval, total) = value
        .split_once('/')
        .ok_or_else(|| range_error(request, url, format!("invalid Content-Range '{value}'")))?;
    let (start, end) = interval
        .split_once('-')
        .ok_or_else(|| range_error(request, url, format!("invalid Content-Range '{value}'")))?;
    let start = parse_range_number(request, url, start, value)?;
    let end = parse_range_number(request, url, end, value)?;
    let total = parse_range_number(request, url, total, value)?;
    if start > end || end >= total || total == 0 {
        return Err(range_error(
            request,
            url,
            format!("inconsistent Content-Range '{value}'"),
        ));
    }
    Ok((start, end, total))
}

fn parse_range_number(
    request: &ArtifactRequest,
    url: &Url,
    number: &str,
    complete: &str,
) -> Result<u64, TransferError> {
    number.parse::<u64>().map_err(|source| {
        range_error(
            request,
            url,
            format!("invalid Content-Range '{complete}': {source}"),
        )
    })
}

fn stream_full_response(
    request: &ArtifactRequest,
    mut open: OpenResponse,
    staging: &Path,
    progress: &Arc<TaskProgress>,
) -> Result<(), TransferError> {
    progress.ensure_active()?;
    cleanup_file(staging, "prepare streaming staging file");
    let header_length = open
        .response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if let (Some(expected), Some(actual)) = (request.expected_size, header_length)
        && expected != actual
    {
        return Err(TransferError::SizeMismatch {
            task_id: request.task_id.clone(),
            expected,
            actual,
        });
    }
    if let (Some(maximum), Some(actual)) = (request.maximum_size, header_length)
        && actual > maximum
    {
        return Err(TransferError::SizeMismatch {
            task_id: request.task_id.clone(),
            expected: maximum,
            actual,
        });
    }
    progress.set_total(request.expected_size.or(header_length));
    let mut file = create_new_file(request, staging, "create streaming temporary file")?;
    let result = stream_unbounded(
        request,
        &open.final_url,
        &mut open.response,
        &mut file,
        progress,
        request.expected_size,
        request.maximum_size,
    );
    if let Err(error) = result {
        drop(file);
        cleanup_file(staging, "discard interrupted streaming staging file");
        return Err(error);
    }
    file.sync_all().map_err(|source| TransferError::Io {
        task_id: request.task_id.clone(),
        operation: "synchronize streaming temporary file",
        path: staging.to_path_buf(),
        source,
    })
}

fn stream_unbounded(
    request: &ArtifactRequest,
    url: &Url,
    source: &mut dyn Read,
    target: &mut File,
    progress: &Arc<TaskProgress>,
    expected: Option<u64>,
    maximum: Option<u64>,
) -> Result<u64, TransferError> {
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    let mut written = 0_u64;
    loop {
        progress.ensure_active()?;
        let count = source
            .read(&mut buffer)
            .map_err(|source| response_io_error(request, url, source))?;
        if count == 0 {
            break;
        }
        written = written.saturating_add(count as u64);
        let bound = expected.or(maximum);
        if bound.is_some_and(|maximum| written > maximum) {
            return Err(TransferError::SizeMismatch {
                task_id: request.task_id.clone(),
                expected: bound.expect("checked Some"),
                actual: written,
            });
        }
        target
            .write_all(&buffer[..count])
            .map_err(|source| TransferError::Io {
                task_id: request.task_id.clone(),
                operation: "write streaming temporary file",
                path: request.target.clone(),
                source,
            })?;
        progress.add_bytes(count as u64, TransferPhase::Single);
    }
    if let Some(expected) = expected
        && written != expected
    {
        return Err(TransferError::SizeMismatch {
            task_id: request.task_id.clone(),
            expected,
            actual: written,
        });
    }
    Ok(written)
}

#[allow(clippy::too_many_arguments)]
fn stream_exact(
    request: &ArtifactRequest,
    url: &Url,
    source: &mut dyn Read,
    target: &mut File,
    expected: u64,
    progress: &Arc<TaskProgress>,
    phase: TransferPhase,
    cancelled: Option<&Arc<CancellationToken>>,
) -> Result<(), TransferError> {
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    let mut written = 0_u64;
    loop {
        if cancelled.is_some_and(|cancelled| cancelled.is_cancelled()) {
            return Err(progress.cancellation_error());
        }
        let count = source
            .read(&mut buffer)
            .map_err(|source| response_io_error(request, url, source))?;
        if count == 0 {
            break;
        }
        written = written.saturating_add(count as u64);
        if written > expected {
            return Err(range_error(
                request,
                url,
                format!("segment body exceeded expected length {expected}"),
            ));
        }
        target
            .write_all(&buffer[..count])
            .map_err(|source| TransferError::Io {
                task_id: request.task_id.clone(),
                operation: "write Range segment",
                path: request.target.clone(),
                source,
            })?;
        progress.add_bytes(count as u64, phase);
    }
    if written != expected {
        return Err(range_error(
            request,
            url,
            format!("segment body had {written} bytes, expected {expected}"),
        ));
    }
    Ok(())
}

fn response_total(open: &OpenResponse) -> Option<u64> {
    open.response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn validate_identity_encoding(
    task_id: &str,
    url: &Url,
    headers: &HeaderMap,
) -> Result<(), TransferError> {
    let Some(value) = headers.get(CONTENT_ENCODING) else {
        return Ok(());
    };
    let value = value
        .to_str()
        .map_err(|source| TransferError::RangeProtocol {
            task_id: task_id.to_string(),
            url: redact_url(url),
            reason: format!("Content-Encoding is not valid text: {source}"),
        })?;
    if value.eq_ignore_ascii_case("identity") {
        Ok(())
    } else {
        Err(TransferError::RangeProtocol {
            task_id: task_id.to_string(),
            url: redact_url(url),
            reason: format!("Content-Encoding '{value}' is incompatible with verified transfer"),
        })
    }
}

fn segment_ranges(total: u64, count: usize) -> Vec<(u64, u64)> {
    debug_assert!(total > 0);
    debug_assert!(count > 0);
    let segment_size = total.div_ceil(count as u64);
    (0..count)
        .filter_map(|index| {
            let start = index as u64 * segment_size;
            if start >= total {
                return None;
            }
            let end = (start + segment_size - 1).min(total - 1);
            Some((start, end))
        })
        .collect()
}

fn merge_segments(
    request: &ArtifactRequest,
    segments: &[PathBuf],
    staging: &Path,
) -> Result<(), TransferError> {
    let mut output = create_new_file(request, staging, "create merged temporary file")?;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    for segment in segments {
        let mut input = File::open(segment).map_err(|source| TransferError::Io {
            task_id: request.task_id.clone(),
            operation: "open Range segment for merge",
            path: segment.clone(),
            source,
        })?;
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|source| TransferError::Io {
                    task_id: request.task_id.clone(),
                    operation: "read Range segment for merge",
                    path: segment.clone(),
                    source,
                })?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|source| TransferError::Io {
                    task_id: request.task_id.clone(),
                    operation: "write merged temporary file",
                    path: staging.to_path_buf(),
                    source,
                })?;
        }
    }
    output.sync_all().map_err(|source| TransferError::Io {
        task_id: request.task_id.clone(),
        operation: "synchronize merged temporary file",
        path: staging.to_path_buf(),
        source,
    })
}

fn verify_file(request: &ArtifactRequest, path: &Path) -> Result<u64, TransferError> {
    let mut file = File::open(path).map_err(|source| TransferError::Io {
        task_id: request.task_id.clone(),
        operation: "open completed temporary file for verification",
        path: path.to_path_buf(),
        source,
    })?;
    let mut sha1 = request.expected_sha1.as_ref().map(|_| Sha1::new());
    let mut sha256 = request.expected_sha256.as_ref().map(|_| Sha256::new());
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    let mut size = 0_u64;
    loop {
        let read = file.read(&mut buffer).map_err(|source| TransferError::Io {
            task_id: request.task_id.clone(),
            operation: "read completed temporary file for verification",
            path: path.to_path_buf(),
            source,
        })?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        if let Some(hasher) = &mut sha1 {
            hasher.update(&buffer[..read]);
        }
        if let Some(hasher) = &mut sha256 {
            hasher.update(&buffer[..read]);
        }
    }
    if let Some(expected) = request.expected_size
        && expected != size
    {
        return Err(TransferError::SizeMismatch {
            task_id: request.task_id.clone(),
            expected,
            actual: size,
        });
    }
    if let (Some(expected), Some(hasher)) = (&request.expected_sha1, sha1) {
        let actual = format!("{:x}", hasher.finalize());
        if actual != *expected {
            return Err(TransferError::Sha1Mismatch {
                task_id: request.task_id.clone(),
                expected: expected.clone(),
                actual,
            });
        }
    }
    if let (Some(expected), Some(hasher)) = (&request.expected_sha256, sha256) {
        let actual = format!("{:x}", hasher.finalize());
        if actual != *expected {
            return Err(TransferError::Sha256Mismatch {
                task_id: request.task_id.clone(),
                expected: expected.clone(),
                actual,
            });
        }
    }
    Ok(size)
}

fn ensure_parent(request: &ArtifactRequest) -> Result<(), TransferError> {
    let parent = request
        .target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| TransferError::Io {
        task_id: request.task_id.clone(),
        operation: "create artifact target directory",
        path: parent.to_path_buf(),
        source,
    })
}

fn create_new_file(
    request: &ArtifactRequest,
    path: &Path,
    operation: &'static str,
) -> Result<File, TransferError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| TransferError::Io {
            task_id: request.task_id.clone(),
            operation,
            path: path.to_path_buf(),
            source,
        })
}

fn unique_temp_path(target: &Path, suffix: &str) -> PathBuf {
    let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(".");
    name.push(
        target
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("artifact")),
    );
    name.push(format!(
        ".mc-server-download-tool-{}-{id}.{suffix}",
        std::process::id()
    ));
    target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(name)
}

fn segment_path(staging: &Path, index: usize) -> PathBuf {
    let mut value = staging.as_os_str().to_os_string();
    value.push(format!(".segment-{index}"));
    PathBuf::from(value)
}

fn atomic_publish(request: &ArtifactRequest, staging: &Path) -> Result<(), TransferError> {
    atomic_publish_with(request, staging, atomic_replace, sync_parent)
}

fn atomic_publish_with<Replace, Sync>(
    request: &ArtifactRequest,
    staging: &Path,
    replace: Replace,
    sync: Sync,
) -> Result<(), TransferError>
where
    Replace: FnOnce(&Path, &Path) -> std::io::Result<()>,
    Sync: FnOnce(&Path) -> std::io::Result<()>,
{
    replace(staging, &request.target).map_err(|source| TransferError::Io {
        task_id: request.task_id.clone(),
        operation: "atomically publish verified artifact",
        path: request.target.clone(),
        source,
    })?;
    if let Err(source) = sync(&request.target) {
        eprintln!(
            "artifact '{}' was published to '{}' but directory durability could not be confirmed: {source}",
            request.task_id,
            request.target.display()
        );
    }
    Ok(())
}

fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::io::Write;

    let mut input = File::open(source)?;
    let mut output = atomic_write_file::AtomicWriteFile::open(target)?;
    std::io::copy(&mut input, &mut output)?;
    output.flush()?;
    output.commit()?;
    if let Err(error) = fs::remove_file(source) {
        eprintln!(
            "verified artifact was published but staging file '{}' could not be removed: {error}",
            source.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(target: &Path) -> std::io::Result<()> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn sync_parent(_target: &Path) -> std::io::Result<()> {
    Ok(())
}

fn cleanup_file(path: &Path, context: &str) {
    const ATTEMPTS: usize = 3;
    for attempt in 1..=ATTEMPTS {
        match fs::remove_file(path) {
            Ok(()) => return,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return,
            Err(source) if attempt < ATTEMPTS => {
                eprintln!(
                    "failed to {context} '{}' on attempt {attempt}/{ATTEMPTS}: {source}",
                    path.display()
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(source) => {
                eprintln!(
                    "failed to {context} '{}' after {ATTEMPTS} attempts: {source}",
                    path.display()
                );
                return;
            }
        }
    }
}

fn range_error(request: &ArtifactRequest, url: &Url, reason: String) -> TransferError {
    TransferError::RangeProtocol {
        task_id: request.task_id.clone(),
        url: redact_url(url),
        reason,
    }
}

fn response_io_error(
    request: &ArtifactRequest,
    url: &Url,
    source: std::io::Error,
) -> TransferError {
    TransferError::Io {
        task_id: request.task_id.clone(),
        operation: "read HTTP response body",
        path: PathBuf::from(redact_url(url)),
        source,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn pool_transfer_error(task_id: &str, source: PoolError) -> TransferError {
    TransferError::Worker {
        task_id: task_id.to_string(),
        reason: source.to_string(),
    }
}

struct TaskProgress {
    task_id: String,
    observer: Arc<dyn TransferObserver>,
    transport: Arc<SharedTransport>,
    cancelled: Arc<CancellationToken>,
    observer_failure: Arc<Mutex<Option<super::model::TransferObserverError>>>,
    started: Instant,
    expected_total: Option<u64>,
    state: Mutex<ProgressState>,
}

struct ProgressState {
    transferred: u64,
    total: Option<u64>,
    last_emit: Option<Instant>,
    last_phase: Option<TransferPhase>,
    terminal_sent: bool,
}

impl TaskProgress {
    fn new(
        task_id: String,
        observer: Arc<dyn TransferObserver>,
        transport: Arc<SharedTransport>,
        cancelled: Arc<CancellationToken>,
        observer_failure: Arc<Mutex<Option<super::model::TransferObserverError>>>,
        total: Option<u64>,
    ) -> Self {
        Self {
            task_id,
            observer,
            transport,
            cancelled,
            observer_failure,
            started: Instant::now(),
            expected_total: total,
            state: Mutex::new(ProgressState {
                transferred: 0,
                total,
                last_emit: None,
                last_phase: None,
                terminal_sent: false,
            }),
        }
    }

    fn transferred(&self) -> u64 {
        self.lock_state().map_or(0, |state| state.transferred)
    }

    fn total(&self) -> Option<u64> {
        self.lock_state().and_then(|state| state.total)
    }

    fn set_total(&self, total: Option<u64>) {
        if let Some(mut state) = self.lock_state() {
            state.total = total.or(state.total);
        }
    }

    fn reset_candidate(&self) {
        if let Some(mut state) = self.lock_state() {
            state.total = self.expected_total;
            state.last_emit = None;
            state.last_phase = None;
        }
    }

    fn add_bytes(&self, bytes: u64, phase: TransferPhase) {
        let (transferred, total) = match self.lock_state() {
            Some(mut state) => {
                state.transferred = state.transferred.saturating_add(bytes);
                (state.transferred, state.total)
            }
            None => return,
        };
        self.emit(phase, transferred, total, false);
    }

    fn ensure_active(&self) -> Result<(), TransferError> {
        if self.cancelled.is_cancelled() {
            Err(self.cancellation_error())
        } else {
            Ok(())
        }
    }

    fn cancellation_error(&self) -> TransferError {
        let failure = self.observer_failure.lock().map_or_else(
            |poisoned| {
                eprintln!(
                    "network observer failure lock was poisoned for task '{}'",
                    self.task_id
                );
                poisoned.into_inner().clone()
            },
            |failure| failure.clone(),
        );
        failure.map_or_else(
            || TransferError::Cancelled {
                task_id: self.task_id.clone(),
            },
            |source| TransferError::Observer {
                task_id: self.task_id.clone(),
                source,
            },
        )
    }

    #[allow(clippy::cast_precision_loss)]
    fn emit(&self, phase: TransferPhase, transferred: u64, total: Option<u64>, force: bool) {
        let now = Instant::now();
        let Some(mut state) = self.lock_state() else {
            return;
        };
        if state.terminal_sent {
            return;
        }
        state.transferred = state.transferred.max(transferred);
        if total.is_some() {
            state.total = total;
        }
        let phase_changed = state.last_phase != Some(phase);
        let elapsed = state.last_emit.map_or(PROGRESS_INTERVAL, |last| {
            now.saturating_duration_since(last)
        });
        if !force && !phase_changed && elapsed < PROGRESS_INTERVAL {
            return;
        }
        if phase.terminal() {
            state.terminal_sent = true;
        }
        state.last_emit = Some(now);
        state.last_phase = Some(phase);
        let seconds = now.saturating_duration_since(self.started).as_secs_f64();
        let event = TransferEvent {
            task_id: self.task_id.clone(),
            phase,
            transferred_bytes: state.transferred,
            total_bytes: state.total,
            active_requests: self.transport.budget.active(),
            bytes_per_second: if seconds > 0.0 {
                state.transferred as f64 / seconds
            } else {
                0.0
            },
        };
        self.transport.dispatch_event(
            Arc::clone(&self.observer),
            event,
            Arc::clone(&self.cancelled),
            Arc::clone(&self.observer_failure),
        );
    }

    fn lock_state(&self) -> Option<std::sync::MutexGuard<'_, ProgressState>> {
        if let Ok(state) = self.state.lock() {
            Some(state)
        } else {
            eprintln!(
                "progress state lock was poisoned for task '{}'",
                self.task_id
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn post_commit_durability_failure_does_not_report_publication_as_uncommitted() {
        let request = ArtifactRequest::builder(
            "durability",
            PathBuf::from("published.bin"),
            "https://example.invalid/artifact",
        )
        .build()
        .unwrap();
        let replaced = AtomicBool::new(false);

        let result = atomic_publish_with(
            &request,
            Path::new("staging.bin"),
            |_, _| {
                replaced.store(true, Ordering::Release);
                Ok(())
            },
            |_| Err(io::Error::other("injected directory sync failure")),
        );

        assert!(result.is_ok());
        assert!(replaced.load(Ordering::Acquire));
    }
}

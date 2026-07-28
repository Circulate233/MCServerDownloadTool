use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mc_server_download_tool::net::{
    ArtifactRequest, ArtifactTransfer, DownloadMode, HttpRequest, HttpTransport, NetworkConfig,
    NetworkEngine, NetworkError, NetworkLimits, SensitiveHeaders, TransferError, TransferEvent,
    TransferObserver, TransferObserverError, TransferPhase,
};
use reqwest::header::{HeaderName, HeaderValue};
use sha1::{Digest, Sha1};
use sha2::Sha256;

#[derive(Clone)]
struct TestRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

impl TestRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

struct TestResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay: Duration,
    omit_length: bool,
}

impl TestResponse {
    fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
            delay: Duration::ZERO,
            omit_length: false,
        }
    }

    fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            delay: Duration::ZERO,
            omit_length: false,
        }
    }

    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn without_length(mut self) -> Self {
        self.omit_length = true;
        self
    }
}

#[derive(Default)]
struct RequestMetrics {
    active: AtomicUsize,
    peak: AtomicUsize,
    requests: AtomicUsize,
    path_active: Mutex<HashMap<String, usize>>,
    path_peak: Mutex<HashMap<String, usize>>,
}

impl RequestMetrics {
    fn enter(self: &Arc<Self>, path: &str) -> ActiveRequest {
        self.requests.fetch_add(1, Ordering::AcqRel);
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        update_peak(&self.peak, active);
        if let Ok(mut active_by_path) = self.path_active.lock() {
            let path_active = active_by_path.entry(path.to_string()).or_default();
            *path_active += 1;
            if let Ok(mut peak_by_path) = self.path_peak.lock() {
                let path_peak = peak_by_path.entry(path.to_string()).or_default();
                *path_peak = (*path_peak).max(*path_active);
            }
        }
        ActiveRequest {
            metrics: Arc::clone(self),
            path: path.to_string(),
        }
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::Acquire)
    }

    fn path_peak(&self, path: &str) -> usize {
        self.path_peak
            .lock()
            .ok()
            .and_then(|peaks| peaks.get(path).copied())
            .unwrap_or(0)
    }
}

struct ActiveRequest {
    metrics: Arc<RequestMetrics>,
    path: String,
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.metrics.active.fetch_sub(1, Ordering::AcqRel);
        if let Ok(mut active_by_path) = self.metrics.path_active.lock()
            && let Some(active) = active_by_path.get_mut(&self.path)
        {
            *active = active.saturating_sub(1);
        }
    }
}

struct TestServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    accept_thread: Option<thread::JoinHandle<()>>,
    metrics: Arc<RequestMetrics>,
    errors: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    fn start(handler: impl Fn(TestRequest) -> TestResponse + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let handler = Arc::new(handler);
        let stop = Arc::new(AtomicBool::new(false));
        let metrics = Arc::new(RequestMetrics::default());
        let errors = Arc::new(Mutex::new(Vec::new()));
        let accept_stop = Arc::clone(&stop);
        let accept_metrics = Arc::clone(&metrics);
        let accept_errors = Arc::clone(&errors);
        let accept_thread =
            thread::spawn(move || {
                let mut connections = Vec::new();
                while !accept_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Err(source) = stream.set_nonblocking(false) {
                                accept_errors.lock().unwrap().push(format!(
                                    "failed to make accepted stream blocking: {source}"
                                ));
                                continue;
                            }
                            let handler = Arc::clone(&handler);
                            let metrics = Arc::clone(&accept_metrics);
                            let errors = Arc::clone(&accept_errors);
                            connections.push(thread::spawn(move || {
                                let request_stream = match stream.try_clone() {
                                    Ok(stream) => stream,
                                    Err(source) => {
                                        errors.lock().unwrap().push(format!(
                                            "failed to clone accepted stream: {source}"
                                        ));
                                        return;
                                    }
                                };
                                match read_request(request_stream) {
                                    Ok(request) => {
                                        let _active = metrics.enter(&request.path);
                                        let response = handler(request);
                                        if let Err(source) = write_response(stream, response) {
                                            errors.lock().unwrap().push(format!(
                                                "failed to write response: {source}"
                                            ));
                                        }
                                    }
                                    Err(source) => errors
                                        .lock()
                                        .unwrap()
                                        .push(format!("failed to read request: {source}")),
                                }
                            }));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
                for connection in connections {
                    connection.join().unwrap();
                }
            });
        Self {
            address,
            stop,
            accept_thread: Some(accept_thread),
            metrics,
            errors,
        }
    }

    fn errors(&self) -> Vec<String> {
        self.errors.lock().unwrap().clone()
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.accept_thread.take() {
            handle.join().unwrap();
        }
    }
}

fn read_request(mut stream: TcpStream) -> std::io::Result<TestRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > 64 * 1024 {
            return Err(std::io::Error::other("request headers too large"));
        }
    }
    let text = String::from_utf8(bytes).map_err(std::io::Error::other)?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| std::io::Error::other("missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let path = request_parts.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Ok(TestRequest {
        method,
        path,
        headers,
    })
}

fn write_response(mut stream: TcpStream, response: TestResponse) -> std::io::Result<()> {
    thread::sleep(response.delay);
    let reason = match response.status {
        200 => "OK",
        206 => "Partial Content",
        302 => "Found",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Test",
    };
    let has_length = response
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"));
    write!(stream, "HTTP/1.1 {} {}\r\n", response.status, reason)?;
    for (name, value) in response.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    if !has_length && !response.omit_length {
        write!(stream, "Content-Length: {}\r\n", response.body.len())?;
    }
    write!(stream, "Connection: close\r\n\r\n")?;
    stream.write_all(&response.body)?;
    stream.flush()
}

fn update_peak(peak: &AtomicUsize, candidate: usize) {
    let mut current = peak.load(Ordering::Acquire);
    while candidate > current {
        match peak.compare_exchange_weak(current, candidate, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

fn test_engine() -> NetworkEngine {
    let config = NetworkConfig {
        max_attempts: 3,
        retry_base_delay: Duration::from_millis(1),
        retry_max_delay: Duration::from_millis(10),
        ..NetworkConfig::default()
    };
    NetworkEngine::with_limits(
        config,
        NetworkLimits {
            global_requests: 24,
            requests_per_host: 12,
            requests_per_file: 8,
            segment_threshold: 1024,
            target_segment_size: 1024,
        },
    )
    .unwrap()
}

fn noop_observer() -> Arc<dyn TransferObserver> {
    Arc::new(|_: TransferEvent| {})
}

fn artifact(id: &str, target: &Path, url: &str, bytes: &[u8]) -> ArtifactRequest {
    ArtifactRequest::builder(id, target, url)
        .expected_size(bytes.len() as u64)
        .expected_sha1(format!("{:x}", Sha1::digest(bytes)))
        .expected_sha256(format!("{:x}", Sha256::digest(bytes)))
        .build()
        .unwrap()
}

#[test]
fn automatic_limit_formula_clamps_each_budget() {
    let detected = std::thread::available_parallelism().unwrap().get();
    assert_eq!(
        NetworkLimits::automatic().unwrap(),
        NetworkLimits::for_parallelism(detected).unwrap()
    );

    for (cpus, expected) in [
        (1, (8, 4, 2)),
        (2, (8, 4, 2)),
        (3, (12, 6, 3)),
        (8, (32, 16, 8)),
        (16, (64, 32, 16)),
        (32, (64, 32, 16)),
        (usize::MAX, (64, 32, 16)),
    ] {
        let limits = NetworkLimits::for_parallelism(cpus).unwrap();
        assert_eq!(
            (
                limits.global_requests,
                limits.requests_per_host,
                limits.requests_per_file,
            ),
            expected
        );
    }
    assert!(matches!(
        NetworkLimits::for_parallelism(0),
        Err(NetworkError::InvalidConfiguration { .. })
    ));
}

#[test]
fn range_segment_count_uses_file_size_and_smallest_request_budget() {
    let limits = NetworkLimits {
        global_requests: 12,
        requests_per_host: 6,
        requests_per_file: 8,
        segment_threshold: 1,
        target_segment_size: 1024,
    };
    assert_eq!(limits.range_segment_count(0), 0);
    assert_eq!(limits.range_segment_count(1), 1);
    assert_eq!(limits.range_segment_count(1024), 1);
    assert_eq!(limits.range_segment_count(1025), 2);
    assert_eq!(limits.range_segment_count(6 * 1024), 6);
    assert_eq!(limits.range_segment_count(100 * 1024), 6);
}

#[test]
fn transfer_one_delegates_to_the_unified_batch_interface() {
    struct RecordingTransfer {
        calls: AtomicUsize,
    }

    impl ArtifactTransfer for RecordingTransfer {
        fn transfer_many(
            &self,
            requests: Vec<ArtifactRequest>,
            _observer: Arc<dyn TransferObserver>,
        ) -> Vec<Result<mc_server_download_tool::net::ArtifactOutcome, TransferError>> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            assert_eq!(requests.len(), 1);
            vec![Err(TransferError::Cancelled {
                task_id: requests[0].task_id().to_string(),
            })]
        }
    }

    let transfer = RecordingTransfer {
        calls: AtomicUsize::new(0),
    };
    let request = ArtifactRequest::builder(
        "single-interface",
        "single-interface.bin",
        "http://127.0.0.1/single-interface",
    )
    .build()
    .unwrap();
    assert!(matches!(
        transfer.transfer_one(request, noop_observer()),
        Err(TransferError::Cancelled { .. })
    ));
    assert_eq!(transfer.calls.load(Ordering::Acquire), 1);
}

fn parse_range(value: &str) -> Option<(usize, usize)> {
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

fn valid_range_response(
    request: &TestRequest,
    bytes: &[u8],
    etag: &str,
    delay: Duration,
) -> TestResponse {
    assert_eq!(request.method, "GET");
    assert_eq!(request.header("accept-encoding"), Some("identity"));
    if let Some((start, end)) = request.header("range").and_then(parse_range) {
        TestResponse {
            status: 206,
            headers: vec![
                (
                    "Content-Range".to_string(),
                    format!("bytes {start}-{end}/{}", bytes.len()),
                ),
                ("ETag".to_string(), etag.to_string()),
            ],
            body: bytes[start..=end].to_vec(),
            delay,
            omit_length: false,
        }
    } else {
        TestResponse::ok(bytes.to_vec())
            .header("ETag", etag)
            .delayed(delay)
    }
}

#[test]
fn bounded_http_retries_429_but_not_404_and_enforces_body_limit() {
    let retry_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&retry_calls);
    let server = TestServer::start(move |request| match request.path.as_str() {
        "/retry" => {
            if calls.fetch_add(1, Ordering::AcqRel) == 0 {
                TestResponse::status(429).header("Retry-After", "0")
            } else {
                TestResponse::ok(b"ok".to_vec())
            }
        }
        "/missing" => TestResponse::status(404),
        "/large" => TestResponse::ok(vec![7; 33]),
        "/stream-large" => TestResponse::ok(vec![8; 33]).without_length(),
        _ => TestResponse::status(500),
    });
    let engine = test_engine();

    let response = engine
        .get_bytes(HttpRequest::get(server.url("/retry"), 16).build().unwrap())
        .unwrap();
    assert_eq!(response.body, b"ok");
    assert_eq!(retry_calls.load(Ordering::Acquire), 2);

    let before_404 = server.metrics.requests();
    let error = engine
        .get_bytes(
            HttpRequest::get(server.url("/missing"), 16)
                .build()
                .unwrap(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        NetworkError::HttpStatus {
            status: 404,
            attempts: 1,
            ..
        }
    ));
    assert_eq!(server.metrics.requests() - before_404, 1);

    let before_500 = server.metrics.requests();
    let error = engine
        .get_bytes(
            HttpRequest::get(server.url("/always-500"), 16)
                .build()
                .unwrap(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        NetworkError::HttpStatus {
            status: 500,
            attempts: 3,
            ..
        }
    ));
    assert_eq!(server.metrics.requests() - before_500, 3);

    let error = engine
        .get_bytes(HttpRequest::get(server.url("/large"), 32).build().unwrap())
        .unwrap_err();
    assert!(matches!(
        error,
        NetworkError::ResponseTooLarge { limit: 32, .. }
    ));

    let error = engine
        .get_bytes(
            HttpRequest::get(server.url("/stream-large"), 32)
                .build()
                .unwrap(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        NetworkError::ResponseTooLarge { limit: 32, .. }
    ));
}

#[test]
fn redirect_scopes_arbitrary_sensitive_headers_to_the_exact_origin() {
    let received = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let destination_received = Arc::clone(&received);
    let destination = TestServer::start(move |request| {
        destination_received
            .lock()
            .unwrap()
            .push(request.header("x-custom-secret").map(str::to_string));
        TestResponse::ok(b"done".to_vec())
    });
    let redirect_target = destination.url("/destination");
    let source_received = Arc::clone(&received);
    let source = TestServer::start(move |request| {
        source_received
            .lock()
            .unwrap()
            .push(request.header("x-custom-secret").map(str::to_string));
        TestResponse::status(302).header("Location", redirect_target.clone())
    });
    let sensitive = SensitiveHeaders::new()
        .allow_origin(source.url("/"))
        .unwrap()
        .insert(
            HeaderName::from_static("x-custom-secret"),
            HeaderValue::from_static("secret-value"),
        )
        .unwrap();
    let request = HttpRequest::get(source.url("/source"), 32)
        .sensitive_headers(sensitive)
        .build()
        .unwrap();

    let response = test_engine().get_bytes(request).unwrap();
    assert_eq!(response.body, b"done");
    assert_eq!(
        received.lock().unwrap().as_slice(),
        &[Some("secret-value".to_string()), None]
    );

    let regular_credential = HttpRequest::get(source.url("/source"), 32).header(
        HeaderName::from_static("authorization"),
        HeaderValue::from_static("Bearer secret"),
    );
    assert!(matches!(
        regular_credential,
        Err(NetworkError::InvalidConfiguration { .. })
    ));
}

#[test]
fn legal_ranges_use_parallel_segments_and_publish_verified_file() {
    let bytes = Arc::new((0_u8..=250).cycle().take(8192).collect::<Vec<_>>());
    let response_bytes = Arc::clone(&bytes);
    let server = TestServer::start(move |request| {
        valid_range_response(
            &request,
            &response_bytes,
            "\"stable\"",
            Duration::from_millis(30),
        )
    });
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("directory with spaces/artifact file.bin");
    let request = artifact("segmented", &target, &server.url("/artifact"), &bytes);
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_log = Arc::clone(&events);
    let observer: Arc<dyn TransferObserver> = Arc::new(move |event: TransferEvent| {
        event_log.lock().unwrap().push(event);
    });

    let outcome = test_engine()
        .transfer_one(request, observer)
        .unwrap_or_else(|error| {
            panic!(
                "transfer failed: {error:?}; server errors: {:?}",
                server.errors()
            )
        });
    assert_eq!(outcome.mode, DownloadMode::Segmented);
    assert_eq!(std::fs::read(&target).unwrap(), *bytes);
    assert!(server.metrics.path_peak("/artifact") >= 2);
    assert!(server.metrics.path_peak("/artifact") <= 8);
    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.phase == TransferPhase::Segmented)
    );
    assert_eq!(events.last().unwrap().phase, TransferPhase::Completed);
    assert!(
        events
            .windows(2)
            .all(|window| { window[1].transferred_bytes >= window[0].transferred_bytes })
    );
}

#[test]
fn ignored_range_and_fake_accept_ranges_reuse_probe_200_response() {
    let bytes = vec![4_u8; 4096];
    let response_bytes = bytes.clone();
    let server = TestServer::start(move |request| {
        assert_eq!(request.header("range"), Some("bytes=0-0"));
        TestResponse::ok(response_bytes.clone()).header("Accept-Ranges", "bytes")
    });
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("ignored.bin");
    let request = artifact("ignored", &target, &server.url("/ignored"), &bytes);

    let outcome = test_engine()
        .transfer_one(request, noop_observer())
        .unwrap();
    assert_eq!(outcome.mode, DownloadMode::Single);
    assert_eq!(std::fs::read(target).unwrap(), bytes);
    assert_eq!(server.metrics.requests(), 1);
}

#[test]
fn invalid_content_range_and_416_fail_without_replacing_target() {
    for behavior in ["invalid", "416"] {
        let server = TestServer::start(move |request| {
            assert!(request.header("range").is_some());
            if behavior == "416" {
                TestResponse::status(416)
            } else {
                TestResponse {
                    status: 206,
                    headers: vec![("Content-Range".to_string(), "bytes 1-1/4096".to_string())],
                    body: vec![1],
                    delay: Duration::ZERO,
                    omit_length: false,
                }
            }
        });
        let bytes = vec![1_u8; 4096];
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("existing.bin");
        std::fs::write(&target, b"preserve").unwrap();
        let request = artifact("invalid-range", &target, &server.url("/file"), &bytes);

        let error = test_engine()
            .transfer_one(request, noop_observer())
            .unwrap_err();
        assert!(matches!(
            error,
            TransferError::RangeProtocol { .. } | TransferError::Network { .. }
        ));
        assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
    }
}

#[test]
fn truncated_segment_and_changed_etag_fail_without_publication() {
    for changed_etag in [false, true] {
        let bytes = Arc::new(vec![9_u8; 4096]);
        let response_bytes = Arc::clone(&bytes);
        let server = TestServer::start(move |request| {
            let (start, end) = parse_range(request.header("range").unwrap()).unwrap();
            if start == 0 && end == 0 {
                return valid_range_response(
                    &request,
                    &response_bytes,
                    "\"first\"",
                    Duration::ZERO,
                );
            }
            let etag = if changed_etag {
                "\"changed\""
            } else {
                "\"first\""
            };
            let mut response =
                valid_range_response(&request, &response_bytes, etag, Duration::from_millis(5));
            if !changed_etag && start == 1024 {
                response.body.pop();
                response
                    .headers
                    .push(("Content-Length".to_string(), (end - start + 1).to_string()));
            }
            response
        });
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.bin");
        std::fs::write(&target, b"old").unwrap();
        let request = artifact("identity", &target, &server.url("/file"), &bytes);

        let error = test_engine()
            .transfer_one(request, noop_observer())
            .unwrap_err();
        if changed_etag {
            assert!(matches!(error, TransferError::ResourceChanged { .. }));
        } else {
            assert!(matches!(
                error,
                TransferError::Io { .. } | TransferError::RangeProtocol { .. }
            ));
        }
        assert_eq!(std::fs::read(target).unwrap(), b"old");
        assert!(std::fs::read_dir(temp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".mc-server-download-tool-")
        }));
    }
}

#[test]
fn non_identity_range_response_is_rejected() {
    let server = TestServer::start(move |_| TestResponse {
        status: 206,
        headers: vec![
            ("Content-Range".to_string(), "bytes 0-0/4096".to_string()),
            ("Content-Encoding".to_string(), "gzip".to_string()),
        ],
        body: vec![1],
        delay: Duration::ZERO,
        omit_length: false,
    });
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("encoded.bin");
    let bytes = vec![1_u8; 4096];
    let request = artifact("encoded", &target, &server.url("/encoded"), &bytes);

    let error = test_engine()
        .transfer_one(request, noop_observer())
        .unwrap_err();
    assert!(matches!(error, TransferError::RangeProtocol { .. }));
    assert!(!target.exists());
}

#[test]
fn size_and_hash_failures_preserve_existing_targets() {
    let bytes = vec![3_u8; 128];
    let response_bytes = bytes.clone();
    let server = TestServer::start(move |_| TestResponse::ok(response_bytes.clone()));
    for case in ["size", "sha1", "sha256"] {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join(format!("{case} target.bin"));
        std::fs::write(&target, b"existing").unwrap();
        let mut builder = ArtifactRequest::builder(case, &target, server.url("/file"));
        builder = match case {
            "size" => builder.expected_size((bytes.len() + 1) as u64),
            "sha1" => builder.expected_sha1("0000000000000000000000000000000000000000"),
            "sha256" => builder.expected_sha256(
                "0000000000000000000000000000000000000000000000000000000000000000",
            ),
            _ => unreachable!(),
        };
        let request = builder.build().unwrap();
        let error = test_engine()
            .transfer_one(request, noop_observer())
            .unwrap_err();
        assert!(matches!(
            error,
            TransferError::SizeMismatch { .. }
                | TransferError::Sha1Mismatch { .. }
                | TransferError::Sha256Mismatch { .. }
        ));
        assert_eq!(std::fs::read(target).unwrap(), b"existing");
    }
}

#[test]
fn multiple_files_share_global_host_and_file_budgets_without_deadlock() {
    let bytes = Arc::new(vec![5_u8; 8192]);
    let response_bytes = Arc::clone(&bytes);
    let server = TestServer::start(move |request| {
        valid_range_response(
            &request,
            &response_bytes,
            "\"budget\"",
            Duration::from_millis(15),
        )
    });
    let temp = tempfile::tempdir().unwrap();
    let requests = (0..20)
        .map(|index| {
            let path = format!("/file-{index}");
            artifact(
                &format!("task-{index}"),
                &temp.path().join(format!("output-{index}.bin")),
                &server.url(&path),
                &bytes,
            )
        })
        .collect::<Vec<_>>();
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_log = Arc::clone(&events);
    let observer: Arc<dyn TransferObserver> = Arc::new(move |event: TransferEvent| {
        event_log.lock().unwrap().push(event);
    });
    let engine = test_engine();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(engine.transfer_many(requests, observer));
    });
    let results = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("transfer batch deadlocked");

    assert_eq!(results.len(), 20);
    assert!(
        results.iter().all(Result::is_ok),
        "results: {results:?}; server errors: {:?}",
        server.errors()
    );
    assert!(server.metrics.peak() >= 2);
    assert!(server.metrics.peak() <= 12);
    for index in 0..20 {
        assert!(server.metrics.path_peak(&format!("/file-{index}")) <= 8);
    }
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event.active_requests <= 24)
    );
}

#[test]
fn first_failure_cancels_unstarted_tasks_and_returns_without_deadlock() {
    let server = TestServer::start(move |request| {
        if request.path == "/bad" {
            TestResponse {
                status: 206,
                headers: vec![("Content-Range".to_string(), "bytes 99-99/4096".to_string())],
                body: vec![0],
                delay: Duration::ZERO,
                omit_length: false,
            }
        } else {
            TestResponse::ok(vec![1_u8; 4096]).delayed(Duration::from_millis(20))
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let bytes = vec![1_u8; 4096];
    let mut requests = vec![artifact(
        "bad",
        &temp.path().join("bad.bin"),
        &server.url("/bad"),
        &bytes,
    )];
    requests.extend((0..40).map(|index| {
        artifact(
            &format!("later-{index}"),
            &temp.path().join(format!("later-{index}.bin")),
            &server.url(&format!("/later-{index}")),
            &bytes,
        )
    }));
    let engine = test_engine();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(engine.transfer_many(requests, noop_observer()));
    });
    let results = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("failed batch deadlocked");
    assert!(results[0].is_err());
    assert!(
        results
            .iter()
            .any(|result| { matches!(result, Err(TransferError::Cancelled { .. })) })
    );
}

struct FailWhenRequestIsActive {
    metrics: Arc<RequestMetrics>,
    failed: AtomicBool,
}

impl TransferObserver for FailWhenRequestIsActive {
    fn observe(&self, event: TransferEvent) -> Result<(), TransferObserverError> {
        if event.phase != TransferPhase::Queued || self.failed.load(Ordering::Acquire) {
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.metrics.active.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        if self.metrics.active.load(Ordering::Acquire) > 0 {
            self.failed.store(true, Ordering::Release);
            return Err(TransferObserverError::new("persistent log unavailable"));
        }
        Ok(())
    }
}

#[test]
fn observer_failure_cancels_active_batch_without_publishing_targets() {
    let bytes = vec![9_u8; 4096];
    let response_bytes = bytes.clone();
    let server = TestServer::start(move |_| {
        TestResponse::ok(response_bytes.clone()).delayed(Duration::from_millis(300))
    });
    let temp = tempfile::tempdir().unwrap();
    let preserved_target = temp.path().join("preserved.bin");
    std::fs::write(&preserved_target, b"existing target").unwrap();
    let mut targets = vec![preserved_target.clone()];
    targets.extend((0..15).map(|index| temp.path().join(format!("queued-{index}.bin"))));
    let requests = targets
        .iter()
        .enumerate()
        .map(|(index, target)| {
            artifact(
                &format!("observer-cancel-{index}"),
                target,
                &server.url(&format!("/observer-cancel-{index}")),
                &bytes,
            )
        })
        .collect();
    let observer = Arc::new(FailWhenRequestIsActive {
        metrics: Arc::clone(&server.metrics),
        failed: AtomicBool::new(false),
    });
    let started = Instant::now();

    let results = test_engine().transfer_many(requests, observer);

    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        results
            .iter()
            .any(|result| matches!(result, Err(TransferError::Observer { .. })))
    );
    assert_eq!(std::fs::read(preserved_target).unwrap(), b"existing target");
    assert!(targets.iter().skip(1).all(|target| !target.exists()));
}

#[test]
fn progress_is_throttled_but_terminal_event_is_always_delivered() {
    let bytes = vec![8_u8; 4096];
    let response_bytes = bytes.clone();
    let server = TestServer::start(move |request| {
        valid_range_response(&request, &response_bytes, "\"events\"", Duration::ZERO)
    });
    let temp = tempfile::tempdir().unwrap();
    let request = artifact(
        "events",
        &temp.path().join("events.bin"),
        &server.url("/file"),
        &bytes,
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_log = Arc::clone(&events);
    let observer: Arc<dyn TransferObserver> = Arc::new(move |event: TransferEvent| {
        event_log.lock().unwrap().push(event);
    });

    test_engine().transfer_one(request, observer).unwrap();
    let events = events.lock().unwrap();
    assert_eq!(events.first().unwrap().phase, TransferPhase::Queued);
    assert_eq!(events.last().unwrap().phase, TransferPhase::Completed);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.phase == TransferPhase::Completed)
            .count(),
        1
    );
}

#[test]
fn retry_after_http_date_is_parsed_without_unbounded_wait() {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let server = TestServer::start(move |_| {
        if handler_calls.fetch_add(1, Ordering::AcqRel) == 0 {
            TestResponse::status(429).header(
                "Retry-After",
                httpdate::fmt_http_date(std::time::SystemTime::now()),
            )
        } else {
            TestResponse::ok(b"ok".to_vec())
        }
    });
    let started = Instant::now();
    let response = test_engine()
        .get_bytes(HttpRequest::get(server.url("/date"), 8).build().unwrap())
        .unwrap();
    assert_eq!(response.body, b"ok");
    assert_eq!(calls.load(Ordering::Acquire), 2);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn body_read_failures_retry_api_single_and_segmented_transfers_from_clean_files() {
    let api_calls = Arc::new(AtomicUsize::new(0));
    let single_calls = Arc::new(AtomicUsize::new(0));
    let segment_calls = Arc::new(AtomicUsize::new(0));
    let api_handler_calls = Arc::clone(&api_calls);
    let single_handler_calls = Arc::clone(&single_calls);
    let segment_handler_calls = Arc::clone(&segment_calls);
    let single_bytes = Arc::new(vec![11_u8; 128]);
    let segmented_bytes = Arc::new(vec![12_u8; 4096]);
    let served_single = Arc::clone(&single_bytes);
    let served_segmented = Arc::clone(&segmented_bytes);
    let server = TestServer::start(move |request| match request.path.as_str() {
        "/api" => {
            if api_handler_calls.fetch_add(1, Ordering::AcqRel) == 0 {
                TestResponse::ok(b"do".to_vec()).header("Content-Length", "4")
            } else {
                TestResponse::ok(b"done".to_vec())
            }
        }
        "/single" => {
            if single_handler_calls.fetch_add(1, Ordering::AcqRel) == 0 {
                TestResponse::ok(served_single[..64].to_vec())
                    .header("Content-Length", served_single.len().to_string())
            } else {
                TestResponse::ok(served_single.to_vec())
            }
        }
        "/segmented" => {
            let (start, end) = parse_range(request.header("range").unwrap()).unwrap();
            if start == 1024 && segment_handler_calls.fetch_add(1, Ordering::AcqRel) == 0 {
                let mut response = valid_range_response(
                    &request,
                    &served_segmented,
                    "\"retry-body\"",
                    Duration::ZERO,
                );
                response.body.pop();
                response
                    .headers
                    .push(("Content-Length".to_string(), (end - start + 1).to_string()));
                response
            } else {
                valid_range_response(
                    &request,
                    &served_segmented,
                    "\"retry-body\"",
                    Duration::ZERO,
                )
            }
        }
        _ => TestResponse::status(404),
    });
    let engine = test_engine();
    let api = engine
        .get_bytes(HttpRequest::get(server.url("/api"), 8).build().unwrap())
        .unwrap();
    assert_eq!(api.body, b"done");
    assert_eq!(api_calls.load(Ordering::Acquire), 2);

    let temp = tempfile::tempdir().unwrap();
    let single_target = temp.path().join("single.bin");
    let single = artifact(
        "single-retry",
        &single_target,
        &server.url("/single"),
        &single_bytes,
    );
    engine.transfer_one(single, noop_observer()).unwrap();
    assert_eq!(std::fs::read(single_target).unwrap(), *single_bytes);
    assert_eq!(single_calls.load(Ordering::Acquire), 2);

    let segmented_target = temp.path().join("segmented.bin");
    let segmented = artifact(
        "segment-retry",
        &segmented_target,
        &server.url("/segmented"),
        &segmented_bytes,
    );
    let outcome = engine.transfer_one(segmented, noop_observer()).unwrap();
    assert_eq!(outcome.mode, DownloadMode::Segmented);
    assert_eq!(std::fs::read(segmented_target).unwrap(), *segmented_bytes);
    assert_eq!(segment_calls.load(Ordering::Acquire), 2);
}

#[test]
fn unknown_empty_artifact_handles_unsatisfied_zero_range_with_full_fallback() {
    let server = TestServer::start(move |request| {
        if request.header("range").is_some() {
            TestResponse::status(416).header("Content-Range", "bytes */0")
        } else {
            TestResponse::ok(Vec::new())
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("empty.bin");
    let request = ArtifactRequest::builder("empty", &target, server.url("/empty"))
        .build()
        .unwrap();

    let outcome = test_engine()
        .transfer_one(request, noop_observer())
        .unwrap();
    assert_eq!(outcome.mode, DownloadMode::Single);
    assert_eq!(outcome.bytes, 0);
    assert_eq!(std::fs::read(target).unwrap(), Vec::<u8>::new());
    assert_eq!(server.metrics.requests(), 2);
}

#[test]
fn last_modified_only_segments_when_sha256_will_verify_the_final_file() {
    let bytes = Arc::new(vec![21_u8; 4096]);
    let response_bytes = Arc::clone(&bytes);
    let weak_segments = Arc::new(AtomicUsize::new(0));
    let strong_segments = Arc::new(AtomicUsize::new(0));
    let weak_handler_segments = Arc::clone(&weak_segments);
    let strong_handler_segments = Arc::clone(&strong_segments);
    let server = TestServer::start(move |request| {
        if let Some((start, end)) = request.header("range").and_then(parse_range) {
            if start != 0 || end != 0 {
                if request.path == "/weak" {
                    weak_handler_segments.fetch_add(1, Ordering::AcqRel);
                } else {
                    strong_handler_segments.fetch_add(1, Ordering::AcqRel);
                    assert_eq!(request.header("if-range"), None);
                }
            }
            TestResponse {
                status: 206,
                headers: vec![
                    (
                        "Content-Range".to_string(),
                        format!("bytes {start}-{end}/{}", response_bytes.len()),
                    ),
                    (
                        "Last-Modified".to_string(),
                        "Mon, 27 Jul 2026 00:00:00 GMT".to_string(),
                    ),
                ],
                body: response_bytes[start..=end].to_vec(),
                delay: Duration::ZERO,
                omit_length: false,
            }
        } else {
            TestResponse::ok(response_bytes.to_vec())
                .header("Last-Modified", "Mon, 27 Jul 2026 00:00:00 GMT")
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let weak_target = temp.path().join("weak.bin");
    let weak = ArtifactRequest::builder("weak", &weak_target, server.url("/weak"))
        .expected_size(bytes.len() as u64)
        .build()
        .unwrap();
    let weak_outcome = test_engine().transfer_one(weak, noop_observer()).unwrap();
    assert_eq!(weak_outcome.mode, DownloadMode::Single);
    assert_eq!(weak_segments.load(Ordering::Acquire), 0);

    let strong_target = temp.path().join("strong.bin");
    let strong = ArtifactRequest::builder("strong", &strong_target, server.url("/strong"))
        .expected_size(bytes.len() as u64)
        .expected_sha256(format!("{:x}", Sha256::digest(&*bytes)))
        .build()
        .unwrap();
    let strong_outcome = test_engine().transfer_one(strong, noop_observer()).unwrap();
    assert_eq!(strong_outcome.mode, DownloadMode::Segmented);
    assert!(strong_segments.load(Ordering::Acquire) >= 2);
    assert_eq!(std::fs::read(strong_target).unwrap(), *bytes);
}

#[test]
fn observer_isolated_reentrant_transfer_and_panic_do_not_deadlock_or_escape() {
    let outer_bytes = Arc::new(vec![31_u8; 512 * 1024]);
    let inner_bytes = Arc::new(vec![32_u8; 64]);
    let served_outer = Arc::clone(&outer_bytes);
    let served_inner = Arc::clone(&inner_bytes);
    let server = TestServer::start(move |request| {
        if request.path == "/outer" {
            TestResponse::ok(served_outer.to_vec()).delayed(Duration::from_millis(150))
        } else {
            TestResponse::ok(served_inner.to_vec())
        }
    });
    let engine = Arc::new(
        NetworkEngine::with_limits(
            NetworkConfig {
                max_attempts: 2,
                retry_base_delay: Duration::from_millis(1),
                retry_max_delay: Duration::from_millis(2),
                ..NetworkConfig::default()
            },
            NetworkLimits {
                global_requests: 1,
                requests_per_host: 1,
                requests_per_file: 1,
                segment_threshold: 1024 * 1024,
                target_segment_size: 1024,
            },
        )
        .unwrap(),
    );
    let temp = tempfile::tempdir().unwrap();
    let outer_target = temp.path().join("outer.bin");
    let inner_target = temp.path().join("inner.bin");
    let outer = artifact("outer", &outer_target, &server.url("/outer"), &outer_bytes);
    let nested_engine = Arc::clone(&engine);
    let nested_url = server.url("/inner");
    let nested_bytes = Arc::clone(&inner_bytes);
    let nested_target = inner_target.clone();
    let triggered = Arc::new(AtomicBool::new(false));
    let callback_triggered = Arc::clone(&triggered);
    let observer: Arc<dyn TransferObserver> = Arc::new(move |event: TransferEvent| {
        if event.phase == TransferPhase::Single
            && event.transferred_bytes > 0
            && !callback_triggered.swap(true, Ordering::AcqRel)
        {
            let nested = artifact("inner", &nested_target, &nested_url, &nested_bytes);
            nested_engine
                .transfer_one(nested, noop_observer())
                .expect("reentrant transfer must complete");
            panic!("intentional observer panic after reentrant transfer");
        }
    });
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let outer_engine = Arc::clone(&engine);
    thread::spawn(move || {
        let _ = sender.send(outer_engine.transfer_one(outer, observer));
    });
    let outcome = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("observer reentrancy deadlocked")
        .unwrap();
    assert_eq!(outcome.bytes, outer_bytes.len() as u64);
    assert!(triggered.load(Ordering::Acquire));
    assert_eq!(std::fs::read(outer_target).unwrap(), *outer_bytes);
    assert_eq!(std::fs::read(inner_target).unwrap(), *inner_bytes);
}

#[test]
fn normalized_target_ownership_rejects_cross_batch_and_alias_races() {
    let bytes = Arc::new(vec![41_u8; 128]);
    let response_bytes = Arc::clone(&bytes);
    let started = Arc::new(AtomicBool::new(false));
    let handler_started = Arc::clone(&started);
    let server = TestServer::start(move |_| {
        handler_started.store(true, Ordering::Release);
        TestResponse::ok(response_bytes.to_vec()).delayed(Duration::from_millis(200))
    });
    let first_engine = test_engine();
    let competing_engine = test_engine();
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("owned.bin");
    let first = artifact("first-owner", &target, &server.url("/file"), &bytes);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(first_engine.transfer_one(first, noop_observer()));
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while !started.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
    assert!(started.load(Ordering::Acquire));
    let competing = artifact("second-owner", &target, &server.url("/file"), &bytes);
    assert!(matches!(
        competing_engine.transfer_one(competing, noop_observer()),
        Err(TransferError::InvalidRequest { .. })
    ));
    receiver
        .recv_timeout(Duration::from_secs(3))
        .expect("first target owner stalled")
        .unwrap();

    let alias_parent = temp.path().join("alias-parent");
    std::fs::create_dir_all(&alias_parent).unwrap();
    let alias = alias_parent.join("..").join("owned.bin");
    let duplicate_batch = vec![
        artifact("alias-a", &target, &server.url("/file"), &bytes),
        artifact("alias-b", &alias, &server.url("/file"), &bytes),
    ];
    let results = competing_engine.transfer_many(duplicate_batch, noop_observer());
    assert!(
        results
            .iter()
            .all(|result| matches!(result, Err(TransferError::InvalidRequest { .. })))
    );
}

#[test]
fn concurrency_configuration_fails_before_allocating_unbounded_workers_or_queues() {
    let result = NetworkEngine::with_limits(
        NetworkConfig::default(),
        NetworkLimits {
            global_requests: usize::MAX,
            requests_per_host: 1,
            requests_per_file: 1,
            segment_threshold: 1,
            target_segment_size: 1,
        },
    );
    assert!(matches!(
        result,
        Err(NetworkError::InvalidConfiguration { .. })
    ));

    for config in [
        NetworkConfig {
            max_attempts: usize::MAX,
            ..NetworkConfig::default()
        },
        NetworkConfig {
            max_redirects: usize::MAX,
            ..NetworkConfig::default()
        },
        NetworkConfig {
            idle_connections_per_host: usize::MAX,
            ..NetworkConfig::default()
        },
    ] {
        assert!(matches!(
            NetworkEngine::new(config),
            Err(NetworkError::InvalidConfiguration { .. })
        ));
    }
}

#[test]
fn fallback_progress_is_cumulative_and_candidate_errors_redact_url_secrets() {
    let expected = vec![51_u8; 128];
    let wrong = vec![52_u8; 128];
    let served_expected = expected.clone();
    let served_wrong = wrong.clone();
    let server = TestServer::start(move |request| {
        if request.path.starts_with("/bad") {
            TestResponse::ok(served_wrong.clone())
        } else if request.path.starts_with("/good") {
            TestResponse::ok(served_expected.clone())
        } else {
            TestResponse::status(404)
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("fallback.bin");
    let request =
        ArtifactRequest::builder("fallback", &target, server.url("/bad?token=first-secret"))
            .candidate_url(server.url("/good?token=second-secret"))
            .expected_size(expected.len() as u64)
            .expected_sha256(format!("{:x}", Sha256::digest(&expected)))
            .build()
            .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_log = Arc::clone(&events);
    test_engine()
        .transfer_one(
            request,
            Arc::new(move |event: TransferEvent| event_log.lock().unwrap().push(event)),
        )
        .unwrap();
    let events = events.lock().unwrap();
    assert!(
        events
            .windows(2)
            .all(|pair| pair[1].transferred_bytes >= pair[0].transferred_bytes)
    );
    let completed = events.last().unwrap();
    assert_eq!(completed.phase, TransferPhase::Completed);
    assert!(completed.transferred_bytes > completed.total_bytes.unwrap());
    drop(events);

    let failed = ArtifactRequest::builder(
        "redacted",
        temp.path().join("missing.bin"),
        server.url("/missing?token=third-secret"),
    )
    .candidate_url(server.url("/also-missing?signature=fourth-secret"))
    .expected_size(1)
    .build()
    .unwrap();
    let message = test_engine()
        .transfer_one(failed, noop_observer())
        .unwrap_err()
        .to_string();
    assert!(!message.contains("third-secret"));
    assert!(!message.contains("fourth-secret"));
    assert!(!message.contains("?token="));
    assert!(!message.contains("?signature="));

    let invalid = HttpRequest::get("http://user:password-secret@example.com/file", 8)
        .build()
        .unwrap_err()
        .to_string();
    assert!(!invalid.contains("password-secret"));
}

#[test]
fn successful_publication_replaces_an_existing_target_atomically() {
    let bytes = vec![61_u8; 128];
    let response_bytes = bytes.clone();
    let server = TestServer::start(move |_| TestResponse::ok(response_bytes.clone()));
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("replace.bin");
    std::fs::write(&target, b"old target").unwrap();
    let request = artifact("replace", &target, &server.url("/replace"), &bytes);

    let outcome = test_engine()
        .transfer_one(request, noop_observer())
        .unwrap();
    assert_eq!(outcome.bytes, bytes.len() as u64);
    assert_eq!(std::fs::read(&target).unwrap(), bytes);
    assert!(std::fs::read_dir(temp.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".mc-server-download-tool-")
    }));
}

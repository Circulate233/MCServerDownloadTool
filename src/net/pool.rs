use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::thread;

use super::model::{
    MAX_GLOBAL_REQUESTS, MAX_QUEUED_JOBS, NetworkLimits, TransferEvent, TransferObserver,
    TransferObserverError,
};

type Job = Box<dyn FnOnce() + Send + 'static>;
const MAX_OBSERVER_EVENTS: usize = 1024;

enum Message {
    Run(Job),
    Shutdown,
}

/// Fixed blocking-worker pool used only for HTTP request work.
pub(crate) struct WorkerPool {
    sender: mpsc::SyncSender<Message>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl WorkerPool {
    pub(crate) fn new(
        name: &str,
        worker_count: usize,
        queue_capacity: usize,
    ) -> Result<Self, PoolError> {
        if worker_count == 0 || queue_capacity == 0 {
            return Err(PoolError::Unavailable(
                "worker count and queue capacity must be greater than zero".to_string(),
            ));
        }
        if worker_count > MAX_GLOBAL_REQUESTS || queue_capacity > MAX_QUEUED_JOBS {
            return Err(PoolError::Unavailable(format!(
                "worker pool exceeds hard limits ({MAX_GLOBAL_REQUESTS} workers, {MAX_QUEUED_JOBS} queued jobs)"
            )));
        }
        let (sender, receiver) = mpsc::sync_channel::<Message>(queue_capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            let thread_name = format!("mc-server-download-tool-{name}-{index}");
            let handle = thread::Builder::new()
                .name(thread_name)
                .spawn(move || worker_loop(&receiver))
                .map_err(|source| {
                    PoolError::Unavailable(format!("failed to start HTTP worker {index}: {source}"))
                })?;
            workers.push(handle);
        }
        Ok(Self {
            sender,
            workers: Mutex::new(workers),
        })
    }

    pub(crate) fn submit<T, F>(&self, operation: F) -> Result<JobHandle<T>, PoolError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (sender, receiver) = mpsc::sync_channel(1);
        let job = Box::new(move || {
            let outcome =
                catch_unwind(AssertUnwindSafe(operation)).map_err(|_| PoolError::Panicked);
            let _ = sender.send(outcome);
        });
        self.sender
            .send(Message::Run(job))
            .map_err(|_| PoolError::Unavailable("HTTP worker queue is closed".to_string()))?;
        Ok(JobHandle { receiver })
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        let worker_count = self.workers.lock().map_or(0, |workers| workers.len());
        for _ in 0..worker_count {
            if self.sender.send(Message::Shutdown).is_err() {
                eprintln!("network worker pool shutdown channel closed unexpectedly");
                break;
            }
        }
        if let Ok(mut workers) = self.workers.lock() {
            for worker in workers.drain(..) {
                if worker.join().is_err() {
                    eprintln!("network worker panicked during shutdown");
                }
            }
        } else {
            eprintln!("network worker list lock was poisoned during shutdown");
        }
    }
}

enum ObserverMessage {
    Event(
        Arc<dyn TransferObserver>,
        TransferEvent,
        Arc<CancellationToken>,
        Arc<Mutex<Option<TransferObserverError>>>,
        Arc<RequestBudget>,
    ),
    Flush(mpsc::SyncSender<()>),
    Shutdown,
}

/// One engine-wide callback thread keeps user observers out of network workers.
pub(crate) struct ObserverDispatcher {
    sender: mpsc::SyncSender<ObserverMessage>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    thread_id: Arc<Mutex<Option<thread::ThreadId>>>,
}

impl ObserverDispatcher {
    pub(crate) fn new() -> Result<Arc<Self>, PoolError> {
        let (sender, receiver) = mpsc::sync_channel(MAX_OBSERVER_EVENTS);
        let thread_id = Arc::new(Mutex::new(None));
        let worker_thread_id = Arc::clone(&thread_id);
        let worker = thread::Builder::new()
            .name("mc-server-download-tool-observer".to_string())
            .spawn(move || {
                match worker_thread_id.lock() {
                    Ok(mut slot) => *slot = Some(thread::current().id()),
                    Err(poisoned) => {
                        eprintln!(
                            "network observer thread identity lock was poisoned during startup"
                        );
                        *poisoned.into_inner() = Some(thread::current().id());
                    }
                }
                while let Ok(message) = receiver.recv() {
                    match message {
                        ObserverMessage::Event(observer, event, cancelled, failure, budget) => {
                            invoke_observer(&observer, event, &cancelled, &failure, &budget);
                        }
                        ObserverMessage::Flush(done) => {
                            if done.send(()).is_err() {
                                eprintln!("network observer flush receiver was dropped");
                            }
                        }
                        ObserverMessage::Shutdown => return,
                    }
                }
            })
            .map_err(|source| {
                PoolError::Unavailable(format!("failed to start observer worker: {source}"))
            })?;
        Ok(Arc::new(Self {
            sender,
            worker: Mutex::new(Some(worker)),
            thread_id,
        }))
    }

    pub(crate) fn emit(
        &self,
        observer: Arc<dyn TransferObserver>,
        event: TransferEvent,
        cancelled: Arc<CancellationToken>,
        failure: Arc<Mutex<Option<TransferObserverError>>>,
        budget: Arc<RequestBudget>,
    ) {
        if self.on_dispatcher_thread() {
            invoke_observer(&observer, event, &cancelled, &failure, &budget);
            return;
        }
        if self
            .sender
            .send(ObserverMessage::Event(
                observer, event, cancelled, failure, budget,
            ))
            .is_err()
        {
            eprintln!("network observer dispatcher stopped before receiving an event");
        }
    }

    pub(crate) fn flush(&self) {
        if self.on_dispatcher_thread() {
            return;
        }
        let (done, completed) = mpsc::sync_channel(1);
        if self.sender.send(ObserverMessage::Flush(done)).is_err() {
            eprintln!("network observer dispatcher stopped before flush");
            return;
        }
        if completed.recv().is_err() {
            eprintln!("network observer dispatcher stopped during flush");
        }
    }

    fn on_dispatcher_thread(&self) -> bool {
        let thread_id = match self.thread_id.lock() {
            Ok(thread_id) => *thread_id,
            Err(poisoned) => {
                eprintln!("network observer thread identity lock was poisoned");
                *poisoned.into_inner()
            }
        };
        thread_id.is_some_and(|thread_id| thread_id == thread::current().id())
    }
}

impl Drop for ObserverDispatcher {
    fn drop(&mut self) {
        let on_dispatcher = self.on_dispatcher_thread();
        if !on_dispatcher && self.sender.send(ObserverMessage::Shutdown).is_err() {
            eprintln!("network observer dispatcher was already stopped");
        }
        let current = thread::current().id();
        if let Ok(mut worker) = self.worker.lock() {
            if let Some(worker) = worker.take()
                && worker.thread().id() != current
                && worker.join().is_err()
            {
                eprintln!("network observer worker panicked during shutdown");
            }
        } else {
            eprintln!("network observer worker lock was poisoned during shutdown");
        }
    }
}

fn invoke_observer(
    observer: &Arc<dyn TransferObserver>,
    event: TransferEvent,
    cancelled: &Arc<CancellationToken>,
    failure: &Arc<Mutex<Option<TransferObserverError>>>,
    budget: &Arc<RequestBudget>,
) {
    match catch_unwind(AssertUnwindSafe(|| observer.observe(event))) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            match failure.lock() {
                Ok(mut stored) if stored.is_none() => *stored = Some(error),
                Ok(_) => {}
                Err(_) => eprintln!("network observer failure lock was poisoned"),
            }
            cancelled.cancel();
            budget.notify_cancelled();
        }
        Err(_) => eprintln!("network transfer observer panicked; callback was isolated"),
    }
}

pub(crate) struct CancellationToken {
    cancelled: AtomicBool,
    parent: Option<Arc<Self>>,
    signal: Arc<CancellationSignal>,
}

#[derive(Default)]
struct CancellationSignal {
    state: Mutex<()>,
    changed: Condvar,
}

impl CancellationToken {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            cancelled: AtomicBool::new(false),
            parent: None,
            signal: Arc::new(CancellationSignal::default()),
        })
    }

    pub(crate) fn child(parent: &Arc<Self>) -> Arc<Self> {
        Arc::new(Self {
            cancelled: AtomicBool::new(false),
            parent: Some(Arc::clone(parent)),
            signal: Arc::clone(&parent.signal),
        })
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.signal.changed.notify_all();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.is_cancelled())
    }

    pub(crate) fn wait(&self, duration: std::time::Duration) -> bool {
        if self.is_cancelled() {
            return true;
        }
        let Ok(state) = self.signal.state.lock() else {
            eprintln!("network cancellation wait lock was poisoned");
            return true;
        };
        if self.signal.changed.wait_timeout(state, duration).is_err() {
            eprintln!("network cancellation wait lock was poisoned while waiting");
            return true;
        }
        self.is_cancelled()
    }
}

#[derive(Default)]
pub(crate) struct TargetRegistry {
    active: Mutex<HashSet<PathBuf>>,
}

pub(crate) fn global_target_registry() -> Arc<TargetRegistry> {
    static TARGETS: OnceLock<Arc<TargetRegistry>> = OnceLock::new();
    Arc::clone(TARGETS.get_or_init(|| Arc::new(TargetRegistry::default())))
}

impl TargetRegistry {
    pub(crate) fn acquire(self: &Arc<Self>, targets: Vec<PathBuf>) -> Result<TargetLease, String> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| "target ownership registry lock was poisoned".to_string())?;
        if let Some(target) = targets.iter().find(|target| active.contains(*target)) {
            return Err(format!(
                "normalized target '{}' is already owned by another transfer",
                target.display()
            ));
        }
        active.extend(targets.iter().cloned());
        Ok(TargetLease {
            registry: Arc::clone(self),
            targets,
        })
    }
}

pub(crate) struct TargetLease {
    registry: Arc<TargetRegistry>,
    targets: Vec<PathBuf>,
}

impl Drop for TargetLease {
    fn drop(&mut self) {
        match self.registry.active.lock() {
            Ok(mut active) => {
                for target in &self.targets {
                    active.remove(target);
                }
            }
            Err(_) => eprintln!("target ownership registry lock was poisoned during release"),
        }
    }
}

fn worker_loop(receiver: &Mutex<mpsc::Receiver<Message>>) {
    loop {
        let message = if let Ok(receiver) = receiver.lock() {
            receiver.recv()
        } else {
            eprintln!("network worker receiver lock was poisoned");
            return;
        };
        match message {
            Ok(Message::Run(job)) => job(),
            Ok(Message::Shutdown) | Err(_) => return,
        }
    }
}

pub(crate) struct JobHandle<T> {
    receiver: mpsc::Receiver<Result<T, PoolError>>,
}

impl<T> JobHandle<T> {
    pub(crate) fn wait(self) -> Result<T, PoolError> {
        self.receiver.recv().map_err(|_| {
            PoolError::Unavailable("HTTP worker ended without returning a result".to_string())
        })?
    }
}

#[derive(Debug)]
pub(crate) enum PoolError {
    Unavailable(String),
    Panicked,
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) => formatter.write_str(reason),
            Self::Panicked => formatter.write_str("an HTTP worker panicked"),
        }
    }
}

/// Shared request counters prove global, host, and artifact concurrency bounds.
pub(crate) struct RequestBudget {
    limits: NetworkLimits,
    state: Mutex<BudgetState>,
    changed: Condvar,
    active: AtomicUsize,
}

pub(crate) enum BudgetAcquireError {
    Cancelled,
    Unavailable,
}

#[derive(Default)]
struct BudgetState {
    global: usize,
    hosts: HashMap<String, usize>,
    files: HashMap<String, usize>,
}

impl RequestBudget {
    pub(crate) fn new(limits: NetworkLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(BudgetState::default()),
            changed: Condvar::new(),
            active: AtomicUsize::new(0),
        }
    }

    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn acquire(
        self: &Arc<Self>,
        host: &str,
        task_id: Option<&str>,
        cancelled: Option<&CancellationToken>,
    ) -> Result<RequestPermit, BudgetAcquireError> {
        let mut state = self.state.lock().map_err(|_| {
            eprintln!("network request budget lock was poisoned while acquiring a permit");
            BudgetAcquireError::Unavailable
        })?;
        loop {
            if cancelled.is_some_and(CancellationToken::is_cancelled) {
                return Err(BudgetAcquireError::Cancelled);
            }
            let host_count = state.hosts.get(host).copied().unwrap_or(0);
            let file_count = task_id
                .and_then(|task_id| state.files.get(task_id).copied())
                .unwrap_or(0);
            if state.global < self.limits.global_requests
                && host_count < self.limits.requests_per_host
                && task_id.is_none_or(|_| file_count < self.limits.requests_per_file)
            {
                state.global += 1;
                *state.hosts.entry(host.to_string()).or_default() += 1;
                if let Some(task_id) = task_id {
                    *state.files.entry(task_id.to_string()).or_default() += 1;
                }
                self.active.fetch_add(1, Ordering::AcqRel);
                return Ok(RequestPermit {
                    budget: Arc::clone(self),
                    host: host.to_string(),
                    task_id: task_id.map(str::to_string),
                });
            }
            state = self.changed.wait(state).map_err(|_| {
                eprintln!("network request budget lock was poisoned while waiting for a permit");
                BudgetAcquireError::Unavailable
            })?;
        }
    }

    pub(crate) fn notify_cancelled(&self) {
        self.changed.notify_all();
    }
}

pub(crate) struct RequestPermit {
    budget: Arc<RequestBudget>,
    host: String,
    task_id: Option<String>,
}

impl Drop for RequestPermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.budget.state.lock() {
            state.global = state.global.saturating_sub(1);
            decrement(&mut state.hosts, &self.host);
            if let Some(task_id) = self.task_id.as_deref() {
                decrement(&mut state.files, task_id);
            }
            self.budget.active.fetch_sub(1, Ordering::AcqRel);
            self.budget.changed.notify_all();
        } else {
            eprintln!("network request budget lock was poisoned while releasing a permit");
        }
    }
}

fn decrement(counts: &mut HashMap<String, usize>, key: &str) {
    let remove = if let Some(count) = counts.get_mut(key) {
        *count = count.saturating_sub(1);
        *count == 0
    } else {
        false
    };
    if remove {
        counts.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::net::model::TransferPhase;

    fn event(index: usize) -> TransferEvent {
        TransferEvent {
            task_id: format!("queued-{index}"),
            phase: TransferPhase::Queued,
            transferred_bytes: 0,
            total_bytes: None,
            active_requests: 0,
            bytes_per_second: 0.0,
        }
    }

    #[test]
    fn observer_queue_applies_bounded_backpressure_without_losing_delivery() {
        let dispatcher = ObserverDispatcher::new().unwrap();
        let blocked_once = Arc::new(AtomicBool::new(false));
        let callback_blocked = Arc::clone(&blocked_once);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let release_rx = Arc::new(Mutex::new(release_rx));
        let callback_release = Arc::clone(&release_rx);
        let observer: Arc<dyn TransferObserver> = Arc::new(move |_: TransferEvent| {
            if !callback_blocked.swap(true, Ordering::AcqRel) {
                entered_tx.send(()).unwrap();
                callback_release.lock().unwrap().recv().unwrap();
            }
        });
        let cancelled = CancellationToken::new();
        let observer_failure = Arc::new(Mutex::new(None));
        let budget = Arc::new(RequestBudget::new(
            NetworkLimits::for_parallelism(1).unwrap(),
        ));

        dispatcher.emit(
            Arc::clone(&observer),
            event(0),
            Arc::clone(&cancelled),
            Arc::clone(&observer_failure),
            Arc::clone(&budget),
        );
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let producer_dispatcher = Arc::clone(&dispatcher);
        let producer_observer = Arc::clone(&observer);
        let producer_cancelled = Arc::clone(&cancelled);
        let producer_observer_failure = Arc::clone(&observer_failure);
        let producer_budget = Arc::clone(&budget);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let producer = thread::spawn(move || {
            for index in 1..=MAX_OBSERVER_EVENTS + 1 {
                producer_dispatcher.emit(
                    Arc::clone(&producer_observer),
                    event(index),
                    Arc::clone(&producer_cancelled),
                    Arc::clone(&producer_observer_failure),
                    Arc::clone(&producer_budget),
                );
            }
            done_tx.send(()).unwrap();
        });

        let producer_was_blocked = done_rx.recv_timeout(Duration::from_millis(50)).is_err();
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        producer.join().unwrap();
        dispatcher.flush();
        assert!(producer_was_blocked);
    }
}

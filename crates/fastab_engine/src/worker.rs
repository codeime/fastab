use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::anyhow;
// Not `tokio::sync::oneshot`. The desktop awaits the reply on GPUI's foreground
// executor, which used to run nested inside one `tokio::Runtime::block_on` poll
// that hosted `NSApplication::run` and never returned. `block_on` fixes tokio's
// cooperative budget (128 units) for the life of that poll, and every tokio
// channel that resolves on that thread spends one. Once they were gone the
// 129th reply never resolved: `poll_proceed` reported Pending and woke the task
// at once, GPUI re-queued it onto the same main-queue drain, and the desktop sat
// at 100% CPU with the overlay frozen. `fastab_desktop` now starts the UI loop
// outside `block_on`; this channel has no budget accounting at all, so the reply
// path stays correct even if a caller polls it from inside one again.
use futures::channel::oneshot;

use crate::cancellation::{CancellationToken, CompletionCancelled};
use crate::diagnostics::{EngineClientDiagnostics, RequestDiagnostics, WorkerResourceDiagnostics};
use crate::ir::Registry;
use crate::rank::AcceptanceIndex;
use crate::runtime::{CompleteRequest, CompleteResult, Engine};

/// Thread-safe handle around the completion [`Engine`].
#[derive(Clone)]
pub struct EngineClient {
    tx: JobSender,
    acceptance: Arc<Mutex<AcceptanceIndex>>,
    submission: Arc<Mutex<Submission>>,
}

/// Caller identity for one terminal session; zero is the legacy anonymous caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct SessionId(u128);

impl SessionId {
    pub const ANONYMOUS: Self = Self(0);
    pub const fn new(value: u128) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone)]
pub struct EngineClientOptions {
    /// Idle grace for specs and hook results in controlled resource replay.
    /// Normal clients use the default.
    pub spec_idle_grace: Duration,
    /// How long after its last use loaded history stays resident.
    pub history_idle_grace: Duration,
}

impl Default for EngineClientOptions {
    fn default() -> Self {
        Self {
            spec_idle_grace: crate::ir::SPEC_IDLE_GRACE,
            history_idle_grace: crate::runtime::HISTORY_IDLE_GRACE,
        }
    }
}

#[derive(Default)]
struct Submission {
    sequence: u64,
    owner: Option<SessionId>,
    latest: Option<CancellationToken>,
    requests: RequestDiagnostics,
}

/// Already submitted completion. Dropping an unpolled task cancels that request.
/// Its reply uses no Tokio cooperative budget and can be polled by GPUI.
pub struct CompletionTask {
    receiver: oneshot::Receiver<anyhow::Result<CompleteResult>>,
    token: CancellationToken,
}

impl Future for CompletionTask {
    type Output = anyhow::Result<CompleteResult>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.receiver).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_)) => Poll::Ready(Err(anyhow!("engine dropped the reply"))),
        }
    }
}

impl Drop for CompletionTask {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

struct Job {
    kind: JobKind,
}

// Completion submissions are latest-wins, while controls retain FIFO order.
// Bound controls as well: a stalled filesystem must not grow a mailbox forever.
const MAX_QUEUED_JOBS: usize = 256;
const MAX_QUEUED_BYTES: usize = 8 * 1024 * 1024;
// Only the committed owner and the running attempt can still own resources
// after queued, cancelled completions are removed. Their EndInput notifications
// must remain enqueueable even when ordinary control traffic fills the mailbox.
const RESERVED_END_INPUT_JOBS: usize = 2;

struct JobQueue {
    state: Mutex<JobQueueState>,
    ready: Condvar,
}

struct JobQueueState {
    jobs: VecDeque<(Job, usize)>,
    bytes: usize,
    senders: usize,
    receiver_alive: bool,
    resource_session: Option<SessionId>,
    attempt_session: Option<SessionId>,
}

struct JobSender(Arc<JobQueue>);
struct JobReceiver(Arc<JobQueue>);

struct JobSendError(Box<Job>, &'static str, usize);

impl std::fmt::Debug for JobSendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.1)
    }
}

fn job_channel() -> (JobSender, JobReceiver) {
    let queue = Arc::new(JobQueue {
        state: Mutex::new(JobQueueState {
            jobs: VecDeque::new(),
            bytes: 0,
            senders: 1,
            receiver_alive: true,
            resource_session: None,
            attempt_session: None,
        }),
        ready: Condvar::new(),
    });
    (JobSender(Arc::clone(&queue)), JobReceiver(queue))
}

impl Clone for JobSender {
    fn clone(&self) -> Self {
        self.0.state.lock().unwrap_or_else(|error| error.into_inner()).senders += 1;
        Self(Arc::clone(&self.0))
    }
}

impl Drop for JobSender {
    fn drop(&mut self) {
        self.0.state.lock().unwrap_or_else(|error| error.into_inner()).senders -= 1;
        self.0.ready.notify_one();
    }
}

impl JobSender {
    /// Returns the number of superseded requests removed before enqueueing.
    fn send(&self, job: Job) -> Result<usize, JobSendError> {
        let bytes = job.payload_bytes();
        let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.receiver_alive {
            return Err(JobSendError(Box::new(job), "engine thread is gone", 0));
        }
        let mut removed = 0;
        let mut retired_bytes = 0;
        state.jobs.retain_mut(|(queued, bytes)| {
            if let JobKind::Complete { token, reply, .. } = &queued.kind
                && (token.is_cancelled() || reply.is_canceled())
            {
                removed += 1;
                retired_bytes += *bytes;
                if let JobKind::Complete { reply, .. } = std::mem::replace(&mut queued.kind, JobKind::ClearCaches) {
                    let _ = reply.send(Err(CompletionCancelled.into()));
                }
                false
            } else {
                true
            }
        });
        state.bytes -= retired_bytes;
        let end_input = if let JobKind::EndInput { session } = &job.kind {
            // A queued request from this session has already been cancelled
            // above. No other session may acquire its resources later.
            if state.resource_session != Some(*session) && state.attempt_session != Some(*session) {
                return Ok(removed);
            }
            // Keep the first notification at its original FIFO position. A
            // retained completion for this session is the only boundary that
            // could make a later notification meaningful again.
            for (queued, _) in state.jobs.iter().rev() {
                match &queued.kind {
                    JobKind::Complete {
                        session: queued_session,
                        ..
                    } if queued_session == session => break,
                    JobKind::EndInput {
                        session: queued_session,
                    } if queued_session == session => return Ok(removed),
                    _ => {},
                }
            }
            true
        } else {
            false
        };
        let reserved_jobs = if end_input { 0 } else { RESERVED_END_INPUT_JOBS };
        let job_limit = MAX_QUEUED_JOBS - reserved_jobs;
        let byte_limit = MAX_QUEUED_BYTES - reserved_jobs * std::mem::size_of::<Job>();
        if state.jobs.len() >= job_limit || bytes > byte_limit.saturating_sub(state.bytes) {
            return Err(JobSendError(
                Box::new(job),
                "engine request queue capacity exceeded",
                removed,
            ));
        }
        state.bytes += bytes;
        state.jobs.push_back((job, bytes));
        self.0.ready.notify_one();
        Ok(removed)
    }
}

impl Drop for JobReceiver {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
        state.receiver_alive = false;
        state.jobs.clear();
        state.bytes = 0;
    }
}

impl JobReceiver {
    fn set_resource_session(&self, session: Option<SessionId>) {
        let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
        state.resource_session = session;
        state.attempt_session = None;
    }

    fn diagnostics(&self, operations: &OperationCounters) -> WorkerResourceDiagnostics {
        let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
        let operations = operations.0.lock().unwrap_or_else(|error| error.into_inner());
        WorkerResourceDiagnostics {
            queued_jobs: state.jobs.len(),
            queued_payload_bytes: state.bytes,
            active_operations: operations.active,
            abandoned_operations: operations.abandoned,
        }
    }

    fn recv(&self) -> Result<Job, mpsc::RecvTimeoutError> {
        self.receive(None)
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<Job, mpsc::RecvTimeoutError> {
        self.receive(Some(Instant::now() + timeout))
    }

    #[cfg(test)]
    fn try_recv(&self) -> Result<Job, mpsc::RecvTimeoutError> {
        self.recv_timeout(Duration::ZERO)
    }

    fn receive(&self, deadline: Option<Instant>) -> Result<Job, mpsc::RecvTimeoutError> {
        let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if let Some((job, bytes)) = state.jobs.pop_front() {
                state.bytes -= bytes;
                if let JobKind::Complete { session, .. } = &job.kind {
                    state.attempt_session = Some(*session);
                }
                return Ok(job);
            }
            if state.senders == 0 {
                return Err(mpsc::RecvTimeoutError::Disconnected);
            }
            state = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(mpsc::RecvTimeoutError::Timeout);
                    }
                    self.0
                        .ready
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|error| error.into_inner())
                        .0
                },
                None => self.0.ready.wait(state).unwrap_or_else(|error| error.into_inner()),
            };
        }
    }
}

impl Job {
    fn payload_bytes(&self) -> usize {
        let payload = match &self.kind {
            JobKind::Complete { request, .. } => {
                // Count the shared environment conservatively for each queued
                // owner; it is not cloned for each completion.
                let strings = [
                    Some(&request.buffer),
                    Some(&request.cwd),
                    request.current_shell.as_ref(),
                    request.current_process.as_ref(),
                    request.alias.as_ref(),
                ];
                strings
                    .into_iter()
                    .flatten()
                    .map(String::capacity)
                    .chain(
                        request
                            .environment_variables
                            .iter()
                            .flat_map(|(key, value)| [key.capacity(), value.capacity()]),
                    )
                    .fold(
                        request
                            .environment_variables
                            .capacity()
                            .saturating_mul(std::mem::size_of::<(String, String)>()),
                        usize::saturating_add,
                    )
            },
            JobKind::RecordAcceptance {
                root_command,
                accepted_name,
                ..
            } => root_command.capacity().saturating_add(accepted_name.capacity()),
            JobKind::RecordScopedAcceptance {
                scope, accepted_name, ..
            } => scope.capacity().saturating_add(accepted_name.capacity()),
            #[cfg(test)]
            JobKind::InspectIdle { relative, .. } => relative.capacity(),
            _ => 0,
        };
        std::mem::size_of::<Self>().saturating_add(payload)
    }
}

enum JobKind {
    Complete {
        request: CompleteRequest,
        reply: oneshot::Sender<anyhow::Result<CompleteResult>>,
        token: CancellationToken,
        session: SessionId,
        sequence: u64,
    },
    RecordAcceptance {
        root_command: String,
        accepted_name: String,
        timestamp: u64,
    },
    RecordScopedAcceptance {
        scope: String,
        accepted_name: String,
        timestamp: u64,
    },
    /// `ftab hook clear-autocomplete-cache`: drop every cached spec and
    /// generator result before the next completion runs.
    ClearCaches,
    Diagnostics {
        reply: oneshot::Sender<EngineClientDiagnostics>,
    },
    EndInput {
        session: SessionId,
    },
    #[cfg(test)]
    InspectIdle {
        relative: String,
        reply: oneshot::Sender<IdleFileState>,
    },
}

#[cfg(test)]
struct IdleFileState {
    cached: bool,
    // Missing, active, and pending are distinct states under test.
    #[allow(clippy::option_option)]
    mark: Option<Option<Instant>>,
}

// A completion attempt can legitimately spend the legacy 5s script timeout
// inside a native generator. Keep the supervisor watchdog above that default
// (and above the 15s scriptTimeout used by several bundled specs) so the
// worker does not reset the engine while the generator is still within its
// configured budget.
const MIN_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Stack for the worker and every attempt thread. Catalog load now keeps
/// each descriptor as `RawValue` (no deep `from_slice` of the IR trees).
/// Attempt threads still walk those trees in `evaluate_inner`, including
/// `git#generateSpec`. macOS gives a secondary thread 512 KB by default;
/// the reservation is virtual and costs nothing unless touched, so both
/// builds keep the same headroom.
const ENGINE_THREAD_STACK: usize = 16 * 1024 * 1024;

/// Slack added to the user's script budget before the UI stops waiting, so a
/// generator that finishes right on its own deadline still gets rendered.
const UI_DEADLINE_MARGIN: Duration = Duration::from_secs(1);
/// Floor for the UI deadline, for the case where `scriptTimeout` is set to
/// something tiny. Below this the overlay would give up on completions the
/// engine was about to return anyway.
const MIN_UI_DEADLINE: Duration = Duration::from_secs(2);

/// Watchdog used by the normal engine client. A user script timeout above the
/// bundled-spec baseline extends the watchdog instead of being silently
/// truncated by it. Read this for every job so changing the setting does not
/// require restarting the desktop process.
pub fn engine_attempt_timeout() -> Duration {
    let configured_ms = fastab_settings::settings::get_int("autocomplete.scriptTimeout")
        .ok()
        .flatten()
        .unwrap_or(crate::generate::DEFAULT_SCRIPT_TIMEOUT_MS);
    engine_attempt_timeout_for(configured_ms)
}

fn engine_attempt_timeout_for(configured_ms: i64) -> Duration {
    let configured = Duration::from_millis(u64::try_from(configured_ms).unwrap_or(0));
    MIN_ATTEMPT_TIMEOUT.max(configured.saturating_add(Duration::from_secs(5)))
}

/// How long the overlay waits for a completion before giving up on it.
///
/// Deliberately not [`engine_attempt_timeout`]. That one is the supervisor's
/// "has this worker thread wedged" floor and sits at 30s so a spec with a long
/// `scriptTimeout` is never killed mid-run. Reusing it for the UI meant a
/// single stuck generator pinned the `···` marker on screen for half a minute.
/// What the user is actually waiting on is their own script budget.
pub fn ui_completion_deadline() -> Duration {
    ui_completion_deadline_for(crate::generate::configured_script_timeout_ms())
}

fn ui_completion_deadline_for(configured_ms: i64) -> Duration {
    let configured = Duration::from_millis(u64::try_from(configured_ms).unwrap_or(0));
    MIN_UI_DEADLINE.max(configured.saturating_add(UI_DEADLINE_MARGIN))
}

#[derive(Debug)]
enum AttemptFailure {
    TimedOut,
    Panicked,
}

#[derive(Default)]
struct OperationCounters(Mutex<OperationCounts>);

#[derive(Default)]
struct OperationCounts {
    active: usize,
    abandoned: usize,
}

#[derive(Default)]
struct OperationProgress {
    finished: bool,
    abandoned: bool,
}

struct OperationState {
    counters: Arc<OperationCounters>,
    progress: Mutex<OperationProgress>,
}

impl OperationState {
    fn abandon(&self) {
        let mut progress = self.progress.lock().unwrap_or_else(|error| error.into_inner());
        if !progress.finished && !progress.abandoned {
            progress.abandoned = true;
            self.counters
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .abandoned += 1;
        }
    }
}

struct OperationGuard(Arc<OperationState>);

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let mut progress = self.0.progress.lock().unwrap_or_else(|error| error.into_inner());
        progress.finished = true;
        let mut counters = self.0.counters.0.lock().unwrap_or_else(|error| error.into_inner());
        counters.active -= 1;
        if progress.abandoned {
            counters.abandoned -= 1;
        }
    }
}

#[cfg(test)]
type AttemptResult = Result<(Engine, anyhow::Result<CompleteResult>), AttemptFailure>;

impl EngineClient {
    pub fn spawn(specs_dir: PathBuf) -> anyhow::Result<Self> {
        Self::spawn_with_options(specs_dir, EngineClientOptions::default())
    }

    pub fn spawn_with_options(specs_dir: PathBuf, options: EngineClientOptions) -> anyhow::Result<Self> {
        Self::spawn_supervised(specs_dir, None, options)
    }

    #[cfg(test)]
    fn spawn_with_timeout(specs_dir: PathBuf, attempt_timeout: Duration) -> anyhow::Result<Self> {
        Self::spawn_supervised(specs_dir, Some(attempt_timeout), EngineClientOptions::default())
    }

    #[cfg(test)]
    fn spawn_with_idle_grace(
        specs_dir: PathBuf,
        attempt_timeout: Duration,
        idle_grace: Duration,
    ) -> anyhow::Result<Self> {
        Self::spawn_supervised(
            specs_dir,
            Some(attempt_timeout),
            EngineClientOptions {
                spec_idle_grace: idle_grace,
                ..EngineClientOptions::default()
            },
        )
    }

    fn spawn_supervised(
        specs_dir: PathBuf,
        fixed_attempt_timeout: Option<Duration>,
        options: EngineClientOptions,
    ) -> anyhow::Result<Self> {
        let EngineClientOptions {
            spec_idle_grace: idle_grace,
            history_idle_grace,
        } = options;
        let (tx, rx) = job_channel();
        let supervisor_specs_dir = specs_dir.clone();
        let acceptance = Arc::new(Mutex::new(AcceptanceIndex::load()));
        let worker_acceptance = acceptance.clone();
        let submission = Arc::new(Mutex::new(Submission::default()));
        let worker_submission = Arc::clone(&submission);
        let operations = Arc::new(OperationCounters::default());

        thread::Builder::new()
            .name("ec-engine".into())
            .stack_size(ENGINE_THREAD_STACK)
            .spawn(move || {
                // Initialize lazily for the first real request.  A malformed
                // or temporarily unavailable specs directory must not poison
                // this worker forever: the next request gets another chance
                // to build the registry after the caller repairs the input.
                let mut engine = None;
                // Resource ownership follows committed normal completions, not
                // submissions which may be cancelled before becoming active.
                let mut active_session = None;
                // Pristine index kept aside so recovering from a timed-out
                // attempt does not re-walk the specs directory. It is cloned,
                // never handed out, so a poisoned attempt cannot corrupt it.
                let mut registry_template: Option<Registry> = None;
                let mut jobs_without_attempt = 0;
                loop {
                    rx.set_resource_session(active_session);
                    let first = match wait_for_engine_job(&rx, engine.as_ref(), idle_grace, history_idle_grace) {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            // No queued completion. The attempt thread is not
                            // holding this engine, so the deadline may release.
                            if let Some(engine) = engine.as_mut() {
                                let now = Instant::now();
                                engine.release_idle_specs(now, idle_grace);
                                engine.release_idle_history(now, history_idle_grace);
                            }
                            jobs_without_attempt = 0;
                            continue;
                        },
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    maintain_idle_between_queued_jobs(
                        engine.as_mut(),
                        &first,
                        &mut jobs_without_attempt,
                        Instant::now(),
                        idle_grace,
                    );
                    // Submission cancels superseded tokens immediately, so the worker
                    // can consume FIFO without draining control messages across jobs.
                    let (request, reply, token, session, _sequence) = match first.kind {
                        JobKind::RecordAcceptance {
                            root_command,
                            accepted_name,
                            timestamp,
                        } => {
                            record_acceptance(
                                &mut engine,
                                &worker_acceptance,
                                &root_command,
                                &accepted_name,
                                timestamp,
                            );
                            continue;
                        },
                        JobKind::RecordScopedAcceptance {
                            scope,
                            accepted_name,
                            timestamp,
                        } => {
                            record_scoped_acceptance(
                                &mut engine,
                                &worker_acceptance,
                                &scope,
                                &accepted_name,
                                timestamp,
                            );
                            continue;
                        },
                        JobKind::ClearCaches => {
                            let mut current_engine = engine.take();
                            let mut template = registry_template.clone();
                            let specs_dir = supervisor_specs_dir.clone();
                            let timeout = fixed_attempt_timeout.unwrap_or_else(engine_attempt_timeout);
                            match run_supervised(timeout, Arc::clone(&operations), move || {
                                let cleared = clear_caches(&specs_dir, &mut current_engine, &mut template);
                                (current_engine, template, cleared)
                            }) {
                                Ok((next_engine, next_template, cleared)) => {
                                    engine = next_engine;
                                    registry_template = next_template;
                                    if cleared {
                                        active_session = None;
                                    }
                                },
                                Err(failure) => {
                                    active_session = None;
                                    tracing::error!(?failure, "engine cache refresh abandoned; engine reset");
                                    record_attempt_failure(&worker_submission, &failure, false);
                                },
                            }
                            continue;
                        },
                        JobKind::EndInput { session } => {
                            if active_session == Some(session) {
                                if let Some(engine) = engine.as_mut() {
                                    engine.end_input();
                                }
                                active_session = None;
                            }
                            continue;
                        },
                        JobKind::Diagnostics { reply } => {
                            // A timed-out IPC caller no longer needs this queued
                            // snapshot. Do not walk caches or lock counters for it.
                            if reply.is_canceled() {
                                continue;
                            }
                            let snapshot = EngineClientDiagnostics {
                                engine: engine.as_ref().map(|engine| engine.diagnostics_with_grace(idle_grace)),
                                worker: rx.diagnostics(&operations),
                                host: None,
                                requests: worker_submission
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .requests,
                            };
                            let _ = reply.send(snapshot);
                            continue;
                        },
                        #[cfg(test)]
                        JobKind::InspectIdle { relative, reply } => {
                            let _ = reply.send(idle_file_state(engine.as_ref(), &relative));
                            continue;
                        },
                        JobKind::Complete {
                            request,
                            reply,
                            token,
                            session,
                            sequence,
                        } => (request, reply, token, session, sequence),
                    };
                    if token.is_cancelled() || reply.is_canceled() {
                        token.cancel();
                        update_requests(&worker_submission, |counts| {
                            counts.cancelled = counts.cancelled.saturating_add(1);
                        });
                        let _ = reply.send(Err(CompletionCancelled.into()));
                        continue;
                    }
                    let current_engine = engine.take();
                    let mut template = if current_engine.is_none() {
                        registry_template.clone()
                    } else {
                        None
                    };
                    let specs_dir = supervisor_specs_dir.clone();
                    let acceptance = Arc::clone(&worker_acceptance);
                    let history_only = request.history_only;
                    let attempt_timeout = fixed_attempt_timeout.unwrap_or_else(engine_attempt_timeout);
                    let attempt_context = attempt_log_context(&request);
                    jobs_without_attempt = 0;
                    update_requests(&worker_submission, |counts| {
                        counts.started = counts.started.saturating_add(1);
                    });
                    let attempt_token = token.clone();
                    // Preserve a verified registry even if native-hook loading
                    // or the first completion never returns. This channel is
                    // private to this attempt: dropping its receiver below
                    // prevents a late abandoned attempt from publishing over
                    // a newer generation.
                    let (checkpoint, checkpoints) = mpsc::sync_channel(1);
                    // Registry capture and refresh may block in a filesystem
                    // call too. They belong inside the same watchdog boundary
                    // as generators, never on the supervisor itself.
                    let panic_context = attempt_context.clone();
                    let outcome = run_supervised(attempt_timeout, Arc::clone(&operations), move || {
                        // Each attempt has a thread of its own, so this needs
                        // no clearing.
                        ATTEMPT_CONTEXT.with(|context| *context.borrow_mut() = Some(panic_context));
                        let _scope = crate::cancellation::enter(attempt_token);
                        let initialized = current_engine.is_none();
                        let built = match current_engine {
                            Some(engine) => Ok(engine),
                            None => rebuild_engine(&specs_dir, &mut template, &acceptance, |registry| {
                                let _ = checkpoint.send(registry);
                            }),
                        };
                        match built {
                            Ok(mut engine) => {
                                let result = engine.complete(request);
                                (Some(engine), template, initialized, result)
                            },
                            Err(error) => (
                                None,
                                template,
                                false,
                                Err(anyhow!("completion engine initialization failed: {error}")),
                            ),
                        }
                    });
                    if let Ok(checkpoint) = checkpoints.try_recv() {
                        registry_template = Some(checkpoint);
                    }
                    drop(checkpoints);
                    match outcome {
                        Ok((mut next_engine, next_template, initialized, result)) => {
                            if result.is_ok() && !history_only {
                                active_session = Some(session);
                            }
                            // `complete` already stamped idle marks. A mark from
                            // this attempt is younger than the grace, so this
                            // only releases files whose grace elapsed while the
                            // attempt held the engine.
                            if let Some(engine) = next_engine.as_mut() {
                                engine.release_idle_specs(Instant::now(), idle_grace);
                            }
                            engine = next_engine;
                            if next_template.is_some() {
                                registry_template = next_template;
                            }
                            update_requests(&worker_submission, |counts| {
                                if initialized {
                                    counts.engine_initializations = counts.engine_initializations.saturating_add(1);
                                }
                                if result.as_ref().is_err_and(|error| error.is::<CompletionCancelled>()) {
                                    counts.cancelled = counts.cancelled.saturating_add(1);
                                } else if result.is_ok() {
                                    counts.completed = counts.completed.saturating_add(1);
                                } else {
                                    counts.failed = counts.failed.saturating_add(1);
                                }
                            });
                            let _ = reply.send(result);
                        },
                        Err(failure) => {
                            token.cancel();
                            record_attempt_failure(&worker_submission, &failure, true);
                            // The default log filter is ERROR, so this is the
                            // only durable evidence of a wedged or crashed
                            // generator. Keep it at that level.
                            tracing::error!(
                                timeout_ms = attempt_timeout.as_millis() as u64,
                                context = %attempt_context,
                                failure = ?failure,
                                "completion attempt abandoned; engine reset"
                            );
                            let error = failure.error(attempt_timeout);
                            let _ = reply.send(Err(error));
                            // The timed-out/panicked attempt owns the old
                            // engine and may still be unwinding in its
                            // detached thread.  Drop it from the supervisor;
                            // the next request will retry initialization and
                            // can recover if the specs directory was repaired.
                            engine = None;
                            active_session = None;
                        },
                    }
                }
            })
            .map_err(|err| anyhow!("spawn engine thread: {err}"))?;

        Ok(Self {
            tx,
            acceptance,
            submission,
        })
    }

    pub fn complete(&self, request: CompleteRequest) -> CompletionTask {
        self.complete_for_session(SessionId::ANONYMOUS, request)
    }

    /// Submits synchronously: a later call wins even if its future is polled first.
    pub fn complete_for_session(&self, session: SessionId, request: CompleteRequest) -> CompletionTask {
        let (reply, receiver) = oneshot::channel();
        let token = CancellationToken::new();
        let mut submission = self.submission.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(previous) = &submission.latest {
            previous.cancel();
        }
        submission.sequence = submission.sequence.wrapping_add(1);
        submission.owner = Some(session);
        submission.latest = Some(token.clone());
        submission.requests.submitted = submission.requests.submitted.saturating_add(1);
        let sequence = submission.sequence;
        let sent = self.tx.send(Job {
            kind: JobKind::Complete {
                request,
                reply,
                token: token.clone(),
                session,
                sequence,
            },
        });
        match sent {
            Ok(removed) => submission.requests.cancelled = submission.requests.cancelled.saturating_add(removed as u64),
            Err(error) => {
                submission.requests.cancelled = submission.requests.cancelled.saturating_add(error.2 as u64);
                submission.requests.failed = submission.requests.failed.saturating_add(1);
                if let JobKind::Complete { reply, .. } = error.0.kind {
                    let _ = reply.send(Err(anyhow!(error.1)));
                }
            },
        }
        CompletionTask { receiver, token }
    }

    /// Use from CLI/tests only; the desktop polls CompletionTask on GPUI.
    pub fn complete_blocking(&self, request: CompleteRequest) -> anyhow::Result<CompleteResult> {
        futures::executor::block_on(self.complete(request))
    }

    /// End a session without cancelling another session's submitted work.
    /// The worker also checks the owner of committed resources: a newer
    /// cancelled submission must not swallow the previous owner's end.
    pub fn end_input(&self, session: SessionId) -> anyhow::Result<()> {
        let mut submission = self.submission.lock().unwrap_or_else(|error| error.into_inner());
        if submission.owner == Some(session) {
            if let Some(token) = submission.latest.take() {
                token.cancel();
            }
            submission.owner = None;
        }
        submission.sequence = submission.sequence.wrapping_add(1);
        // The mailbox keeps retained notifications in FIFO order and reserves
        // capacity for the committed owner and the currently running attempt.
        send_with_counters(&self.tx, &mut submission, JobKind::EndInput { session })
    }

    /// An ordered, read-only snapshot. Does not initialize or interrupt Engine.
    pub fn diagnostics(&self) -> impl Future<Output = anyhow::Result<EngineClientDiagnostics>> + Send + 'static {
        let (reply, receiver) = oneshot::channel();
        let sent = self.send_control(JobKind::Diagnostics { reply });
        async move {
            sent?;
            receiver
                .await
                .map_err(|_cancelled| anyhow!("engine dropped diagnostics reply"))
        }
    }

    fn send_control(&self, kind: JobKind) -> anyhow::Result<()> {
        let mut submission = self.submission.lock().unwrap_or_else(|error| error.into_inner());
        send_with_counters(&self.tx, &mut submission, kind)
    }

    /// Queue a successful acceptance without participating in completion
    /// latest-job cancellation. Sending is bounded and returns immediately;
    /// the worker applies the record between completion attempts and persists
    /// it best-effort. This keeps shell/UI acceptance independent of a slow or
    /// timed-out generator.
    pub fn record_acceptance(
        &self,
        root_command: impl Into<String>,
        accepted_name: impl Into<String>,
    ) -> anyhow::Result<()> {
        let root_command = root_command.into();
        let accepted_name = accepted_name.into();
        let timestamp = AcceptanceIndex::now_millis();
        {
            let mut acceptance = self.acceptance.lock().unwrap_or_else(|err| err.into_inner());
            let _ = acceptance.record_at(&root_command, &accepted_name, timestamp);
        }
        self.send_control(JobKind::RecordAcceptance {
            root_command,
            accepted_name,
            timestamp,
        })
    }

    /// Update in-memory scoped ranking immediately; persistence is replayed
    /// only by the worker so the GPUI insertion path never waits on SQLite.
    pub fn record_scoped_acceptance(
        &self,
        scope: impl Into<String>,
        accepted_name: impl Into<String>,
    ) -> anyhow::Result<()> {
        let scope = scope.into();
        let accepted_name = accepted_name.into();
        let timestamp = AcceptanceIndex::now_millis();
        let valid = self
            .acceptance
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .record_scoped_at(&scope, &accepted_name, timestamp);
        if !valid {
            return Ok(());
        }
        self.send_control(JobKind::RecordScopedAcceptance {
            scope,
            accepted_name,
            timestamp,
        })
    }

    /// Forget every cached spec and generator result. Applied on the worker
    /// between completions, like an acceptance record, so it is never
    /// coalesced away by a newer completion request.
    pub fn clear_caches(&self) -> anyhow::Result<()> {
        self.send_control(JobKind::ClearCaches)
    }

    #[cfg(test)]
    fn inspect_idle(&self, relative: &str) -> anyhow::Result<IdleFileState> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job {
                kind: JobKind::InspectIdle {
                    relative: relative.to_string(),
                    reply,
                },
            })
            .map_err(|_err| anyhow!("engine thread is gone"))?;
        futures::executor::block_on(rx).map_err(|_err| anyhow!("engine dropped the reply"))
    }
}

fn update_requests(submission: &Mutex<Submission>, update: impl FnOnce(&mut RequestDiagnostics)) {
    update(&mut submission.lock().unwrap_or_else(|error| error.into_inner()).requests);
}

fn send_with_counters(tx: &JobSender, submission: &mut Submission, kind: JobKind) -> anyhow::Result<()> {
    match tx.send(Job { kind }) {
        Ok(removed) => {
            submission.requests.cancelled = submission.requests.cancelled.saturating_add(removed as u64);
            Ok(())
        },
        Err(error) => {
            submission.requests.cancelled = submission.requests.cancelled.saturating_add(error.2 as u64);
            Err(anyhow!(error.1))
        },
    }
}

fn record_attempt_failure(submission: &Mutex<Submission>, failure: &AttemptFailure, completion: bool) {
    update_requests(submission, |counts| {
        if completion {
            counts.failed = counts.failed.saturating_add(1);
        }
        match failure {
            AttemptFailure::TimedOut => counts.watchdog_timeouts = counts.watchdog_timeouts.saturating_add(1),
            AttemptFailure::Panicked => counts.panics = counts.panics.saturating_add(1),
        }
    });
}

fn clear_caches(specs_dir: &Path, engine: &mut Option<Engine>, registry_template: &mut Option<Registry>) -> bool {
    #[cfg(test)]
    tests::pause_filesystem_if_requested(specs_dir);
    if let Some(engine) = engine.as_mut() {
        // Keep the old snapshot/template when the canonical directory is
        // temporarily absent during publication. A failed reset must not
        // turn a valid generation into an empty/stale worker state.
        if engine.clear_caches_and_report() {
            *registry_template = None;
            return true;
        }
        return false;
    }

    // A timed-out attempt may have left only the pristine template. Refresh
    // it when the new generation is available, but retain the last good one
    // across a missing-window so the next completion can still use it.
    if let Ok(registry) = Engine::load_registry(specs_dir) {
        *registry_template = Some(registry);
    }
    false
}

impl AttemptFailure {
    fn error(self, timeout: Duration) -> anyhow::Error {
        match self {
            Self::TimedOut => anyhow!(
                "completion attempt timed out after {}ms; engine reset",
                timeout.as_millis()
            ),
            Self::Panicked => anyhow!("completion attempt panicked; engine reset"),
        }
    }
}

/// Run one completion in an isolated thread.  The engine is returned with the
/// result so successful attempts retain the registry/frecency caches.  If the
/// attempt gets stuck, its thread (and the engine it owns) is intentionally
/// abandoned; the supervisor can then rebuild a fresh engine and accept the
/// next request.
#[cfg(test)]
fn run_engine_attempt(engine: Engine, request: CompleteRequest, timeout: Duration) -> AttemptResult {
    run_engine_attempt_with_token(engine, request, timeout, CancellationToken::new())
}

#[cfg(test)]
fn run_engine_attempt_with_token(
    engine: Engine,
    request: CompleteRequest,
    timeout: Duration,
    token: CancellationToken,
) -> AttemptResult {
    run_attempt(engine, request, timeout, move |mut engine, request| {
        let _scope = crate::cancellation::enter(token);
        let result = engine.complete(request);
        (engine, result)
    })
}

#[cfg(test)]
fn run_attempt<F>(engine: Engine, request: CompleteRequest, timeout: Duration, complete: F) -> AttemptResult
where
    F: FnOnce(Engine, CompleteRequest) -> (Engine, anyhow::Result<CompleteResult>) + Send + 'static,
{
    run_supervised(timeout, Arc::new(OperationCounters::default()), move || {
        complete(engine, request)
    })
}

fn run_supervised<T: Send + 'static>(
    timeout: Duration,
    counters: Arc<OperationCounters>,
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, AttemptFailure> {
    let (tx, rx) = mpsc::sync_channel(1);
    counters.0.lock().unwrap_or_else(|error| error.into_inner()).active += 1;
    let state = Arc::new(OperationState {
        counters,
        progress: Mutex::new(OperationProgress::default()),
    });
    let guard = OperationGuard(Arc::clone(&state));
    let spawn_result = thread::Builder::new()
        .name("ec-engine-attempt".into())
        .stack_size(ENGINE_THREAD_STACK)
        .spawn(move || {
            let _guard = guard;
            // Catches a panic only where panics unwind (dev, test, release).
            // The shipped `dist` profile aborts: there a panic in an attempt
            // ends the process, and the host's panic hook is what records it
            // (`attempt_context_for_panic`).
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation));
            let message = match outcome {
                Ok(result) => Ok(result),
                Err(_) => Err(()),
            };
            let _ = tx.send(message);
        });
    if spawn_result.is_err() {
        return Err(AttemptFailure::Panicked);
    }

    match rx.recv_timeout(timeout) {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(())) | Err(mpsc::RecvTimeoutError::Disconnected) => Err(AttemptFailure::Panicked),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            state.abandon();
            Err(AttemptFailure::TimedOut)
        },
    }
}

/// Root command plus cwd, enough to identify which spec's generator wedged
/// without copying the whole edit buffer into the log.
fn attempt_log_context(request: &CompleteRequest) -> String {
    let root = request.buffer.split_whitespace().next().unwrap_or("");
    format!("root_command={root:?} cwd={:?}", request.cwd)
}

thread_local! {
    /// What the completion attempt running on this thread is completing.
    static ATTEMPT_CONTEXT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The root command and working directory of the completion attempt running
/// on the calling thread, for the host's panic hook.
///
/// `run_supervised` catches a panicking attempt only in a build that unwinds.
/// The shipped `dist` profile aborts instead, so there the supervisor never
/// gets to log which command it was: the process is gone. A panic hook still
/// runs first, on the panicking thread, and this is what it can say.
pub fn attempt_context_for_panic() -> Option<String> {
    ATTEMPT_CONTEXT
        .try_with(|context| context.try_borrow().ok().and_then(|context| context.clone()))
        .ok()
        .flatten()
}

fn record_acceptance(
    engine: &mut Option<Engine>,
    acceptance: &Arc<Mutex<AcceptanceIndex>>,
    root_command: &str,
    accepted_name: &str,
    timestamp: u64,
) {
    if let Some(engine) = engine.as_mut() {
        engine.record_acceptance_at(root_command, accepted_name, timestamp);
    } else {
        // As in `Engine::record_acceptance_at`: persist a snapshot outside
        // the lock so a slow SQLite write cannot stall threads cloning the
        // index for ranking. This runs on the supervisor thread, where any
        // stall also delays every queued completion.
        let snapshot = {
            let mut index = acceptance.lock().unwrap_or_else(|err| err.into_inner());
            index
                .record_at(root_command, accepted_name, timestamp)
                .then(|| index.clone())
        };
        if let Some(snapshot) = snapshot {
            snapshot.persist();
        }
    }
}

fn record_scoped_acceptance(
    engine: &mut Option<Engine>,
    acceptance: &Arc<Mutex<AcceptanceIndex>>,
    scope: &str,
    accepted_name: &str,
    timestamp: u64,
) {
    if let Some(engine) = engine.as_mut() {
        engine.record_scoped_acceptance_at(scope, accepted_name, timestamp);
    } else {
        let snapshot = {
            let mut index = acceptance.lock().unwrap_or_else(|err| err.into_inner());
            index
                .record_scoped_at(scope, accepted_name, timestamp)
                .then(|| index.scoped_snapshot())
        };
        if let Some(snapshot) = snapshot {
            snapshot.persist();
        }
    }
}

/// Build an engine from the last known generation when it is still current;
/// refresh a stale template before handing it to a replacement attempt.
///
/// A timed-out attempt keeps the engine it was given, so the supervisor has to
/// construct a fresh one. Re-reading the index on every reset made the first
/// completion after a wedged generator slow enough to show the loading marker
/// again, which reads to the user as the overlay never recovering.
fn rebuild_engine(
    specs_dir: &Path,
    template: &mut Option<Registry>,
    acceptance: &Arc<Mutex<AcceptanceIndex>>,
    checkpoint: impl FnOnce(Registry),
) -> anyhow::Result<Engine> {
    #[cfg(test)]
    tests::pause_filesystem_if_requested(specs_dir);
    let registry = match template {
        Some(registry) if !registry.needs_refresh() => registry.clone(),
        Some(registry) => {
            // A successful generation replacement must not leave a timed-out
            // worker rebuilding from the old template. If the publisher is
            // in its brief missing window, keep that last good template as a
            // fallback; Engine::complete will retry the atomic rebind at the
            // next request boundary.
            let previous = registry.clone();
            match Engine::load_registry(specs_dir) {
                Ok(registry) => {
                    *template = Some(registry.clone());
                    registry
                },
                Err(error) => {
                    tracing::debug!(%error, "engine rebuild deferred while retaining the last registry template");
                    previous
                },
            }
        },
        None => {
            let registry = Engine::load_registry(specs_dir)?;
            template.get_or_insert(registry).clone()
        },
    };
    // Publish only the pristine index before handing a separate clone to the
    // mutable Engine. Catalog parsing is deferred to the first hook dispatch;
    // constructor and completion work still share the watchdog boundary.
    #[cfg(test)]
    tests::pause_initialization_if_requested(specs_dir, tests::InitializationStage::Checkpoint);
    checkpoint(registry.clone());
    #[cfg(test)]
    tests::pause_initialization_if_requested(specs_dir, tests::InitializationStage::Construction);
    Ok(Engine::from_registry(specs_dir, registry, acceptance.clone()))
}

// Control traffic can keep recv_timeout ready forever. Give expired resources a
// bounded maintenance opportunity without moving a control across a request.
const MAX_JOBS_WITHOUT_ATTEMPT: usize = 64;

fn maintain_idle_between_queued_jobs(
    engine: Option<&mut Engine>,
    head: &Job,
    jobs_without_attempt: &mut usize,
    now: Instant,
    grace: Duration,
) {
    *jobs_without_attempt = jobs_without_attempt.saturating_add(1);
    let live_completion = matches!(&head.kind, JobKind::Complete { token, reply, .. }
        if !token.is_cancelled() && !reply.is_canceled());
    if *jobs_without_attempt < MAX_JOBS_WITHOUT_ATTEMPT || live_completion {
        return;
    }
    if let Some(engine) = engine {
        engine.release_idle_specs(now, grace);
    }
    *jobs_without_attempt = 0;
}

/// Block until the next job, or until the earliest idle deadline.
/// A job already queued is returned without waiting out the grace.
fn wait_for_engine_job(
    rx: &JobReceiver,
    engine: Option<&Engine>,
    grace: Duration,
    history_grace: Duration,
) -> Result<Job, mpsc::RecvTimeoutError> {
    let deadline = engine.and_then(|engine| {
        engine
            .next_idle_deadline(grace)
            .into_iter()
            .chain(engine.next_history_deadline(history_grace))
            .min()
    });
    let Some(deadline) = deadline else {
        return rx.recv().map_err(|_disconnected| mpsc::RecvTimeoutError::Disconnected);
    };
    let timeout = deadline.saturating_duration_since(Instant::now());
    rx.recv_timeout(timeout)
}

#[cfg(test)]
fn idle_file_state(engine: Option<&Engine>, relative: &str) -> IdleFileState {
    match engine {
        Some(engine) => {
            let (cached, mark) = engine.idle_file_state(relative);
            IdleFileState { cached, mark }
        },
        None => IdleFileState {
            cached: false,
            mark: None,
        },
    }
}

pub fn default_specs_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("EC_SPECS_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = bundled_specs_ir_dir() {
        return dir;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bundle/specs-ir")
}

fn bundled_specs_ir_dir() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    match exe.parent().filter(|dir| dir.ends_with("MacOS")) {
        Some(macos) => Ok(specs_ir_in_bundle(macos)),
        None => anyhow::bail!("not running from an app bundle"),
    }
}

/// `Fastab.app/Contents/MacOS` → `Contents/Resources/specs-ir`. Only the
/// compiled IR is bundled; the JS specs it was built from never enter the `.app`,
/// so pointing at `Resources/specs` would silently yield an empty registry.
fn specs_ir_in_bundle(macos_dir: &Path) -> PathBuf {
    macos_dir.join("../Resources/specs-ir")
}

#[cfg(test)]
mod tests {
    use super::*;

    type FilesystemPause = (mpsc::Sender<()>, mpsc::Receiver<()>);
    static FILESYSTEM_PAUSES: std::sync::OnceLock<Mutex<std::collections::HashMap<PathBuf, FilesystemPause>>> =
        std::sync::OnceLock::new();
    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    pub(super) enum InitializationStage {
        Checkpoint,
        Construction,
    }

    type InitializationPauses = std::collections::HashMap<(PathBuf, InitializationStage), FilesystemPause>;
    static INITIALIZATION_PAUSES: std::sync::OnceLock<Mutex<InitializationPauses>> = std::sync::OnceLock::new();

    pub(super) fn pause_initialization_if_requested(path: &Path, stage: InitializationStage) {
        let pause = INITIALIZATION_PAUSES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(&(path.to_path_buf(), stage));
        if let Some((entered, release)) = pause {
            let _ = entered.send(());
            let _ = release.recv();
        }
    }

    fn pause_next_initialization(path: &Path, stage: InitializationStage) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, observed) = mpsc::channel();
        let (release, resumed) = mpsc::channel();
        INITIALIZATION_PAUSES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert((path.to_path_buf(), stage), (entered, resumed));
        (observed, release)
    }

    pub(super) fn pause_filesystem_if_requested(path: &Path) {
        let pause = FILESYSTEM_PAUSES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(path);
        if let Some((entered, release)) = pause {
            let _ = entered.send(());
            let _ = release.recv();
        }
    }

    fn pause_next_filesystem_operation(path: &Path) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, observed) = mpsc::channel();
        let (release, resumed) = mpsc::channel();
        FILESYSTEM_PAUSES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), (entered, resumed));
        (observed, release)
    }

    #[test]
    fn blocked_initialization_is_supervised_and_submissions_coalesce_around_controls() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        let (entered, release) = pause_next_filesystem_operation(dir.path());
        let client = EngineClient::spawn_with_timeout(dir.path().to_path_buf(), Duration::from_secs(2)).unwrap();
        let first = client.complete(CompleteRequest::default());
        entered.recv_timeout(Duration::from_secs(3)).unwrap();

        let mut latest = None;
        for index in 0..1000 {
            latest = Some(client.complete(CompleteRequest {
                buffer: "x".repeat(32 * 1024),
                include_history: false,
                ..CompleteRequest::default()
            }));
            if index == 250 {
                client.end_input(SessionId::ANONYMOUS).unwrap();
            }
            if index == 500 {
                client.clear_caches().unwrap();
            }
        }
        {
            let state = client.tx.0.state.lock().unwrap();
            assert_eq!(state.jobs.len(), 3);
            assert!(state.bytes < 256 * 1024);
            assert!(matches!(state.jobs[0].0.kind, JobKind::EndInput { .. }));
            assert!(matches!(state.jobs[1].0.kind, JobKind::ClearCaches));
            assert!(matches!(state.jobs[2].0.kind, JobKind::Complete { .. }));
        }
        drop(latest);
        let (done, result) = mpsc::channel();
        let waiter = thread::spawn(move || {
            let _ = done.send(futures::executor::block_on(first));
        });
        let observed = result.recv_timeout(Duration::from_secs(5));
        // Release even on regression: a missing watchdog must fail rather
        // than leaving the fixture's engine thread permanently blocked.
        release.send(()).unwrap();
        waiter.join().unwrap();
        assert!(observed.unwrap().unwrap_err().to_string().contains("timed out"));
        let recovered = client
            .complete_blocking(CompleteRequest {
                buffer: "git ".into(),
                cwd: "/tmp".into(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .unwrap();
        assert!(
            recovered
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "status")
        );
        let diagnostics = futures::executor::block_on(client.diagnostics()).unwrap();
        assert_eq!(diagnostics.requests.watchdog_timeouts, 1);
        assert!(diagnostics.requests.cancelled >= 999);
    }

    #[test]
    fn blocked_clear_caches_times_out_and_the_following_diagnostic_progresses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("git.json"), r#"{"names":["git"]}"#).unwrap();
        let client = EngineClient::spawn_with_timeout(dir.path().to_path_buf(), Duration::from_millis(500)).unwrap();
        client
            .complete_blocking(CompleteRequest {
                include_history: false,
                ..CompleteRequest::default()
            })
            .unwrap();
        let (entered, release) = pause_next_filesystem_operation(dir.path());
        client.clear_caches().unwrap();
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let diagnostic = client.diagnostics();
        let (done, result) = mpsc::channel();
        let waiter = thread::spawn(move || {
            let _ = done.send(futures::executor::block_on(diagnostic));
        });
        let observed = result.recv_timeout(Duration::from_secs(3));
        release.send(()).unwrap();
        waiter.join().unwrap();
        let snapshot = observed.unwrap().unwrap();
        assert_eq!(snapshot.requests.watchdog_timeouts, 1);
        assert_eq!(
            snapshot.requests.failed, 0,
            "control failures are not failed completions"
        );
        assert!(snapshot.engine.is_none());
        assert_eq!(snapshot.worker.active_operations, 1);
        assert_eq!(snapshot.worker.abandoned_operations, 1);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = futures::executor::block_on(client.diagnostics()).unwrap();
            if snapshot.worker.active_operations == 0 {
                assert_eq!(snapshot.worker.abandoned_operations, 0);
                break;
            }
            assert!(Instant::now() < deadline, "released operation must retire its counters");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(unix)]
    #[test]
    fn fifo_initialization_and_clear_caches_recover_after_removal() {
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        let fifo_path = dir.path().join("unrelated-file");
        let c_path = std::ffi::CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
        // SAFETY: a new fixture FIFO with a valid, terminated pathname.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let client = EngineClient::spawn_with_timeout(dir.path().to_path_buf(), Duration::from_secs(2)).unwrap();
        let request = CompleteRequest {
            buffer: "git ".into(),
            cwd: "/tmp".into(),
            include_history: false,
            ..CompleteRequest::default()
        };
        let error = client.complete_blocking(request.clone()).unwrap_err();
        assert!(error.to_string().contains("initialization failed"));
        std::fs::remove_file(&fifo_path).unwrap();
        assert!(
            client
                .complete_blocking(request.clone())
                .unwrap()
                .suggestions
                .iter()
                .any(|row| row.name == "status")
        );
        // A failed refresh preserves the previous usable generation and lets
        // the next ordered control run, even with no writer on this FIFO.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        client.clear_caches().unwrap();
        let snapshot = futures::executor::block_on(client.diagnostics()).unwrap();
        assert!(snapshot.engine.is_some());
        assert_eq!(snapshot.requests.watchdog_timeouts, 0);
        std::fs::remove_file(&fifo_path).unwrap();
        client.clear_caches().unwrap();
        assert!(
            client
                .complete_blocking(request)
                .unwrap()
                .suggestions
                .iter()
                .any(|row| row.name == "status")
        );
    }

    #[test]
    fn queued_controls_and_request_allocations_have_explicit_budgets() {
        let (tx, rx) = job_channel();
        for _ in 0..MAX_QUEUED_JOBS - RESERVED_END_INPUT_JOBS {
            tx.send(Job {
                kind: JobKind::ClearCaches,
            })
            .unwrap();
        }
        assert_eq!(
            tx.send(Job {
                kind: JobKind::ClearCaches
            })
            .unwrap_err()
            .1,
            "engine request queue capacity exceeded"
        );
        for _ in 0..MAX_QUEUED_JOBS - RESERVED_END_INPUT_JOBS {
            rx.recv().unwrap();
        }
        let (reply, _receiver) = oneshot::channel();
        let request = CompleteRequest {
            buffer: String::with_capacity(MAX_QUEUED_BYTES),
            ..CompleteRequest::default()
        };
        assert_eq!(
            tx.send(completion_job(request, reply)).unwrap_err().1,
            "engine request queue capacity exceeded"
        );
        assert_eq!(tx.0.state.lock().unwrap().bytes, 0);
    }

    #[test]
    fn saturated_mailbox_reserves_ordered_end_notifications_for_both_possible_owners() {
        let (tx, rx) = job_channel();
        let client = EngineClient {
            tx,
            acceptance: Arc::new(Mutex::new(AcceptanceIndex::default())),
            submission: Arc::new(Mutex::new(Submission::default())),
        };
        let committed = SessionId::new(1);
        let running = SessionId::new(2);
        rx.set_resource_session(Some(committed));
        let task = client.complete_for_session(running, CompleteRequest::default());
        let _attempt = rx.recv().unwrap();
        for _ in 0..MAX_QUEUED_JOBS - RESERVED_END_INPUT_JOBS {
            client.clear_caches().unwrap();
        }
        assert!(client.clear_caches().is_err());
        // Unrelated and duplicate ends cannot exhaust the reserved capacity.
        for session in 3..1000 {
            client.end_input(SessionId::new(session)).unwrap();
            client.end_input(committed).unwrap();
        }
        client.end_input(running).unwrap();
        assert!(task.token.is_cancelled());
        let counts = rx.diagnostics(&OperationCounters::default());
        assert_eq!(counts.queued_jobs, MAX_QUEUED_JOBS);
        assert!(counts.queued_payload_bytes <= MAX_QUEUED_BYTES);
        for _ in 0..MAX_QUEUED_JOBS - RESERVED_END_INPUT_JOBS {
            assert!(matches!(rx.recv().unwrap().kind, JobKind::ClearCaches));
        }
        assert!(matches!(rx.recv().unwrap().kind, JobKind::EndInput { session } if session == committed));
        assert!(matches!(rx.recv().unwrap().kind, JobKind::EndInput { session } if session == running));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn end_notifications_keep_their_byte_reservation_when_payload_budget_is_full() {
        let (tx, rx) = job_channel();
        rx.set_resource_session(Some(SessionId::new(1)));
        let payload = MAX_QUEUED_BYTES - (RESERVED_END_INPUT_JOBS + 1) * std::mem::size_of::<Job>();
        tx.send(Job {
            kind: JobKind::RecordAcceptance {
                root_command: String::with_capacity(payload),
                accepted_name: String::new(),
                timestamp: 0,
            },
        })
        .unwrap();
        assert!(
            tx.send(Job {
                kind: JobKind::ClearCaches
            })
            .is_err()
        );
        tx.send(Job {
            kind: JobKind::EndInput {
                session: SessionId::new(1),
            },
        })
        .unwrap();
        assert!(rx.diagnostics(&OperationCounters::default()).queued_payload_bytes <= MAX_QUEUED_BYTES);
        assert!(matches!(rx.recv().unwrap().kind, JobKind::RecordAcceptance { .. }));
        assert!(matches!(rx.recv().unwrap().kind, JobKind::EndInput { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn saturated_controls_cannot_swallow_the_committed_owners_end() {
        let dir = tempfile::tempdir().unwrap();
        slow_fixture(dir.path());
        let client = EngineClient::spawn(dir.path().to_path_buf()).unwrap();
        futures::executor::block_on(
            client.complete_for_session(SessionId::new(1), fixture_request(dir.path(), "fast ")),
        )
        .unwrap();
        let running = client.complete_for_session(SessionId::new(2), fixture_request(dir.path(), "slow "));
        await_fixture_start(dir.path());
        let diagnostics: Vec<_> = (0..MAX_QUEUED_JOBS - RESERVED_END_INPUT_JOBS)
            .map(|_| client.diagnostics())
            .collect();
        assert!(client.clear_caches().is_err());
        client.end_input(SessionId::new(1)).unwrap();
        drop(running);
        for diagnostic in diagnostics {
            futures::executor::block_on(diagnostic).unwrap();
        }
        let state = client.inspect_idle("fast").unwrap();
        assert!(
            state.mark.is_none_or(|mark| mark.is_some()),
            "the committed owner's end must reach the engine"
        );
        let snapshot = futures::executor::block_on(client.diagnostics()).unwrap();
        assert_eq!(snapshot.requests.submitted, 2);
        assert_eq!(snapshot.requests.completed, 1);
        assert_eq!(snapshot.requests.cancelled, 1);
        assert_eq!(snapshot.requests.failed, 0);
    }

    fn completion_job(request: CompleteRequest, reply: oneshot::Sender<anyhow::Result<CompleteResult>>) -> Job {
        Job {
            kind: JobKind::Complete {
                request,
                reply,
                token: CancellationToken::new(),
                session: SessionId::ANONYMOUS,
                sequence: 0,
            },
        }
    }

    #[test]
    fn the_ui_deadline_tracks_the_script_budget_not_the_supervisor_floor() {
        // The supervisor waits 30s before declaring a worker wedged. Showing
        // `···` for that long reads as a hang, so the overlay follows the
        // user's own script budget instead.
        assert_eq!(ui_completion_deadline_for(5_000), Duration::from_secs(6));
        assert!(ui_completion_deadline_for(5_000) < engine_attempt_timeout_for(5_000));
    }

    #[test]
    fn a_long_script_budget_extends_the_ui_deadline() {
        assert_eq!(ui_completion_deadline_for(15_000), Duration::from_secs(16));
    }

    #[test]
    fn a_tiny_script_budget_still_leaves_the_engine_time_to_answer() {
        assert_eq!(ui_completion_deadline_for(0), MIN_UI_DEADLINE);
        assert_eq!(ui_completion_deadline_for(-1), MIN_UI_DEADLINE);
    }

    #[test]
    fn rebuilding_the_engine_indexes_the_specs_directory_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("git.json"), r#"{"names":["git"]}"#).unwrap();
        std::fs::write(dir.path().join("index.json"), r#"{"files":{"git":"git.json"}}"#).unwrap();
        let acceptance = Arc::new(Mutex::new(AcceptanceIndex::default()));
        let mut template = None;

        rebuild_engine(dir.path(), &mut template, &acceptance, drop).expect("first build");
        assert!(template.is_some(), "the index should be retained for reuse");

        // Deleting the index proves the rebuild came from the cached template
        // rather than the disk, which is what keeps the first completion after
        // a timed-out attempt fast.
        std::fs::remove_file(dir.path().join("index.json")).unwrap();
        let engine = rebuild_engine(dir.path(), &mut template, &acceptance, drop).expect("rebuild without disk");
        assert!(!engine.registry().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rebuilding_engine_does_not_reuse_a_stale_registry_template() {
        let root = tempfile::tempdir().unwrap();
        let generation_a = root.path().join("generation-a");
        let generation_b = root.path().join("generation-b");
        for (dir, child) in [(&generation_a, "from-a"), (&generation_b, "from-b")] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                dir.join("git.json"),
                serde_json::json!({
                    "names": ["git"],
                    "subcommands": [{"names": [child]}]
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(dir.join("index.json"), r#"{"files":{"git":"git.json"}}"#).unwrap();
        }
        let canonical = root.path().join("specs-ir");
        let backup = root.path().join("specs-ir.backup");
        std::fs::rename(&generation_a, &canonical).unwrap();
        let acceptance = Arc::new(Mutex::new(AcceptanceIndex::default()));
        let mut template = None;
        let _engine_a = rebuild_engine(&canonical, &mut template, &acceptance, drop).expect("generation A");

        // The old template is retained for a missing-window fallback, but a
        // stable replacement must be indexed before a timed-out rebuild uses
        // it again.
        std::fs::rename(&canonical, &backup).unwrap();
        std::fs::rename(&generation_b, &canonical).unwrap();
        let mut engine_b = rebuild_engine(&canonical, &mut template, &acceptance, drop).expect("generation B");
        let result = engine_b
            .complete(CompleteRequest {
                buffer: "git ".into(),
                cwd: "/tmp".into(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("generation B completion");
        assert!(
            result.suggestions.iter().any(|suggestion| suggestion.name == "from-b"),
            "the stale A template must not survive a stable B replacement"
        );
        assert!(!template.as_ref().expect("refreshed template").needs_refresh());

        // Cleanup must not be needed for the safety assertion, but mirrors
        // the publisher's backup lifecycle.
        std::fs::remove_dir_all(backup).unwrap();
    }

    #[test]
    fn synchronous_submission_cancels_only_older_tasks_and_preserves_control_order() {
        let (tx, rx) = job_channel();
        let client = EngineClient {
            tx,
            acceptance: Arc::new(Mutex::new(AcceptanceIndex::default())),
            submission: Arc::new(Mutex::new(Submission::default())),
        };
        let older = client.complete_for_session(SessionId::new(1), CompleteRequest::default());
        client.clear_caches().unwrap();
        let diagnostic = client.diagnostics();
        let newer = client.complete_for_session(SessionId::new(2), CompleteRequest::default());
        assert!(older.token.is_cancelled());
        drop(older);
        assert!(!newer.token.is_cancelled());
        assert!(matches!(rx.recv().unwrap().kind, JobKind::ClearCaches));
        assert!(matches!(rx.recv().unwrap().kind, JobKind::Diagnostics { .. }));
        assert!(matches!(rx.recv().unwrap().kind, JobKind::Complete { sequence: 2, .. }));
        drop(diagnostic);
        let token = newer.token.clone();
        drop(newer);
        assert!(token.is_cancelled(), "even an unpolled task cancels its own work");
    }

    #[test]
    fn cancelled_diagnostics_do_not_read_counters_or_block_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let client = EngineClient::spawn(dir.path().to_path_buf()).unwrap();
        // A diagnostic which still gathers its snapshot would block on this
        // counter lock. The worker must instead reach the following real job.
        let counters = client.submission.lock().unwrap();
        let (reply, receiver) = oneshot::channel();
        drop(receiver);
        client
            .tx
            .send(Job {
                kind: JobKind::Diagnostics { reply },
            })
            .unwrap();
        let (done, received) = mpsc::channel();
        let observer = client.clone();
        let observer = thread::spawn(move || {
            done.send(observer.inspect_idle("unused.json")).unwrap();
        });
        let observed = received.recv_timeout(Duration::from_secs(2));
        // Release the lock before asserting so even a regression can shut down
        // both threads instead of leaving a blocked worker behind.
        drop(counters);
        observer.join().unwrap();
        assert!(!observed.expect("cancelled diagnostic must not block").unwrap().cached);
        assert_eq!(
            futures::executor::block_on(client.diagnostics()).unwrap(),
            EngineClientDiagnostics::default(),
            "diagnostics must leave the lazy engine and request counts untouched"
        );
    }

    #[test]
    fn queued_controls_and_cancelled_jobs_cannot_starve_idle_release_or_reorder_live_work() {
        let dir = tempfile::tempdir().unwrap();
        write_idle_specs(dir.path());
        let now = Instant::now();
        let mut registry = Registry::load(dir.path()).unwrap();
        let child = registry.load_referenced_spec("child").unwrap();
        let weak = Arc::downgrade(&child);
        drop(child);
        registry.set_idle_since("child", Some(now));
        let mut engine = Engine::from_registry(dir.path(), registry, Arc::new(Mutex::new(AcceptanceIndex::default())));
        let (tx, rx) = mpsc::channel();
        let mut receivers = Vec::new();
        for index in 0..MAX_JOBS_WITHOUT_ATTEMPT {
            if index % 2 == 0 {
                let (reply, receiver) = oneshot::channel();
                receivers.push(receiver);
                tx.send(Job {
                    kind: JobKind::Diagnostics { reply },
                })
                .unwrap();
            } else {
                let (reply, receiver) = oneshot::channel();
                let job = completion_job(CompleteRequest::default(), reply);
                if let JobKind::Complete { token, .. } = &job.kind {
                    token.cancel();
                }
                tx.send(job).unwrap();
                drop(receiver);
            }
        }
        let (live_reply, live_receiver) = oneshot::channel();
        tx.send(completion_job(CompleteRequest::default(), live_reply)).unwrap();
        tx.send(Job {
            kind: JobKind::ClearCaches,
        })
        .unwrap();
        let mut streak = 0;
        for _ in 0..MAX_JOBS_WITHOUT_ATTEMPT {
            let head = rx.recv().unwrap();
            maintain_idle_between_queued_jobs(
                Some(&mut engine),
                &head,
                &mut streak,
                now + Duration::from_secs(1),
                Duration::ZERO,
            );
        }
        assert!(
            weak.upgrade().is_none(),
            "a continuously nonempty queue must still release an expired file"
        );
        assert!(matches!(rx.recv().unwrap().kind, JobKind::Complete { .. }));
        assert!(matches!(rx.recv().unwrap().kind, JobKind::ClearCaches));
        assert!(rx.try_recv().is_err());
        drop(live_receiver);

        // At the same threshold a live FIFO head wins, so its cached tree
        // remains available until the attempt can touch it again.
        let mut registry = Registry::load(dir.path()).unwrap();
        let child = registry.load_referenced_spec("child").unwrap();
        let weak = Arc::downgrade(&child);
        drop(child);
        registry.set_idle_since("child", Some(now));
        let mut engine = Engine::from_registry(dir.path(), registry, Arc::new(Mutex::new(AcceptanceIndex::default())));
        let (reply, receiver) = oneshot::channel();
        let live = completion_job(CompleteRequest::default(), reply);
        let mut streak = MAX_JOBS_WITHOUT_ATTEMPT;
        maintain_idle_between_queued_jobs(
            Some(&mut engine),
            &live,
            &mut streak,
            now + Duration::from_secs(1),
            Duration::ZERO,
        );
        assert!(weak.upgrade().is_some(), "a live completion must precede maintenance");
        drop(receiver);
    }

    #[test]
    fn end_input_is_ordered_with_submission_and_does_not_initialize_engine() {
        let (tx, rx) = job_channel();
        rx.set_resource_session(Some(SessionId::new(1)));
        let client = EngineClient {
            tx,
            acceptance: Arc::new(Mutex::new(AcceptanceIndex::default())),
            submission: Arc::new(Mutex::new(Submission::default())),
        };
        let old = client.complete_for_session(SessionId::new(1), CompleteRequest::default());
        client.end_input(SessionId::new(1)).unwrap();
        assert!(old.token.is_cancelled());
        client.end_input(SessionId::new(1)).unwrap();
        let fresh = client.complete_for_session(SessionId::new(1), CompleteRequest::default());
        assert!(!fresh.token.is_cancelled());
        assert!(matches!(rx.recv().unwrap().kind, JobKind::EndInput { .. }));
        assert!(matches!(rx.recv().unwrap().kind, JobKind::Complete { .. }));
        assert!(rx.try_recv().is_err());

        let dir = tempfile::tempdir().unwrap();
        let lazy = EngineClient::spawn(dir.path().to_path_buf()).unwrap();
        lazy.end_input(SessionId::new(1)).unwrap();
        assert!(
            futures::executor::block_on(lazy.diagnostics())
                .unwrap()
                .engine
                .is_none()
        );
    }

    #[test]
    fn cold_history_only_session_releases_specs_after_input_ends() {
        let dir = tempfile::tempdir().unwrap();
        write_idle_specs(dir.path());
        std::fs::write(
            dir.path().join("child.json"),
            r#"{"names":["child"],"filterStrategy":"prefix"}"#,
        )
        .unwrap();
        let client = EngineClient::spawn_with_idle_grace(
            dir.path().to_path_buf(),
            WATCHDOG_UNDER_TEST,
            Duration::from_millis(100),
        )
        .unwrap();
        let result = futures::executor::block_on(client.complete_for_session(
            SessionId::new(1),
            CompleteRequest {
                buffer: "tool child ".into(),
                cwd: dir.path().display().to_string(),
                fuzzy: true,
                history_only: true,
                ..CompleteRequest::default()
            },
        ))
        .unwrap();
        assert!(
            !result.fuzzy,
            "history-only must really load the child's filter strategy"
        );
        client.end_input(SessionId::new(1)).unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = futures::executor::block_on(client.diagnostics()).unwrap();
            assert_eq!(snapshot.requests.engine_initializations, 1);
            assert_eq!(snapshot.requests.completed, 1);
            if snapshot.engine.unwrap().registry.cached_file_count == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "history-only files never started their idle grace"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn background_end_and_repeated_end_cannot_change_current_input_or_restart_grace() {
        let dir = tempfile::tempdir().unwrap();
        write_idle_specs(dir.path());
        let client = EngineClient::spawn(dir.path().to_path_buf()).unwrap();
        let request = CompleteRequest {
            buffer: "tool child ".into(),
            cwd: dir.path().display().to_string(),
            ..CompleteRequest::default()
        };
        futures::executor::block_on(client.complete_for_session(SessionId::new(1), request.clone())).unwrap();
        client.end_input(SessionId::new(2)).unwrap();
        assert_eq!(
            client.inspect_idle("child").unwrap().mark,
            Some(None),
            "an empty background session must not guess the owner ended"
        );
        client.end_input(SessionId::new(1)).unwrap();
        let since = client.inspect_idle("child").unwrap().mark.unwrap().unwrap();
        client.end_input(SessionId::new(1)).unwrap();
        assert_eq!(client.inspect_idle("child").unwrap().mark, Some(Some(since)));
        futures::executor::block_on(client.complete_for_session(SessionId::new(2), request)).unwrap();
        client.end_input(SessionId::new(1)).unwrap();
        assert_eq!(client.inspect_idle("child").unwrap().mark, Some(None));
    }

    #[cfg(unix)]
    #[test]
    fn end_input_cancels_the_running_owner_and_old_completion_cannot_restore_activity() {
        let dir = tempfile::tempdir().unwrap();
        slow_fixture(dir.path());
        let client = EngineClient::spawn(dir.path().to_path_buf()).unwrap();
        let old = client.complete_for_session(SessionId::new(1), fixture_request(dir.path(), "slow "));
        await_fixture_start(dir.path());
        client.end_input(SessionId::new(1)).unwrap();
        assert!(
            futures::executor::block_on(old)
                .unwrap_err()
                .is::<CompletionCancelled>()
        );
        let mark = client.inspect_idle("slow").unwrap().mark.unwrap();
        assert!(
            mark.is_some(),
            "cancelled attempt cannot restore an active marker after end"
        );
        std::fs::write(dir.path().join("allow"), "").unwrap();
        let next = futures::executor::block_on(
            client.complete_for_session(SessionId::new(1), fixture_request(dir.path(), "slow ")),
        )
        .unwrap();
        assert!(next.suggestions.iter().any(|row| row.name == "recovered"));
        assert_eq!(client.inspect_idle("slow").unwrap().mark, Some(None));
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_new_session_cannot_swallow_the_active_sessions_end() {
        let dir = tempfile::tempdir().unwrap();
        slow_fixture(dir.path());
        let client = EngineClient::spawn_with_idle_grace(
            dir.path().to_path_buf(),
            WATCHDOG_UNDER_TEST,
            Duration::from_millis(100),
        )
        .unwrap();
        futures::executor::block_on(
            client.complete_for_session(SessionId::new(1), fixture_request(dir.path(), "fast ")),
        )
        .unwrap();
        assert_eq!(client.inspect_idle("fast").unwrap().mark, Some(None));
        let abandoned = client.complete_for_session(SessionId::new(2), fixture_request(dir.path(), "slow "));
        await_fixture_start(dir.path());
        drop(abandoned);
        client.end_input(SessionId::new(1)).unwrap();
        let state = client.inspect_idle("fast").unwrap();
        assert!(
            state.mark.is_none_or(|mark| mark.is_some()),
            "old active owner must enter grace"
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while futures::executor::block_on(client.diagnostics())
            .unwrap()
            .engine
            .unwrap()
            .registry
            .cached_file_count
            != 0
        {
            assert!(
                Instant::now() < deadline,
                "cancelled session swallowed the resource owner's end"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let counts = futures::executor::block_on(client.diagnostics()).unwrap().requests;
        assert_eq!(counts.engine_initializations, 1);
        assert_eq!(counts.cancelled, 1);
    }

    #[cfg(unix)]
    #[test]
    fn committed_session_wins_over_older_end_after_an_interrupted_attempt() {
        let dir = tempfile::tempdir().unwrap();
        slow_fixture(dir.path());
        let client = EngineClient::spawn(dir.path().to_path_buf()).unwrap();
        futures::executor::block_on(
            client.complete_for_session(SessionId::new(1), fixture_request(dir.path(), "fast ")),
        )
        .unwrap();
        let old = client.complete_for_session(SessionId::new(1), fixture_request(dir.path(), "slow "));
        await_fixture_start(dir.path());
        let newer = client.complete_for_session(SessionId::new(2), fixture_request(dir.path(), "fast "));
        client.end_input(SessionId::new(1)).unwrap();
        assert!(!newer.token.is_cancelled(), "ending A must not cancel B");
        assert!(
            futures::executor::block_on(old)
                .unwrap_err()
                .is::<CompletionCancelled>()
        );
        assert!(
            futures::executor::block_on(newer)
                .unwrap()
                .suggestions
                .iter()
                .any(|row| row.name == "ready")
        );
        assert_eq!(client.inspect_idle("fast").unwrap().mark, Some(None));

        // A history-only success has no normal idle commit and cannot steal
        // the resource owner from B, even though it is the latest submission.
        let mut history = fixture_request(dir.path(), "fast ");
        history.history_only = true;
        futures::executor::block_on(client.complete_for_session(SessionId::new(3), history)).unwrap();
        client.end_input(SessionId::new(3)).unwrap();
        assert_eq!(client.inspect_idle("fast").unwrap().mark, Some(None));
        client.end_input(SessionId::new(2)).unwrap();
        assert!(client.inspect_idle("fast").unwrap().mark.unwrap().is_some());
    }

    #[cfg(unix)]
    fn slow_fixture(dir: &Path) {
        let body = serde_json::json!({
            "names": ["slow"], "args": [{
                "name": "value", "cacheStrategy": "max-age", "splitOn": "\n",
                "script": ["/bin/sh", "-c",
                    "if [ ! -e \"$2\" ]; then : > \"$1\"; while :; do sleep 1; done; fi; printf 'recovered\\n'",
                    "cancel-fixture", dir.join("started"), dir.join("allow")]
            }]
        });
        std::fs::write(dir.join("slow.json"), body.to_string()).unwrap();
        std::fs::write(
            dir.join("fast.json"),
            r#"{"names":["fast"],"subcommands":[{"names":["ready"]}]}"#,
        )
        .unwrap();
    }

    #[cfg(unix)]
    fn await_fixture_start(dir: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dir.join("started").exists() {
            assert!(Instant::now() < deadline, "real script did not signal startup");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(unix)]
    fn fixture_request(dir: &Path, buffer: &str) -> CompleteRequest {
        CompleteRequest {
            buffer: buffer.into(),
            cwd: dir.display().to_string(),
            ..CompleteRequest::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn newer_input_cancels_a_running_script_without_resetting_engine_or_caching_empty_rows() {
        let dir = tempfile::tempdir().unwrap();
        slow_fixture(dir.path());
        let client = EngineClient::spawn(dir.path().to_path_buf()).unwrap();
        assert!(
            futures::executor::block_on(client.diagnostics())
                .unwrap()
                .engine
                .is_none()
        );
        let old = client.complete_for_session(SessionId::new(1), fixture_request(dir.path(), "slow "));
        await_fixture_start(dir.path());
        let latest = client.complete_for_session(SessionId::new(2), fixture_request(dir.path(), "fast "));
        let error = futures::executor::block_on(old).unwrap_err();
        assert!(error.is::<CompletionCancelled>(), "{error}");
        let result = futures::executor::block_on(latest).unwrap();
        assert!(result.suggestions.iter().any(|row| row.name == "ready"));
        let snapshot = futures::executor::block_on(client.diagnostics()).unwrap();
        assert_eq!(snapshot.requests.engine_initializations, 1);
        assert_eq!(
            (
                snapshot.requests.cancelled,
                snapshot.requests.completed,
                snapshot.requests.failed
            ),
            (1, 1, 0)
        );
        assert_eq!(snapshot.engine.unwrap().hooks.script_output.entries, 0);
        std::fs::write(dir.path().join("allow"), "").unwrap();
        let retried = client.complete_blocking(fixture_request(dir.path(), "slow ")).unwrap();
        assert!(
            retried.suggestions.iter().any(|row| row.name == "recovered"),
            "must rerun instead of replaying cancelled generator state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_running_task_releases_its_new_tree_without_another_completion() {
        let dir = tempfile::tempdir().unwrap();
        slow_fixture(dir.path());
        let client = EngineClient::spawn_with_options(
            dir.path().to_path_buf(),
            EngineClientOptions {
                spec_idle_grace: Duration::from_millis(40),
                ..EngineClientOptions::default()
            },
        )
        .unwrap();
        let task = client.complete(fixture_request(dir.path(), "slow "));
        await_fixture_start(dir.path());
        drop(task);
        let returned = futures::executor::block_on(client.diagnostics()).unwrap();
        assert_eq!(returned.requests.cancelled, 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            thread::sleep(Duration::from_millis(20));
            let snapshot = futures::executor::block_on(client.diagnostics()).unwrap();
            assert_eq!(snapshot.requests.submitted, 1);
            if snapshot.engine.unwrap().registry.cached_file_count == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cancelled first load must get an idle deadline"
            );
        }
    }

    #[test]
    fn spawn_completes_on_a_plain_thread() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        let engine = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");
        let result = engine
            .complete_blocking(CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            result.suggestions.iter().any(|s| s.name == "status"),
            "{:?}",
            result.suggestions
        );
    }

    fn counting_script(count: &std::path::Path) -> String {
        format!("printf 'src\\n'; printf x >> '{}'", count.display())
    }

    /// Watchdog for the reset tests. The request after a reset runs on a
    /// fresh engine that still has to load shell history, so this needs
    /// real headroom over that on a loaded test host; 100ms did not have it.
    const WATCHDOG_UNDER_TEST: Duration = Duration::from_secs(1);

    #[test]
    fn generator_session_survives_attempt_threads() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let spec = serde_json::json!({
            "names": ["demo"],
            "args": [{
                "name": "slot",
                "script": ["sh", "-c", counting_script(&count)],
                "splitOn": "\n",
                "debounceMs": 200
            }]
        });
        std::fs::write(dir.path().join("demo.json"), spec.to_string()).unwrap();
        let engine = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");
        let cwd = dir.path().display().to_string();
        let request = CompleteRequest {
            buffer: "demo ".into(),
            cwd,
            include_history: false,
            ..CompleteRequest::default()
        };
        let first = engine.complete_blocking(request.clone()).expect("first");
        assert!(
            first.pending_generators,
            "debounce should delay the first run: {first:?}"
        );
        assert!(!count.exists() || std::fs::read_to_string(&count).unwrap().is_empty());

        let second = engine.complete_blocking(request).expect("follow-up");
        assert!(
            second.suggestions.iter().any(|row| row.name == "src"),
            "{:?}",
            second.suggestions
        );
        assert_eq!(std::fs::read_to_string(&count).unwrap().matches('x').count(), 1);
        assert!(!second.pending_generators);
    }

    #[test]
    fn clear_caches_rereads_specs_and_forgets_generator_results() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let spec = |names: &[&str]| {
            serde_json::json!({
                "names": ["demo"],
                "subcommands": names.iter().map(|name| serde_json::json!({"names": [name]})).collect::<Vec<_>>(),
                "args": [{
                    "name": "slot",
                    "script": ["sh", "-c", counting_script(&count)],
                    "splitOn": "\n",
                    "cacheTtl": 60000
                }]
            })
        };
        std::fs::write(dir.path().join("demo.json"), spec(&["before"]).to_string()).unwrap();
        let engine = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");
        let request = CompleteRequest {
            buffer: "demo ".into(),
            cwd: dir.path().display().to_string(),
            include_history: false,
            ..CompleteRequest::default()
        };
        let first = engine.complete_blocking(request.clone()).expect("first");
        assert!(first.suggestions.iter().any(|row| row.name == "before"), "{first:?}");
        assert_eq!(std::fs::read_to_string(&count).unwrap().matches('x').count(), 1);

        // Edit the spec on disk; a cached spec and a cached script result
        // would both hide the change.
        std::fs::write(dir.path().join("demo.json"), spec(&["after"]).to_string()).unwrap();
        let stale = engine.complete_blocking(request.clone()).expect("stale");
        assert!(stale.suggestions.iter().any(|row| row.name == "before"), "{stale:?}");
        assert_eq!(std::fs::read_to_string(&count).unwrap().matches('x').count(), 1);

        engine.clear_caches().expect("clear");
        let fresh = engine.complete_blocking(request).expect("fresh");
        assert!(fresh.suggestions.iter().any(|row| row.name == "after"), "{fresh:?}");
        assert!(fresh.suggestions.iter().all(|row| row.name != "before"), "{fresh:?}");
        assert_eq!(std::fs::read_to_string(&count).unwrap().matches('x').count(), 2);
    }

    #[test]
    fn trailing_space_and_typed_token_share_generator_session_across_threads() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let script = counting_script(&count);
        let spec = serde_json::json!({
            "names": ["git"],
            "subcommands": [
                {"names": ["add"], "args": [{"name": "pathspec", "script": ["sh", "-c", script.clone()], "splitOn": "\n"}]},
                {"names": ["rm"], "args": [{"name": "pathspec", "script": ["sh", "-c", script], "splitOn": "\n"}]}
            ]
        });
        std::fs::write(dir.path().join("git.json"), spec.to_string()).unwrap();
        let engine = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");
        let cwd = dir.path().display().to_string();
        let after_space = engine
            .complete_blocking(CompleteRequest {
                buffer: "git add ".into(),
                cwd: cwd.clone(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("git add ");
        let while_typing = engine
            .complete_blocking(CompleteRequest {
                buffer: "git add s".into(),
                cwd: cwd.clone(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("git add s");
        let other_command = engine
            .complete_blocking(CompleteRequest {
                buffer: "git rm ".into(),
                cwd,
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("git rm ");
        assert!(
            after_space.suggestions.iter().any(|row| row.name == "src"),
            "{after_space:?}"
        );
        assert!(
            while_typing.suggestions.iter().any(|row| row.name == "src"),
            "{while_typing:?}"
        );
        assert!(
            other_command.suggestions.iter().any(|row| row.name == "src"),
            "{other_command:?}"
        );
        assert_eq!(std::fs::read_to_string(&count).unwrap().matches('x').count(), 2);
    }

    #[test]
    fn supervisor_retries_engine_initialization_after_specs_are_repaired() {
        let dir = tempfile::tempdir().unwrap();
        // Registry::load parses index.json during Engine::new, so an invalid
        // index gives us a deterministic initialization failure before any
        // completion attempt starts.
        std::fs::write(dir.path().join("index.json"), b"{").unwrap();
        let client = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");

        let first = client.complete_blocking(CompleteRequest {
            buffer: "git ".into(),
            cwd: dir.path().display().to_string(),
            ..CompleteRequest::default()
        });
        let error = first.expect_err("the malformed registry should fail initialization");
        assert!(
            error.to_string().contains("completion engine initialization failed"),
            "{error}"
        );

        // Repair the directory after the first request.  A permanently
        // failed supervisor would return the old initialization error again;
        // a retrying supervisor should build the registry and complete.
        std::fs::write(dir.path().join("index.json"), r#"{"files":{"git":"git.json"}}"#).unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        let second = client
            .complete_blocking(CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("the repaired registry should be retried");
        assert!(second.suggestions.iter().any(|suggestion| suggestion.name == "status"));
    }

    #[test]
    fn cancelled_latest_job_is_detected_before_running_an_attempt() {
        let (tx, rx) = mpsc::channel::<Job>();
        let (reply, caller) = oneshot::channel();
        tx.send(completion_job(CompleteRequest::default(), reply)).unwrap();
        drop(caller);

        let job = rx.recv().unwrap();
        let JobKind::Complete { reply, .. } = job.kind else {
            unreachable!("job should be a completion")
        };
        assert!(
            reply.is_canceled(),
            "a cancelled caller must be observable before execution"
        );
    }

    /// Reproduces the desktop's shape exactly: a tokio `block_on` whose poll
    /// never returns, and inside it a foreign executor driving `complete()`
    /// futures to completion by hand. tokio's budget for that poll is 128 and
    /// is never refilled, so a `tokio::sync::oneshot` reply stops resolving on
    /// the 129th round and wakes itself forever instead — the livelock that
    /// pinned the desktop at 100% CPU after a few hours of typing.
    #[test]
    fn replies_keep_resolving_after_tokio_exhausts_its_budget_on_the_polling_thread() {
        use std::task::{Context, Poll, Waker};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        let client = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");
        let request = CompleteRequest {
            buffer: "git ".into(),
            cwd: dir.path().display().to_string(),
            ..CompleteRequest::default()
        };

        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            // Well past tokio's 128-unit budget. Each round polls the reply
            // future directly, never yielding back to `block_on`.
            for round in 0..200u32 {
                let mut future = Box::pin(client.complete(request.clone()));
                // A livelocked reply never resolves, so a wall-clock bound
                // catches it; a spin count did not survive a loaded test
                // host, where the first round also pays for the engine's
                // history load.
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                let result = loop {
                    match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
                        Poll::Ready(result) => break result,
                        Poll::Pending => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "round {round}: the reply future stopped making progress on this thread"
                            );
                            thread::yield_now();
                        },
                    }
                };
                let result = result.expect("completion");
                assert!(result.suggestions.iter().any(|s| s.name == "status"), "round {round}");
            }
        });
    }

    #[test]
    fn supervisor_resets_after_a_timeout_before_serving_the_next_request() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hang.json"),
            r#"{"names":["hang"],"args":[{"script":["sleep","5"]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();

        let client = EngineClient::spawn_with_timeout(dir.path().to_path_buf(), WATCHDOG_UNDER_TEST).expect("spawn");
        let first = client.complete_blocking(CompleteRequest {
            buffer: "hang ".into(),
            cwd: dir.path().display().to_string(),
            ..CompleteRequest::default()
        });
        let error = first.expect_err("the hanging attempt should time out");
        assert!(error.to_string().contains("timed out"), "{error}");

        let second = client
            .complete_blocking(CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("the reset engine should serve the next request");
        assert!(second.suggestions.iter().any(|suggestion| suggestion.name == "status"));
    }

    #[test]
    fn a_reset_after_a_timeout_survives_the_specs_directory_going_bad() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hang.json"),
            r#"{"names":["hang"],"args":[{"script":["sleep","5"]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"git":"git.json","hang":"hang.json"}}"#,
        )
        .unwrap();

        let client = EngineClient::spawn_with_timeout(dir.path().to_path_buf(), WATCHDOG_UNDER_TEST).expect("spawn");
        let first = client.complete_blocking(CompleteRequest {
            buffer: "hang ".into(),
            cwd: dir.path().display().to_string(),
            ..CompleteRequest::default()
        });
        assert!(
            first
                .expect_err("the hanging attempt should time out")
                .to_string()
                .contains("timed out")
        );

        // The reset rebuilds from the index cached at startup, so corrupting
        // the directory afterwards cannot take completions down with it. A
        // directory that never loaded still retries — see
        // `supervisor_retries_engine_initialization_after_specs_are_repaired`.
        std::fs::write(dir.path().join("index.json"), b"{").unwrap();
        let second = client
            .complete_blocking(CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("the cached index should carry the reset engine");
        assert!(second.suggestions.iter().any(|suggestion| suggestion.name == "status"));
    }

    #[test]
    fn registry_checkpoint_survives_a_timeout_before_engine_construction() {
        let dir = tempfile::tempdir().unwrap();
        write_checkpoint_specs(dir.path(), "from-checkpoint");
        let (entered, release) = pause_next_initialization(dir.path(), InitializationStage::Construction);
        let client = EngineClient::spawn_with_timeout(dir.path().to_path_buf(), WATCHDOG_UNDER_TEST).unwrap();
        let request = CompleteRequest {
            buffer: "git ".into(),
            cwd: "/tmp".into(),
            include_history: false,
            ..CompleteRequest::default()
        };
        let first = client.complete(request.clone());
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let error = futures::executor::block_on(first).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");

        // No Engine has been constructed yet. Its verified registry still
        // belongs to the supervisor and can recover through a broken index.
        std::fs::write(dir.path().join("index.json"), b"{").unwrap();
        let recovered = client.complete_blocking(request).unwrap();
        release.send(()).unwrap();
        wait_for_operations_to_retire(&client);
        assert!(recovered.suggestions.iter().any(|row| row.name == "from-checkpoint"));
        assert_eq!(
            futures::executor::block_on(client.diagnostics())
                .unwrap()
                .requests
                .watchdog_timeouts,
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn late_checkpoint_cannot_replace_a_newer_clear_caches_generation() {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().join("specs-ir");
        let replacement = root.path().join("replacement");
        let backup = root.path().join("backup");
        std::fs::create_dir(&canonical).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        write_checkpoint_specs(&canonical, "from-old");
        write_checkpoint_specs(&replacement, "from-new");
        let (entered, release) = pause_next_initialization(&canonical, InitializationStage::Checkpoint);
        let client = EngineClient::spawn_with_timeout(canonical.clone(), WATCHDOG_UNDER_TEST).unwrap();
        let request = CompleteRequest {
            buffer: "git ".into(),
            cwd: "/tmp".into(),
            include_history: false,
            ..CompleteRequest::default()
        };
        let first = client.complete(request.clone());
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let error = futures::executor::block_on(first).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");

        // The abandoned attempt owns a captured old generation but has not
        // published it. ClearCaches establishes a newer pristine template.
        std::fs::rename(&canonical, &backup).unwrap();
        std::fs::rename(&replacement, &canonical).unwrap();
        client.clear_caches().unwrap();
        assert!(
            futures::executor::block_on(client.diagnostics())
                .unwrap()
                .engine
                .is_none()
        );
        release.send(()).unwrap();
        wait_for_operations_to_retire(&client);

        // Prevent a disk refresh from hiding a late overwrite of the template.
        // Recovery must still use ClearCaches' new generation, not the old one.
        std::fs::write(canonical.join("index.json"), b"{").unwrap();
        let recovered = client.complete_blocking(request).unwrap();
        assert!(recovered.suggestions.iter().any(|row| row.name == "from-new"));
        assert!(!recovered.suggestions.iter().any(|row| row.name == "from-old"));
    }

    fn write_checkpoint_specs(path: &Path, child: &str) {
        std::fs::write(path.join("index.json"), r#"{"files":{"git":"git.json"}}"#).unwrap();
        std::fs::write(
            path.join("git.json"),
            serde_json::json!({"names": ["git"], "subcommands": [{"names": [child]}]}).to_string(),
        )
        .unwrap();
    }

    fn wait_for_operations_to_retire(client: &EngineClient) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = futures::executor::block_on(client.diagnostics()).unwrap();
            if snapshot.worker.active_operations == 0 {
                assert_eq!(snapshot.worker.abandoned_operations, 0);
                return;
            }
            assert!(Instant::now() < deadline, "released operation must retire");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn timed_out_attempt_can_be_followed_by_a_fresh_engine_attempt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        let engine = Engine::new(dir.path().to_path_buf()).unwrap();
        let timed_out = run_attempt(
            engine,
            CompleteRequest::default(),
            Duration::from_millis(10),
            |engine, _request| {
                thread::sleep(Duration::from_millis(80));
                (engine, Ok(CompleteResult::default()))
            },
        );
        assert!(matches!(timed_out, Err(AttemptFailure::TimedOut)));

        // The timed-out attempt owns the old engine, so the supervisor's reset
        // path creates a new one before accepting the next request.
        let fresh_engine = Engine::new(dir.path().to_path_buf()).unwrap();
        let (_, result) = run_engine_attempt(
            fresh_engine,
            CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            },
            Duration::from_secs(1),
        )
        .expect("fresh attempt should not be blocked by the old one");
        let result = result.expect("fresh completion should succeed");
        assert!(result.suggestions.iter().any(|suggestion| suggestion.name == "status"));
    }

    #[test]
    fn default_watchdog_does_not_cut_off_the_legacy_five_second_generator_budget() {
        assert!(engine_attempt_timeout() >= Duration::from_secs(5));
        assert_eq!(engine_attempt_timeout_for(5_000), Duration::from_secs(30));
        assert_eq!(engine_attempt_timeout_for(60_000), Duration::from_secs(65));
        assert_eq!(engine_attempt_timeout_for(-1), Duration::from_secs(30));
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(dir.path().to_path_buf()).unwrap();
        let result = run_attempt(
            engine,
            CompleteRequest::default(),
            engine_attempt_timeout(),
            |engine, _request| {
                // This is deliberately just beyond the old three-second
                // watchdog while remaining below the legacy 5s default.
                thread::sleep(Duration::from_millis(3_200));
                (engine, Ok(CompleteResult::default()))
            },
        )
        .expect("the attempt should remain within the default watchdog");
        assert!(result.1.is_ok());
    }

    #[test]
    fn panicked_attempt_returns_an_error_and_does_not_block_reset() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(dir.path().to_path_buf()).unwrap();
        let panicked = run_attempt(
            engine,
            CompleteRequest::default(),
            Duration::from_secs(1),
            |_engine, _request| panic!("simulated completion panic"),
        );
        assert!(matches!(panicked, Err(AttemptFailure::Panicked)));

        let fresh_engine = Engine::new(dir.path().to_path_buf()).unwrap();
        let (_, result) = run_engine_attempt(fresh_engine, CompleteRequest::default(), Duration::from_secs(1))
            .expect("fresh attempt should run after a panic");
        assert!(result.is_ok());
    }

    /// The shipped build aborts on that same panic, so the host's panic hook
    /// has to be able to say what the thread was completing.
    #[test]
    fn a_panic_hook_can_name_what_an_attempt_thread_is_completing() {
        assert_eq!(attempt_context_for_panic(), None);
        let request = CompleteRequest {
            buffer: "git checkout ma".into(),
            cwd: "/repo".into(),
            ..CompleteRequest::default()
        };
        let context = attempt_log_context(&request);
        let seen = thread::spawn(move || {
            ATTEMPT_CONTEXT.with(|slot| *slot.borrow_mut() = Some(context));
            attempt_context_for_panic()
        })
        .join()
        .unwrap();
        assert_eq!(seen.as_deref(), Some(r#"root_command="git" cwd="/repo""#));
        assert_eq!(
            attempt_context_for_panic(),
            None,
            "another thread's attempt is not ours"
        );
    }

    fn write_idle_specs(dir: &std::path::Path) {
        std::fs::write(
            dir.join("tool.json"),
            r#"{"names":["tool"],"subcommands":[{"names":["child"],"loadSpec":"child"}]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("child.json"), r#"{"names":["child"],"description":"nested"}"#).unwrap();
        std::fs::write(
            dir.join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("index.json"),
            r#"{"files":{"tool":"tool.json","git":"git.json"}}"#,
        )
        .unwrap();
    }

    fn write_catalog_fixture(dir: &std::path::Path) {
        let (id, entry) = crate::hook_backend::test_typed_entry(
            "tool#custom#0",
            "custom",
            serde_json::json!({"op": "array", "items": []}),
        );
        std::fs::write(
            dir.join("typed-hooks.json"),
            serde_json::json!({
                "version": 1,
                "kind": "typed-hook-expressions",
                "contracts": crate::hook_backend::test_sidecar_contracts(),
                "hooks": {id: entry}
            })
            .to_string(),
        )
        .unwrap();
    }

    fn complete_buf(client: &EngineClient, buffer: &str) {
        client
            .complete_blocking(CompleteRequest {
                buffer: buffer.into(),
                cwd: "/".into(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("complete");
    }

    #[test]
    fn supervisor_releases_idle_history_without_another_completion() {
        let dir = tempfile::tempdir().unwrap();
        write_idle_specs(dir.path());
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".zsh_history"), ": 1:0;git status\n: 2:0;git add\n").unwrap();
        let client = EngineClient::spawn_with_options(
            dir.path().to_path_buf(),
            EngineClientOptions {
                history_idle_grace: Duration::from_millis(300),
                ..EngineClientOptions::default()
            },
        )
        .expect("spawn");
        let complete = || {
            let request = CompleteRequest {
                buffer: "git status".into(),
                cwd: "/".into(),
                current_shell: Some("/bin/zsh".into()),
                environment_variables: Arc::new(vec![("HOME".to_string(), home.path().display().to_string())]),
                ..CompleteRequest::default()
            };
            futures::executor::block_on(client.complete(request)).expect("complete");
        };
        let lines = || {
            futures::executor::block_on(client.diagnostics())
                .expect("diagnostics")
                .engine
                .expect("engine")
                .history
                .line_count
        };
        complete();
        assert_eq!(lines(), 2);
        // The deadline is absolute, so these diagnostics jobs do not push it back.
        let started = Instant::now();
        while lines() != 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "supervisor did not release idle history"
            );
            thread::sleep(Duration::from_millis(30));
        }
        // The engine and its index stay, and the next request reads history again.
        complete();
        assert_eq!(lines(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn supervisor_releases_an_idle_file_without_another_completion() {
        let dir = tempfile::tempdir().unwrap();
        write_idle_specs(dir.path());
        let grace = Duration::from_millis(200);
        let client =
            EngineClient::spawn_with_idle_grace(dir.path().to_path_buf(), WATCHDOG_UNDER_TEST, grace).expect("spawn");
        complete_buf(&client, "tool child ");
        complete_buf(&client, "git status");
        let pending = client.inspect_idle("child").expect("inspect");
        assert!(pending.cached, "the grace keeps the child");
        assert!(pending.mark.expect("tracked").is_some(), "git status starts the grace");

        // Stay quiet so the supervisor's own deadline fires. An inspect in
        // this window would be a job and would restart the wait.
        thread::sleep(grace + Duration::from_millis(80));
        let mut released = client.inspect_idle("child").expect("inspect");
        let started = Instant::now();
        while released.cached {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "supervisor did not release the idle child"
            );
            thread::sleep(Duration::from_millis(30));
            released = client.inspect_idle("child").expect("inspect");
        }
        assert!(released.mark.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn hook_results_wake_idle_maintenance_without_any_file_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        registry.insert(
            serde_json::from_value(serde_json::json!({
                "names": ["tool"], "args": [{
                    "cacheStrategy": "max-age", "splitOn": "\n", "script": ["/bin/echo", "cached-result"]
                }]
            }))
            .unwrap(),
        );
        let engine = Engine::from_registry(dir.path(), registry, Arc::new(Mutex::new(AcceptanceIndex::default())));
        let (mut engine, result) = run_engine_attempt(
            engine,
            CompleteRequest {
                buffer: "tool ".into(),
                cwd: dir.path().display().to_string(),
                include_history: false,
                ..CompleteRequest::default()
            },
            WATCHDOG_UNDER_TEST,
        )
        .unwrap();
        assert!(
            result
                .unwrap()
                .suggestions
                .iter()
                .any(|row| row.name == "cached-result")
        );
        assert_eq!(engine.diagnostics().hooks.script_output.entries, 1);
        assert_eq!(engine.diagnostics().registry.cached_file_count, 0);
        engine.end_input();
        let grace = Duration::from_millis(20);
        let (tx, rx) = job_channel();
        let (done, received) = mpsc::channel();
        let waiter = thread::spawn(move || {
            let result = wait_for_engine_job(&rx, Some(&engine), grace, crate::runtime::HISTORY_IDLE_GRACE);
            let _ = done.send((result, engine));
        });
        let observed = received.recv_timeout(Duration::from_secs(2));
        // A missing deadline would block in recv. Disconnect it before failing
        // the assertion, so a regression cannot leave a waiting test thread.
        drop(tx);
        waiter.join().unwrap();
        let (result, mut engine) = observed.expect("hook-only idle deadline must wake the worker");
        assert!(matches!(result, Err(mpsc::RecvTimeoutError::Timeout)));
        engine.release_idle_specs(Instant::now(), grace);
        assert_eq!(engine.diagnostics().hooks.script_output.entries, 0);
        assert!(engine.next_idle_deadline(grace).is_none());
    }

    #[test]
    fn catalog_wakes_idle_maintenance_without_any_spec_or_hook_result_deadline() {
        let dir = tempfile::tempdir().unwrap();
        write_catalog_fixture(dir.path());
        let mut registry = Registry::new();
        registry.insert(
            serde_json::from_value(serde_json::json!({
                "names": ["tool"], "args": [{"jsCustom": "tool#custom#0"}]
            }))
            .unwrap(),
        );
        let engine = Engine::from_registry(dir.path(), registry, Arc::new(Mutex::new(AcceptanceIndex::default())));
        let (mut engine, result) = run_engine_attempt(
            engine,
            CompleteRequest {
                buffer: "tool ".into(),
                cwd: dir.path().display().to_string(),
                include_history: false,
                ..CompleteRequest::default()
            },
            WATCHDOG_UNDER_TEST,
        )
        .unwrap();
        assert!(result.is_ok());
        assert!(engine.diagnostics().hook_catalog.loaded);
        assert_eq!(engine.diagnostics().hook_catalog.descriptor_count, 1);
        assert_eq!(engine.diagnostics().registry.cached_file_count, 0);
        assert_eq!(engine.diagnostics().hooks, Default::default());
        engine.end_input();
        let grace = Duration::from_millis(20);
        let (tx, rx) = job_channel();
        let (done, received) = mpsc::channel();
        let waiter = thread::spawn(move || {
            let result = wait_for_engine_job(&rx, Some(&engine), grace, crate::runtime::HISTORY_IDLE_GRACE);
            let _ = done.send((result, engine));
        });
        let observed = received.recv_timeout(Duration::from_secs(2));
        drop(tx);
        waiter.join().unwrap();
        let (result, mut engine) = observed.expect("catalog-only deadline must wake the worker");
        assert!(matches!(result, Err(mpsc::RecvTimeoutError::Timeout)));
        engine.release_idle_specs(Instant::now(), grace);
        assert!(!engine.diagnostics().hook_catalog.load_attempted);
        assert!(engine.next_idle_deadline(grace).is_none());
    }

    #[test]
    fn cancelling_before_the_first_attempt_keeps_a_catalog_release_deadline() {
        let dir = tempfile::tempdir().unwrap();
        write_catalog_fixture(dir.path());
        let engine = Engine::from_registry(
            dir.path(),
            Registry::load(dir.path()).unwrap(),
            Arc::new(Mutex::new(AcceptanceIndex::default())),
        );
        let token = CancellationToken::new();
        token.cancel();
        let (mut engine, result) =
            run_engine_attempt_with_token(engine, CompleteRequest::default(), WATCHDOG_UNDER_TEST, token).unwrap();
        assert!(result.unwrap_err().is::<CompletionCancelled>());
        assert_eq!(engine.diagnostics().hook_catalog, Default::default());
        let grace = Duration::from_secs(10);
        let deadline = engine
            .next_idle_deadline(grace)
            .expect("cancelled cold catalog is idle");
        engine.release_idle_specs(deadline, grace);
        assert!(!engine.diagnostics().hook_catalog.load_attempted);
        assert!(engine.next_idle_deadline(grace).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_the_first_input_releases_results_cached_before_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("slow.json"),
            serde_json::json!({
                "names": ["slow"], "args": [{ "generators": [
                    {"cacheStrategy": "max-age", "script": ["/bin/echo", "cached-first"]},
                    {"script": ["/bin/sh", "-c", ": > \"$1\"; while :; do sleep 1; done",
                        "cancel-cache-fixture", dir.path().join("started")]}
                ]}]
            })
            .to_string(),
        )
        .unwrap();
        let grace = Duration::from_millis(200);
        let client = EngineClient::spawn_with_idle_grace(dir.path().to_path_buf(), WATCHDOG_UNDER_TEST, grace).unwrap();
        let task = client.complete_for_session(SessionId::new(1), fixture_request(dir.path(), "slow "));
        await_fixture_start(dir.path());
        client.end_input(SessionId::new(1)).unwrap();
        assert!(
            futures::executor::block_on(task)
                .unwrap_err()
                .is::<CompletionCancelled>()
        );
        let snapshot = futures::executor::block_on(client.diagnostics()).unwrap();
        assert_eq!(
            snapshot.engine.unwrap().hooks.script_output.entries,
            1,
            "a newly idle result gets a full grace"
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = futures::executor::block_on(client.diagnostics()).unwrap();
            if snapshot.engine.unwrap().hooks.script_output.entries == 0 {
                assert_eq!(snapshot.requests.engine_initializations, 1);
                assert_eq!(snapshot.requests.cancelled, 1);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "a cancelled first request left an ownerless hook cache"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn cached_hooks_follow_committed_session_across_history_and_cancelled_submission() {
        let dir = tempfile::tempdir().unwrap();
        slow_fixture(dir.path());
        write_catalog_fixture(dir.path());
        std::fs::write(
            dir.path().join("fast.json"),
            r#"{
            "names":["fast"],"args":[{"generators":[
                {"cacheStrategy":"max-age","script":["/bin/echo","ready"]},
                {"jsCustom":"tool#custom#0"}
            ]}]
        }"#,
        )
        .unwrap();
        let grace = Duration::from_millis(100);
        let client = EngineClient::spawn_with_idle_grace(dir.path().to_path_buf(), WATCHDOG_UNDER_TEST, grace).unwrap();
        futures::executor::block_on(
            client.complete_for_session(SessionId::new(1), fixture_request(dir.path(), "fast ")),
        )
        .unwrap();
        let mut history = fixture_request(dir.path(), "fast ");
        history.history_only = true;
        futures::executor::block_on(client.complete_for_session(SessionId::new(2), history)).unwrap();
        client.end_input(SessionId::new(2)).unwrap();
        thread::sleep(grace + Duration::from_millis(20));
        assert_eq!(
            futures::executor::block_on(client.diagnostics())
                .unwrap()
                .engine
                .unwrap()
                .hooks
                .script_output
                .entries,
            1
        );
        assert!(
            futures::executor::block_on(client.diagnostics())
                .unwrap()
                .engine
                .unwrap()
                .hook_catalog
                .loaded,
            "a history-only session cannot end the committed catalog owner"
        );
        let abandoned = client.complete_for_session(SessionId::new(2), fixture_request(dir.path(), "slow "));
        await_fixture_start(dir.path());
        drop(abandoned);
        client.end_input(SessionId::new(1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = futures::executor::block_on(client.diagnostics()).unwrap();
            if snapshot.engine.unwrap().hooks.script_output.entries == 0 {
                assert_eq!(snapshot.requests.engine_initializations, 1);
                assert!(!snapshot.engine.unwrap().hook_catalog.load_attempted);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cancelled newer session swallowed the owner's idle release"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn completion_during_the_grace_runs_and_clears_the_idle_mark() {
        let dir = tempfile::tempdir().unwrap();
        write_idle_specs(dir.path());
        let client =
            EngineClient::spawn_with_idle_grace(dir.path().to_path_buf(), WATCHDOG_UNDER_TEST, Duration::from_secs(5))
                .expect("spawn");
        complete_buf(&client, "tool child ");
        complete_buf(&client, "git status");
        let started = Instant::now();
        complete_buf(&client, "tool child ");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a queued completion waited out the grace"
        );
        let state = client.inspect_idle("child").expect("inspect");
        assert!(state.cached);
        assert_eq!(state.mark, Some(None));
    }

    #[test]
    fn clear_caches_drops_a_pending_idle_file() {
        let dir = tempfile::tempdir().unwrap();
        write_idle_specs(dir.path());
        let client =
            EngineClient::spawn_with_idle_grace(dir.path().to_path_buf(), WATCHDOG_UNDER_TEST, Duration::from_secs(5))
                .expect("spawn");
        complete_buf(&client, "tool child ");
        complete_buf(&client, "git status");
        let pending = client.inspect_idle("child").expect("inspect");
        assert!(pending.cached);
        assert!(pending.mark.expect("tracked").is_some());

        client.clear_caches().expect("clear");
        let state = client.inspect_idle("child").expect("inspect");
        assert!(!state.cached);
        assert!(state.mark.is_none());
    }

    #[test]
    fn app_bundle_uses_specs_ir_not_js_specs() {
        let macos = Path::new("/Applications/Fastab.app/Contents/MacOS");
        let dir = specs_ir_in_bundle(macos);
        assert_eq!(dir.file_name().unwrap(), "specs-ir");
        assert_eq!(
            dir,
            PathBuf::from("/Applications/Fastab.app/Contents/MacOS/../Resources/specs-ir")
        );
    }
}

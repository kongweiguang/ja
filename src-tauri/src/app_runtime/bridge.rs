// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Single-owner bridge actor for the Java sidecar.
//!
//! The actor owns the supervisor and all lifecycle mutations.  A separate
//! EventPump worker owns the foundation's one-shot event consumer so a slow
//! start/turn request cannot starve notifications or terminal cleanup.

use super::config::{
    EventSink, LaunchConfig, RuntimeCommandError, RuntimeStatus, RuntimeStatusKind, TurnAccepted,
    TurnStartInput, clear_recovery_record, ensure_recovery_clear, persist_recovery_record,
    recovery_marker_path,
};
use super::projection::{emit_frame, emit_status, frame_to_value};
use crate::agent_process::codec::RpcFrame;
use crate::agent_process::{EventPump, Session, SessionEvent, SidecarSupervisor};
use serde_json::{Value, json};
use std::path::PathBuf;
#[cfg(feature = "tauri-smoke")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const COMMAND_QUEUE_CAPACITY: usize = 64;
const INTERNAL_QUEUE_CAPACITY: usize = 128;
const COMMAND_DEADLINE: Duration = Duration::from_secs(15);
const EXIT_DEADLINE: Duration = Duration::from_secs(30);
const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(25);
const ACTOR_POLL_TIMEOUT: Duration = Duration::from_millis(25);
const EVENT_CANCEL_GRACE: Duration = Duration::from_millis(100);

const TERMINAL_NONE: u8 = 0;
const TERMINAL_SIDECAR: u8 = 1;
const TERMINAL_EVENT_DELIVERY: u8 = 2;

#[cfg(feature = "test-support")]
const PHASE_ACTOR_ENTER: u8 = 1;
#[cfg(feature = "test-support")]
const PHASE_START_RECEIVED: u8 = 2;
#[cfg(feature = "test-support")]
const PHASE_START_FAILED: u8 = 3;
#[cfg(feature = "test-support")]
const PHASE_STOP_RECEIVED: u8 = 4;
#[cfg(feature = "test-support")]
const PHASE_STOP_REPLIED: u8 = 5;
#[cfg(feature = "test-support")]
const PHASE_SHUTDOWN_RECEIVED: u8 = 6;
#[cfg(feature = "test-support")]
const PHASE_SHUTDOWN_CONFIRMED: u8 = 7;
#[cfg(feature = "test-support")]
const PHASE_ACTOR_COMPLETED: u8 = 8;

#[cfg(feature = "tauri-smoke")]
type ShutdownFailureInjector = Arc<AtomicUsize>;

/// A test-only gate pauses one bridge actor before it can consume commands;
/// this makes bounded queue admission observable without relying on timing.
#[cfg(feature = "test-support")]
struct QueueTestGate {
    released: AtomicBool,
    armed: AtomicBool,
    processed: AtomicU64,
    lock: Mutex<()>,
    wake: Condvar,
}

#[cfg(feature = "test-support")]
impl QueueTestGate {
    /// Creates a closed gate so the test owns the exact point at which the
    /// actor starts draining its queue.
    fn new() -> Self {
        Self {
            released: AtomicBool::new(false),
            armed: AtomicBool::new(false),
            processed: AtomicU64::new(0),
            lock: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    /// Blocks the actor until the test has proved that admission is full.
    fn wait(&self) {
        self.armed.store(true, Ordering::Release);
        self.wake.notify_all();
        if self.released.load(Ordering::Acquire) {
            return;
        }
        let mut guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !self.released.load(Ordering::Acquire) {
            guard = self
                .wake
                .wait(guard)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Opens the gate and wakes the single actor that owns the command queue.
    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    /// Waits until the actor has entered the gate, removing startup
    /// scheduling from the queue-capacity assertion.
    fn wait_until_armed(&self, deadline: Instant) -> bool {
        if self.armed.load(Ordering::Acquire) {
            return true;
        }
        let mut guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !self.armed.load(Ordering::Acquire) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, timeout) = self
                .wake
                .wait_timeout(guard, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard = next;
            if timeout.timed_out() && !self.armed.load(Ordering::Acquire) {
                return false;
            }
        }
        true
    }

    /// Waits until the actor has processed every probe, proving the data lane
    /// drained before the test sends its separate priority shutdown.
    fn wait_until_processed(&self, expected: u64, deadline: Instant) -> bool {
        if self.processed.load(Ordering::Acquire) >= expected {
            return true;
        }
        let mut guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while self.processed.load(Ordering::Acquire) < expected {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, timeout) = self
                .wake
                .wait_timeout(guard, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard = next;
            if timeout.timed_out() && self.processed.load(Ordering::Acquire) < expected {
                return false;
            }
        }
        true
    }

    /// Records one probe after the actor has consumed it and wakes waiters.
    fn mark_processed(&self) {
        self.processed.fetch_add(1, Ordering::AcqRel);
        self.wake.notify_all();
    }
}

/// Test-only handle for releasing a paused bridge actor after queue admission
/// assertions have completed; no equivalent production control is exposed.
#[cfg(feature = "test-support")]
pub struct QueueAdmissionBarrier {
    gate: Arc<QueueTestGate>,
}

#[cfg(feature = "test-support")]
impl QueueAdmissionBarrier {
    /// Waits for the actor to be paused before any command is admitted.
    pub fn wait_until_armed(&self, deadline: Instant) -> bool {
        self.gate.wait_until_armed(deadline)
    }

    /// Waits until the requested number of bounded probe commands was drained.
    pub fn wait_until_processed(&self, expected: u64, deadline: Instant) -> bool {
        self.gate.wait_until_processed(expected, deadline)
    }

    /// Releases the actor so previously admitted commands can be completed.
    pub fn release(&self) {
        self.gate.release();
    }
}

/// Test-only gate pauses a consumed start command immediately before process
/// construction, exposing the priority-shutdown interleaving deterministically.
#[cfg(feature = "test-support")]
struct StartFailureGate {
    released: AtomicBool,
    armed: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
}

#[cfg(feature = "test-support")]
impl StartFailureGate {
    /// Creates a closed gate so the test can enqueue shutdown after admission.
    fn new() -> Self {
        Self {
            released: AtomicBool::new(false),
            armed: AtomicBool::new(false),
            lock: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    /// Blocks only the actor's admitted start operation until the test releases it.
    fn wait(&self) {
        self.armed.store(true, Ordering::Release);
        self.wake.notify_all();
        if self.released.load(Ordering::Acquire) {
            return;
        }
        let mut guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !self.released.load(Ordering::Acquire) {
            guard = self
                .wake
                .wait(guard)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Opens the start operation so its deterministic launch failure can return.
    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    /// Waits until the actor has consumed the start command and reached the gate.
    fn wait_until_armed(&self, deadline: Instant) -> bool {
        if self.armed.load(Ordering::Acquire) {
            return true;
        }
        let mut guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !self.armed.load(Ordering::Acquire) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, timeout) = self
                .wake
                .wait_timeout(guard, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard = next;
            if timeout.timed_out() && !self.armed.load(Ordering::Acquire) {
                return false;
            }
        }
        true
    }
}

/// Test-only handle for releasing the admitted start failure after shutdown
/// has been proven to occupy the independent priority lane.
#[cfg(feature = "test-support")]
pub struct StartFailureBarrier {
    gate: Arc<StartFailureGate>,
}

#[cfg(feature = "test-support")]
impl StartFailureBarrier {
    /// Waits for the actor admission point without relying on a scheduler sleep.
    pub fn wait_until_armed(&self, deadline: Instant) -> bool {
        self.gate.wait_until_armed(deadline)
    }

    /// Releases the launch failure and lets the actor consume shutdown next.
    pub fn release(&self) {
        self.gate.release();
    }
}

#[cfg(feature = "test-support")]
struct BridgeTraceState {
    events: Mutex<Vec<String>>,
    wake: Condvar,
}

/// Test-only lifecycle trace used to make failed-start/shutdown interleavings
/// observable without adding production logging or changing actor timing.
#[cfg(feature = "test-support")]
#[derive(Clone)]
pub struct BridgeTestTrace {
    state: Arc<BridgeTraceState>,
}

#[cfg(feature = "test-support")]
impl BridgeTestTrace {
    /// Creates an empty trace whose entries are owned by the test instance.
    pub fn new() -> Self {
        Self {
            state: Arc::new(BridgeTraceState {
                events: Mutex::new(Vec::new()),
                wake: Condvar::new(),
            }),
        }
    }

    /// Returns a snapshot so a failed assertion cannot hold the actor trace
    /// lock while formatting or inspecting the observed lifecycle.
    pub fn events(&self) -> Vec<String> {
        self.state
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Waits for one actor transition so concurrency tests can prove ordering
    /// with a condition variable rather than a timing-dependent sleep.
    pub fn wait_for(&self, expected: &str, deadline: Instant) -> bool {
        let mut guard = self
            .state
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !guard.iter().any(|event| event == expected) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, timeout) = self
                .state
                .wake
                .wait_timeout(guard, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard = next;
            if timeout.timed_out() && !guard.iter().any(|event| event == expected) {
                return false;
            }
        }
        true
    }
}

#[cfg(feature = "test-support")]
impl Default for BridgeTestTrace {
    /// Delegates to the explicit constructor so test traces always own a
    /// fresh condition-variable state and never share observations.
    fn default() -> Self {
        Self::new()
    }
}

/// Appends a test-only lifecycle marker while leaving production bridges with
/// no allocation, logging, or synchronization on this diagnostic path.
#[cfg(feature = "test-support")]
fn trace_event(trace: &Option<Arc<BridgeTraceState>>, event: impl Into<String>) {
    if let Some(trace) = trace {
        trace
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.into());
        trace.wake.notify_all();
    }
}

type Reply<T> = SyncSender<Result<T, RuntimeCommandError>>;

enum BridgeCommand {
    Start {
        reply: Reply<RuntimeStatus>,
    },
    Stop {
        reply: Reply<RuntimeStatus>,
    },
    State {
        reply: Reply<RuntimeStatus>,
    },
    TurnStart {
        params: Value,
        reply: Reply<TurnAccepted>,
    },
    #[cfg(feature = "test-support")]
    QueueProbe,
}

/// Shutdown has its own one-slot control lane so queued data commands cannot
/// delay application exit; the actor still performs the final process-tree
/// cleanup before acknowledging this request.
struct ShutdownRequest {
    reply: Reply<()>,
    deadline: Instant,
}

enum BridgeSignal {
    ServerRequest { generation: u64, frame: RpcFrame },
}

/// Reserves terminal faults outside the server-request queue so a burst of
/// nested requests cannot make crash cleanup unobservable to the actor.
struct TerminalFault {
    generation: AtomicU64,
    reason: AtomicU8,
    wake: SyncSender<()>,
}

impl TerminalFault {
    /// Creates an empty fault slot whose generation is the linearization key.
    fn new(wake: SyncSender<()>) -> Self {
        Self {
            generation: AtomicU64::new(0),
            reason: AtomicU8::new(TERMINAL_NONE),
            wake,
        }
    }

    /// Publishes one generation fault before waking the actor; a full wake
    /// lane is safe only when the same atomic generation is already pending.
    fn publish(&self, generation: u64, reason: u8) {
        self.reason.store(reason, Ordering::Release);
        let previous = self.generation.swap(generation, Ordering::AcqRel);
        if previous == generation {
            return;
        }
        match self.wake.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {
                tracing::error!(generation, "runtime terminal lane disconnected");
            }
        }
    }

    /// Takes the currently pending fault only when it still belongs to the
    /// observed generation, avoiding stale cleanup of a newer sidecar.
    fn take(&self, observed: u64) -> Option<u8> {
        let generation = self.generation.load(Ordering::Acquire);
        if generation == 0 || generation != observed {
            return None;
        }
        let reason = self.reason.load(Ordering::Acquire);
        self.generation
            .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| reason)
    }

    /// Clears a fault when its runtime is deliberately stopped or replaced.
    fn clear(&self) {
        self.generation.store(0, Ordering::Release);
        self.reason.store(TERMINAL_NONE, Ordering::Release);
    }
}

/// Retains a cleanup failure by generation until a later attempt proves that
/// the same process tree and event worker are actually gone.  The actor must
/// not turn a failed kill/reap into a completed lifecycle merely because its
/// own loop returned.
struct CleanupFault {
    generation: AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExitAttempt {
    id: u64,
    deadline: Instant,
}

type ExitCancellationHook = Arc<dyn Fn(Instant) + Send + Sync + 'static>;

/// Stores one immutable deadline per explicit exit attempt.  A retry creates
/// a new numbered attempt only after the previous actor attempt is complete;
/// an in-flight exit can therefore never be silently extended by a callback.
struct ExitControl {
    attempt: Mutex<Option<ExitAttempt>>,
    cancelled: AtomicBool,
    cancellation_hook: Mutex<Option<ExitCancellationHook>>,
    timeout: Duration,
}

struct SessionCancellationGuard<'a> {
    control: &'a ExitControl,
}

impl<'a> SessionCancellationGuard<'a> {
    /// Keeps a session cancellation hook scoped to one blocking operation so
    /// later generations cannot inherit a stale exit callback.
    fn new(control: &'a ExitControl) -> Self {
        Self { control }
    }
}

impl Drop for SessionCancellationGuard<'_> {
    /// Clears only the hook owned by this operation; the shared attempt and
    /// its deadline remain immutable until an explicit retry starts.
    fn drop(&mut self) {
        self.control.clear_session();
    }
}

impl ExitControl {
    /// Creates an exit gate with no attempt and no session cancellation hook.
    fn new(timeout: Duration) -> Self {
        Self {
            attempt: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            cancellation_hook: Mutex::new(None),
            timeout,
        }
    }

    /// Starts the first exit attempt exactly once and wakes any active
    /// session; later calls reuse its deadline instead of extending it.
    fn trigger(&self) -> ExitAttempt {
        let mut attempt = self
            .attempt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(current) = *attempt {
            self.cancelled.store(true, Ordering::Release);
            return current;
        }
        let current = ExitAttempt {
            id: 1,
            deadline: shutdown_deadline(self.timeout),
        };
        *attempt = Some(current);
        self.cancelled.store(true, Ordering::Release);
        drop(attempt);
        self.invoke_cancellation_hook(current.deadline);
        current
    }

    /// Starts a new bounded retry only after the caller has proved the prior
    /// attempt reached actor completion; its id makes deadline transitions
    /// observable in recovery records and tests.
    fn retry(&self) -> ExitAttempt {
        let mut attempt = self
            .attempt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = attempt.map_or(1, |current| current.id.saturating_add(1));
        let current = ExitAttempt {
            id,
            deadline: shutdown_deadline(self.timeout),
        };
        *attempt = Some(current);
        self.cancelled.store(true, Ordering::Release);
        drop(attempt);
        self.invoke_cancellation_hook(current.deadline);
        current
    }

    /// Returns the active attempt without creating a new operation budget.
    fn attempt(&self) -> Option<ExitAttempt> {
        self.attempt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .to_owned()
    }

    /// Returns the active absolute deadline for actor polling and cleanup.
    fn deadline(&self) -> Option<Instant> {
        self.attempt().map(|attempt| attempt.deadline)
    }

    /// Allows synchronous operations to stop admission after exit is requested.
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Installs the current session as a direct cancellation target so a
    /// shutdown request can wake an in-flight handshake/request wait.
    fn attach_session(&self, session: crate::agent_process::session::Session) {
        let hook: ExitCancellationHook = Arc::new(move |deadline| {
            if let Err(error) = SidecarSupervisor::close_session_until(&session, deadline) {
                tracing::debug!(
                    ?error,
                    "session cancellation did not finish before exit deadline"
                );
            }
        });
        let already_cancelled = self.cancelled.load(Ordering::Acquire);
        *self
            .cancellation_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook.clone());
        if already_cancelled && let Some(deadline) = self.deadline() {
            self.invoke_cancellation_hook(deadline);
        }
    }

    /// Removes a completed operation's hook so a later generation cannot be
    /// closed by a stale cancellation callback.
    fn clear_session(&self) {
        *self
            .cancellation_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Invokes a copied hook outside its mutex, preventing close/join code
    /// from blocking future retry registration.
    fn invoke_cancellation_hook(&self, deadline: Instant) {
        let hook = self
            .cancellation_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(deadline);
        }
    }
}

impl CleanupFault {
    /// Creates an empty fault ledger whose zero value means no cleanup debt.
    fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
        }
    }

    /// Records the generation whose process cleanup still needs confirmation.
    fn mark(&self, generation: u64) {
        self.generation.store(generation, Ordering::Release);
    }

    /// Reports whether a prior cleanup attempt remains unconfirmed.
    fn is_pending(&self) -> bool {
        self.generation.load(Ordering::Acquire) != 0
    }

    /// Returns the debt generation so state projection cannot downgrade an
    /// unconfirmed process owner to a misleading generation-zero stop.
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Clears only after the owning generation completed all cleanup stages.
    fn clear(&self, generation: u64) {
        let _ =
            self.generation
                .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// A condition-variable completion gate lets the owner wait for a worker
/// without polling or dropping a live JoinHandle into a detached thread.
struct Completion {
    done: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
}

impl Completion {
    /// Creates a completion gate whose only invariant is monotonic completion.
    fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
            lock: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    /// Publishes worker termination before waking bounded waiters.
    fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    /// Waits until the worker publishes completion or the absolute deadline.
    fn wait_until(&self, deadline: Instant) -> bool {
        if self.done.load(Ordering::Acquire) {
            return true;
        }
        let mut guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !self.done.load(Ordering::Acquire) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, wait) = self
                .wake
                .wait_timeout(guard, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard = next;
            if wait.timed_out() && !self.done.load(Ordering::Acquire) {
                return false;
            }
        }
        true
    }
}

struct RuntimeBridgeInner {
    commands: SyncSender<BridgeCommand>,
    shutdown: SyncSender<ShutdownRequest>,
    completion: Arc<Completion>,
    detached_event_generation: Arc<AtomicU64>,
    cleanup_fault: Arc<CleanupFault>,
    exit_control: Arc<ExitControl>,
    quarantine: Arc<ExitQuarantine>,
    shutdown_completed: Arc<AtomicBool>,
    recovery_path: PathBuf,
    actor_join: Mutex<Option<JoinHandle<()>>>,
    #[cfg(feature = "test-support")]
    test_trace: Option<Arc<BridgeTraceState>>,
    #[cfg(feature = "test-support")]
    test_phase: Arc<AtomicU8>,
}

impl RuntimeBridgeInner {
    /// Sends the high-priority shutdown and joins the actor before its last
    /// managed-state owner disappears, preserving process-tree ownership.
    fn shutdown(&self) -> Result<(), RuntimeCommandError> {
        self.shutdown_with_retry(true)
    }

    /// Runs Drop cleanup without creating a new retry budget; only an
    /// explicit host exit request may advance the numbered exit attempt.
    fn shutdown_without_retry(&self) -> Result<(), RuntimeCommandError> {
        self.shutdown_with_retry(false)
    }

    /// Performs bounded cleanup and optionally starts an explicit retry
    /// attempt.  The caller decides whether a new user operation is allowed.
    fn shutdown_with_retry(&self, allow_retry: bool) -> Result<(), RuntimeCommandError> {
        #[cfg(feature = "test-support")]
        trace_event(&self.test_trace, "inner_shutdown_begin");
        let needs_retry = self.completion.done.load(Ordering::Acquire)
            && (!self.shutdown_completed.load(Ordering::Acquire)
                || self.cleanup_fault.is_pending()
                || !self.quarantine.is_empty()
                || self.detached_event_generation.load(Ordering::Acquire) != 0);
        let attempt = if allow_retry && needs_retry {
            self.exit_control.retry()
        } else {
            self.exit_control.trigger()
        };
        let deadline = attempt.deadline;
        if !self.completion.done.load(Ordering::Acquire) {
            let (reply, receiver) = mpsc::sync_channel(1);
            match self.shutdown.try_send(ShutdownRequest { reply, deadline }) {
                Ok(()) => {
                    #[cfg(feature = "test-support")]
                    trace_event(&self.test_trace, "inner_shutdown_sent");
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(RuntimeCommandError::shutdown_timeout());
                    }
                    loop {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            #[cfg(feature = "test-support")]
                            trace_event(&self.test_trace, "inner_shutdown_reply_timeout");
                            return Err(RuntimeCommandError::unavailable());
                        }
                        match receiver.recv_timeout(remaining.min(ACTOR_POLL_TIMEOUT)) {
                            Ok(Ok(())) => {
                                #[cfg(feature = "test-support")]
                                trace_event(&self.test_trace, "inner_shutdown_reply_ok");
                                break;
                            }
                            Ok(Err(error)) => {
                                #[cfg(feature = "test-support")]
                                trace_event(
                                    &self.test_trace,
                                    format!("inner_shutdown_reply_err:{}", error.code),
                                );
                                return Err(error);
                            }
                            Err(RecvTimeoutError::Timeout)
                                if self.completion.done.load(Ordering::Acquire) =>
                            {
                                if self.shutdown_completed.load(Ordering::Acquire) {
                                    // The actor may have observed the exit
                                    // cancellation before the request became
                                    // visible.  A confirmed clean completion is
                                    // equivalent to the lost acknowledgement.
                                    #[cfg(feature = "test-support")]
                                    trace_event(
                                        &self.test_trace,
                                        "inner_shutdown_completed_without_reply",
                                    );
                                    break;
                                }
                                #[cfg(feature = "test-support")]
                                trace_event(
                                    &self.test_trace,
                                    "inner_shutdown_completed_unconfirmed",
                                );
                                return Err(RuntimeCommandError::shutdown_timeout());
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => {
                                #[cfg(feature = "test-support")]
                                trace_event(&self.test_trace, "inner_shutdown_reply_disconnected");
                                return Err(RuntimeCommandError::unavailable());
                            }
                        }
                    }
                }
                Err(TrySendError::Full(_)) => {
                    #[cfg(feature = "test-support")]
                    trace_event(&self.test_trace, "inner_shutdown_lane_full");
                    if !self.completion.wait_until(deadline) {
                        #[cfg(feature = "test-support")]
                        trace_event(&self.test_trace, "inner_shutdown_completion_timeout");
                        return Err(RuntimeCommandError::queue_full());
                    }
                }
                Err(TrySendError::Disconnected(_)) => {
                    #[cfg(feature = "test-support")]
                    trace_event(&self.test_trace, "inner_shutdown_lane_disconnected");
                    if !self.completion.done.load(Ordering::Acquire) {
                        return Err(RuntimeCommandError::unavailable());
                    }
                }
            }
        }
        if self.cleanup_fault.is_pending() {
            if let Err(error) = self.quarantine.retry_until(deadline, &self.cleanup_fault) {
                tracing::error!(?error, "quarantined runtime cleanup retry failed");
            }
            if self.quarantine.is_empty() && !self.cleanup_fault.is_pending() {
                self.shutdown_completed.store(true, Ordering::Release);
            }
        }
        if !self.shutdown_completed.load(Ordering::Acquire) {
            #[cfg(feature = "test-support")]
            trace_event(&self.test_trace, "inner_shutdown_not_completed");
            return Err(RuntimeCommandError::shutdown_timeout());
        }
        if self.detached_event_generation.load(Ordering::Acquire) != 0 {
            #[cfg(feature = "test-support")]
            trace_event(&self.test_trace, "inner_shutdown_event_detached");
            return Err(RuntimeCommandError::shutdown_timeout());
        }
        if self.cleanup_fault.is_pending() {
            #[cfg(feature = "test-support")]
            trace_event(&self.test_trace, "inner_shutdown_cleanup_fault");
            return Err(RuntimeCommandError::shutdown_timeout());
        }
        if !self.completion.wait_until(deadline) {
            #[cfg(feature = "test-support")]
            trace_event(&self.test_trace, "inner_shutdown_completion_wait_timeout");
            return Err(RuntimeCommandError::unavailable());
        }
        let handle = self
            .actor_join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            handle
                .join()
                .map_err(|_| RuntimeCommandError::unavailable())?;
        }
        if let Err(error) = clear_recovery_record(&self.recovery_path) {
            tracing::error!(
                ?error,
                "runtime recovery marker removal failed after clean exit"
            );
            return Err(RuntimeCommandError::recovery_required());
        }
        #[cfg(feature = "test-support")]
        trace_event(&self.test_trace, "inner_shutdown_complete");
        Ok(())
    }

    /// Reports whether Tauri may accept the final Exit event without risking
    /// a still-live actor, event worker, or sidecar process owner.
    fn exit_ready(&self) -> bool {
        self.completion.done.load(Ordering::Acquire)
            && self.shutdown_completed.load(Ordering::Acquire)
            && !self.cleanup_fault.is_pending()
            && self.detached_event_generation.load(Ordering::Acquire) == 0
            && self.quarantine.is_empty()
    }

    /// Persists a sanitized forced-exit marker when the platform cannot be
    /// prevented from terminating after cleanup was not confirmed.
    fn record_forced_exit(&self) {
        let attempt = self.exit_control.attempt();
        if let Err(error) = persist_recovery_record(
            &self.recovery_path,
            attempt.map(|value| value.id).unwrap_or(0),
            self.cleanup_fault.generation(),
        ) {
            tracing::error!(?error, "runtime recovery marker write failed");
        }
    }
}

impl Drop for RuntimeBridgeInner {
    /// Drop is only the final insurance; normal Tauri exit calls `shutdown`
    /// explicitly and therefore reports a bounded cleanup result to the host.
    fn drop(&mut self) {
        if let Err(error) = self.shutdown_without_retry() {
            tracing::error!(?error, "runtime bridge drop cleanup was not confirmed");
            self.record_forced_exit();
        }
    }
}

/// Cloneable managed Tauri state whose only mutable supervisor owner is its
/// actor thread.
#[derive(Clone)]
pub struct RuntimeBridge {
    inner: Arc<RuntimeBridgeInner>,
}

impl RuntimeBridge {
    /// Starts one actor and leaves all process I/O outside Tauri command locks.
    pub fn new(config: LaunchConfig, sink: EventSink) -> Result<Self, RuntimeCommandError> {
        Self::new_with_controls(
            config,
            sink,
            EXIT_DEADLINE,
            #[cfg(feature = "tauri-smoke")]
            None,
            #[cfg(feature = "test-support")]
            None,
            #[cfg(feature = "test-support")]
            None,
            #[cfg(feature = "test-support")]
            None,
        )
    }

    /// Uses a short host-controlled exit budget only in deterministic
    /// lifecycle tests; production always uses the fixed thirty-second gate.
    #[cfg(feature = "tauri-smoke")]
    pub fn new_for_exit_test(
        config: LaunchConfig,
        sink: EventSink,
        timeout: Duration,
    ) -> Result<Self, RuntimeCommandError> {
        Self::new_with_controls(
            config,
            sink,
            timeout,
            None,
            #[cfg(feature = "test-support")]
            None,
            #[cfg(feature = "test-support")]
            None,
            #[cfg(feature = "test-support")]
            None,
        )
    }

    /// Uses an instance-owned failure counter so one quarantine test cannot
    /// alter cleanup behavior in another concurrently running bridge.
    #[cfg(feature = "tauri-smoke")]
    pub fn new_for_exit_test_with_injector(
        config: LaunchConfig,
        sink: EventSink,
        timeout: Duration,
        injector: Arc<AtomicUsize>,
    ) -> Result<Self, RuntimeCommandError> {
        Self::new_with_controls(
            config,
            sink,
            timeout,
            Some(injector),
            #[cfg(feature = "test-support")]
            None,
            #[cfg(feature = "test-support")]
            None,
            #[cfg(feature = "test-support")]
            None,
        )
    }

    /// Builds a paused test bridge whose actor cannot consume the command
    /// lane until the caller has proved all bounded slots are occupied.
    #[cfg(feature = "test-support")]
    pub fn new_for_queue_test(
        config: LaunchConfig,
        sink: EventSink,
    ) -> Result<(Self, QueueAdmissionBarrier), RuntimeCommandError> {
        let gate = Arc::new(QueueTestGate::new());
        let bridge = Self::new_with_controls(
            config,
            sink,
            EXIT_DEADLINE,
            #[cfg(feature = "tauri-smoke")]
            None,
            Some(Arc::clone(&gate)),
            None,
            None,
        )?;
        Ok((bridge, QueueAdmissionBarrier { gate }))
    }

    /// Builds a bridge whose admitted start pauses before process creation;
    /// the returned barrier makes concurrent shutdown ordering deterministic.
    #[cfg(feature = "test-support")]
    pub fn new_for_start_failure_test(
        config: LaunchConfig,
        sink: EventSink,
        trace: &BridgeTestTrace,
    ) -> Result<(Self, StartFailureBarrier), RuntimeCommandError> {
        let gate = Arc::new(StartFailureGate::new());
        let bridge = Self::new_with_controls(
            config,
            sink,
            EXIT_DEADLINE,
            #[cfg(feature = "tauri-smoke")]
            None,
            None,
            Some(Arc::clone(&gate)),
            Some(Arc::clone(&trace.state)),
        )?;
        Ok((bridge, StartFailureBarrier { gate }))
    }

    /// Builds the application owner with an explicit exit budget while keeping
    /// executable and environment selection inside the trusted config root.
    fn new_with_controls(
        config: LaunchConfig,
        sink: EventSink,
        exit_timeout: Duration,
        #[cfg(feature = "tauri-smoke")] shutdown_failure_injector: Option<ShutdownFailureInjector>,
        #[cfg(feature = "test-support")] queue_gate: Option<Arc<QueueTestGate>>,
        #[cfg(feature = "test-support")] start_failure_gate: Option<Arc<StartFailureGate>>,
        #[cfg(feature = "test-support")] test_trace: Option<Arc<BridgeTraceState>>,
    ) -> Result<Self, RuntimeCommandError> {
        ensure_recovery_clear(&config.sidecar.run_dir)?;
        config.validate()?;
        let recovery_path = recovery_marker_path(&config.sidecar.run_dir);
        let (commands, command_receiver) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let (shutdown, shutdown_receiver) = mpsc::sync_channel(1);
        let (signals, signal_receiver) = mpsc::sync_channel(INTERNAL_QUEUE_CAPACITY);
        let (terminal_wake, terminal_receiver) = mpsc::sync_channel(1);
        let completion = Arc::new(Completion::new());
        let current_generation = Arc::new(AtomicU64::new(0));
        let terminal_fault = Arc::new(TerminalFault::new(terminal_wake));
        let detached_event_generation = Arc::new(AtomicU64::new(0));
        let cleanup_fault = Arc::new(CleanupFault::new());
        let exit_control = Arc::new(ExitControl::new(exit_timeout));
        let quarantine = Arc::new(ExitQuarantine::new());
        let shutdown_completed = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "test-support")]
        let test_phase = Arc::new(AtomicU8::new(0));
        let actor_completion = Arc::clone(&completion);
        let actor_epoch = Arc::clone(&current_generation);
        let actor_fault = Arc::clone(&terminal_fault);
        let actor_detached_event = Arc::clone(&detached_event_generation);
        let actor_cleanup_fault = Arc::clone(&cleanup_fault);
        let actor_exit_control = Arc::clone(&exit_control);
        let actor_quarantine = Arc::clone(&quarantine);
        let actor_shutdown_completed = Arc::clone(&shutdown_completed);
        #[cfg(feature = "test-support")]
        let actor_test_phase = Arc::clone(&test_phase);
        #[cfg(feature = "test-support")]
        let actor_queue_gate = queue_gate.clone();
        #[cfg(feature = "test-support")]
        let actor_start_failure_gate = start_failure_gate.clone();
        #[cfg(feature = "test-support")]
        let actor_trace = test_trace.clone();
        let actor = thread::Builder::new()
            .name("ja-runtime-bridge".to_owned())
            .spawn(move || {
                actor_loop(
                    ActorContext {
                        config,
                        sink,
                        signal_sender: signals,
                        current_generation: actor_epoch,
                        terminal_fault: actor_fault,
                        detached_event_generation: actor_detached_event,
                        cleanup_fault: actor_cleanup_fault,
                        exit_control: actor_exit_control,
                        quarantine: actor_quarantine,
                        shutdown_completed: actor_shutdown_completed,
                        completion: actor_completion,
                        #[cfg(feature = "tauri-smoke")]
                        shutdown_failure_injector,
                        #[cfg(feature = "test-support")]
                        queue_gate: actor_queue_gate,
                        #[cfg(feature = "test-support")]
                        start_failure_gate: actor_start_failure_gate,
                        #[cfg(feature = "test-support")]
                        test_trace: actor_trace,
                        #[cfg(feature = "test-support")]
                        test_phase: actor_test_phase,
                    },
                    command_receiver,
                    shutdown_receiver,
                    signal_receiver,
                    terminal_receiver,
                )
            })
            .map_err(|_| RuntimeCommandError::unavailable())?;
        Ok(Self {
            inner: Arc::new(RuntimeBridgeInner {
                commands,
                shutdown,
                completion,
                detached_event_generation,
                cleanup_fault,
                exit_control,
                quarantine,
                shutdown_completed,
                recovery_path,
                actor_join: Mutex::new(Some(actor)),
                #[cfg(feature = "test-support")]
                test_trace,
                #[cfg(feature = "test-support")]
                test_phase,
            }),
        })
    }

    /// Enqueues start without allowing a full command queue to block a UI
    /// thread; `RUNTIME_QUEUE_FULL` is stable and retryable.
    pub fn start(&self) -> Result<RuntimeStatus, RuntimeCommandError> {
        self.call(|reply| BridgeCommand::Start { reply })
    }

    /// Enqueues graceful stop so process-tree cleanup remains supervisor-owned.
    pub fn stop(&self) -> Result<RuntimeStatus, RuntimeCommandError> {
        self.call(|reply| BridgeCommand::Stop { reply })
    }

    /// Requests high-priority application shutdown and joins the actor.
    pub fn shutdown(&self) -> Result<(), RuntimeCommandError> {
        self.inner.shutdown()
    }

    /// Allows the Tauri run loop to prove that a final Exit event is safe.
    pub fn exit_ready(&self) -> bool {
        self.inner.exit_ready()
    }

    /// Records a token-free manual-recovery diagnostic when the OS can no
    /// longer be prevented from exiting before cleanup was confirmed.
    pub fn record_forced_exit(&self) {
        self.inner.record_forced_exit();
    }

    /// Returns the isolated actor phase for deterministic failure diagnostics;
    /// production builds do not retain this test-only observation channel.
    #[cfg(feature = "test-support")]
    pub fn phase_for_test(&self) -> u8 {
        self.inner.test_phase.load(Ordering::Acquire)
    }

    /// Returns the actor's authoritative lifecycle projection.
    pub fn state(&self) -> Result<RuntimeStatus, RuntimeCommandError> {
        if self.inner.completion.done.load(Ordering::Acquire) {
            return Ok(self.inner.quarantine.state(&self.inner.cleanup_fault));
        }
        self.call(|reply| BridgeCommand::State { reply })
    }

    /// Sends a typed turn through the same bounded path used by production.
    pub fn turn_start(&self, input: TurnStartInput) -> Result<TurnAccepted, RuntimeCommandError> {
        let params = input.into_params()?;
        self.call(|reply| BridgeCommand::TurnStart { params, reply })
    }

    /// Enqueues a no-I/O probe through the production bounded command lane;
    /// its test-only shape cannot block on an unconsumed reply while proving
    /// the exact capacity boundary.
    #[cfg(feature = "test-support")]
    pub fn try_queue_probe_for_test(&self) -> Result<(), RuntimeCommandError> {
        match self.inner.commands.try_send(BridgeCommand::QueueProbe) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(RuntimeCommandError::queue_full()),
            Err(TrySendError::Disconnected(_)) => Err(RuntimeCommandError::unavailable()),
        }
    }

    /// Uses a non-blocking bounded admission and a bounded reply wait; this
    /// invariant prevents a saturated WebView from pinning a native thread.
    fn call<T>(
        &self,
        build: impl FnOnce(Reply<T>) -> BridgeCommand,
    ) -> Result<T, RuntimeCommandError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        match self.inner.commands.try_send(build(reply)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(RuntimeCommandError::queue_full()),
            Err(TrySendError::Disconnected(_)) => return Err(RuntimeCommandError::unavailable()),
        }
        receiver
            .recv_timeout(COMMAND_DEADLINE)
            .map_err(|_| RuntimeCommandError::unavailable())?
    }
}

struct EventDrain {
    cancel: SyncSender<()>,
    completion: Arc<Completion>,
    generation: u64,
    detached_event_generation: Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
}

struct EventDrainContext {
    events: EventPump,
    generation: u64,
    supervisor_generation: u64,
    server_instance_id: String,
    sink: EventSink,
    server_request_sender: SyncSender<BridgeSignal>,
    terminal_fault: Arc<TerminalFault>,
    current_generation: Arc<AtomicU64>,
    cancel_receiver: Receiver<()>,
}

struct EventDrainSpec {
    events: EventPump,
    generation: u64,
    supervisor_generation: u64,
    server_instance_id: String,
    sink: EventSink,
    server_request_sender: SyncSender<BridgeSignal>,
    terminal_fault: Arc<TerminalFault>,
    current_generation: Arc<AtomicU64>,
    detached_event_generation: Arc<AtomicU64>,
}

impl EventDrain {
    /// Requests cancellation without waiting, so the supervisor can close a
    /// blocked session when the event pump does not observe the signal soon.
    fn request_stop(&self) {
        let _ = self.cancel.try_send(());
    }

    /// Waits for the sole EventPump consumer, preserving the shared absolute
    /// deadline rather than extending shutdown with a second timeout.
    fn wait_until(&self, deadline: Instant) -> bool {
        self.completion.wait_until(deadline)
    }

    /// Joins only after completion; an unfinished worker is detached by
    /// dropping its handle and reported as a bounded shutdown fault.
    fn finish(&mut self, deadline: Instant) -> Result<(), RuntimeCommandError> {
        if !self.completion.done.load(Ordering::Acquire) && !self.wait_until(deadline) {
            tracing::error!("runtime event worker exceeded its bounded stop deadline");
            self.detached_event_generation
                .store(self.generation, Ordering::Release);
            self.join.take();
            return Err(RuntimeCommandError::shutdown_timeout());
        }
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| RuntimeCommandError::unavailable())?;
        }
        Ok(())
    }
}

struct RunningRuntime {
    supervisor: SidecarSupervisor,
    event_drain: EventDrain,
    generation: u64,
    server_instance_id: String,
}

enum QuarantineOwner {
    Runtime(RunningRuntime),
    Pending(SidecarSupervisor),
}

/// Keeps an unconfirmed process owner reachable after the actor's shared exit
/// deadline.  Retry is performed by the managed bridge owner, never by an
/// unbounded detached worker or by dropping the child handles.
struct ExitQuarantine {
    owner: Mutex<Option<QuarantineOwner>>,
    status: Mutex<RuntimeStatus>,
}

impl ExitQuarantine {
    /// Creates an empty quarantine whose no-debt projection is stopped.
    fn new() -> Self {
        Self {
            owner: Mutex::new(None),
            status: Mutex::new(RuntimeStatus {
                status: RuntimeStatusKind::Stopped,
                generation: 0,
                server_instance_id: None,
            }),
        }
    }

    /// Retains a complete running runtime and publishes a crash projection.
    fn retain_runtime(&self, current: RunningRuntime, cleanup_fault: &CleanupFault) {
        cleanup_fault.mark(current.generation);
        let status = RuntimeStatus {
            status: RuntimeStatusKind::Crashed,
            generation: current.generation,
            server_instance_id: Some(current.server_instance_id.clone()),
        };
        *self
            .owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(QuarantineOwner::Runtime(current));
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = status;
    }

    /// Retains a pre-runtime supervisor when handshake cleanup could not be
    /// confirmed before exit, keeping its exact process owner retryable.
    fn retain_pending(&self, supervisor: SidecarSupervisor, cleanup_fault: &CleanupFault) {
        let generation = supervisor.generation();
        cleanup_fault.mark(generation);
        *self
            .owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(QuarantineOwner::Pending(supervisor));
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = RuntimeStatus {
            status: RuntimeStatusKind::Crashed,
            generation,
            server_instance_id: None,
        };
    }

    /// Projects a persistent crash while the quarantined owner remains live.
    fn state(&self, cleanup_fault: &CleanupFault) -> RuntimeStatus {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if cleanup_fault.is_pending() {
            status.status = RuntimeStatusKind::Crashed;
            if status.generation == 0 {
                status.generation = cleanup_fault.generation();
            }
        }
        status
    }

    /// Reports whether no process owner remains reachable for a later retry.
    /// The bridge uses this together with the fault ledger so a successful
    /// retry can publish completion only after both owner and debt are gone.
    fn is_empty(&self) -> bool {
        self.owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none()
    }

    /// Retries the quarantined owner with a caller-owned bounded deadline.
    fn retry_until(
        &self,
        deadline: Instant,
        cleanup_fault: &CleanupFault,
    ) -> Result<(), RuntimeCommandError> {
        let Some(mut owner) = self
            .owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return Ok(());
        };
        let generation = match &owner {
            QuarantineOwner::Runtime(current) => current.generation,
            QuarantineOwner::Pending(supervisor) => supervisor.generation(),
        };
        let result = match &mut owner {
            QuarantineOwner::Runtime(current) => shutdown_components(current, deadline),
            QuarantineOwner::Pending(supervisor) => shutdown_supervisor_until(supervisor, deadline),
        };
        if let Err(error) = result {
            cleanup_fault.mark(generation);
            *self
                .owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(owner);
            return Err(error);
        }
        cleanup_fault.clear(generation);
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = RuntimeStatus {
            status: RuntimeStatusKind::Stopped,
            generation: 0,
            server_instance_id: None,
        };
        Ok(())
    }
}

struct ActorContext {
    config: LaunchConfig,
    sink: EventSink,
    signal_sender: SyncSender<BridgeSignal>,
    current_generation: Arc<AtomicU64>,
    terminal_fault: Arc<TerminalFault>,
    detached_event_generation: Arc<AtomicU64>,
    cleanup_fault: Arc<CleanupFault>,
    exit_control: Arc<ExitControl>,
    quarantine: Arc<ExitQuarantine>,
    shutdown_completed: Arc<AtomicBool>,
    completion: Arc<Completion>,
    #[cfg(feature = "tauri-smoke")]
    shutdown_failure_injector: Option<Arc<AtomicUsize>>,
    #[cfg(feature = "test-support")]
    queue_gate: Option<Arc<QueueTestGate>>,
    #[cfg(feature = "test-support")]
    start_failure_gate: Option<Arc<StartFailureGate>>,
    #[cfg(feature = "test-support")]
    test_trace: Option<Arc<BridgeTraceState>>,
    #[cfg(feature = "test-support")]
    test_phase: Arc<AtomicU8>,
}

/// Serializes supervisor operations while independently polling EventPump.
fn actor_loop(
    context: ActorContext,
    command_receiver: Receiver<BridgeCommand>,
    shutdown_receiver: Receiver<ShutdownRequest>,
    signal_receiver: Receiver<BridgeSignal>,
    terminal_receiver: Receiver<()>,
) {
    #[cfg(feature = "test-support")]
    context
        .test_phase
        .store(PHASE_ACTOR_ENTER, Ordering::Release);
    #[cfg(feature = "test-support")]
    trace_event(&context.test_trace, "actor_enter");
    #[cfg(feature = "test-support")]
    if let Some(queue_gate) = context.queue_gate.as_ref() {
        queue_gate.wait();
    }
    let mut runtime: Option<RunningRuntime> = None;
    let mut pending_cleanup: Option<SidecarSupervisor> = None;
    let mut next_generation = 1_u64;
    let mut shutdown_completed = false;
    loop {
        if let Ok(request) = shutdown_receiver.try_recv() {
            #[cfg(feature = "test-support")]
            context
                .test_phase
                .store(PHASE_SHUTDOWN_RECEIVED, Ordering::Release);
            #[cfg(feature = "test-support")]
            trace_event(&context.test_trace, "actor_shutdown_received");
            let result = stop_runtime(
                &context.sink,
                &mut runtime,
                &mut pending_cleanup,
                &context.current_generation,
                &context.terminal_fault,
                &context.cleanup_fault,
                request.deadline,
            );
            #[cfg(feature = "test-support")]
            trace_event(
                &context.test_trace,
                if result.is_ok() {
                    "actor_shutdown_cleanup_ok"
                } else {
                    "actor_shutdown_cleanup_err"
                },
            );
            let cleanup_confirmed = cleanup_confirmed(
                &runtime,
                &pending_cleanup,
                &context.cleanup_fault,
                &context.detached_event_generation,
            );
            if cleanup_confirmed {
                #[cfg(feature = "test-support")]
                context
                    .test_phase
                    .store(PHASE_SHUTDOWN_CONFIRMED, Ordering::Release);
                #[cfg(feature = "test-support")]
                trace_event(&context.test_trace, "actor_shutdown_confirmed");
                shutdown_completed = true;
                context.shutdown_completed.store(true, Ordering::Release);
            }
            // Publish the lifecycle `shutdown_completed` flag before
            // acknowledging this request; the separate actor Completion gate
            // is marked after the reply, so the caller still waits for join
            // ownership without conflating the two state transitions.
            let _ = request.reply.send(result.map(|_| ()));
            if cleanup_confirmed {
                break;
            }
        }
        if let Some(deadline) = context.exit_control.deadline() {
            if cleanup_confirmed(
                &runtime,
                &pending_cleanup,
                &context.cleanup_fault,
                &context.detached_event_generation,
            ) {
                shutdown_completed = true;
                context.shutdown_completed.store(true, Ordering::Release);
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            if let Err(error) = stop_runtime(
                &context.sink,
                &mut runtime,
                &mut pending_cleanup,
                &context.current_generation,
                &context.terminal_fault,
                &context.cleanup_fault,
                deadline,
            ) {
                tracing::error!(?error, "runtime exit cleanup retry was not confirmed");
            }
            if !cleanup_confirmed(
                &runtime,
                &pending_cleanup,
                &context.cleanup_fault,
                &context.detached_event_generation,
            ) {
                thread::park_timeout(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(ACTOR_POLL_TIMEOUT),
                );
            }
            continue;
        }
        drain_signals(&mut runtime, &signal_receiver);
        drain_terminal_fault(
            TerminalCleanupContext {
                config: &context.config,
                sink: &context.sink,
                runtime: &mut runtime,
                current_generation: &context.current_generation,
                terminal_fault: &context.terminal_fault,
                cleanup_fault: &context.cleanup_fault,
                exit_control: &context.exit_control,
            },
            &terminal_receiver,
        );
        match command_receiver.recv_timeout(ACTOR_POLL_TIMEOUT) {
            Ok(BridgeCommand::Start { reply }) => {
                #[cfg(feature = "test-support")]
                context
                    .test_phase
                    .store(PHASE_START_RECEIVED, Ordering::Release);
                #[cfg(feature = "test-support")]
                trace_event(&context.test_trace, "actor_start_received");
                let result = start_runtime(StartRuntimeContext {
                    config: &context.config,
                    sink: &context.sink,
                    runtime: &mut runtime,
                    pending_cleanup: &mut pending_cleanup,
                    next_generation: &mut next_generation,
                    signal_sender: &context.signal_sender,
                    terminal_fault: &context.terminal_fault,
                    detached_event_generation: &context.detached_event_generation,
                    cleanup_fault: &context.cleanup_fault,
                    current_generation: &context.current_generation,
                    exit_control: &context.exit_control,
                    #[cfg(feature = "test-support")]
                    test_trace: &context.test_trace,
                    #[cfg(feature = "test-support")]
                    start_failure_gate: context.start_failure_gate.clone(),
                    #[cfg(feature = "tauri-smoke")]
                    shutdown_failure_injector: context.shutdown_failure_injector.clone(),
                });
                #[cfg(feature = "test-support")]
                trace_event(
                    &context.test_trace,
                    if result.is_ok() {
                        "actor_start_reply_ok"
                    } else {
                        "actor_start_reply_err"
                    },
                );
                #[cfg(feature = "test-support")]
                if result.is_err() {
                    context
                        .test_phase
                        .store(PHASE_START_FAILED, Ordering::Release);
                }
                let _ = reply.send(result);
            }
            Ok(BridgeCommand::Stop { reply }) => {
                #[cfg(feature = "test-support")]
                context
                    .test_phase
                    .store(PHASE_STOP_RECEIVED, Ordering::Release);
                #[cfg(feature = "test-support")]
                trace_event(&context.test_trace, "actor_stop_received");
                let result = stop_runtime(
                    &context.sink,
                    &mut runtime,
                    &mut pending_cleanup,
                    &context.current_generation,
                    &context.terminal_fault,
                    &context.cleanup_fault,
                    shutdown_deadline(context.config.shutdown_timeout),
                );
                #[cfg(feature = "test-support")]
                trace_event(
                    &context.test_trace,
                    if result.is_ok() {
                        "actor_stop_reply_ok"
                    } else {
                        "actor_stop_reply_err"
                    },
                );
                #[cfg(feature = "test-support")]
                context
                    .test_phase
                    .store(PHASE_STOP_REPLIED, Ordering::Release);
                let _ = reply.send(result);
            }
            Ok(BridgeCommand::State { reply }) => {
                let _ = reply.send(Ok(snapshot(
                    &mut runtime,
                    &context.cleanup_fault,
                    pending_cleanup.as_ref(),
                )));
            }
            #[cfg(feature = "test-support")]
            Ok(BridgeCommand::QueueProbe) => {
                if let Some(queue_gate) = context.queue_gate.as_ref() {
                    queue_gate.mark_processed();
                }
            }
            Ok(BridgeCommand::TurnStart { params, reply }) => {
                let _ = reply.send(turn_runtime(
                    &context.config,
                    &context.sink,
                    &mut runtime,
                    params,
                    &context.exit_control,
                ));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                #[cfg(feature = "test-support")]
                trace_event(&context.test_trace, "actor_command_disconnected");
                context.exit_control.trigger();
                break;
            }
        }
    }
    if !shutdown_completed {
        let deadline = context.exit_control.trigger().deadline;
        while !cleanup_confirmed(
            &runtime,
            &pending_cleanup,
            &context.cleanup_fault,
            &context.detached_event_generation,
        ) && Instant::now() < deadline
        {
            if let Err(error) = stop_runtime(
                &context.sink,
                &mut runtime,
                &mut pending_cleanup,
                &context.current_generation,
                &context.terminal_fault,
                &context.cleanup_fault,
                deadline,
            ) {
                tracing::error!(?error, "runtime actor cleanup retry was not confirmed");
            }
            if !cleanup_confirmed(
                &runtime,
                &pending_cleanup,
                &context.cleanup_fault,
                &context.detached_event_generation,
            ) {
                thread::park_timeout(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(ACTOR_POLL_TIMEOUT),
                );
            }
        }
        if !cleanup_confirmed(
            &runtime,
            &pending_cleanup,
            &context.cleanup_fault,
            &context.detached_event_generation,
        ) {
            if let Some(current) = runtime.take() {
                context
                    .quarantine
                    .retain_runtime(current, &context.cleanup_fault);
            }
            if let Some(supervisor) = pending_cleanup.take() {
                context
                    .quarantine
                    .retain_pending(supervisor, &context.cleanup_fault);
            }
        }
        if cleanup_confirmed(
            &runtime,
            &pending_cleanup,
            &context.cleanup_fault,
            &context.detached_event_generation,
        ) {
            context.shutdown_completed.store(true, Ordering::Release);
        }
    }
    context.terminal_fault.clear();
    #[cfg(feature = "test-support")]
    trace_event(
        &context.test_trace,
        if shutdown_completed {
            "actor_completion_shutdown"
        } else {
            "actor_completion_unconfirmed"
        },
    );
    #[cfg(feature = "test-support")]
    context
        .test_phase
        .store(PHASE_ACTOR_COMPLETED, Ordering::Release);
    context.completion.mark_done();
}

/// Actor completion is legal only after every runtime/process/event owner and
/// every cleanup debt ledger have reached a confirmed empty state.
fn cleanup_confirmed(
    runtime: &Option<RunningRuntime>,
    pending_cleanup: &Option<SidecarSupervisor>,
    cleanup_fault: &CleanupFault,
    detached_event_generation: &AtomicU64,
) -> bool {
    runtime.is_none()
        && pending_cleanup.is_none()
        && !cleanup_fault.is_pending()
        && detached_event_generation.load(Ordering::Acquire) == 0
}

/// Keeps start dependencies explicit so no command path can inject a process.
struct StartRuntimeContext<'a> {
    config: &'a LaunchConfig,
    sink: &'a EventSink,
    runtime: &'a mut Option<RunningRuntime>,
    pending_cleanup: &'a mut Option<SidecarSupervisor>,
    next_generation: &'a mut u64,
    signal_sender: &'a SyncSender<BridgeSignal>,
    terminal_fault: &'a Arc<TerminalFault>,
    detached_event_generation: &'a Arc<AtomicU64>,
    cleanup_fault: &'a Arc<CleanupFault>,
    current_generation: &'a Arc<AtomicU64>,
    exit_control: &'a Arc<ExitControl>,
    #[cfg(feature = "test-support")]
    test_trace: &'a Option<Arc<BridgeTraceState>>,
    #[cfg(feature = "test-support")]
    start_failure_gate: Option<Arc<StartFailureGate>>,
    #[cfg(feature = "tauri-smoke")]
    shutdown_failure_injector: Option<Arc<AtomicUsize>>,
}

/// Starts at most one external generation and emits a token-free ready state.
fn start_runtime(context: StartRuntimeContext<'_>) -> Result<RuntimeStatus, RuntimeCommandError> {
    let StartRuntimeContext {
        config,
        sink,
        runtime,
        pending_cleanup,
        next_generation,
        signal_sender,
        terminal_fault,
        detached_event_generation,
        cleanup_fault,
        current_generation,
        exit_control,
        #[cfg(feature = "test-support")]
        test_trace,
        #[cfg(feature = "test-support")]
        start_failure_gate,
        #[cfg(feature = "tauri-smoke")]
        shutdown_failure_injector,
    } = context;
    if exit_control.is_cancelled() {
        return Err(RuntimeCommandError::shutdown_timeout());
    }
    if detached_event_generation.load(Ordering::Acquire) != 0
        || cleanup_fault.is_pending()
        || pending_cleanup.is_some()
    {
        return Err(RuntimeCommandError::shutdown_timeout());
    }
    if let Some(current) = runtime.as_mut() {
        return Ok(current_status(current));
    }
    emit_status(sink, RuntimeStatusKind::Starting, 0, None, "starting")?;
    #[cfg(feature = "test-support")]
    if let Some(gate) = start_failure_gate.as_ref() {
        #[cfg(feature = "test-support")]
        trace_event(test_trace, "start_failure_gate_armed");
        gate.wait();
        #[cfg(feature = "test-support")]
        trace_event(test_trace, "start_failure_gate_released");
    }
    #[cfg(feature = "test-support")]
    trace_event(test_trace, "start_supervisor_new");
    #[cfg(feature = "tauri-smoke")]
    let mut supervisor = if let Some(injector) = shutdown_failure_injector {
        SidecarSupervisor::new_with_shutdown_failure_injector(config.sidecar.clone(), injector)
    } else {
        SidecarSupervisor::new(config.sidecar.clone())
    }
    .map_err(|error| {
        #[cfg(feature = "test-support")]
        trace_event(test_trace, "start_supervisor_new_error");
        RuntimeCommandError::from_process(&error)
    })?;
    #[cfg(not(feature = "tauri-smoke"))]
    let mut supervisor = SidecarSupervisor::new(config.sidecar.clone()).map_err(|error| {
        #[cfg(feature = "test-support")]
        trace_event(test_trace, "start_supervisor_new_error");
        RuntimeCommandError::from_process(&error)
    })?;
    let session_hook = |session: Session| exit_control.attach_session(session);
    let _session_cancellation_guard = SessionCancellationGuard::new(exit_control);
    let start_result = if let Some(deadline) = exit_control.deadline() {
        supervisor.start_until_with_session_hook(deadline, Some(&session_hook))
    } else {
        supervisor.start_with_session_hook(Some(&session_hook))
    };
    if let Err(error) = start_result {
        let status = RuntimeStatusKind::from_lifecycle(supervisor.state());
        if let Err(emit_error) =
            emit_status(sink, status, supervisor.generation(), None, "start_failed")
        {
            tracing::error!(
                ?emit_error,
                "sidecar start failure status projection failed"
            );
        }
        retain_failed_supervisor(
            supervisor,
            pending_cleanup,
            cleanup_fault,
            cleanup_deadline(exit_control, config.shutdown_timeout),
        );
        return Err(RuntimeCommandError::from_process(&error));
    }
    if exit_control.is_cancelled() {
        retain_failed_supervisor(
            supervisor,
            pending_cleanup,
            cleanup_fault,
            cleanup_deadline(exit_control, config.shutdown_timeout),
        );
        return Err(RuntimeCommandError::shutdown_timeout());
    }
    let supervisor_generation = supervisor.generation();
    let version_timeout = match operation_timeout(config.request_timeout, exit_control) {
        Ok(timeout) => timeout,
        Err(error) => {
            retain_failed_supervisor(
                supervisor,
                pending_cleanup,
                cleanup_fault,
                cleanup_deadline(exit_control, config.shutdown_timeout),
            );
            return Err(error);
        }
    };
    let response = match supervisor.request("version", json!({}), version_timeout) {
        Ok(response) => response,
        Err(error) => {
            retain_failed_supervisor(
                supervisor,
                pending_cleanup,
                cleanup_fault,
                cleanup_deadline(exit_control, config.shutdown_timeout),
            );
            return Err(RuntimeCommandError::from_process(&error));
        }
    };
    let version = match frame_to_value(&response) {
        Ok(version) => version,
        Err(_) => {
            retain_failed_supervisor(
                supervisor,
                pending_cleanup,
                cleanup_fault,
                cleanup_deadline(exit_control, config.shutdown_timeout),
            );
            return Err(RuntimeCommandError::unavailable());
        }
    };
    let server_instance_id = match version
        .get("result")
        .and_then(|result| result.get("serverInstanceId"))
        .and_then(Value::as_str)
        .filter(|value| value.starts_with("srv_") && value.len() <= 128)
    {
        Some(value) => value.to_owned(),
        None => {
            retain_failed_supervisor(
                supervisor,
                pending_cleanup,
                cleanup_fault,
                cleanup_deadline(exit_control, config.shutdown_timeout),
            );
            return Err(RuntimeCommandError::unavailable());
        }
    };
    let events = match supervisor.take_event_pump() {
        Ok(events) => events,
        Err(error) => {
            retain_failed_supervisor(
                supervisor,
                pending_cleanup,
                cleanup_fault,
                cleanup_deadline(exit_control, config.shutdown_timeout),
            );
            return Err(RuntimeCommandError::from_process(&error));
        }
    };
    let generation = *next_generation;
    *next_generation = next_generation.saturating_add(1);
    current_generation.store(generation, Ordering::Release);
    terminal_fault.clear();
    let event_drain = match spawn_event_drain(EventDrainSpec {
        events,
        generation,
        supervisor_generation,
        server_instance_id: server_instance_id.clone(),
        sink: sink.clone(),
        server_request_sender: signal_sender.clone(),
        terminal_fault: Arc::clone(terminal_fault),
        current_generation: Arc::clone(current_generation),
        detached_event_generation: Arc::clone(detached_event_generation),
    }) {
        Ok(event_drain) => event_drain,
        Err(error) => {
            current_generation.store(0, Ordering::Release);
            retain_failed_supervisor(
                supervisor,
                pending_cleanup,
                cleanup_fault,
                cleanup_deadline(exit_control, config.shutdown_timeout),
            );
            return Err(error);
        }
    };
    let current = RunningRuntime {
        supervisor,
        event_drain,
        generation,
        server_instance_id: server_instance_id.clone(),
    };
    if let Err(error) = emit_status(
        sink,
        RuntimeStatusKind::Ready,
        generation,
        Some(&server_instance_id),
        "ready",
    ) {
        current_generation.store(0, Ordering::Release);
        let mut current = current;
        if let Err(cleanup_error) = shutdown_components(
            &mut current,
            cleanup_deadline(exit_control, config.shutdown_timeout),
        ) {
            cleanup_fault.mark(generation);
            *runtime = Some(current);
            tracing::error!(
                ?cleanup_error,
                generation,
                "ready projection cleanup deferred"
            );
        } else {
            cleanup_fault.clear(generation);
        }
        return Err(error);
    }
    let status = RuntimeStatus {
        status: RuntimeStatusKind::Ready,
        generation,
        server_instance_id: Some(server_instance_id),
    };
    *runtime = Some(current);
    Ok(status)
}

/// Stops the event worker and supervisor under one absolute deadline; the
/// process close is allowed to unblock a worker that did not observe cancel.
fn shutdown_components(
    current: &mut RunningRuntime,
    deadline: Instant,
) -> Result<(), RuntimeCommandError> {
    current.event_drain.request_stop();
    let grace_deadline = std::cmp::min(
        deadline,
        Instant::now()
            .checked_add(EVENT_CANCEL_GRACE)
            .unwrap_or(deadline),
    );
    let _ = current.event_drain.wait_until(grace_deadline);
    let remaining = deadline.saturating_duration_since(Instant::now());
    let supervisor_result = if remaining.is_zero() {
        Err(RuntimeCommandError::shutdown_timeout())
    } else {
        current
            .supervisor
            .shutdown_until(deadline)
            .map_err(|error| RuntimeCommandError::from_process(&error))
    };
    let event_result = current.event_drain.finish(deadline);
    supervisor_result.and(event_result)
}

/// Performs bounded event-worker and process-tree shutdown in that order.
fn stop_runtime(
    sink: &EventSink,
    runtime: &mut Option<RunningRuntime>,
    pending_cleanup: &mut Option<SidecarSupervisor>,
    current_generation: &Arc<AtomicU64>,
    terminal_fault: &Arc<TerminalFault>,
    cleanup_fault: &Arc<CleanupFault>,
    deadline: Instant,
) -> Result<RuntimeStatus, RuntimeCommandError> {
    if let Some(mut supervisor) = pending_cleanup.take() {
        let generation = supervisor.generation();
        if let Err(error) = shutdown_supervisor_until(&mut supervisor, deadline) {
            cleanup_fault.mark(generation);
            *pending_cleanup = Some(supervisor);
            return Err(error);
        }
        cleanup_fault.clear(generation);
    }
    let Some(mut current) = runtime.take() else {
        if cleanup_fault.is_pending() {
            return Err(RuntimeCommandError::shutdown_timeout());
        }
        current_generation.store(0, Ordering::Release);
        terminal_fault.clear();
        emit_status(sink, RuntimeStatusKind::Stopped, 0, None, "stopped")?;
        return Ok(RuntimeStatus {
            status: RuntimeStatusKind::Stopped,
            generation: 0,
            server_instance_id: None,
        });
    };
    current_generation.store(0, Ordering::Release);
    terminal_fault.clear();
    let generation = current.generation;
    let server_instance_id = current.server_instance_id.clone();
    let result = shutdown_components(&mut current, deadline);
    if result.is_err() {
        // Keep the supervisor and worker handles owned by the actor so a later
        // high-priority shutdown can retry the same generation.  The fault is
        // deliberately not cleared by actor termination or status emission.
        cleanup_fault.mark(current.generation);
        *runtime = Some(current);
    } else {
        cleanup_fault.clear(generation);
    }
    let status = RuntimeStatus {
        status: if result.is_ok() {
            RuntimeStatusKind::Stopped
        } else {
            RuntimeStatusKind::Crashed
        },
        generation,
        server_instance_id: Some(server_instance_id.clone()),
    };
    let reason = if result.is_ok() {
        "stopped"
    } else {
        "stop_failed"
    };
    let emit_result = emit_status(
        sink,
        status.status,
        status.generation,
        Some(&server_instance_id),
        reason,
    );
    result.and(emit_result).map(|_| status)
}

/// Gives every failed pre-runtime owner one cleanup attempt under the same
/// absolute deadline, retaining its process/session when confirmation fails.
fn retain_failed_supervisor(
    mut supervisor: SidecarSupervisor,
    pending_cleanup: &mut Option<SidecarSupervisor>,
    cleanup_fault: &Arc<CleanupFault>,
    deadline: Instant,
) {
    let generation = supervisor.generation();
    if let Err(error) = shutdown_supervisor_until(&mut supervisor, deadline) {
        cleanup_fault.mark(generation);
        *pending_cleanup = Some(supervisor);
        tracing::error!(?error, generation, "sidecar failure cleanup deferred");
    } else {
        cleanup_fault.clear(generation);
    }
}

/// Converts the remaining absolute budget once; the supervisor never creates
/// a fresh timeout that could extend a Tauri exit past its caller's deadline.
fn shutdown_supervisor_until(
    supervisor: &mut SidecarSupervisor,
    deadline: Instant,
) -> Result<(), RuntimeCommandError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(RuntimeCommandError::shutdown_timeout());
    }
    supervisor
        .shutdown_until(deadline)
        .map_err(|error| RuntimeCommandError::from_process(&error))
}

/// Creates a monotonic deadline at the owner boundary; descendants only
/// consume the returned instant and never reset the shutdown budget.
fn shutdown_deadline(timeout: Duration) -> Instant {
    Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now)
}

/// Bounds a protocol request by the immutable exit deadline when shutdown is
/// already in flight, while preserving normal request limits otherwise.
fn operation_timeout(
    configured: Duration,
    exit_control: &ExitControl,
) -> Result<Duration, RuntimeCommandError> {
    if let Some(deadline) = exit_control.deadline() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RuntimeCommandError::shutdown_timeout());
        }
        Ok(configured.min(remaining))
    } else {
        Ok(configured)
    }
}

/// Reuses the first exit deadline for cleanup, creating a local budget only
/// when no application exit has been requested yet.
fn cleanup_deadline(exit_control: &ExitControl, fallback: Duration) -> Instant {
    exit_control
        .deadline()
        .unwrap_or_else(|| shutdown_deadline(fallback))
}

/// Sends one typed turn request; timeline facts remain EventPump events.
fn turn_runtime(
    config: &LaunchConfig,
    sink: &EventSink,
    runtime: &mut Option<RunningRuntime>,
    params: Value,
    exit_control: &ExitControl,
) -> Result<TurnAccepted, RuntimeCommandError> {
    let Some(current) = runtime.as_mut() else {
        return Err(RuntimeCommandError {
            code: "RUNTIME_NOT_READY",
            message: "runtime is not ready",
            retryable: true,
        });
    };
    if let Some(session) = current.supervisor.session_for_cancellation() {
        exit_control.attach_session(session);
    }
    let _session_cancellation_guard = SessionCancellationGuard::new(exit_control);
    let timeout = operation_timeout(config.request_timeout, exit_control)?;
    let response = current
        .supervisor
        .request("turn/start", params, timeout)
        .map_err(|error| RuntimeCommandError::from_process(&error))?;
    let value = frame_to_value(&response).map_err(|_| RuntimeCommandError::unavailable())?;
    if let Some(error) = value.get("error") {
        return Err(command_error_from_rpc(error));
    }
    let result = value
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(RuntimeCommandError::unavailable)?;
    let accepted = result
        .get("accepted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let turn_id = result
        .get("turnId")
        .and_then(Value::as_str)
        .filter(|value| valid_id(value, 128))
        .ok_or_else(RuntimeCommandError::unavailable)?;
    let queued = result
        .get("queued")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let status = result
        .get("status")
        .and_then(Value::as_str)
        .filter(|value| valid_id(value, 64))
        .ok_or_else(RuntimeCommandError::unavailable)?;
    emit_status(
        sink,
        RuntimeStatusKind::Busy,
        current.generation,
        Some(&current.server_instance_id),
        "turn_started",
    )?;
    Ok(TurnAccepted {
        accepted,
        turn_id: turn_id.to_owned(),
        queued,
        status: status.to_owned(),
    })
}

/// Keeps the foundation EventPump as the sole reader and routes only its
/// validated observations back to the lifecycle actor.
fn spawn_event_drain(spec: EventDrainSpec) -> Result<EventDrain, RuntimeCommandError> {
    let (cancel, cancel_receiver) = mpsc::sync_channel(1);
    let completion = Arc::new(Completion::new());
    let worker_completion = Arc::clone(&completion);
    let generation = spec.generation;
    let detached_event_generation = Arc::clone(&spec.detached_event_generation);
    let join = thread::Builder::new()
        .name(format!("ja-runtime-events-{generation}"))
        .spawn(move || {
            event_drain_loop(EventDrainContext {
                events: spec.events,
                generation: spec.generation,
                supervisor_generation: spec.supervisor_generation,
                server_instance_id: spec.server_instance_id,
                sink: spec.sink,
                server_request_sender: spec.server_request_sender,
                terminal_fault: spec.terminal_fault,
                current_generation: spec.current_generation,
                cancel_receiver,
            });
            let _ = detached_event_generation.compare_exchange(
                generation,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            worker_completion.mark_done();
        })
        .map_err(|_| RuntimeCommandError::unavailable())?;
    Ok(EventDrain {
        cancel,
        completion,
        generation,
        detached_event_generation: spec.detached_event_generation,
        join: Some(join),
    })
}

/// Polls notifications continuously, rejecting stale identity before the UI.
fn event_drain_loop(mut context: EventDrainContext) {
    loop {
        if !event_is_current(
            &context.cancel_receiver,
            &context.current_generation,
            context.generation,
        ) {
            break;
        }
        let Some(event) = context.events.next_event(EVENT_POLL_TIMEOUT) else {
            continue;
        };
        // The event pump can return concurrently with stop/new-generation;
        // rechecking here prevents a stale frame from reaching the WebView.
        if !event_is_current(
            &context.cancel_receiver,
            &context.current_generation,
            context.generation,
        ) {
            break;
        }
        match event {
            SessionEvent::Notification(frame) => {
                if !notification_matches_identity(&frame, &context.server_instance_id) {
                    continue;
                }
                if emit_frame(&context.sink, &frame).is_err() {
                    signal_projection_failure(&context.terminal_fault, context.generation);
                    break;
                }
            }
            SessionEvent::ServerRequest(frame) => {
                if context
                    .server_request_sender
                    .try_send(BridgeSignal::ServerRequest {
                        generation: context.generation,
                        frame,
                    })
                    .is_err()
                {
                    tracing::error!("runtime server-request signal queue is full");
                }
            }
            SessionEvent::QueueOverflow(_) => {
                if emit_status(
                    &context.sink,
                    RuntimeStatusKind::Busy,
                    context.generation,
                    Some(&context.server_instance_id),
                    "event_queue_overflow",
                )
                .is_err()
                {
                    signal_projection_failure(&context.terminal_fault, context.generation);
                    break;
                }
            }
            SessionEvent::ProcessExited {
                generation: observed,
                ..
            } if observed == context.supervisor_generation => {
                context
                    .terminal_fault
                    .publish(context.generation, TERMINAL_SIDECAR);
                break;
            }
            SessionEvent::Eof
            | SessionEvent::HandshakeFailed
            | SessionEvent::WriterTimedOut
            | SessionEvent::ProtocolFault(_)
            | SessionEvent::QueueFatalOverflow(_)
            | SessionEvent::ResponseRejected(_) => {
                context
                    .terminal_fault
                    .publish(context.generation, TERMINAL_SIDECAR);
                break;
            }
            SessionEvent::ProcessExited { .. }
            | SessionEvent::StderrLine(_)
            | SessionEvent::StderrTruncated => {}
        }
    }
}

/// Checks cancellation both before polling and before emitting an event.
fn event_is_current(
    cancel_receiver: &Receiver<()>,
    current_generation: &AtomicU64,
    generation: u64,
) -> bool {
    cancel_receiver.try_recv().is_err() && generation_is_current(current_generation, generation)
}

/// Keeps the generation comparison pure so the stop/new-generation race can
/// be tested without constructing a live EventPump or opening a second reader.
fn generation_is_current(current_generation: &AtomicU64, generation: u64) -> bool {
    current_generation.load(Ordering::Acquire) == generation
}

/// Reports projection failure through the reserved terminal lane rather than
/// sharing capacity with server requests.
fn signal_projection_failure(fault: &Arc<TerminalFault>, generation: u64) {
    fault.publish(generation, TERMINAL_EVENT_DELIVERY);
}

/// Handles asynchronous terminal facts without allowing a stale generation to
/// mutate a newly started supervisor.
fn drain_signals(runtime: &mut Option<RunningRuntime>, receiver: &Receiver<BridgeSignal>) {
    while let Ok(signal) = receiver.try_recv() {
        match signal {
            BridgeSignal::ServerRequest { generation, frame } => {
                if runtime
                    .as_ref()
                    .is_some_and(|current| current.generation == generation)
                    && let Some(current) = runtime.as_mut()
                    && let Ok(client) = current.supervisor.client()
                {
                    let _ = client.respond_error(
                        frame.id(),
                        -32_006,
                        "method not found",
                        "METHOD_NOT_FOUND",
                        false,
                    );
                }
            }
        }
    }
}

/// Groups terminal cleanup dependencies so every failure path consumes the
/// same actor-owned exit deadline without widening function signatures.
struct TerminalCleanupContext<'a> {
    config: &'a LaunchConfig,
    sink: &'a EventSink,
    runtime: &'a mut Option<RunningRuntime>,
    current_generation: &'a Arc<AtomicU64>,
    terminal_fault: &'a Arc<TerminalFault>,
    cleanup_fault: &'a Arc<CleanupFault>,
    exit_control: &'a Arc<ExitControl>,
}

/// Drains the reserved terminal wake lane and atomically observes any fault
/// that could not fit in that wake slot while requests were being processed.
fn drain_terminal_fault(context: TerminalCleanupContext<'_>, wake_receiver: &Receiver<()>) {
    let TerminalCleanupContext {
        config,
        sink,
        runtime,
        current_generation,
        terminal_fault,
        cleanup_fault,
        exit_control,
    } = context;
    while wake_receiver.try_recv().is_ok() {}
    let Some(current) = runtime.as_ref() else {
        return;
    };
    let Some(reason) = terminal_fault.take(current.generation) else {
        return;
    };
    terminal_runtime(
        TerminalCleanupContext {
            config,
            sink,
            runtime,
            current_generation,
            terminal_fault,
            cleanup_fault,
            exit_control,
        },
        terminal_reason(reason),
    );
}

/// Converts the compact atomic reason into a stable lifecycle projection.
fn terminal_reason(reason: u8) -> &'static str {
    match reason {
        TERMINAL_EVENT_DELIVERY => "event_delivery",
        _ => "sidecar_terminated",
    }
}

/// Closes a failed generation and publishes a crash projection exactly once.
fn terminal_runtime(context: TerminalCleanupContext<'_>, reason: &str) {
    let TerminalCleanupContext {
        config,
        sink,
        runtime,
        current_generation,
        terminal_fault,
        cleanup_fault,
        exit_control,
    } = context;
    let Some(mut current) = runtime.take() else {
        return;
    };
    current_generation.store(0, Ordering::Release);
    terminal_fault.clear();
    let deadline = cleanup_deadline(exit_control, config.shutdown_timeout);
    let generation = current.generation;
    let server_instance_id = current.server_instance_id.clone();
    if shutdown_components(&mut current, deadline).is_err() {
        // A terminal notification is not proof that the process tree was
        // reaped; retain the runtime owner and block a new generation until a
        // later stop attempt confirms the same generation is clean.
        cleanup_fault.mark(generation);
        *runtime = Some(current);
    } else {
        cleanup_fault.clear(generation);
    }
    if emit_status(
        sink,
        RuntimeStatusKind::Crashed,
        generation,
        Some(&server_instance_id),
        reason,
    )
    .is_err()
    {
        tracing::error!("runtime event projection failed during terminal cleanup");
    }
}

/// Requires the current sidecar identity on every notification/server request.
fn notification_matches_identity(frame: &RpcFrame, expected: &str) -> bool {
    frame
        .params()
        .and_then(|params| params.get("serverInstanceId"))
        .and_then(Value::as_str)
        .is_some_and(|instance| instance == expected)
}

/// Returns a state snapshot while consulting the foundation lifecycle machine.
fn snapshot(
    runtime: &mut Option<RunningRuntime>,
    cleanup_fault: &CleanupFault,
    pending_cleanup: Option<&SidecarSupervisor>,
) -> RuntimeStatus {
    let projected = if let Some(current) = runtime.as_mut() {
        current_status(current)
    } else {
        RuntimeStatus {
            status: RuntimeStatusKind::Stopped,
            generation: 0,
            server_instance_id: None,
        }
    };
    let debt_generation = cleanup_fault
        .generation()
        .max(pending_cleanup.map_or(0, SidecarSupervisor::generation));
    if debt_generation == 0 {
        return projected;
    }
    RuntimeStatus {
        status: RuntimeStatusKind::Crashed,
        generation: debt_generation,
        server_instance_id: projected.server_instance_id,
    }
}

/// Projects the supervisor lifecycle at the actor serialization point so no
/// cached status can outrun a terminal signal.
fn current_status(runtime: &mut RunningRuntime) -> RuntimeStatus {
    RuntimeStatus {
        status: RuntimeStatusKind::from_lifecycle(runtime.supervisor.state()),
        generation: runtime.generation,
        server_instance_id: Some(runtime.server_instance_id.clone()),
    }
}

/// Maps a validated RPC error into the stable command envelope.
fn command_error_from_rpc(value: &Value) -> RuntimeCommandError {
    match value
        .get("data")
        .and_then(|data| data.get("jaCode"))
        .and_then(Value::as_str)
    {
        Some("THREAD_BUSY") => RuntimeCommandError {
            code: "THREAD_BUSY",
            message: "thread is busy",
            retryable: true,
        },
        Some("INVALID_PARAMS") => RuntimeCommandError::invalid_params(),
        _ => RuntimeCommandError::unavailable(),
    }
}

/// Restricts returned identifiers to the same bounded alphabet as input IDs,
/// preventing untrusted sidecar strings from becoming UI control data.
fn valid_id(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ':')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A missing server identity cannot be treated as current because stale
    /// generations otherwise have no routing boundary.
    #[test]
    fn notification_identity_is_required() {
        let missing =
            RpcFrame::notification("turn/completed".to_owned(), json!({"turnId": "turn_1"}))
                .expect("valid notification");
        let stale = RpcFrame::notification(
            "turn/completed".to_owned(),
            json!({"serverInstanceId": "srv_old"}),
        )
        .expect("valid notification");
        let current = RpcFrame::notification(
            "turn/completed".to_owned(),
            json!({"serverInstanceId": "srv_current"}),
        )
        .expect("valid notification");
        assert!(!notification_matches_identity(&missing, "srv_current"));
        assert!(!notification_matches_identity(&stale, "srv_current"));
        assert!(notification_matches_identity(&current, "srv_current"));
    }

    /// A full server-request lane must not consume the reserved terminal
    /// wake slot, otherwise a fault could be silently downgraded to logging.
    #[test]
    fn terminal_fault_survives_full_server_request_lane() {
        let (server_sender, server_receiver) = mpsc::sync_channel(INTERNAL_QUEUE_CAPACITY);
        for index in 0..INTERNAL_QUEUE_CAPACITY {
            let frame = RpcFrame::notification(
                format!("server/request/{index}"),
                json!({"serverInstanceId": "srv_current"}),
            )
            .expect("server request fixture");
            server_sender
                .try_send(BridgeSignal::ServerRequest {
                    generation: 7,
                    frame,
                })
                .expect("server lane capacity");
        }
        let (wake_sender, wake_receiver) = mpsc::sync_channel(1);
        let fault = TerminalFault::new(wake_sender);
        fault.publish(7, TERMINAL_SIDECAR);
        fault.publish(7, TERMINAL_SIDECAR);
        assert!(wake_receiver.try_recv().is_ok());
        assert_eq!(fault.take(7), Some(TERMINAL_SIDECAR));
        assert_eq!(server_receiver.try_iter().count(), INTERNAL_QUEUE_CAPACITY);
    }

    /// A generation change invalidates an event-pump observation before it is
    /// emitted, which protects a new sidecar from stale frames.
    #[test]
    fn event_generation_check_rejects_stale_worker() {
        let (cancel_sender, cancel_receiver) = mpsc::sync_channel(1);
        let current_generation = Arc::new(AtomicU64::new(9));
        assert!(!event_is_current(&cancel_receiver, &current_generation, 7));
        current_generation.store(7, Ordering::Release);
        assert!(event_is_current(&cancel_receiver, &current_generation, 7));
        cancel_sender.try_send(()).expect("cancel fixture");
        assert!(!event_is_current(&cancel_receiver, &current_generation, 7));
    }

    /// A failed kill/reap remains observable until the exact generation is
    /// confirmed clean; actor completion alone cannot clear this ledger.
    #[test]
    fn cleanup_fault_requires_same_generation_confirmation() {
        let fault = CleanupFault::new();
        fault.mark(7);
        assert!(fault.is_pending());
        fault.clear(8);
        assert!(fault.is_pending());
        fault.clear(7);
        assert!(!fault.is_pending());
    }

    /// Cleanup debt must dominate an empty runtime snapshot, otherwise a
    /// failed kill/reap could be exposed to the WebView as an unsafe stopped
    /// state and a new generation could be admitted too early.
    #[test]
    fn cleanup_debt_projects_crashed_instead_of_stopped() {
        let fault = CleanupFault::new();
        fault.mark(7);
        let mut runtime = None;
        let projected = snapshot(&mut runtime, &fault, None);
        assert_eq!(projected.status, RuntimeStatusKind::Crashed);
        assert_eq!(projected.generation, 7);
        assert_eq!(projected.server_instance_id, None);
    }

    /// Exit initiation is a one-shot boundary: later Drop/RunEvent paths may
    /// retry cleanup but cannot move the deadline that bounded the first exit.
    #[test]
    fn exit_control_keeps_one_absolute_deadline() {
        let control = ExitControl::new(Duration::from_secs(30));
        let first = control.trigger();
        let second = control.trigger();
        assert_eq!(first, second);
        assert!(control.is_cancelled());
        assert_eq!(
            cleanup_deadline(&control, Duration::from_secs(1)),
            first.deadline
        );
        let retry = control.retry();
        assert_eq!(retry.id, first.id + 1);
        assert!(retry.deadline >= first.deadline);
    }

    /// The cancellation hook must receive the exact attempt deadline, not a
    /// fresh writer timeout, so a blocked handshake/request cannot overrun the
    /// host's already-started exit budget.
    #[test]
    fn exit_control_passes_absolute_deadline_to_cancellation_hook() {
        let control = ExitControl::new(Duration::from_secs(30));
        let observed = Arc::new(Mutex::new(None));
        let observed_by_hook = Arc::clone(&observed);
        *control
            .cancellation_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(move |deadline| {
            *observed_by_hook
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(deadline);
        }));
        let attempt = control.trigger();
        assert_eq!(
            *observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some(attempt.deadline)
        );
    }

    /// A quarantined fault remains a crash projection after actor completion,
    /// so state cannot report stopped while a retryable owner is outstanding.
    #[test]
    fn quarantine_state_preserves_crashed_projection() {
        let quarantine = ExitQuarantine::new();
        let fault = CleanupFault::new();
        fault.mark(11);
        let state = quarantine.state(&fault);
        assert_eq!(state.status, RuntimeStatusKind::Crashed);
        assert_eq!(state.generation, 11);
        assert!(quarantine.retry_until(Instant::now(), &fault).is_ok());
        assert!(fault.is_pending());
    }
}

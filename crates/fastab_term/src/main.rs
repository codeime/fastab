mod child_exit;
#[cfg(target_os = "linux")]
mod cleanup;
pub mod cli;
mod event_handler;
pub mod history;
pub mod input;
pub mod interceptor;
pub mod ipc;
pub mod logger;
mod message;
pub mod pty;
mod resource_diagnostics;
pub mod term;
pub mod update;

use std::collections::VecDeque;
use std::env;
#[cfg(unix)]
use std::ffi::{CString, OsStr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::ansi::Processor;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::term::{ShellState, SizeInfo};
use anyhow::{Context as _, Result, anyhow};
use bytes::BytesMut;
use cfg_if::cfg_if;
use clap::Parser;
use cli::Cli;
use fastab_log::{LogArgs, initialize_logging};
use fastab_os_shim::{Context, Env};
use fastab_proto::local::{self, EnvironmentVariable, TerminalCursorCoordinates};
use fastab_proto::remote_hooks::{hook_to_message, new_edit_buffer_hook};
use fastab_settings::state;
use fastab_util::env_var::{Q_LOG_LEVEL, Q_SHELL, Q_TERM, QTERM_SESSION_ID};
use fastab_util::process_info::{Pid, PidExt};
use fastab_util::{PRODUCT_NAME, PTY_BINARY_NAME, directories, terminal::current_terminal};
#[cfg(unix)]
use nix::unistd::execvp;
use portable_pty::PtySize;
use tokio::io::{self, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::{runtime, select};
use tracing::{debug, error, info, trace, warn};

use crate::event_handler::{EventHandler, LocalEvents};
use crate::input::{InputEvent, KeyCode, KeyCodeEncodeModes, KeyboardEncoding, Modifiers};
use crate::interceptor::KeyInterceptor;
use crate::ipc::{
    ContextAdmission, ContextProgress, Generation, LocalIncoming, RemoteIncoming, RemoteSender, RequestOrigin,
    spawn_figterm_ipc, spawn_remote_ipc,
};
use crate::message::{process_figterm_message, process_remote_message};
#[cfg(unix)]
use crate::pty::unix::open_pty;
#[cfg(windows)]
use crate::pty::win::open_pty;
use crate::pty::{AsyncMasterPty, AsyncMasterPtyExt, CommandBuilder};
use crate::term::{SystemTerminal, Terminal};

struct DeferredInsert {
    insert: Vec<u8>,
    unlock: bool,
    bracketed: bool,
    execute: bool,
    origin: RequestOrigin,
}

enum MainLoopReady {
    SyncTimeout,
    RemoteStateChanged,
    KeyDeliveryChanged,
    Control(std::result::Result<MainLoopEvent, flume::RecvError>),
    Input(std::result::Result<term::InputEventResult, flume::RecvError>),
    Pty(std::io::Result<usize>),
    Remote(std::result::Result<RemoteIncoming, flume::RecvError>),
    Local(std::result::Result<LocalIncoming, flume::RecvError>),
    InsertionRetry,
    ChildExited,
}

/// Preserve settlement barriers before ordinary I/O and already-ready I/O
/// before child exit. Only readiness futures belong here; handlers run after
/// this returns so a timer cannot cancel a partial PTY or stdout write.
async fn select_main_loop_ready(
    sync_timeout: impl std::future::Future<Output = ()>,
    state_changed: impl std::future::Future,
    key_changed: impl std::future::Future<Output = ()>,
    ordinary_io: (
        impl std::future::Future<Output = std::result::Result<MainLoopEvent, flume::RecvError>>,
        impl std::future::Future<Output = std::result::Result<term::InputEventResult, flume::RecvError>>,
        impl std::future::Future<Output = std::io::Result<usize>>,
        impl std::future::Future<Output = std::result::Result<RemoteIncoming, flume::RecvError>>,
        impl std::future::Future<Output = std::result::Result<LocalIncoming, flume::RecvError>>,
        impl std::future::Future<Output = ()>,
    ),
    child_exited: impl std::future::Future,
) -> MainLoopReady {
    let (control, input, pty, remote, local, retry) = ordinary_io;
    select! {
        biased;
        _ = sync_timeout => MainLoopReady::SyncTimeout,
        _ = state_changed => MainLoopReady::RemoteStateChanged,
        _ = key_changed => MainLoopReady::KeyDeliveryChanged,
        ready = async {
            select! {
                result = control => MainLoopReady::Control(result),
                result = input => MainLoopReady::Input(result),
                result = pty => MainLoopReady::Pty(result),
                result = remote => MainLoopReady::Remote(result),
                result = local => MainLoopReady::Local(result),
                _ = retry => MainLoopReady::InsertionRetry,
            }
        } => ready,
        _ = child_exited => MainLoopReady::ChildExited,
    }
}

/// Replay settled failed intercepted keys, then flush deferred ordinary PTY
/// input and deferred Inserts once no intercepted key is still awaiting write
/// settlement. Never blocks the main loop on in-flight key delivery.
async fn flush_settled_pty_input(
    remote_sender: &RemoteSender,
    master: &mut (dyn AsyncMasterPty + Send),
    deferred: &mut BytesMut,
    deferred_inserts: &mut VecDeque<DeferredInsert>,
    key_interceptor: &mut KeyInterceptor,
    bracketed_paste: bool,
) -> Result<()> {
    resource_diagnostics::deferred(deferred.len(), deferred.capacity(), deferred_inserts.len());
    let result = async {
        for failed in remote_sender.take_failed_keys() {
            master.write_all(&failed).await?;
        }
        if remote_sender.has_pending_keys() {
            return Ok(());
        }
        if !deferred.is_empty() {
            master.write_all(deferred).await?;
            deferred.clear();
            if deferred.capacity() > 64 * 1024 {
                *deferred = BytesMut::new();
            }
        }
        while let Some(item) = deferred_inserts.pop_front() {
            if !item.origin.is_current(remote_sender.phase().ready_generation()) {
                continue;
            }
            write_insert_bytes(master, &item, bracketed_paste).await?;
            if item.unlock {
                key_interceptor.reset();
            }
        }
        Ok(())
    }
    .await;
    resource_diagnostics::deferred(deferred.len(), deferred.capacity(), deferred_inserts.len());
    result
}

async fn finish_pty_input(
    remote_sender: &RemoteSender,
    master: &mut (dyn AsyncMasterPty + Send),
    deferred: &mut BytesMut,
    deferred_inserts: &mut VecDeque<DeferredInsert>,
    key_interceptor: &mut KeyInterceptor,
    bracketed_paste: bool,
) -> Result<()> {
    // EOF cannot overtake an admitted intercepted key. The existing bounded
    // settlement barrier retires an unresponsive connection before replay.
    for failed in remote_sender.take_failed_keys_before_input().await {
        master.write_all(&failed).await?;
    }
    flush_settled_pty_input(
        remote_sender,
        master,
        deferred,
        deferred_inserts,
        key_interceptor,
        bracketed_paste,
    )
    .await
}

async fn write_insert_bytes(
    master: &mut (dyn AsyncMasterPty + Send),
    item: &DeferredInsert,
    bracketed_paste: bool,
) -> Result<()> {
    use bstr::ByteSlice;
    if item.bracketed {
        if bracketed_paste {
            master.write_all(b"\x1b[200~").await?;
            master.write_all(&item.insert.replace(b"\x1b", "")).await?;
            master.write_all(b"\x1b[201~").await?;
        } else {
            master
                .write_all(&item.insert.replace("\r\n", "\r").replace("\n", "\r"))
                .await?;
        }
    } else {
        master.write_all(&item.insert).await?;
    }
    if item.execute {
        master.write_all(b"\r").await?;
    }
    Ok(())
}

#[derive(Default)]
struct InsertionRetry {
    owner: Option<Instant>,
    deadline: Option<tokio::time::Instant>,
}

impl InsertionRetry {
    fn update(&mut self, owner: Option<Instant>) {
        if self.owner != owner {
            self.owner = owner;
            self.deadline = owner.map(|_| tokio::time::Instant::now() + Duration::from_millis(16));
        }
    }

    fn active(&self) -> bool {
        self.deadline.is_some()
    }

    async fn wait(&self) {
        match self.deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    }

    fn tick(&mut self) {
        self.deadline = self
            .owner
            .map(|_| tokio::time::Instant::now() + Duration::from_millis(16));
    }
}

#[derive(Default)]
struct CsiUState {
    // Fastab's successfully written, unpaired push; not the terminal's stack
    // or another application's keyboard mode. Complete shell hooks hand this
    // ownership back before an application runs and acquire it after return.
    owned: bool,
    desired: bool,
}

struct MainLoopControl<'a> {
    remote_sender: &'a RemoteSender,
    master: &'a mut (dyn AsyncMasterPty + Send),
    deferred_pty_input: &'a mut BytesMut,
    deferred_inserts: &'a mut VecDeque<DeferredInsert>,
    key_interceptor: &'a mut KeyInterceptor,
    terminal: &'a mut dyn Terminal,
    stdout: &'a mut (dyn tokio::io::AsyncWrite + Unpin),
    csi_u: &'a mut CsiUState,
    bracketed_paste: bool,
}

impl MainLoopControl<'_> {
    async fn apply(&mut self, event: MainLoopEvent) -> Result<()> {
        match event {
            MainLoopEvent::Insert {
                insert,
                unlock,
                bracketed,
                execute,
                origin,
            } => {
                if !origin.is_current(self.remote_sender.phase().ready_generation()) {
                    return Ok(());
                }
                // Queue behind any still-pending intercepted keys so Insert
                // cannot overtake deferred ordinary input.
                self.deferred_inserts.push_back(DeferredInsert {
                    insert,
                    unlock,
                    bracketed,
                    execute,
                    origin,
                });
                let bracketed_paste = self.bracketed_paste;
                flush_settled_pty_input(
                    self.remote_sender,
                    self.master,
                    self.deferred_pty_input,
                    self.deferred_inserts,
                    self.key_interceptor,
                    bracketed_paste,
                )
                .await?;
            },
            MainLoopEvent::UnlockInterception => {
                self.key_interceptor.reset();
            },
            MainLoopEvent::SetImmediateMode(mode) => {
                if let Err(err) = self.terminal.set_immediate_mode(mode) {
                    error!(%err, "Failed to set immediate mode");
                }
            },
            MainLoopEvent::SetCsiU => {
                self.csi_u.desired = true;
            },
            MainLoopEvent::UnsetCsiU => {
                self.csi_u.desired = false;
            },
        }
        Ok(())
    }

    fn csi_u_pending(&self) -> bool {
        self.csi_u.desired != self.csi_u.owned
    }

    async fn flush_csi_u(&mut self) -> Result<()> {
        if self.csi_u_pending() {
            let sequence: &[u8] = if self.csi_u.desired { b"\x1b[>1u" } else { b"\x1b[<u" };
            self.stdout.write_all(sequence).await?;
            self.stdout.flush().await?;
            self.csi_u.owned = self.csi_u.desired;
        }
        Ok(())
    }

    async fn drain_local(&mut self, events: &LocalEvents) -> Result<()> {
        while let Some(event) = events.pop() {
            self.apply(event).await?;
        }
        Ok(())
    }
}

async fn process_pty_bytes(
    bytes: &[u8],
    processor: &mut Processor,
    term: &mut Term<EventHandler>,
    events: &LocalEvents,
    control: &mut MainLoopControl<'_>,
) -> Result<()> {
    let old_delayed_count = term.get_delayed_events_count();
    let mut forwarded = 0;
    for (index, byte) in bytes.iter().enumerate() {
        processor.advance(term, *byte);
        let boundary = processor.is_output_boundary();
        if events.has_pending() || (boundary && control.csi_u_pending()) {
            // The real terminal must see all output up to this OSC before
            // applying its controls. In particular, CSI-u pushes/pops belong
            // to the screen selected by preceding alternate-screen changes.
            // Drain non-output controls immediately. VTE can emit OSC events
            // at ESC; only the desired CSI-u state waits for complete ST or a
            // replacement sequence. An unterminated OSC chain cannot retain
            // an ever-growing queue, and repeated modes own at most one push.
            let end = index + 1;
            control.stdout.write_all(&bytes[forwarded..end]).await?;
            forwarded = end;
            control.bracketed_paste = bracketed_paste_enabled(term);
            control.drain_local(events).await?;
            if boundary {
                control.flush_csi_u().await?;
            }
        }
    }
    control.stdout.write_all(&bytes[forwarded..]).await?;

    // Preserve the delayed prompt policy, but forward this read's entire raw
    // output before controls produced by the end-of-read flush.
    let delayed_count = term.get_delayed_events_count();
    if delayed_count > 0 && delayed_count == old_delayed_count {
        term.flush_delayed_events();
        control.bracketed_paste = bracketed_paste_enabled(term);
        control.drain_local(events).await?;
    }
    if processor.is_output_boundary() {
        control.flush_csi_u().await?;
    }
    control.stdout.flush().await?;
    record_sync_output(processor);
    Ok(())
}

fn bracketed_paste_enabled<T: EventListener>(term: &Term<T>) -> bool {
    term.mode()
        .contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE)
}

fn record_sync_output(processor: &Processor) {
    resource_diagnostics::sync_output(
        processor.sync_bytes_count(),
        processor.sync_buffer_capacity(),
        processor.sync_timeout().is_some(),
    );
}

async fn wait_for_pty_sync_timeout(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

async fn finish_pty_sync(
    processor: &mut Processor,
    term: &mut Term<EventHandler>,
    events: &LocalEvents,
    control: &mut MainLoopControl<'_>,
) -> Result<()> {
    // Every retained byte has already been forwarded by process_pty_bytes.
    // Replay changes the tracking Term only; controls follow that raw output.
    processor.stop_sync(term);
    record_sync_output(processor);
    // The Windows prompt grace has already lasted through this timeout. Do
    // not require another PTY read to publish prompt events replayed above.
    term.flush_delayed_events();
    control.bracketed_paste = bracketed_paste_enabled(term);
    control.drain_local(events).await?;
    if processor.is_output_boundary() {
        control.flush_csi_u().await?;
    }
    control.stdout.flush().await?;
    Ok(())
}

const BUFFER_SIZE: usize = 16384;

pub(crate) struct PendingInsertion {
    text: String,
    bracketed: bool,
    execute: bool,
    origin: RequestOrigin,
}

pub(crate) struct InsertionLock {
    /// Monotonic: a wall-clock step backwards must not keep the lock (and
    /// with it every edit-buffer frame) held until the clock catches up.
    at: Instant,
    expected: String,
    origin: RequestOrigin,
}

static INSERT_ON_NEW_CMD: Mutex<Option<PendingInsertion>> = Mutex::new(None);
static INSERTION_LOCK: Mutex<Option<InsertionLock>> = Mutex::new(None);

static SHELL_ENVIRONMENT_VARIABLES: LazyLock<Mutex<Arc<Vec<EnvironmentVariable>>>> =
    LazyLock::new(|| Mutex::new(Arc::new(Vec::new())));
static SHELL_ALIAS: Mutex<Option<String>> = Mutex::new(None);
/// Bumped by `UpdateShellContext` (`ftab _ pre-cmd` at prompt). Edit-buffer
/// frames send env/alias only when this changes, so the desktop session
/// learns about a just-finished `export` without cloning env on every key.
static SHELL_CONTEXT_EPOCH: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_shell_context_updated() {
    SHELL_CONTEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
}

static USER_ENABLED_SHELLS: LazyLock<Vec<String>> = LazyLock::new(|| {
    fastab_settings::state::get("user.enabled-shells")
        .ok()
        .flatten()
        .unwrap_or_default()
});

static HOSTNAME: LazyLock<Option<String>> = LazyLock::new(hostname);

fn hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        let rc = unsafe { nix::libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            return None;
        }
        let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        std::str::from_utf8(&buf[..nul]).ok().map(str::to_owned)
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").ok()
    }
}

pub(crate) enum MainLoopEvent {
    Insert {
        insert: Vec<u8>,
        unlock: bool,
        bracketed: bool,
        execute: bool,
        origin: RequestOrigin,
    },
    UnlockInterception,
    SetImmediateMode(bool),
    SetCsiU,
    UnsetCsiU,
}

/// Prompt / preexec / postexec / intercepted-key frames carry the full
/// context. Edit-buffer ticks use [`edit_buffer_context`] so they do not
/// clone env, aliases, or the parent-terminal lookup on every keystroke.
fn shell_state_to_context(shell_state: &ShellState) -> local::ShellContext {
    local::ShellContext {
        pid: shell_state.local_context.pid,
        ttys: shell_state.local_context.tty.clone(),
        process_name: shell_state.local_context.shell.clone(),
        shell_path: shell_state
            .local_context
            .shell_path
            .clone()
            .map(|path| path.display().to_string()),
        wsl_distro: shell_state.local_context.wsl_distro.clone(),
        current_working_directory: cwd_string(shell_state),
        session_id: shell_state.local_context.session_id.clone(),
        terminal: cached_parent_terminal(),
        hostname: cached_host_label(shell_state.local_context.username.as_deref()),
        environment_variables: SHELL_ENVIRONMENT_VARIABLES.lock().unwrap().as_ref().clone(),
        qterm_version: Some(env!("CARGO_PKG_VERSION").into()),
        preexec: Some(shell_state.preexec),
        osc_lock: Some(shell_state.osc_lock),
        alias: SHELL_ALIAS.lock().unwrap().clone(),
    }
}

/// OSC 7 can move cwd between prompts. Env/alias ride along only after
/// `UpdateShellContext`; `process_name` / `shell_path` stay on every frame
/// so a keystroke before the first Prompt still selects the right history.
fn edit_buffer_context(shell_state: &ShellState, include_environment: bool) -> local::ShellContext {
    local::ShellContext {
        current_working_directory: cwd_string(shell_state),
        process_name: shell_state.local_context.shell.clone(),
        shell_path: shell_state
            .local_context
            .shell_path
            .as_ref()
            .map(|path| path.display().to_string()),
        environment_variables: if include_environment {
            SHELL_ENVIRONMENT_VARIABLES.lock().unwrap().as_ref().clone()
        } else {
            Vec::new()
        },
        alias: if include_environment {
            SHELL_ALIAS.lock().unwrap().clone()
        } else {
            None
        },
        ..Default::default()
    }
}

fn shell_context_epoch() -> u64 {
    SHELL_CONTEXT_EPOCH.load(Ordering::Relaxed)
}

/// Watch notifications may coalesce A -> disconnected -> B. Check the owners
/// themselves against the current generation, preserving newer local writes.
fn reconcile_remote_state(sender: &RemoteSender, interceptor: &mut KeyInterceptor) -> Option<Generation> {
    let ready = sender.phase().ready_generation();
    interceptor.retire_remote_except(ready);
    let mut insertion = INSERTION_LOCK.lock().unwrap();
    if insertion.as_ref().is_some_and(|lock| !lock.origin.is_current(ready)) {
        insertion.take();
    }
    drop(insertion);
    let mut pending = INSERT_ON_NEW_CMD.lock().unwrap();
    if pending
        .as_ref()
        .is_some_and(|pending| !pending.origin.is_current(ready))
    {
        pending.take();
    }
    ready
}

fn cwd_string(shell_state: &ShellState) -> Option<String> {
    shell_state
        .local_context
        .current_working_directory
        .as_ref()
        .map(|cwd| cwd.display().to_string())
}

fn cached_parent_terminal() -> Option<String> {
    current_terminal().map(|terminal| terminal.to_string())
}

fn cached_host_label(username: Option<&str>) -> Option<String> {
    static LABEL: OnceLock<String> = OnceLock::new();
    if let Some(existing) = LABEL.get() {
        return Some(existing.clone());
    }
    let label = username.and_then(|user| HOSTNAME.as_deref().map(|host| format!("{user}@{host}")))?;
    Some(LABEL.get_or_init(|| label).clone())
}

#[allow(clippy::needless_return)]
fn get_cursor_coordinates(terminal: &dyn Terminal) -> Option<TerminalCursorCoordinates> {
    cfg_if! {
        if #[cfg(target_os = "windows")] {
            use term::cast;

            let coordinate = terminal.get_cursor_coordinate().ok()?;
            let screen_size = terminal.get_screen_size().ok()?;
            return Some(TerminalCursorCoordinates {
                x: cast(coordinate.cols).ok()?,
                y: cast(coordinate.rows).ok()?,
                xpixel: cast(screen_size.xpixel).ok()?,
                ypixel: cast(screen_size.ypixel).ok()?,
            });
        } else {
            let _terminal = terminal;
            return None;
        }
    }
}

/// Process titles from the pre hook are `zsh (fterm)`. Older and sibling PTYs
/// also wrap shell names; matching must see the inner name.
fn without_pty_wrapper_suffix(name: &str) -> &str {
    name.strip_suffix(" (fterm)")
        .or_else(|| name.strip_suffix(" (fastabterm)"))
        .or_else(|| name.strip_suffix(" (figterm)"))
        .or_else(|| name.strip_suffix(" (ecterm)"))
        .or_else(|| name.strip_suffix(" (qterm)"))
        .unwrap_or(name)
}

fn can_send_edit_buffer<T>(term: &Term<T>) -> bool
where
    T: EventListener,
{
    // Nested PTY titles are `zsh (fterm)` (or an older/sibling PTY name).
    let shell = term
        .shell_state()
        .get_context()
        .shell
        .as_deref()
        .map(without_pty_wrapper_suffix);
    let shell_enabled = ["bash", "zsh", "fish", "nu", "dash"]
        .into_iter()
        .chain(USER_ENABLED_SHELLS.iter().map(|s| s.as_str()))
        .any(|s| shell == Some(s));
    let preexec = term.shell_state().preexec;

    let mut handle = INSERTION_LOCK.lock().unwrap();
    let insertion_locked = insertion_lock_is_active(&mut handle, term);
    drop(handle);

    trace!(%shell_enabled, %preexec, %insertion_locked, "can_send_edit_buffer");

    shell_enabled && !insertion_locked && !preexec
}

fn insertion_lock_is_active<T: EventListener>(handle: &mut Option<InsertionLock>, term: &Term<T>) -> bool {
    match handle.as_ref() {
        Some(lock) => {
            let lock_expired = lock.at.elapsed() > Duration::from_millis(16);
            let should_unlock = lock_expired
                || term
                    .get_current_buffer()
                    .is_none_or(|buff| buff.buffer == lock.expected);
            if should_unlock {
                handle.take();
                if lock_expired {
                    trace!("insertion lock released because lock expired");
                } else {
                    trace!("insertion lock released because buffer looks like how we expect");
                }
                false
            } else {
                true
            }
        },
        None => false,
    }
}

const Q_DISABLE_AUTOCOMPLETE: &str = "Q_DISABLE_AUTOCOMPLETE";

fn autocomplete_enabled(env: &Env) -> bool {
    env.get_os(Q_DISABLE_AUTOCOMPLETE).is_none_or(|s| s.is_empty())
}

static AUTOCOMPLETE_ENABLED: LazyLock<bool> = LazyLock::new(|| autocomplete_enabled(&Env::new()));

fn send_edit_buffer<T>(
    term: &Term<T>,
    sender: &RemoteSender,
    cursor_coordinates: Option<TerminalCursorCoordinates>,
) -> Result<()>
where
    T: EventListener,
{
    if !*AUTOCOMPLETE_ENABLED {
        return Ok(());
    }
    let Some(bound) = sender.current() else {
        return Ok(());
    };
    let Some(progress): Option<ContextProgress> = bound.context_progress() else {
        return Ok(());
    };

    match term.get_current_buffer() {
        Some(edit_buffer) => {
            if let Some(cursor_idx) = edit_buffer.cursor_idx.and_then(|i| i.try_into().ok()) {
                debug!("edit_buffer: {edit_buffer:?}");
                trace!("buffer bytes: {:02X?}", edit_buffer.buffer.as_bytes());
                trace!("buffer chars: {:?}", edit_buffer.buffer.chars().collect::<Vec<_>>());

                let epoch = shell_context_epoch();
                let full_context = !progress.full_context_admitted;
                let include_environment = full_context || progress.needs_environment(epoch);
                let context = if full_context {
                    shell_state_to_context(term.shell_state())
                } else {
                    edit_buffer_context(term.shell_state(), include_environment)
                };

                let edit_buffer_hook =
                    new_edit_buffer_hook(Some(context), edit_buffer.buffer, cursor_idx, 0, cursor_coordinates);
                let message = hook_to_message(edit_buffer_hook);

                trace!("Sending: {message:?}");

                // Admission, generation validation and the context marker
                // commit are one operation. A rejected frame synchronizes nothing.
                let _ = bound.try_send_with_context(
                    message,
                    ContextAdmission {
                        full_context,
                        environment_epoch: include_environment.then_some(epoch),
                    },
                );
            }
            Ok(())
        },
        None => Err(anyhow!("No edit buffer to send")),
    }
}

fn get_parent_shell() -> Result<String> {
    match env::var(Q_SHELL).ok().filter(|s| !s.is_empty()) {
        Some(v) => Ok(v),
        None => match env::var("SHELL").ok().filter(|s| !s.is_empty()) {
            Some(shell) => Ok(shell),
            None => {
                anyhow::bail!("No Q_SHELL or SHELL found");
            },
        },
    }
}

fn build_shell_command(command: Option<&[String]>) -> Result<CommandBuilder> {
    let mut builder = match command {
        Some(command) => {
            let mut iter = command.iter().map(|s| s.as_str());

            let mut builder = CommandBuilder::new(iter.next().unwrap());
            for arg in iter {
                builder.arg(arg);
            }
            builder
        },
        None => {
            let parent_shell = get_parent_shell()?;
            let mut builder = CommandBuilder::new(parent_shell);

            if env::var("Q_IS_LOGIN_SHELL").ok().as_deref() == Some("1") {
                builder.arg("--login");
            }

            if let Some(execution_string) = env::var("Q_EXECUTION_STRING").ok().filter(|s| !s.is_empty()) {
                builder.args(["-c", &execution_string]);
            }

            if let Some(extra_args) = env::var("Q_SHELL_EXTRA_ARGS").ok().filter(|s| !s.is_empty()) {
                builder.args(extra_args.split_whitespace().filter(|arg| arg != &"--login"));
            }

            builder
        },
    };

    builder.env(Q_TERM, env!("CARGO_PKG_VERSION"));
    if env::var_os("TMUX").is_some() {
        builder.env("Q_TERM_TMUX", env!("CARGO_PKG_VERSION"));
    }

    // Clean up environment and launch shell.
    builder.env_remove(Q_SHELL);
    builder.env_remove("Q_IS_LOGIN_SHELL");
    builder.env_remove("Q_START_TEXT");
    builder.env_remove("Q_SHELL_EXTRA_ARGS");
    builder.env_remove("Q_EXECUTION_STRING");

    if let Ok(dir) = std::env::current_dir() {
        builder.cwd(dir);
    }

    Ok(builder)
}

#[cfg(unix)]
fn launch_shell(command: Option<&[String]>) -> Result<()> {
    let cmd = build_shell_command(command)?.as_command()?;
    let mut args: Vec<&OsStr> = std::vec![cmd.get_program()];
    args.extend(cmd.get_args());

    let cargs: Vec<_> = args
        .into_iter()
        .map(|arg| CString::new(arg.to_string_lossy().as_ref()).expect("Failed to convert arg to CString"))
        .collect();
    for (key, val) in cmd.get_envs() {
        unsafe {
            match val {
                Some(value) => env::set_var(key, value),
                None => {
                    env::remove_var(key);
                },
            }
        }
    }

    execvp(&cargs[0], &cargs).expect("Failed to execvp");
    unreachable!()
}

fn figterm_main(command: Option<&[String]>) -> Result<()> {
    fastab_util::build_identity::initialize();
    fastab_settings::settings::init_global().ok();

    let context = Context::new();

    let session_id = match std::env::var("MOCK_QTERM_SESSION_ID") {
        Ok(id) => id,
        Err(_) => uuid::Uuid::new_v4().simple().to_string(),
    };

    unsafe {
        std::env::set_var(QTERM_SESSION_ID, &session_id);
    }

    let parent_id = fastab_os_shim::Env::new().q_parent().ok();

    let mut terminal = SystemTerminal::new_from_stdio()?;
    let screen_size = terminal.get_screen_size()?;

    let pty_size = PtySize {
        rows: screen_size.rows as u16,
        cols: screen_size.cols as u16,
        pixel_width: screen_size.xpixel as u16,
        pixel_height: screen_size.ypixel as u16,
    };

    let pty = open_pty(&pty_size).context("Failed to open pty")?;
    let command = build_shell_command(command)?;

    let pty_name = pty.slave.get_name().unwrap_or_else(|| session_id.clone());

    // A file appender starts a worker thread. Default filter is ERROR, so the
    // per-tab log is empty unless someone raised `Q_LOG_LEVEL` — skip the file
    // (and that thread) until they have.
    let log_file_path = std::env::var_os(Q_LOG_LEVEL)
        .map(|_| directories::logs_dir().map(|dir| dir.join(format!("{PTY_BINARY_NAME}{pty_name}.log"))))
        .transpose()?;
    let _log_guard = match initialize_logging(LogArgs {
        log_level: None,
        log_to_stdout: false,
        log_file_path,
        delete_old_log_file: true,
    }) {
        Ok(logger_guard) => Some(logger_guard),
        Err(err) => {
            if !fastab_settings::state::get_bool_or("pty.suppress_log_error", false) {
                // let id = capture_anyhow(&err);
                eprintln!("{PRODUCT_NAME} failed to init logger: {err:?}");
            }
            None
        },
    };

    logger::stdio_debug_log(format!("pty name: {pty_name}"));
    logger::stdio_debug_log("Forking child shell process");

    #[cfg(unix)]
    {
        let pid = nix::unistd::getpid();
        logger::stdio_debug_log(format!("Parent pid: {pid}"));
    }

    let child = pty.slave.spawn_command(command)?;
    info!("Shell: {:?}", child.process_id());
    if let Some(pid) = child.process_id() {
        logger::stdio_debug_log(format!("Child pid: {pid}"));
    }

    let (child_tx, mut child_rx) = oneshot::channel();

    info!("Pid: {}", Pid::current());
    info!("Pty name: {pty_name}");

    // Two workers is enough for this process: the main loop is one `block_on`,
    // and the rest is I/O (stdin, the figterm listener, remote IPC, history).
    // The default pool is one thread per core, and `fterm` multiplies by tab,
    // so that was paying for stacks the grid never used.
    let runtime = runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name_fn(|| {
            static ATOMIC_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = ATOMIC_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            format!("{PTY_BINARY_NAME}-runtime-worker-{id}")
        })
        .build()?;

    // On the reactor, so no thread sits in waitpid for the life of the tab.
    {
        let _runtime = runtime.enter();
        child_exit::watch(child, child_tx);
    }

    let runtime_result = runtime.block_on(async {
        update::check_for_update(&context);

        terminal.set_raw_mode()?;

        let (main_loop_tx, main_loop_rx) = flume::bounded::<MainLoopEvent>(16);

        let history_sender = history::spawn_history_task().await;

        // Spawn thread to handle figterm ipc
        let (incoming_receiver, _socket_file) = spawn_figterm_ipc(&session_id).await?;

        // Spawn thread to handle remote ipc
        let (remote_sender, remote_receiver, stop_ipc_tx) = spawn_remote_ipc(session_id.clone(), parent_id).await?;
        let mut remote_state_changes = remote_sender.subscribe();
        let mut deferred_pty_input = BytesMut::new();
        let mut deferred_inserts = VecDeque::new();

        let mut stdout = io::stdout();
        let mut master = pty.master.get_async_master_pty()?;

        let mut processor = Processor::new();
        let size = SizeInfo::new(pty_size.rows as usize, pty_size.cols as usize);
        let local_events = LocalEvents::default();
        let event_sender = EventHandler::new(
            remote_sender.clone(),
            history_sender.clone(),
            local_events.clone(),
            fastab_settings::settings::get_bool_or("qterm.csi-u.enabled", false),
        );
        let mut term = alacritty_terminal::Term::new(size, event_sender, 1, session_id.clone());

        #[cfg(target_os = "windows")]
        term.set_windows_delay_end_prompt(true);

        let mut pty_read_buffer: Vec<u8> = vec![0; BUFFER_SIZE];

        let mut key_interceptor = KeyInterceptor::new();
        key_interceptor.load_key_intercepts()?;

        let mut insertion_retry = InsertionRetry::default();

        let mut first_time = true;

        let input_rx = terminal.read_input()?;

        let key_code_encode_mode = KeyCodeEncodeModes {
            #[cfg(unix)]
            encoding: KeyboardEncoding::Xterm,
            #[cfg(windows)]
            encoding: KeyboardEncoding::Win32,
            application_cursor_keys: false,
            newline_mode: false,
        };

        if let Ok(shell) = get_parent_shell() {
            let path = std::path::Path::new(&shell);
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(shell.as_str());
            let title_osc = format!("\x1b]0;{name}\x07");
            if let Err(err) = stdout.write(title_osc.as_bytes()).await {
                error!("Failed to write title osc: {err}");
            }
        }

        let mut csi_u = CsiUState::default();

        // Every arm that applies controls borrows the same loop state.
        macro_rules! main_loop_control {
            () => {
                MainLoopControl {
                    remote_sender: &remote_sender,
                    master: master.as_mut(),
                    deferred_pty_input: &mut deferred_pty_input,
                    deferred_inserts: &mut deferred_inserts,
                    key_interceptor: &mut key_interceptor,
                    terminal: &mut terminal,
                    stdout: &mut stdout,
                    csi_u: &mut csi_u,
                    bracketed_paste: bracketed_paste_enabled(&term),
                }
            };
        }

        let result: Result<()> = 'select_loop: loop {
            if first_time && term.shell_state().has_seen_prompt {
                trace!("Has seen prompt and first time");
                let initial_command = env::var("Q_START_TEXT").ok().filter(|s| !s.is_empty());
                if let Some(mut initial_command) = initial_command {
                    debug!("Sending initial text: {initial_command}");
                    initial_command.push('\n');
                    if let Err(err) = master.write_all(initial_command.as_bytes()).await {
                        error!("Failed to write initial command: {err}");
                    }
                }
                first_time = false;
            }

            insertion_retry.update(INSERTION_LOCK.lock().unwrap().as_ref().map(|lock| lock.at));
            let sync_deadline = processor.sync_timeout().copied();
            let ready = select_main_loop_ready(
                wait_for_pty_sync_timeout(sync_deadline),
                remote_state_changes.changed(),
                remote_sender.key_delivery_changed(),
                (
                    main_loop_rx.recv_async(),
                    input_rx.recv_async(),
                    master.read(&mut pty_read_buffer),
                    remote_receiver.recv_async(),
                    incoming_receiver.recv_async(),
                    async {
                        if insertion_retry.active() {
                            insertion_retry.wait().await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    },
                ),
                &mut child_rx,
            )
            .await;
            let select_result: Result<()> = match ready {
                MainLoopReady::SyncTimeout => {
                    let mut control = main_loop_control!();
                    finish_pty_sync(&mut processor, &mut term, &local_events, &mut control).await?;
                    if can_send_edit_buffer(&term) {
                        let cursor_coordinates = get_cursor_coordinates(&terminal);
                        if let Err(err) = send_edit_buffer(&term, &remote_sender, cursor_coordinates) {
                            warn!("Failed to send edit buffer after synchronized output: {err}");
                        }
                    }
                    Ok(())
                },
                MainLoopReady::RemoteStateChanged | MainLoopReady::KeyDeliveryChanged => {
                    reconcile_remote_state(&remote_sender, &mut key_interceptor);
                    flush_settled_pty_input(
                        &remote_sender,
                        master.as_mut(),
                        &mut deferred_pty_input,
                        &mut deferred_inserts,
                        &mut key_interceptor,
                        bracketed_paste_enabled(&term),
                    )
                    .await
                },
                MainLoopReady::Control(res) => {
                    match res {
                        Ok(event) => {
                            let mut control = main_loop_control!();
                            control.apply(event).await?;
                            if processor.is_output_boundary() {
                                control.flush_csi_u().await?;
                            }
                        },
                        Err(err) => warn!("Failed to recv: {err}"),
                    };
                    Ok(())
                },
                MainLoopReady::Input(res) => {
                    let mut input_res = Ok(());
                    match res {
                        Ok(events) => {
                            let mut write_buffer = BytesMut::new();
                            for event in events {
                                match event {
                                    Ok((raw, InputEvent::Key(event))) => {
                                        // Capture the generation whose state made this key
                                        // interceptable. Never retarget the key after reconnect.
                                        let key_generation =
                                            reconcile_remote_state(&remote_sender, &mut key_interceptor);
                                        // Do not do most stuff during not preexec since that means a command is running
                                        let preexec = term.shell_state().preexec;

                                        debug!(?event, ?raw, %preexec,  "Got key event");

                                        // if we are in CSI u mode we try to encode first, otherwise we try to send the raw bytes first
                                        let raw = if csi_u.owned {
                                            event
                                                .key
                                                .encode(event.modifiers, key_code_encode_mode, true)
                                                .ok()
                                                .map(|s| s.into_bytes().into())
                                                .or(raw)
                                        } else {
                                            raw.or_else(|| {
                                                event
                                                    .key
                                                    .encode(event.modifiers, key_code_encode_mode, true)
                                                    .ok()
                                                    .map(|s| s.into_bytes().into())
                                            })
                                        };

                                        let handled_action = if !preexec {
                                            if let Some(action) = key_interceptor.intercept_key(&event) {
                                                // Park ordinary bytes from this batch behind any
                                                // still-pending intercepted keys without blocking.
                                                // Insert / key_delivery flush paths drain deferred
                                                // once settlement clears, so desktop Insert cannot
                                                // overtake those bytes.
                                                if !write_buffer.is_empty() {
                                                    deferred_pty_input.extend_from_slice(&write_buffer);
                                                    write_buffer.clear();
                                                }
                                                flush_settled_pty_input(
                                                    &remote_sender,
                                                    master.as_mut(),
                                                    &mut deferred_pty_input,
                                                    &mut deferred_inserts,
                                                    &mut key_interceptor,
                                                    bracketed_paste_enabled(&term),
                                                )
                                                .await?;
                                                if !deferred_pty_input.is_empty() {
                                                    // Ordinary input is still parked behind an
                                                    // unsettled key. A failed replay of this key
                                                    // would be written ahead of that input, so
                                                    // queue it behind as ordinary input instead.
                                                    if event.key == KeyCode::Escape {
                                                        key_interceptor.reset();
                                                    }
                                                    false
                                                } else {
                                                    debug!(?action, "Intercepted action");
                                                    let s = raw
                                                        .clone()
                                                        .and_then(|b| String::from_utf8(b.to_vec()).ok())
                                                        .unwrap_or_default();
                                                    // The desktop neither applies nor reads a key's
                                                    // context, so do not clone, encode and decode the
                                                    // environment and aliases on every arrow key. For
                                                    // the same reason the key must not advance the
                                                    // generation's context synchronization marker.
                                                    let hook = fastab_proto::remote_hooks::new_intercepted_key_hook(
                                                        None, action, s,
                                                    );
                                                    let admitted = key_generation.is_some_and(|generation| {
                                                        remote_sender
                                                            .for_generation(generation)
                                                            .try_send_key(
                                                                hook_to_message(hook),
                                                                raw.clone().unwrap_or_default(),
                                                            )
                                                            .is_ok()
                                                    });
                                                    if !admitted {
                                                        reconcile_remote_state(&remote_sender, &mut key_interceptor);
                                                    }

                                                    if event.key == KeyCode::Escape {
                                                        key_interceptor.reset();
                                                    }
                                                    admitted
                                                }
                                            } else {
                                                false
                                            }
                                        } else {
                                            false
                                        };

                                        if !handled_action {
                                            if let Some(bytes) = raw {
                                                if (event.key == KeyCode::Char('c') || event.key == KeyCode::Char('d'))
                                                    && event.modifiers == Modifiers::CTRL
                                                {
                                                    key_interceptor.reset();
                                                }
                                                write_buffer.extend(&bytes);
                                            }
                                        }
                                    },
                                    Ok((_, InputEvent::Resized)) => {
                                        terminal.flush()?;

                                        let size = terminal.get_screen_size()?;
                                        let pty_size = PtySize {
                                            rows: size.rows as u16,
                                            cols: size.cols as u16,
                                            pixel_width: size.xpixel as u16,
                                            pixel_height: size.ypixel as u16,
                                        };

                                        master.resize(pty_size)?;
                                        let window_size = SizeInfo::new(size.rows, size.cols);
                                        debug!("Window size changed: {window_size:?}");
                                        term.resize(window_size);
                                    },
                                    Ok((None, InputEvent::Paste(string))) => {
                                        // Pass through bracketed pastes.
                                        if bracketed_paste_enabled(&term) {
                                            write_buffer.extend(b"\x1b[200~");
                                            write_buffer.extend(string.replace('\x1b', "").as_bytes());
                                            write_buffer.extend(b"\x1b[201~");
                                        } else {
                                            write_buffer
                                                .extend(string.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
                                        }
                                    },
                                    Ok((raw, _)) => {
                                        if let Some(raw) = raw {
                                            info!("Fallback write");
                                            write_buffer.extend(&raw);
                                        } else {
                                            info!("Unhandled input event with no raw pass-through data");
                                        }
                                    },
                                    Err(err) => {
                                        error!("Failed receiving input from stdin: {err}");
                                        input_res = Err(err);
                                        break;
                                    },
                                };
                            }
                            if !write_buffer.is_empty() {
                                deferred_pty_input.extend_from_slice(&write_buffer);
                            }
                            flush_settled_pty_input(
                                &remote_sender,
                                master.as_mut(),
                                &mut deferred_pty_input,
                                &mut deferred_inserts,
                                &mut key_interceptor,
                                bracketed_paste_enabled(&term),
                            )
                            .await?;
                            if input_res.is_err() {
                                finish_pty_input(
                                    &remote_sender,
                                    master.as_mut(),
                                    &mut deferred_pty_input,
                                    &mut deferred_inserts,
                                    &mut key_interceptor,
                                    bracketed_paste_enabled(&term),
                                )
                                .await?;
                            }
                        },
                        Err(_) => {
                            finish_pty_input(
                                &remote_sender,
                                master.as_mut(),
                                &mut deferred_pty_input,
                                &mut deferred_inserts,
                                &mut key_interceptor,
                                bracketed_paste_enabled(&term),
                            )
                            .await?;
                            break 'select_loop Ok(());
                        },
                    };
                    input_res
                },
                MainLoopReady::Pty(res) => {
                    #[cfg(feature = "profiling_early_exit")]
                    break 'select_loop Ok(());
                    match res {
                        Ok(0) => {
                            trace!("EOF from master");
                            break 'select_loop Ok(());
                        },
                        Ok(size) => {
                            trace!("Read {size} bytes from master");

                            let mut control = main_loop_control!();
                            process_pty_bytes(
                                &pty_read_buffer[..size],
                                &mut processor,
                                &mut term,
                                &local_events,
                                &mut control,
                            )
                            .await?;

                            if can_send_edit_buffer(&term) {
                                let cursor_coordinates = get_cursor_coordinates(&terminal);
                                if let Err(err) = send_edit_buffer(&term, &remote_sender, cursor_coordinates) {
                                    warn!("Failed to send edit buffer: {err}");
                                }
                            }

                            Ok(())
                        },
                        Err(err) => {
                            error!("Failed to read from master: {err}");
                            break 'select_loop Ok(());
                        },
                    }
                },
                MainLoopReady::Remote(msg) => {
                    match msg {
                        Ok(incoming) => {
                            reconcile_remote_state(&remote_sender, &mut key_interceptor);
                            let response_sender = remote_sender.for_generation(incoming.generation);
                            if !response_sender.is_current_ready() {
                                continue 'select_loop;
                            }
                            let message = incoming.message;
                            trace!("Received message from socket: {message:?}");
                            process_remote_message(
                                message,
                                main_loop_tx.clone(),
                                response_sender,
                                incoming.permit,
                                &term,
                                &mut master,
                                &mut key_interceptor,
                            )
                            .await?;
                        },
                        Err(err) => {
                            error!("Failed to receive message from socket: {err}");
                        },
                    }
                    Ok(())
                },
                MainLoopReady::Local(msg) => {
                    match msg {
                        Ok(LocalIncoming { message, reply }) => {
                            debug!("Received message from figterm listener: {message:?}");
                            process_figterm_message(
                                message,
                                main_loop_tx.clone(),
                                reply,
                                &term,
                                &history_sender,
                                &mut master,
                                &mut key_interceptor,
                                &session_id,
                            )
                            .await?;
                        },
                        Err(err) => {
                            error!("Failed to receive message from socket: {err}");
                        },
                    }
                    Ok(())
                },
                // Check if to send the edit buffer because of timeout
                MainLoopReady::InsertionRetry => {
                    insertion_retry.tick();
                    reconcile_remote_state(&remote_sender, &mut key_interceptor);
                    let send_eb = INSERTION_LOCK.lock().unwrap().is_some();
                    if send_eb && can_send_edit_buffer(&term) {
                        let cursor_coordinates = get_cursor_coordinates(&terminal);
                        if let Err(err) = send_edit_buffer(&term, &remote_sender, cursor_coordinates) {
                            warn!(%err, "Failed to send edit buffer");
                        }
                    }
                    Ok(())
                },
                MainLoopReady::ChildExited => {
                    trace!("Shell process exited");
                    break 'select_loop Ok(());
                },
            };

            if let Err(err) = select_result {
                error!("Error in select loop: {err}");
                break 'select_loop Err(err);
            }
        };

        let _ = stop_ipc_tx.send(());

        result
    });

    // Reading from stdin is a blocking task on a separate thread:
    // https://github.com/tokio-rs/tokio/issues/2466
    // We must explicitly shutdown the runtime to exit.
    // This can cause resource leaks if we aren't careful about tasks we spawn.
    runtime.shutdown_background();

    // attempt cleanup
    #[cfg(target_os = "linux")]
    cleanup::cleanup()?;

    runtime_result
}

fn main() {
    fastab_util::malloc::start_space_efficient();
    // One fterm stays resident per terminal tab; it reads a few keys at
    // startup and writes a history row per command. No pool: no maintenance
    // thread, no 30 s reaper wakeup, and no SQLite connection open between uses.
    fastab_settings::sqlite::prefer_direct_connections();
    let cli = Cli::parse();
    let command = cli.command.as_deref();

    logger::stdio_debug_log(format!("{Q_LOG_LEVEL}={}", fastab_log::get_log_level()));

    if !state::get_bool_or("qterm.enabled", true) {
        println!("[NOTE] {PTY_BINARY_NAME} is disabled. Autocomplete will not work.");
        logger::stdio_debug_log(format!("{PTY_BINARY_NAME} is disabled. `qterm.enabled` == false"));
        return;
    }

    match figterm_main(command) {
        Ok(()) => {
            info!("Exiting");
        },
        Err(err) => {
            error!("Error in async runtime: {err}");
            println!("{PRODUCT_NAME} had an Error!: {err:?}");
            // capture_anyhow(&err);

            // Fallback to normal shell
            #[cfg(unix)]
            if let Err(err) = launch_shell(command) {
                // capture_anyhow(&err);
                logger::stdio_debug_log(err.to_string());
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    mod resource_regressions {
        use std::future::Future;
        use std::task::Poll;

        use flume::Receiver;
        use tokio::io::AsyncReadExt;
        use tokio::net::UnixStream;

        use super::*;
        use crate::term::{InputEventResult, ScreenSize};

        // Exercise the production PTY consumer against actual async socket I/O
        // without launching or instrumenting a user's shell.
        struct SocketMaster(UnixStream);

        #[async_trait::async_trait]
        impl AsyncMasterPty for SocketMaster {
            async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(buf).await
            }

            async fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.write(buf).await
            }

            fn resize(&self, _: PtySize) -> Result<()> {
                Ok(())
            }
        }

        #[derive(Default)]
        struct ModeTerminal(Vec<bool>);

        impl Terminal for ModeTerminal {
            fn set_raw_mode(&mut self) -> Result<()> {
                Ok(())
            }
            fn set_cooked_mode(&mut self) -> Result<()> {
                Ok(())
            }
            fn get_screen_size(&mut self) -> Result<ScreenSize> {
                Ok(ScreenSize {
                    rows: 24,
                    cols: 80,
                    xpixel: 0,
                    ypixel: 0,
                })
            }
            fn set_screen_size(&mut self, _: ScreenSize) -> Result<()> {
                Ok(())
            }
            fn flush(&mut self) -> Result<()> {
                Ok(())
            }
            fn read_input(&mut self) -> Result<Receiver<InputEventResult>> {
                unreachable!()
            }
            fn set_immediate_mode(&mut self, mode: bool) -> Result<()> {
                self.0.push(mode);
                Ok(())
            }
        }

        fn parser_term(sender: &RemoteSender, events: &LocalEvents, csi_u: bool) -> Term<EventHandler> {
            let (history, _receiver) = flume::unbounded();
            Term::new(
                SizeInfo::new(24, 80),
                EventHandler::new(sender.clone(), history, events.clone(), csi_u),
                1,
                "resource-regression".into(),
            )
        }

        struct ParserOutput {
            bytes: Vec<u8>,
            immediate_modes: Vec<bool>,
            csi_u_set: bool,
            delayed_events: usize,
            sync_bytes: usize,
            sync_pending: bool,
            tracked_buffer: Option<String>,
        }

        async fn capture_pty_reads(reads: &[&[u8]], csi_u: bool, delay_prompt: bool) -> ParserOutput {
            capture_pty_reads_with_sync_timeout(reads, csi_u, delay_prompt, None).await
        }

        async fn capture_pty_reads_with_sync_timeout(
            reads: &[&[u8]],
            csi_u: bool,
            delay_prompt: bool,
            after_timeout: Option<&[u8]>,
        ) -> ParserOutput {
            let sender = RemoteSender::test_disconnected();
            let events = LocalEvents::default();
            let mut term = parser_term(&sender, &events, csi_u);
            term.set_windows_delay_end_prompt(delay_prompt);
            let mut processor = Processor::new();
            let (socket, _peer) = UnixStream::pair().unwrap();
            let mut master = SocketMaster(socket);
            let mut deferred = BytesMut::new();
            let mut inserts = VecDeque::new();
            let mut interceptor = KeyInterceptor::new();
            let mut terminal = ModeTerminal::default();
            let (mut stdout, mut capture) = tokio::io::duplex(16384);
            let captured_output = tokio::spawn(async move {
                let mut output = Vec::new();
                capture.read_to_end(&mut output).await.unwrap();
                output
            });
            let mut csi_u = CsiUState::default();
            let mut control = MainLoopControl {
                remote_sender: &sender,
                master: &mut master,
                deferred_pty_input: &mut deferred,
                deferred_inserts: &mut inserts,
                key_interceptor: &mut interceptor,
                terminal: &mut terminal,
                stdout: &mut stdout,
                csi_u: &mut csi_u,
                bracketed_paste: false,
            };
            tokio::time::timeout(Duration::from_secs(30), async {
                for bytes in reads {
                    process_pty_bytes(bytes, &mut processor, &mut term, &events, &mut control).await?;
                    // Every read, including ones ending inside an OSC/ST,
                    // settles its events rather than retaining queue entries.
                    assert!(!events.has_pending());
                }
                if let Some(after_timeout) = after_timeout {
                    let deadline = processor.sync_timeout().copied();
                    assert!(deadline.is_some());
                    // No subsequent PTY read arrives. Drive the exact wait and
                    // settlement used by the main-loop timer branch.
                    wait_for_pty_sync_timeout(deadline).await;
                    finish_pty_sync(&mut processor, &mut term, &events, &mut control).await?;
                    assert!(processor.sync_timeout().is_none());
                    assert!(!events.has_pending());
                    process_pty_bytes(after_timeout, &mut processor, &mut term, &events, &mut control).await?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .unwrap()
            .unwrap();
            assert!(!events.has_pending());
            assert!(events.pop().is_none());
            drop(stdout);
            ParserOutput {
                bytes: captured_output.await.unwrap(),
                immediate_modes: terminal.0,
                csi_u_set: csi_u.owned,
                delayed_events: term.get_delayed_events_count(),
                sync_bytes: processor.sync_bytes_count(),
                sync_pending: processor.sync_timeout().is_some(),
                tracked_buffer: term.get_current_buffer().map(|buffer| buffer.buffer),
            }
        }

        #[tokio::test]
        async fn synchronized_output_byte_budget_replays_without_an_end_sequence() {
            let mut input = b"\x1bP=1s\x1b\\".to_vec();
            input.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024 + 17));
            input.extend_from_slice(b"\x1b]697;NewCmd\x07tail");
            let reads: Vec<_> = input.chunks(BUFFER_SIZE).collect();
            let output = capture_pty_reads(&reads, true, false).await;
            let mut expected = input[..input.len() - 4].to_vec();
            expected.extend_from_slice(b"\x1b[>1utail");
            assert_eq!(output.bytes, expected);
            assert_eq!(output.immediate_modes, [false]);
            assert_eq!(output.sync_bytes, 0);
            assert!(!output.sync_pending);
            assert_eq!(output.tracked_buffer.as_deref(), Some("tail"));
        }

        #[tokio::test]
        async fn synchronized_output_silent_timeout_orders_controls_after_raw_output() {
            let input = b"\x1bP=1s\x1b\\\x1b]697;NewCmd\x07tail";
            for delay_prompt in [false, true] {
                let output = capture_pty_reads_with_sync_timeout(&[input], true, delay_prompt, Some(b"!")).await;
                let mut expected = input.to_vec();
                expected.extend_from_slice(b"\x1b[>1u!");
                assert_eq!(output.bytes, expected);
                assert_eq!(output.immediate_modes, [false]);
                assert!(output.csi_u_set);
                assert_eq!(output.delayed_events, 0);
                assert_eq!(output.sync_bytes, 0);
                assert!(!output.sync_pending);
                if !delay_prompt {
                    assert_eq!(output.tracked_buffer.as_deref(), Some("tail!"));
                }
            }
        }

        #[tokio::test]
        async fn synchronized_output_legal_end_flushes_controls_at_the_complete_terminator() {
            let input = b"\x1bP=1s\x1b\\\x1b]697;NewCmd\x07tail\x1bP=2s\x1b\\!";
            let reads: Vec<_> = input.chunks(1).collect();
            let output = capture_pty_reads(&reads, true, false).await;
            let mut expected = input[..input.len() - 1].to_vec();
            expected.extend_from_slice(b"\x1b[>1u!");
            assert_eq!(output.bytes, expected);
            assert_eq!(output.immediate_modes, [false]);
            assert!(output.csi_u_set);
            assert_eq!(output.sync_bytes, 0);
            assert!(!output.sync_pending);
            assert_eq!(output.tracked_buffer.as_deref(), Some("tail!"));
        }

        #[tokio::test]
        async fn synchronized_timeout_waits_for_partial_utf8_and_osc_output_boundaries() {
            for (partial, continuation, expected_tail) in [
                (b"\xe4\xb8".as_slice(), b"\xad".as_slice(), b"\xad\x1b[>1u".as_slice()),
                (
                    b"\x1b]0;title".as_slice(),
                    b"\x07".as_slice(),
                    b"\x07\x1b[>1u".as_slice(),
                ),
            ] {
                let mut input = b"\x1bP=1s\x1b\\\x1b]697;NewCmd\x07".to_vec();
                input.extend_from_slice(partial);
                let output = capture_pty_reads_with_sync_timeout(&[&input], true, false, Some(continuation)).await;
                let mut expected = input;
                expected.extend_from_slice(expected_tail);
                assert_eq!(output.bytes, expected);
                assert_eq!(output.immediate_modes, [false]);
                assert!(output.csi_u_set);
                assert_eq!(output.sync_bytes, 0);
                assert!(!output.sync_pending);
            }
        }

        #[tokio::test]
        async fn osc_burst_drains_local_controls_in_parser_order_without_self_send() {
            // Each iteration produces five control events, far beyond the old
            // 16-slot channel in a single PTY read. Verify raw output as well as
            // generated controls, so forwarding the whole read later fails.
            let input = b"\x1b]697;NewCmd\x07\x1b]697;PreExec\x07".repeat(64);
            let output = capture_pty_reads(&[&input], true, false).await;
            assert_eq!(output.immediate_modes, [false, true].repeat(64));
            assert!(!output.csi_u_set);
            assert_eq!(
                output.bytes,
                b"\x1b]697;NewCmd\x07\x1b[>1u\x1b]697;PreExec\x07\x1b[<u".repeat(64)
            );
        }

        #[tokio::test]
        async fn repeated_csi_u_controls_only_push_and_pop_owned_state() {
            // Exercise the controls directly: Term suppresses repeated PreExec
            // hooks, which would otherwise mask an extra CSI-u pop here.
            let sender = RemoteSender::test_disconnected();
            let (socket, _peer) = UnixStream::pair().unwrap();
            let mut master = SocketMaster(socket);
            let mut deferred = BytesMut::new();
            let mut inserts = VecDeque::new();
            let mut interceptor = KeyInterceptor::new();
            let mut terminal = ModeTerminal::default();
            let (mut stdout, mut capture) = tokio::io::duplex(128);
            let mut csi_u = CsiUState::default();
            let mut control = MainLoopControl {
                remote_sender: &sender,
                master: &mut master,
                deferred_pty_input: &mut deferred,
                deferred_inserts: &mut inserts,
                key_interceptor: &mut interceptor,
                terminal: &mut terminal,
                stdout: &mut stdout,
                csi_u: &mut csi_u,
                bracketed_paste: false,
            };

            // No acquired push means there is nothing to pop, including when
            // conflicting requests settle before an output boundary.
            control.apply(MainLoopEvent::UnsetCsiU).await.unwrap();
            control.flush_csi_u().await.unwrap();
            control.apply(MainLoopEvent::SetCsiU).await.unwrap();
            control.apply(MainLoopEvent::UnsetCsiU).await.unwrap();
            control.flush_csi_u().await.unwrap();
            assert!(!control.csi_u.owned);

            for _ in 0..2 {
                for _ in 0..2 {
                    control.apply(MainLoopEvent::SetCsiU).await.unwrap();
                    control.flush_csi_u().await.unwrap();
                    assert!(control.csi_u.owned);
                }
                for _ in 0..2 {
                    control.apply(MainLoopEvent::UnsetCsiU).await.unwrap();
                    control.flush_csi_u().await.unwrap();
                    assert!(!control.csi_u.owned);
                }
            }
            drop(stdout);
            let mut output = Vec::new();
            capture.read_to_end(&mut output).await.unwrap();
            assert_eq!(output, b"\x1b[>1u\x1b[<u".repeat(2));
        }

        #[tokio::test]
        async fn csi_u_controls_follow_screen_transitions_across_pty_read_boundaries() {
            let cases: &[(&[u8], &[u8], &[bool], bool)] = &[
                (b"ordinary\r\noutput", b"ordinary\r\noutput", &[], false),
                (
                    b"\x1b[?1049hscreen\x1b[?1049l\x1b]697;NewCmd\x07prompt> ",
                    b"\x1b[?1049hscreen\x1b[?1049l\x1b]697;NewCmd\x07\x1b[>1uprompt> ",
                    &[false],
                    true,
                ),
                (
                    b"\x1b]697;NewCmd\x07prompt> command\x1b]697;PreExec\x07\x1b[?1049hscreen",
                    b"\x1b]697;NewCmd\x07\x1b[>1uprompt> command\x1b]697;PreExec\x07\x1b[<u\x1b[?1049hscreen",
                    &[false, true],
                    false,
                ),
                (
                    b"\x1b[?1049hscreen\x1b[?1049l\x1b]697;NewCmd\x1b\\prompt> ",
                    b"\x1b[?1049hscreen\x1b[?1049l\x1b]697;NewCmd\x1b\\\x1b[>1uprompt> ",
                    &[false],
                    true,
                ),
                (
                    b"\x1b]697;NewCmd\x1b\\prompt> command\x1b]697;PreExec\x1b\\\x1b[?1049hscreen",
                    b"\x1b]697;NewCmd\x1b\\\x1b[>1uprompt> command\x1b]697;PreExec\x1b\\\x1b[<u\x1b[?1049hscreen",
                    &[false, true],
                    false,
                ),
                // OSC can also end by starting another escape sequence.
                // Controls must wait for its final byte, even with a C0 LF
                // executing midway through CSI parsing.
                (
                    b"\x1b[?1049hscreen\x1b]697;NewCmd\x1b[?1049\nlprompt> ",
                    b"\x1b[?1049hscreen\x1b]697;NewCmd\x1b[?1049\nl\x1b[>1uprompt> ",
                    &[false],
                    true,
                ),
                (
                    b"\x1b]697;NewCmd\x1b]0;title\x07prompt> ",
                    b"\x1b]697;NewCmd\x1b]0;title\x07\x1b[>1uprompt> ",
                    &[false],
                    true,
                ),
                (
                    b"\x1b]697;NewCmd\x1bP0qpayload\x1b\\prompt> ",
                    b"\x1b]697;NewCmd\x1bP0qpayload\x1b\\\x1b[>1uprompt> ",
                    &[false],
                    true,
                ),
                // Synchronized output replays both mode requests at its end;
                // their final disabled state needs no push or pop.
                (
                    b"\x1bP=1s\x1b\\\x1b]697;NewCmd\x07\x1b]697;PreExec\x07\x1bP=2s\x1b\\tail",
                    b"\x1bP=1s\x1b\\\x1b]697;NewCmd\x07\x1b]697;PreExec\x07\x1bP=2s\x1b\\tail",
                    &[false, true],
                    false,
                ),
                // NewCmd emits each prompt; PreExec emits once until the
                // next NewCmd. All duplicate hook bytes still pass through.
                (
                    b"\x1b]697;NewCmd\x1b\\\x1b]697;NewCmd\x07\x1b]697;PreExec\x07\x1b]697;PreExec\x1b\\tail",
                    b"\x1b]697;NewCmd\x1b\\\x1b[>1u\x1b]697;NewCmd\x07\x1b]697;PreExec\x07\x1b[<u\x1b]697;PreExec\x1b\\tail",
                    &[false, false, true],
                    false,
                ),
                (
                    b"\x1b]697;PreExec\x07\x1b]697;PreExec\x1b\\tail",
                    b"\x1b]697;PreExec\x07\x1b]697;PreExec\x1b\\tail",
                    &[true],
                    false,
                ),
                // Give up our push before the application's alternate screen
                // and protocol push; reacquire only after its pop and return.
                (
                    b"\x1b]697;NewCmd\x07cmd\x1b]697;PreExec\x1b\\\x1b[?1049h\x1b[>4uapp\x1b[<u\x1b[?1049l\x1b]697;NewCmd\x1b\\prompt",
                    b"\x1b]697;NewCmd\x07\x1b[>1ucmd\x1b]697;PreExec\x1b\\\x1b[<u\x1b[?1049h\x1b[>4uapp\x1b[<u\x1b[?1049l\x1b]697;NewCmd\x1b\\\x1b[>1uprompt",
                    &[false, true, false],
                    true,
                ),
            ];
            for &(input, expected, modes, csi_u_set) in cases {
                let assert_output = |output: ParserOutput| {
                    assert_eq!(output.bytes, expected);
                    assert_eq!(output.immediate_modes, modes);
                    assert_eq!(output.csi_u_set, csi_u_set);
                };
                assert_output(capture_pty_reads(&[input], true, false).await);
                // Every split covers partial CSI/OSC sequences and their
                // terminators; byte reads also cover more than two chunks.
                for split in 1..input.len() {
                    assert_output(capture_pty_reads(&[&input[..split], &input[split..]], true, false).await);
                }
                let byte_reads: Vec<_> = input.chunks(1).collect();
                assert_output(capture_pty_reads(&byte_reads, true, false).await);
            }
        }

        #[tokio::test]
        async fn delayed_prompt_controls_follow_the_read_that_flushes_them() {
            let prompt = b"\x1b[?1049hscreen\x1b[?1049l\x1b]697;NewCmd\x07";
            let pending = capture_pty_reads(&[prompt], true, true).await;
            assert_eq!(pending.bytes, prompt);
            assert!(pending.immediate_modes.is_empty());
            assert!(!pending.csi_u_set);
            assert_eq!(pending.delayed_events, 1);

            let output = capture_pty_reads(&[prompt, b"prompt> \x1b]697;PreExec\x07tail"], true, true).await;
            assert_eq!(
                output.bytes,
                b"\x1b[?1049hscreen\x1b[?1049l\x1b]697;NewCmd\x07prompt> \x1b]697;PreExec\x07tail\x1b[>1u"
            );
            assert_eq!(output.immediate_modes, [true, false]);
            assert!(output.csi_u_set);
            assert_eq!(output.delayed_events, 0);

            // A pending prompt's CSI-u request waits through partial CSI,
            // then writes at the earliest boundary before later UTF-8 output.
            let output = capture_pty_reads(&[prompt, b"\x1b[3", b"1m\xe4\xb8", b"\xad"], true, true).await;
            assert_eq!(
                output.bytes,
                b"\x1b[?1049hscreen\x1b[?1049l\x1b]697;NewCmd\x07\x1b[31m\x1b[>1u\xe4\xb8\xad"
            );
            assert_eq!(output.immediate_modes, [false]);
            assert!(output.csi_u_set);
            assert_eq!(output.delayed_events, 0);

            let output = capture_pty_reads(&[prompt, b"\xe4\xb8", b"\xad"], true, true).await;
            assert_eq!(
                output.bytes,
                b"\x1b[?1049hscreen\x1b[?1049l\x1b]697;NewCmd\x07\xe4\xb8\xad\x1b[>1u"
            );
            assert_eq!(output.immediate_modes, [false]);
            assert!(output.csi_u_set);
            assert_eq!(output.delayed_events, 0);

            let output = capture_pty_reads(&[prompt, b"\xc2", b"\xc2", b"\x80"], true, true).await;
            assert_eq!(
                output.bytes,
                b"\x1b[?1049hscreen\x1b[?1049l\x1b]697;NewCmd\x07\xc2\xc2\x80\x1b[>1u"
            );
            assert_eq!(output.immediate_modes, [false]);
            assert!(output.csi_u_set);
            assert_eq!(output.delayed_events, 0);

            // CAN can end the opening DCS in ground state while activating
            // synchronized buffering; its unparsed half-CSI is still unsafe.
            let output = capture_pty_reads(
                &[b"\x1b]697;NewCmd\x07\x1bP=1s\x18", b"\x1b[3", b"1m\x1bP=2s\x1b\\"],
                true,
                true,
            )
            .await;
            assert_eq!(
                output.bytes,
                b"\x1b]697;NewCmd\x07\x1bP=1s\x18\x1b[31m\x1bP=2s\x1b\\\x1b[>1u"
            );
            assert_eq!(output.immediate_modes, [false]);
            assert!(output.csi_u_set);
            assert_eq!(output.delayed_events, 0);
        }

        #[tokio::test]
        async fn csi_u_disabled_still_forwards_raw_prompt_and_preexec_output_once() {
            let input = b"before\x1b]697;NewCmd\x07prompt\x1b]697;PreExec\x07after";
            let output = capture_pty_reads(&[input], false, false).await;
            assert_eq!(output.bytes, input);
            assert_eq!(output.immediate_modes, [false, true]);
            assert!(!output.csi_u_set);
        }

        #[tokio::test]
        async fn unterminated_osc_chain_drains_each_read_and_keeps_only_the_final_csi_u_state() {
            let input = b"\x1b]697;NewCmd\x1b]697;PreExec".repeat(2048);
            let mut reads: Vec<&[u8]> = input.chunks(127).collect();
            reads.push(b"\x1b]697;NewCmd\x07tail");
            let output = capture_pty_reads(&reads, true, false).await;
            let mut expected = input;
            expected.extend_from_slice(b"\x1b]697;NewCmd\x07\x1b[>1utail");
            assert_eq!(output.bytes, expected);
            let mut modes = [false, true].repeat(2048);
            modes.push(false);
            assert_eq!(output.immediate_modes, modes);
            assert!(output.csi_u_set);
        }

        #[tokio::test]
        async fn ready_sync_state_and_key_settlement_precede_ordinary_io() {
            let sender = RemoteSender::test_ready();
            let mut changes = sender.subscribe();
            let generation = sender.phase().ready_generation().unwrap();
            sender
                .for_generation(generation)
                .try_send_key(
                    hook_to_message(fastab_proto::remote_hooks::new_intercepted_key_hook(
                        None,
                        "navigateDown",
                        "x",
                    )),
                    bytes::Bytes::from_static(b"x"),
                )
                .unwrap();
            let frame = sender.test_hold_frame().await;
            sender.test_retire();
            drop(frame);
            let (control, controls) = flume::bounded(1);
            control.send(MainLoopEvent::UnlockInterception).unwrap();
            for step in 0..4 {
                let ready = select_main_loop_ready(
                    wait_for_pty_sync_timeout((step == 0).then(|| std::time::Instant::now() - Duration::from_secs(1))),
                    changes.changed(),
                    sender.key_delivery_changed(),
                    (
                        controls.recv_async(),
                        std::future::pending(),
                        std::future::pending(),
                        std::future::pending(),
                        std::future::pending(),
                        std::future::pending(),
                    ),
                    std::future::pending::<()>(),
                )
                .await;
                assert!(matches!(
                    (step, ready),
                    (0, MainLoopReady::SyncTimeout)
                        | (1, MainLoopReady::RemoteStateChanged)
                        | (2, MainLoopReady::KeyDeliveryChanged)
                        | (3, MainLoopReady::Control(Ok(MainLoopEvent::UnlockInterception)))
                ));
            }
        }

        #[tokio::test]
        async fn ready_key_settlement_flushes_ordinary_bytes_before_remote_insert() {
            for _ in 0..32 {
                use fastab_proto::figterm::InsertTextRequest;
                use fastab_proto::remote::{Clientbound, clientbound};

                let sender = RemoteSender::test_ready();
                let mut changes = sender.subscribe();
                let generation = sender.phase().ready_generation().unwrap();
                sender
                    .for_generation(generation)
                    .try_send_key(
                        hook_to_message(fastab_proto::remote_hooks::new_intercepted_key_hook(
                            None,
                            "navigateDown",
                            "x",
                        )),
                        bytes::Bytes::from_static(b"x"),
                    )
                    .unwrap();
                let mut frame = sender.test_hold_frame().await;
                frame.write_to(&mut tokio::io::sink()).await.unwrap();
                drop(frame);
                let (remote, remotes) = flume::bounded(1);
                remote
                    .send(RemoteIncoming {
                        generation,
                        message: Clientbound {
                            packet: Some(clientbound::Packet::Request(clientbound::Request {
                                request: Some(clientbound::request::Request::InsertText(InsertTextRequest {
                                    insertion: Some("completion".into()),
                                    ..Default::default()
                                })),
                                ..Default::default()
                            })),
                        },
                        permit: crate::ipc::incoming::IncomingBudget::new(1, 1024)
                            .admit(1024)
                            .await
                            .unwrap(),
                    })
                    .unwrap();
                let (socket, mut peer) = UnixStream::pair().unwrap();
                let mut master: Box<dyn AsyncMasterPty + Send + Sync> = Box::new(SocketMaster(socket));
                let mut buffer = [0; 16];
                let mut deferred = BytesMut::from(b"ordinary".as_slice());
                let mut inserts = VecDeque::new();
                let mut interceptor = KeyInterceptor::new();
                let events = LocalEvents::default();
                let term = parser_term(&sender, &events, false);
                let (control, _controls) = flume::bounded(1);
                for step in 0..2 {
                    let ready = select_main_loop_ready(
                        wait_for_pty_sync_timeout(None),
                        changes.changed(),
                        sender.key_delivery_changed(),
                        (
                            std::future::pending(),
                            std::future::pending(),
                            master.read(&mut buffer),
                            remotes.recv_async(),
                            std::future::pending(),
                            std::future::pending(),
                        ),
                        std::future::pending::<()>(),
                    )
                    .await;
                    match (step, ready) {
                        (0, MainLoopReady::KeyDeliveryChanged) => {
                            reconcile_remote_state(&sender, &mut interceptor);
                            flush_settled_pty_input(
                                &sender,
                                master.as_mut(),
                                &mut deferred,
                                &mut inserts,
                                &mut interceptor,
                                false,
                            )
                            .await
                            .unwrap();
                        },
                        (1, MainLoopReady::Remote(Ok(incoming))) => {
                            process_remote_message(
                                incoming.message,
                                control.clone(),
                                sender.for_generation(incoming.generation),
                                incoming.permit,
                                &term,
                                &mut master,
                                &mut interceptor,
                            )
                            .await
                            .unwrap();
                        },
                        _ => panic!("settlement must precede the already-ready InsertText"),
                    }
                }
                let mut output = [0; 18];
                tokio::time::timeout(Duration::from_secs(2), peer.read_exact(&mut output))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(&output, b"ordinarycompletion");
                assert!(deferred.is_empty());
            }
        }

        #[tokio::test]
        async fn ready_pty_tail_is_forwarded_before_ready_child_exit() {
            for _ in 0..32 {
                let sender = RemoteSender::test_disconnected();
                let events = LocalEvents::default();
                let mut term = parser_term(&sender, &events, false);
                let mut processor = Processor::new();
                let (socket, mut peer) = UnixStream::pair().unwrap();
                let tail = b"last output before exit";
                peer.write_all(tail).await.unwrap();
                // Prove kernel readiness before marking the child exited; this
                // regression concerns ready output, not a new EOF drain protocol.
                socket.readable().await.unwrap();
                let mut master = SocketMaster(socket);
                let (child, mut child_exit) = oneshot::channel();
                child.send(()).unwrap();
                let (mut stdout, mut captured) = UnixStream::pair().unwrap();
                let mut terminal = ModeTerminal::default();
                let mut csi_u = CsiUState::default();
                let mut deferred = BytesMut::new();
                let mut inserts = VecDeque::new();
                let mut interceptor = KeyInterceptor::new();
                let mut buffer = [0; 8];
                let mut forwarded = 0;
                loop {
                    let ready = select_main_loop_ready(
                        wait_for_pty_sync_timeout(None),
                        std::future::pending::<()>(),
                        sender.key_delivery_changed(),
                        (
                            std::future::pending(),
                            std::future::pending(),
                            master.read(&mut buffer),
                            std::future::pending(),
                            std::future::pending(),
                            std::future::pending(),
                        ),
                        &mut child_exit,
                    )
                    .await;
                    match ready {
                        MainLoopReady::Pty(Ok(size)) if size > 0 => {
                            let mut control = MainLoopControl {
                                remote_sender: &sender,
                                master: &mut master,
                                deferred_pty_input: &mut deferred,
                                deferred_inserts: &mut inserts,
                                key_interceptor: &mut interceptor,
                                terminal: &mut terminal,
                                stdout: &mut stdout,
                                csi_u: &mut csi_u,
                                bracketed_paste: false,
                            };
                            process_pty_bytes(&buffer[..size], &mut processor, &mut term, &events, &mut control)
                                .await
                                .unwrap();
                            forwarded += size;
                        },
                        MainLoopReady::ChildExited => break,
                        _ => panic!("only buffered PTY bytes or child exit should be ready"),
                    }
                }
                assert_eq!(forwarded, tail.len());
                let mut output = vec![0; tail.len()];
                tokio::time::timeout(Duration::from_secs(2), captured.read_exact(&mut output))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(output, tail);
            }
        }

        #[tokio::test]
        async fn eof_waits_for_actual_key_settlement_then_replays_before_input_and_insert() {
            for delivered in [false, true] {
                let sender = RemoteSender::test_ready();
                let generation = sender.phase().ready_generation().unwrap();
                let bound = sender.for_generation(generation);
                let message = || {
                    hook_to_message(fastab_proto::remote_hooks::new_intercepted_key_hook(
                        None,
                        "navigateDown",
                        "x",
                    ))
                };
                bound
                    .try_send_key(message(), bytes::Bytes::from_static(b"\xffA"))
                    .unwrap();
                let mut frame = sender.test_hold_frame().await;
                bound.try_send_key(message(), bytes::Bytes::from_static(b"B")).unwrap();
                let (socket, mut peer) = UnixStream::pair().unwrap();
                let mut master = SocketMaster(socket);
                let mut deferred = BytesMut::from(b"ordinary".as_slice());
                let mut inserts = VecDeque::from([
                    DeferredInsert {
                        insert: b"stale".to_vec(),
                        unlock: false,
                        bracketed: false,
                        execute: false,
                        origin: RequestOrigin::Remote(generation),
                    },
                    DeferredInsert {
                        insert: b"local".to_vec(),
                        unlock: false,
                        bracketed: false,
                        execute: false,
                        origin: RequestOrigin::Local,
                    },
                ]);
                let mut interceptor = KeyInterceptor::new();
                let ending = finish_pty_input(
                    &sender,
                    &mut master,
                    &mut deferred,
                    &mut inserts,
                    &mut interceptor,
                    false,
                );
                tokio::pin!(ending);
                std::future::poll_fn(|cx| {
                    assert!(ending.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                if delivered {
                    frame.write_to(&mut tokio::io::sink()).await.unwrap();
                }
                sender.test_retire();
                // Retirement alone cannot authorize replay of the in-flight
                // frame. Dropping it is the real writer cancellation boundary.
                if !delivered {
                    std::future::poll_fn(|cx| {
                        assert!(ending.as_mut().poll(cx).is_pending());
                        Poll::Ready(())
                    })
                    .await;
                }
                drop(frame);
                tokio::time::timeout(Duration::from_secs(2), &mut ending)
                    .await
                    .unwrap()
                    .unwrap();
                let expected = if delivered {
                    b"Bordinarylocal".as_slice()
                } else {
                    b"\xffABordinarylocal".as_slice()
                };
                let mut output = vec![0; expected.len()];
                tokio::time::timeout(Duration::from_secs(2), peer.read_exact(&mut output))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(output, expected);
                assert!(!sender.has_pending_keys());
            }
        }

        #[tokio::test]
        async fn deferred_large_input_releases_capacity_only_after_successful_write() {
            for success in [false, true] {
                let (socket, peer) = UnixStream::pair().unwrap();
                let mut master = SocketMaster(socket);
                let capture = if success {
                    Some(tokio::spawn(async move {
                        let mut peer = peer;
                        let mut output = Vec::new();
                        peer.read_to_end(&mut output).await.unwrap();
                        output
                    }))
                } else {
                    drop(peer);
                    None
                };
                let expected = vec![b'x'; 128 * 1024];
                let mut deferred = BytesMut::from(expected.as_slice());
                let capacity = deferred.capacity();
                let mut inserts = VecDeque::new();
                let result = flush_settled_pty_input(
                    &RemoteSender::test_disconnected(),
                    &mut master,
                    &mut deferred,
                    &mut inserts,
                    &mut KeyInterceptor::new(),
                    false,
                )
                .await;
                if success {
                    result.unwrap();
                    assert!(deferred.is_empty());
                    assert!(deferred.capacity() <= 64 * 1024);
                    drop(master);
                    assert_eq!(capture.unwrap().await.unwrap(), expected);
                } else {
                    assert!(result.is_err());
                    assert_eq!(deferred.as_ref(), expected);
                    assert_eq!(deferred.capacity(), capacity);
                }
            }
        }

        #[tokio::test]
        async fn insertion_timer_is_idle_without_lock_and_still_expires_pending_insert() {
            let mut retry = InsertionRetry::default();
            assert!(!retry.active());
            let events = LocalEvents::default();
            let mut term = parser_term(&RemoteSender::test_disconnected(), &events, false);
            let mut parser = Processor::new();
            for byte in b"\x1b]697;NewCmd\x07typed" {
                parser.advance(&mut term, *byte);
            }
            let mut lock = Some(InsertionLock {
                at: Instant::now(),
                expected: "pending".into(),
                origin: RequestOrigin::Local,
            });
            retry.update(lock.as_ref().map(|lock| lock.at));
            assert!(retry.active());
            let deadline = retry.deadline;
            retry.update(lock.as_ref().map(|lock| lock.at));
            assert_eq!(
                retry.deadline, deadline,
                "input must not extend an existing retry deadline"
            );
            assert!(insertion_lock_is_active(&mut lock, &term));
            retry.wait().await;
            retry.tick();
            assert!(!insertion_lock_is_active(&mut lock, &term));
            assert!(lock.is_none());
            retry.update(None);
            assert!(!retry.active());
            let waiting = retry.wait();
            tokio::pin!(waiting);
            std::future::poll_fn(|cx| {
                assert!(waiting.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
        }
    }

    #[test]
    fn pty_wrapper_suffix_is_stripped_for_shell_matching() {
        assert_eq!(PTY_BINARY_NAME, "fterm");
        assert_eq!(without_pty_wrapper_suffix("zsh"), "zsh");
        assert_eq!(without_pty_wrapper_suffix(&format!("zsh ({PTY_BINARY_NAME})")), "zsh");
        assert_eq!(without_pty_wrapper_suffix("zsh (fastabterm)"), "zsh");
        assert_eq!(without_pty_wrapper_suffix("bash (ecterm)"), "bash");
        assert_eq!(without_pty_wrapper_suffix("fish (figterm)"), "fish");
        assert_eq!(without_pty_wrapper_suffix("zsh (qterm)"), "zsh");
    }

    #[test]
    fn hostname_does_not_need_sysinfo() {
        assert!(hostname().is_some_and(|name| !name.is_empty()));
    }

    #[test]
    fn autocomplete_enabled_test() {
        assert!(autocomplete_enabled(&Env::new_fake()));
        assert!(autocomplete_enabled(&Env::from_slice(&[(Q_DISABLE_AUTOCOMPLETE, "")])));
        assert!(!autocomplete_enabled(&Env::from_slice(&[(
            Q_DISABLE_AUTOCOMPLETE,
            "1"
        )])));
        assert!(!autocomplete_enabled(&Env::from_slice(&[(
            Q_DISABLE_AUTOCOMPLETE,
            "1"
        )])));
    }

    #[test]
    fn edit_buffer_sends_env_only_after_shell_context_updates() {
        let mut progress = ContextProgress::default();
        note_shell_context_updated();
        let epoch = shell_context_epoch();
        assert!(progress.needs_environment(epoch));
        progress.record(ContextAdmission {
            full_context: true,
            environment_epoch: Some(epoch),
        });
        assert!(!progress.needs_environment(epoch));
        note_shell_context_updated();
        let next = shell_context_epoch();
        assert_ne!(next, epoch);
        assert!(progress.needs_environment(next));
        progress.record(ContextAdmission {
            full_context: false,
            environment_epoch: Some(next),
        });
        assert!(!progress.needs_environment(next));
        // A new connection must synchronize even when the shell epoch did not change.
        let reconnected = ContextProgress::default();
        assert!(!reconnected.full_context_admitted);
        assert!(reconnected.needs_environment(next));
    }
}

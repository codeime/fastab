//! GPUI overlay controller: suggestion list, caret placement, key actions, engine.

pub(crate) mod ai;
mod file_icons;

#[cfg(test)]
#[path = "overlay/pending_tests.rs"]
mod pending_tests;

#[cfg(test)]
#[path = "overlay/tab_tests.rs"]
mod tab_tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fastab_engine::{
    CompleteRequest, CompleteResult, EngineClient, SessionId, ranking_root_command, tokenize, ui_completion_deadline,
};
use fastab_gpui::{
    ClickInsert, DEFAULT_FONT_SIZE, DEFAULT_MAX_LIST_HEIGHT, DEFAULT_WIDTH, OverlayHandle, OverlayState, OverlayTheme,
    SuggestionItem, TabPrefix, open_overlay_window, overlay_content_size_with_context, park_overlay_handle,
    position_overlay, screens_quartz, tab_prefix_insertion, theme_from_json, unquote_shell_token,
};
use fastab_proto::figterm::Action;
use fastab_proto::local::caret_position_hook::Origin;
use fastab_remote_ipc::figterm::{FigtermCommand, FigtermState, InterceptMode};
use fastab_settings::keybindings::{KeyBinding, KeyBindings};
use gpui::{App, AppContext as _, Entity, Pixels, Point, Size, px, size};
use tao::dpi::{LogicalPosition, LogicalSize, Position};
use tracing::{debug, error, warn};
use unicode_width::UnicodeWidthChar;
use uuid::Uuid;

use crate::event::{Event, WindowPosition};
use crate::event_loop::EventLoopProxy;
use crate::platform::PlatformState;

const LAYOUT_RETRY_DELAYS_MS: [u64; 4] = [16, 32, 64, 128];

/// Overlay is always on. `EC_GPUI_OVERLAY=0` remains an emergency kill switch.
pub fn gpui_overlay_enabled() -> bool {
    if let Ok(val) = std::env::var("EC_GPUI_OVERLAY") {
        let val = val.to_ascii_lowercase();
        return val != "0" && val != "false" && val != "off";
    }
    true
}

/// Live TCC state: the cached flag behind `ReloadAccessibility` can lag.
fn accessibility_grant_missing() -> bool {
    #[cfg(target_os = "macos")]
    {
        !macos_utils::accessibility::accessibility_is_enabled()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// IME-only terminals keep every split in one AX element, so a new pane's
/// first prompt is the only sign the user moved. Elsewhere a pane switch
/// already arrives as a focus change.
fn frontmost_reports_pane_switches() -> bool {
    // Tests must not depend on whichever app is frontmost on the host.
    #[cfg(all(target_os = "macos", not(test)))]
    {
        macos_utils::window_server::frontmost_application()
            .is_none_or(|app| crate::platform::hide_overlay_on_element_change(&app.bundle_id))
    }
    #[cfg(any(not(target_os = "macos"), test))]
    {
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LastInput {
    buffer: String,
    cwd: String,
    cursor: u32,
    session_id: Uuid,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompletionDisplayState {
    Idle,
    Requested,
    WaitingForCaret,
}

/// Explicit visibility requests are independent of retained rows and caret state.
/// A Tab intent belongs to one exact input; it must never accept a newer result.
struct ShowIntent {
    input: LastInput,
    token: u64,
    tabs: usize,
    result_ready: bool,
    // Dropping an intent cancels its timer as well. A long script budget must
    // not keep one detached timeout alive for every completed/cancelled Show.
    _expiry: gpui::Task<()>,
}

/// How long a request may run before `···` replaces the list. Fig's
/// `App.tsx` started this timer as soon as any generator was `loading`.
const LOADING_THRESHOLD: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy)]
struct RequestOptions {
    /// Bypass the duplicate-input guard: settings changes and debounce
    /// follow-ups re-run a buffer whose text did not change.
    force: bool,
    /// Time left before the `···` marker for this request.
    loading_threshold: Duration,
}

impl RequestOptions {
    fn keystroke() -> Self {
        Self {
            force: false,
            loading_threshold: LOADING_THRESHOLD,
        }
    }

    fn forced() -> Self {
        Self {
            force: true,
            loading_threshold: LOADING_THRESHOLD,
        }
    }
}

pub struct OverlayController {
    pub state: Entity<OverlayState>,
    handle: Arc<Mutex<Option<OverlayHandle>>>,
    engine: EngineClient,
    session_id: Arc<Mutex<Option<Uuid>>>,
    generation: Arc<AtomicU64>,
    /// Invalidates an older native-window retry chain when a newer layout,
    /// completion generation, park, or successful placement supersedes it.
    layout_retry_token: AtomicU64,
    /// Generation that turned the `···` marker on, or `NO_LOADING_OWNER`.
    ///
    /// The legacy WebView derived its loading flag from the set of in-flight
    /// generators, so it could never be left on with nothing running. Here the
    /// marker is a latch driven by two separate events, so it needs an explicit
    /// owner: whichever request switched it on must also switch it off, even
    /// when its result arrives too late to be displayed.
    loading_owner: Arc<AtomicU64>,
    enabled: bool,
    /// `enabled` is false because the Accessibility grant was missing, which a
    /// keystroke after the grant may undo before its notification arrives.
    disabled_by_accessibility: bool,
    /// The platform's sample turned completion off for a reason other than the
    /// grant. Only a confirmed sample for the frontmost app survives a keystroke.
    #[cfg(target_os = "macos")]
    terminal_disable: Option<TerminalDisable>,
    /// Last applied saved preference, separate from the temporary shortcut mode.
    configured_fuzzy_search: bool,
    figterm_state: Arc<FigtermState>,
    platform_state: Arc<PlatformState>,
    last_position: Arc<Mutex<Option<WindowPosition>>>,
    last_input: Arc<Mutex<Option<LastInput>>>,
    completion_display: CompletionDisplayState,
    show_intent: Option<ShowIntent>,
    /// Own the outer debounce future so obsolete input releases its payload
    /// immediately. GPUI's already-enqueued native timer may still wake later.
    debounce_task: Option<gpui::Task<()>>,
    /// Keep explicit Show's argument visibility through same-input debounce
    /// follow-ups after the first displayed rows have consumed its Tab.
    explicit_show_pending: bool,
    show_token: u64,
    /// Input of the latest request, keyed by its generation.
    request_input: Option<(u64, LastInput)>,
    /// Input the displayed rows were computed for. Typing on while a request
    /// is in flight leaves them on screen, but their search term no longer
    /// matches the buffer, so inserting from them would garble the token.
    rows_input: Option<LastInput>,
    /// An insert action that arrived while the rows were stale. It belongs to
    /// one exact input and replays on that input's rows, never a newer one.
    /// The raw key fterm handed over may go back to the shell when those rows
    /// turn out to have nothing to act on.
    pending_insert_action: Option<(String, LastInput, Option<String>)>,
    /// Buffer we expect the shell to report back after our own insertion.
    /// Stands in for the legacy `justInserted` flag, which kept the paste
    /// heuristic from mistaking an accepted completion for a paste.
    self_insertion: Arc<Mutex<Option<String>>>,
    proxy: EventLoopProxy,
    jev: ai::JevRuntime,
    file_icons: Entity<file_icons::FileIconCache>,
    _ai_observer: gpui::Subscription,
}

impl OverlayController {
    pub(crate) fn request_visible_icons(
        &mut self,
        generation: u64,
        revision: (u64, u64),
        range: std::ops::Range<usize>,
        cx: &mut App,
    ) {
        if self.generation.load(Ordering::Relaxed) != generation {
            return;
        }
        let Some(input) = self.last_input.lock().unwrap_or_else(|err| err.into_inner()).clone() else {
            return;
        };
        self.file_icons.update(cx, |icons, cx| {
            if icons.prepare_visible(&input.cwd, &self.state, revision, range, cx) {
                icons.finish_batch(&self.state, &self.handle, cx);
                icons.load_requested(
                    &input.cwd,
                    self.state.downgrade(),
                    self.handle.clone(),
                    self.generation.clone(),
                    generation,
                    cx,
                );
            }
        });
    }

    pub fn start(
        cx: &mut App,
        engine: EngineClient,
        proxy: EventLoopProxy,
        figterm_state: Arc<FigtermState>,
        platform_state: Arc<PlatformState>,
    ) -> anyhow::Result<Self> {
        let session_id = Arc::new(Mutex::new(None));
        let state = cx.new(|_| {
            let mut overlay = OverlayState::new();
            apply_settings(&mut overlay, None);
            overlay
        });
        let click_session = session_id.clone();
        let click_generation = Arc::new(AtomicU64::new(0));
        let generation = click_generation.clone();
        let click_proxy = proxy.clone();
        let disable_session = session_id.clone();
        let disable_proxy = proxy.clone();
        let icon_proxy = proxy.clone();
        let icon_generation = generation.clone();
        state.update(cx, |overlay, _cx| {
            overlay.on_visible_range = Some(Arc::new(move |range, revision| {
                let _ = icon_proxy.send_event(Event::GpuiOverlayVisibleRows {
                    generation: icon_generation.load(Ordering::Relaxed),
                    revision,
                    range,
                });
            }));
            overlay.on_click_insert = Some(Arc::new(move |click| {
                let Some(session_id) = *click_session.lock().unwrap_or_else(|err| err.into_inner()) else {
                    return;
                };
                let _ = click_proxy.send_event(Event::AutocompleteClick {
                    click,
                    session_id,
                    generation: click_generation.load(Ordering::Relaxed),
                });
            }));
            overlay.on_disable_dev_mode = Some(Arc::new(move || {
                let _ = fastab_settings::settings::set_value("autocomplete.developerModeNPM", false);
                let Some(session_id) = *disable_session.lock().unwrap_or_else(|err| err.into_inner()) else {
                    return;
                };
                let _ = disable_proxy.send_event(Event::AutocompleteAction {
                    action: "relayoutOverlay".into(),
                    session_id,
                    key: None,
                });
            }));
        });
        let mut observed_ai_revision = state.read(cx).ai_revision;
        let ai_proxy = proxy.clone();
        let ai_observer = cx.observe(&state, move |state, cx| {
            let revision = state.read(cx).ai_revision;
            if revision != observed_ai_revision {
                observed_ai_revision = revision;
                let _ = ai_proxy.send_event(Event::JevContextChanged);
            }
        });
        let handle = Arc::new(Mutex::new(None));
        let configured_fuzzy_search = state.read(cx).fuzzy_search;
        let accessibility_enabled = PlatformState::accessibility_is_enabled().unwrap_or(true);
        let mut controller = Self {
            state,
            handle,
            engine,
            session_id,
            generation,
            layout_retry_token: AtomicU64::new(0),
            loading_owner: Arc::new(AtomicU64::new(NO_LOADING_OWNER)),
            enabled: gpui_overlay_enabled()
                && !fastab_settings::settings::get_bool_or("autocomplete.disable", false)
                && accessibility_enabled,
            disabled_by_accessibility: !accessibility_enabled,
            #[cfg(target_os = "macos")]
            terminal_disable: None,
            configured_fuzzy_search,
            figterm_state,
            platform_state,
            last_position: Arc::new(Mutex::new(None)),
            last_input: Arc::new(Mutex::new(None)),
            completion_display: CompletionDisplayState::Idle,
            show_intent: None,
            debounce_task: None,
            explicit_show_pending: false,
            show_token: 0,
            request_input: None,
            rows_input: None,
            pending_insert_action: None,
            self_insertion: Arc::new(Mutex::new(None)),
            proxy,
            jev: ai::JevRuntime::default(),
            file_icons: cx.new(|_| file_icons::FileIconCache::default()),
            _ai_observer: ai_observer,
        };
        controller.reload_jev(cx);
        Ok(controller)
    }

    fn ensure_window(&mut self, cx: &mut App) -> Option<OverlayHandle> {
        if self.state.read(cx).visible {
            self.file_icons.update(cx, |icons, _| icons.cancel_idle());
        }
        ensure_overlay_window(&self.handle, &self.state, cx)
    }

    fn park_window(&self, cx: &mut App) {
        self.cancel_layout_retry();
        let _ = park_overlay_slot(&self.handle, cx);
        self.file_icons.update(cx, |icons, cx| {
            icons.schedule_idle(self.state.downgrade(), self.handle.clone(), cx);
        });
    }

    fn cancel_layout_retry(&self) {
        self.layout_retry_token.fetch_add(1, Ordering::Relaxed);
    }

    fn bump_generation(&mut self) -> u64 {
        self.debounce_task = None;
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn take_loading_owner(&self) -> Option<u64> {
        match self.loading_owner.swap(NO_LOADING_OWNER, Ordering::Relaxed) {
            NO_LOADING_OWNER => None,
            owner => Some(owner),
        }
    }

    /// Drop the `···` marker while keeping any rows that were already on
    /// screen. With nothing to show, the window is parked instead of being left
    /// as an empty card over the terminal.
    fn clear_stale_loading(&mut self, cx: &mut App) {
        let park = self.state.update(cx, |overlay, cx| {
            if !overlay.loading {
                return false;
            }
            overlay.loading = false;
            let empty = overlay.items.is_empty();
            if empty {
                overlay.visible = false;
            }
            cx.notify();
            empty
        });
        if park {
            self.park_window(cx);
        }
    }

    /// Turn the marker off unless a newer request already owns it. Returns
    /// `true` when this call released the latch.
    fn release_loading_owner(&self, generation: u64) -> bool {
        let owner = self.loading_owner.load(Ordering::Relaxed);
        if !loading_owner_is_released_by(owner, generation) {
            return false;
        }
        self.loading_owner
            .compare_exchange(owner, NO_LOADING_OWNER, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    fn current_session(&self) -> Option<Uuid> {
        *self.session_id.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn forget_last_input(&self) {
        *self.last_input.lock().unwrap_or_else(|err| err.into_inner()) = None;
    }

    fn take_self_insertion(&self) -> Option<String> {
        self.self_insertion.lock().unwrap_or_else(|err| err.into_inner()).take()
    }

    /// Record the session that owns the overlay. Returns `true` only for a
    /// switch between two real sessions (not the initial assignment).
    fn set_session(&mut self, session_id: Uuid) -> bool {
        let (changed, switched) = {
            let mut current = self.session_id.lock().unwrap_or_else(|err| err.into_inner());
            let switched = current.is_some() && *current != Some(session_id);
            let changed = session_changed(*current, session_id);
            *current = Some(session_id);
            (changed, switched)
        };
        if changed {
            // A completion from the previous terminal must not repaint this
            // overlay after a session switch. The caller also clears UI state
            // and the old caret when this is a real session-to-session switch.
            self.bump_generation();
            let mut last_input = self.last_input.lock().unwrap_or_else(|err| err.into_inner());
            if last_input.as_ref().is_some_and(|input| input.session_id != session_id) {
                *last_input = None;
            }
        }
        switched
    }

    pub fn apply_theme(&mut self, cx: &mut App) {
        // Startup and the file watcher populate the shared settings cache;
        // in-process writes also update it before dispatching this event.
        // Reloading here would take a blocking file lock on the UI thread,
        // including after an AI input edit triggers a background save.
        let previous_fuzzy_search = self.configured_fuzzy_search;
        self.configured_fuzzy_search = self.state.update(cx, |overlay, cx| {
            let configured = apply_settings(overlay, Some(previous_fuzzy_search));
            cx.notify();
            configured
        });
        self.reload_jev_if_changed(cx);
    }

    /// `ftab hook clear-autocomplete-cache`. The WebView answered its
    /// `clear-cache` event by dropping every loaded spec and generator result;
    /// the engine worker owns those now.
    pub fn clear_caches(&self) {
        if let Err(err) = self.engine.clear_caches() {
            warn!(%err, "failed to clear the completion engine caches");
        }
    }

    pub(crate) fn resource_diagnostics(&self, cx: &App) -> fastab_engine::HostResourceDiagnostics {
        let mut snapshot = self.file_icons.read(cx).diagnostics();
        snapshot.overlay_visible = self.state.read(cx).visible;
        snapshot
    }

    pub(crate) fn engine_diagnostics(
        &self,
    ) -> impl std::future::Future<Output = anyhow::Result<fastab_engine::EngineClientDiagnostics>> + Send + 'static
    {
        self.engine.diagnostics()
    }

    fn retire_completion(&mut self) {
        self.debounce_task = None;
        self.explicit_show_pending = false;
        self.pending_insert_action = None;
        // Each holds a full copy of the buffer; a long command must not outlive its list.
        self.request_input = None;
        self.rows_input = None;
        self.completion_display = CompletionDisplayState::Idle;
        if let Some(session) = self.current_session() {
            self.notify_input_ended(session);
        }
    }

    pub fn set_enabled(&mut self, enabled: bool, cx: &mut App) {
        self.set_enabled_sampled(enabled, None, cx);
    }

    /// `enabled` and `accessibility_missing` were sampled together for `app`; a
    /// later live read could disagree with the reason that state was computed.
    /// `sample_uncertain` means part of that state could not be read.
    #[cfg(target_os = "macos")]
    pub fn set_terminal_enabled(
        &mut self,
        app: macos_utils::window_server::ApplicationSpecifier,
        enabled: bool,
        accessibility_missing: bool,
        sample_uncertain: bool,
        cx: &mut App,
    ) {
        self.set_enabled_sampled(enabled, Some(accessibility_missing), cx);
        self.terminal_disable = (!self.enabled && !self.disabled_by_accessibility).then_some(if sample_uncertain {
            TerminalDisable::Unconfirmed
        } else {
            TerminalDisable::Confirmed(app)
        });
    }

    fn terminal_disable_is_stale(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            disable_sample_is_stale(
                self.terminal_disable.as_ref(),
                macos_utils::window_server::frontmost_application,
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    fn set_enabled_sampled(&mut self, enabled: bool, accessibility_missing: Option<bool>, cx: &mut App) {
        let was_enabled = self.enabled;
        self.enabled = enabled && gpui_overlay_enabled();
        self.disabled_by_accessibility =
            !self.enabled && accessibility_missing.unwrap_or_else(accessibility_grant_missing);
        #[cfg(target_os = "macos")]
        {
            self.terminal_disable = None;
        }
        if !self.enabled {
            self.dismiss(cx);
        } else if !was_enabled
            && self.has_showable_input()
            && !self.state.read(cx).only_show_on_tab
            && !self.state.read(cx).suppress_until_shown
        {
            *self.last_position.lock().unwrap_or_else(|err| err.into_inner()) = None;
            self.completion_display = CompletionDisplayState::WaitingForCaret;
            let _ = self.proxy.send_event(Event::PlatformBoundEvent(
                crate::platform::PlatformBoundEvent::EditBufferChanged,
            ));
        }
        // Re-enabling Tab-only completion restores its explicit entry point
        // immediately, even when no edit-buffer notification follows.
        self.sync_own_intercept(cx);
    }

    pub fn hide(&mut self, cx: &mut App) {
        self.cancel_show();
        self.retire_completion();
        self.cancel_jev(cx);
        self.bump_generation();
        self.take_loading_owner();
        self.state.update(cx, |overlay, cx| {
            overlay.hide();
            cx.notify();
        });
        self.park_window(cx);
        self.sync_own_intercept(cx);
    }

    pub fn end_input(&mut self, session_id: Uuid, cx: &mut App) {
        self.notify_input_ended(session_id);
        if self.current_session() == Some(session_id) {
            // Settings refreshes must not resubmit an ended buffer. The next
            // real input event will establish a new buffer for this session.
            self.forget_last_input();
            self.take_self_insertion();
            self.dismiss(cx);
        }
    }

    fn notify_input_ended(&self, session_id: Uuid) {
        // Starts the engine's idle grace, so the heap relief has to wait it out.
        crate::heap_trim::note_activity();
        if let Err(error) = self.engine.end_input(SessionId::new(session_id.as_u128())) {
            debug!(%error, "Failed to notify engine that input ended");
        }
    }

    pub fn hide_until_shown(&mut self, cx: &mut App) {
        self.cancel_show();
        self.retire_completion();
        self.cancel_jev(cx);
        // Hiding is a cancellation boundary too. Otherwise an in-flight
        // completion can repopulate hidden state and later be shown as stale.
        self.bump_generation();
        self.take_loading_owner();
        self.state.update(cx, |overlay, cx| {
            overlay.hide_until_shown();
            cx.notify();
        });
        self.park_window(cx);
        self.sync_own_intercept(cx);
    }

    /// WebView `HIDDEN_UNTIL_KEYPRESS`: hide now, but the next real keystroke
    /// may show again. Esc uses `hide_until_shown` instead and must stay hidden.
    fn hide_until_keypress(&mut self, cx: &mut App) {
        self.hide(cx);
        self.state.update(cx, |overlay, cx| {
            overlay.suppress_until_shown = false;
            cx.notify();
        });
    }

    pub fn dismiss(&mut self, cx: &mut App) {
        self.cancel_show();
        self.retire_completion();
        self.cancel_jev(cx);
        self.bump_generation();
        self.take_loading_owner();
        self.state.update(cx, |overlay, cx| {
            overlay.dismiss();
            cx.notify();
        });
        self.park_window(cx);
        self.sync_own_intercept(cx);
    }

    pub fn show(&mut self, cx: &mut App) {
        self.show_kept_items(cx);
    }

    pub fn clear_caret_position(&mut self, cx: &mut App) {
        if self.show_intent.is_some() {
            *self.last_position.lock().unwrap_or_else(|err| err.into_inner()) = None;
            self.hide_until_shown(cx);
            return;
        }
        let resume = self.completion_display != CompletionDisplayState::Idle;
        *self.last_position.lock().unwrap_or_else(|err| err.into_inner()) = None;
        self.hide(cx);
        if resume {
            self.completion_display = CompletionDisplayState::WaitingForCaret;
        }
    }

    /// A caret from another window/pane cannot resume the previous input.
    /// The next real buffer event establishes the newly focused session.
    pub fn invalidate_caret_for_focus_change(&mut self, cx: &mut App) {
        self.forget_last_input();
        self.take_self_insertion();
        *self.last_position.lock().unwrap_or_else(|err| err.into_inner()) = None;
        self.dismiss(cx);
    }

    pub fn apply_position(&mut self, position: WindowPosition, platform_state: &PlatformState, cx: &mut App) {
        *self.last_position.lock().unwrap_or_else(|err| err.into_inner()) = Some(position);
        if self.completion_display == CompletionDisplayState::WaitingForCaret && self.enabled {
            self.recomplete(cx);
        }
        // Kept argument hints do not make a hidden overlay visible. In
        // particular, a caret update must not recreate its retired window.
        if !self.state.read(cx).visible {
            return;
        }
        let positioned = self.ensure_window(cx).is_some_and(|handle| {
            layout_overlay(
                &self.state,
                &self.handle,
                handle,
                &self.last_position,
                platform_state,
                cx,
            )
        });
        self.settle_show_layout(positioned);
        self.sync_own_intercept_for_layout(positioned, cx);
        self.update_layout_retry(positioned, cx);
    }

    fn relayout(&mut self, cx: &mut App) -> bool {
        if !self.state.read(cx).visible {
            return false;
        }
        let Some(handle) = self.ensure_window(cx) else {
            return false;
        };
        layout_overlay(
            &self.state,
            &self.handle,
            handle,
            &self.last_position,
            &self.platform_state,
            cx,
        )
    }

    pub(crate) fn relayout_and_sync(&mut self, cx: &mut App) -> bool {
        let positioned = self.relayout(cx);
        self.settle_show_layout(positioned);
        self.sync_own_intercept_for_layout(positioned, cx);
        self.update_layout_retry(positioned, cx);
        positioned
    }

    fn layout_retry_needed(&self, positioned: bool, cx: &App) -> bool {
        let overlay = self.state.read(cx);
        let has_content = overlay.loading || !overlay.items.is_empty() || overlay.has_current_arg();
        should_retry_layout(LayoutRetryInputs {
            overlay_visible: overlay.visible,
            has_content,
            has_position: self
                .last_position
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .is_some(),
            positioned,
        })
    }

    fn update_layout_retry(&mut self, positioned: bool, cx: &mut App) {
        let token = self.layout_retry_token.fetch_add(1, Ordering::Relaxed) + 1;
        if self.layout_retry_needed(positioned, cx) {
            self.schedule_layout_retry(self.generation.load(Ordering::Relaxed), token, 0, cx);
        }
    }

    fn schedule_layout_retry(&self, generation: u64, token: u64, attempt: u8, cx: &mut App) {
        let Some(delay) = layout_retry_delay(attempt) else {
            return;
        };
        let proxy = self.proxy.clone();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |_cx| {
            executor.timer(delay).await;
            let _ = proxy.send_event(Event::GpuiOverlayRelayoutRetry {
                generation,
                token,
                attempt,
            });
        })
        .detach();
    }

    pub fn retry_layout(&mut self, generation: u64, token: u64, attempt: u8, cx: &mut App) {
        if !layout_retry_is_current(
            generation,
            self.generation.load(Ordering::Relaxed),
            token,
            self.layout_retry_token.load(Ordering::Relaxed),
        ) {
            return;
        }
        // Hides and parks invalidate the token, but also check the live state
        // before touching the GPUI window. This keeps a delayed event from
        // reopening or recreating a window that no longer needs placement.
        if !self.layout_retry_needed(false, cx) {
            self.cancel_layout_retry();
            return;
        }

        let positioned = self.relayout(cx);
        self.settle_show_layout(positioned);
        self.sync_own_intercept_for_layout(positioned, cx);
        if positioned || !self.layout_retry_needed(positioned, cx) {
            self.cancel_layout_retry();
            return;
        }

        let next_attempt = attempt.saturating_add(1);
        if layout_retry_delay(next_attempt).is_some() {
            self.schedule_layout_retry(generation, token, next_attempt, cx);
        } else {
            self.cancel_layout_retry();
            self.hide(cx);
            warn!(
                attempts = LAYOUT_RETRY_DELAYS_MS.len(),
                "overlay native window remained unavailable after bounded relayout retries"
            );
        }
    }

    fn sync_own_intercept(&self, cx: &App) {
        self.sync_intercept(&self.figterm_state, cx);
    }

    fn sync_own_intercept_for_layout(&self, positioned: bool, cx: &App) {
        let Some(session_id) = self.current_session() else {
            return;
        };
        let overlay = self.state.read(cx);
        // Fig kept intercept on whenever suggestions.length > 0, including
        // during the loading spinner, because the previous rows stayed in the
        // store. Match that so Tab/Enter still reach the overlay.
        let has_items = !overlay.items.is_empty();
        let has_last_position = self
            .last_position
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .is_some();
        let (visible_intercept, global_intercept) = positioned_intercept_flags(InterceptInputs {
            overlay_visible: overlay.visible,
            has_items,
            positioned,
            has_last_position,
        });
        set_intercept_flags(
            &self.figterm_state,
            session_id,
            visible_intercept,
            global_intercept || self.has_showable_input(),
        );
    }

    fn sync_intercept(&self, figterm_state: &FigtermState, cx: &App) {
        let Some(session_id) = self.current_session() else {
            return;
        };
        let overlay = self.state.read(cx);
        let has_position = self
            .last_position
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .is_some();
        let has_items = !overlay.items.is_empty();
        set_intercept_flags(
            figterm_state,
            session_id,
            overlay.visible && has_items && has_position,
            self.has_showable_input(),
        );
    }

    fn has_showable_input(&self) -> bool {
        #[cfg(target_os = "macos")]
        if self.platform_state.inner().terminal_completion_disabled() {
            return false;
        }
        self.enabled
            && !fastab_settings::settings::get_bool_or("autocomplete.disable", false)
            && self
                .last_input
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .as_ref()
                .is_some_and(|input| {
                    Some(input.session_id) == self.current_session()
                        && !input.buffer.trim().is_empty()
                        && !cursor_is_inside_word(&input.buffer, input.cursor)
                })
    }

    pub(crate) fn wants_caret(&self, cx: &App) -> bool {
        self.has_showable_input()
            && (self.show_intent.is_some()
                || self.completion_display != CompletionDisplayState::Idle
                || self.state.read(cx).visible)
    }

    fn show_kept_items(&mut self, cx: &mut App) {
        self.request_show(false, cx);
    }

    fn request_show(&mut self, from_tab: bool, cx: &mut App) {
        if !self.has_showable_input() {
            return;
        }
        let input = self
            .last_input
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
            .unwrap();
        if let Some(intent) = &self.show_intent {
            if intent.input == input {
                if from_tab && intent.tabs > 0 {
                    // Repeated Tab before the first result belongs to the
                    // shell's own completion cycle. Return both together;
                    // never apply two edits against one pre-insert snapshot.
                    let mut intent = self.show_intent.take().unwrap();
                    intent.tabs += 1;
                    self.return_tabs(&intent, intent.tabs);
                    self.hide_until_shown(cx);
                } else if from_tab {
                    self.show_intent.as_mut().unwrap().tabs = 1;
                }
                return;
            }
        }
        self.cancel_show();
        self.retire_completion();
        self.bump_generation();
        self.take_loading_owner();
        self.state.update(cx, |overlay, cx| {
            overlay.hide();
            // Hidden input may have changed since these rows were produced.
            // Keep only selection identity: loading must not expose old edits.
            overlay.release_hidden_payload();
            overlay.suppress_until_shown = false;
            cx.notify();
        });
        self.park_window(cx);
        self.show_token = self.show_token.wrapping_add(1);
        let token = self.show_token;
        let proxy = self.proxy.clone();
        let executor = cx.background_executor().clone();
        let deadline = ui_completion_deadline().saturating_add(LOADING_THRESHOLD);
        let expiry = cx.spawn(async move |_cx| {
            executor.timer(deadline).await;
            let _ = proxy.send_event(Event::GpuiOverlayShowExpired { token });
        });
        self.show_intent = Some(ShowIntent {
            input,
            token,
            tabs: usize::from(from_tab),
            result_ready: false,
            _expiry: expiry,
        });
        self.explicit_show_pending = true;
        // Always request fresh geometry. A retained caret can belong to the
        // previous pane, or precede hidden typing that moved this cursor.
        *self.last_position.lock().unwrap_or_else(|err| err.into_inner()) = None;
        self.completion_display = CompletionDisplayState::WaitingForCaret;
        self.sync_own_intercept(cx);
        let _ = self.proxy.send_event(Event::PlatformBoundEvent(
            crate::platform::PlatformBoundEvent::EditBufferChanged,
        ));
    }

    fn show_input_is_current(&self, intent: &ShowIntent) -> bool {
        self.current_session() == Some(intent.input.session_id)
            && self.last_input.lock().unwrap_or_else(|err| err.into_inner()).as_ref() == Some(&intent.input)
            && self
                .figterm_state
                .with(&intent.input.session_id, |session| {
                    session.dead_since.is_none()
                        && session.edit_buffer.text == intent.input.buffer
                        && session.edit_buffer.cursor == i64::from(intent.input.cursor)
                })
                .unwrap_or(false)
    }

    fn return_tabs(&self, intent: &ShowIntent, tabs: usize) {
        if tabs == 0 || !self.show_input_is_current(intent) {
            return;
        }
        // `insertion_buffer` repairs divergent buffers in fterm; it is not a
        // compare-and-drop precondition. Never send an old snapshot as repair.
        let _ = self.figterm_state.with(&intent.input.session_id, |session| {
            if session.edit_buffer.text != intent.input.buffer
                || session.edit_buffer.cursor != i64::from(intent.input.cursor)
            {
                return;
            }
            let _ = session.send(FigtermCommand::InsertText {
                insertion: Some("\t".repeat(tabs)),
                deletion: Some(0),
                offset: None,
                immediate: Some(false),
                insertion_buffer: None,
                insert_during_command: None,
            });
        });
    }

    /// A deferred insert found nothing to insert, so a Tab fterm handed over
    /// goes to the shell after all, unless the line has moved on since. The
    /// buffer check can miss bytes typed a few milliseconds ago, so only a key
    /// that is harmless after them returns: a late Enter would run the newer,
    /// half-typed line.
    fn return_intercepted_key(&self, input: &LastInput, key: Option<String>) {
        let Some(key) = key.filter(|key| key == "\t") else {
            return;
        };
        let _ = self.figterm_state.with(&input.session_id, |session| {
            if session.edit_buffer.text != input.buffer || session.edit_buffer.cursor != i64::from(input.cursor) {
                return;
            }
            let _ = session.send(FigtermCommand::InsertText {
                insertion: Some(key),
                deletion: Some(0),
                offset: None,
                immediate: Some(false),
                insertion_buffer: None,
                insert_during_command: None,
            });
        });
    }

    fn accept_single_show_result(&self, result: &CompleteResult) -> bool {
        !result.pending_generators
            && self
                .show_intent
                .as_ref()
                .is_some_and(|intent| intent.tabs == 1 && self.show_input_is_current(intent))
            && result.suggestions.len() == 1
            && {
                let item = &result.suggestions[0];
                !item.is_dangerous
                    && safe_tab_row(&item.kind, item.insert_value.as_deref().or(Some(&item.name)))
                    && item
                        .separator_to_add
                        .as_deref()
                        .is_none_or(|value| !value.contains(['\n', '\r']))
            }
    }

    fn settle_show_layout(&mut self, positioned: bool) {
        if positioned && self.show_intent.as_ref().is_some_and(|intent| intent.result_ready) {
            self.show_intent = None;
        }
    }

    fn cancel_show(&mut self) {
        if let Some(intent) = self.show_intent.take() {
            self.return_tabs(&intent, intent.tabs);
        }
    }

    pub fn expire_show(&mut self, token: u64, cx: &mut App) {
        if self.show_intent.as_ref().is_none_or(|intent| intent.token != token) {
            return;
        }
        self.hide_until_shown(cx);
    }

    pub fn complete_buffer(
        &mut self,
        buffer: String,
        cwd: String,
        cursor: u32,
        session_id: Uuid,
        figterm_state: Arc<FigtermState>,
        cx: &mut App,
    ) {
        self.complete_buffer_inner(
            buffer,
            cwd,
            cursor,
            session_id,
            figterm_state,
            RequestOptions::keystroke(),
            cx,
        );
    }

    /// Another session owns the overlay, and the frontmost terminal reports
    /// its own pane switches, so input from `session_id` that carries nothing
    /// new must not take over. Not even right after a focus change: every pane
    /// a resize repaints reports at once, and the caret would come from the
    /// focused one whichever won. Typing there is what switches sessions.
    pub(crate) fn is_background_session(&self, session_id: Uuid) -> bool {
        self.current_session().is_some_and(|current| current != session_id) && frontmost_reports_pane_switches()
    }

    #[allow(clippy::too_many_arguments)]
    fn complete_buffer_inner(
        &mut self,
        buffer: String,
        cwd: String,
        cursor: u32,
        session_id: Uuid,
        figterm_state: Arc<FigtermState>,
        options: RequestOptions,
        cx: &mut App,
    ) {
        let RequestOptions {
            force,
            loading_threshold,
        } = options;
        if buffer.trim().is_empty() && self.is_background_session(session_id) {
            // A background tab's fresh prompt has nothing to show. Switching to
            // it would only dismiss the list the user is typing into.
            self.end_input(session_id, cx);
            return;
        }
        if self.current_session().is_some_and(|current| current != session_id) {
            self.show_intent = None;
            self.retire_completion();
        }
        let switched_session = self.set_session(session_id);
        if switched_session {
            // Do not expose rows, one-shot insertion state, or a caret from
            // the previously focused terminal while the new request runs.
            self.state.update(cx, |overlay, cx| {
                overlay.dismiss();
                cx.notify();
            });
            *self.last_position.lock().unwrap_or_else(|err| err.into_inner()) = None;
            self.park_window(cx);
            set_intercept(&figterm_state, session_id, false, false);
        }
        let next_input = LastInput {
            buffer: buffer.clone(),
            cwd: cwd.clone(),
            cursor,
            session_id,
        };
        if self
            .show_intent
            .as_ref()
            .is_some_and(|intent| intent.input != next_input)
        {
            // This input supersedes the intercepted Tab: do not replay it,
            // but retire the same native/loading resources as any other hide.
            self.show_intent = None;
            self.hide_until_shown(cx);
        }
        if self
            .pending_insert_action
            .as_ref()
            .is_some_and(|(_, input, _)| *input != next_input)
        {
            self.pending_insert_action = None;
        }
        let (duplicate_input, previous_input) = {
            let mut last_input = self.last_input.lock().unwrap_or_else(|err| err.into_inner());
            if last_input.as_ref() != Some(&next_input) {
                self.explicit_show_pending = false;
            }
            let duplicate = should_skip_duplicate_input(last_input.as_ref(), &next_input, force);
            (duplicate, last_input.replace(next_input))
        };
        if !duplicate_input || !self.jev_context_is_current() {
            self.cancel_jev(cx);
        }
        let previous_buffer = previous_input
            .filter(|input| input.session_id == session_id)
            .map(|input| input.buffer);
        let self_inserted = self.take_self_insertion().is_some_and(|predicted| predicted == buffer);
        #[cfg(target_os = "macos")]
        let terminal_disabled = self.platform_state.inner().terminal_completion_disabled();
        #[cfg(not(target_os = "macos"))]
        let terminal_disabled = false;
        // A missing grant, a sample taken for an app that is no longer
        // frontmost, or one whose state could not be read is undone here. A
        // terminal turned off for its own reason (e.g. an IME terminal without
        // the input method) stays off.
        if !self.enabled && !terminal_disabled && (self.disabled_by_accessibility || self.terminal_disable_is_stale()) {
            #[cfg(target_os = "macos")]
            {
                // AX grant can land while the cached `enabled` flag is still false (notification
                // lag). Re-read the live TCC flag so the first keystroke after granting works.
                if gpui_overlay_enabled()
                    && !fastab_settings::settings::get_bool_or("autocomplete.disable", false)
                    && macos_utils::accessibility::accessibility_is_enabled()
                {
                    self.enabled = true;
                    self.disabled_by_accessibility = false;
                    self.terminal_disable = None;
                }
            }
        }
        let input_ended = buffer.trim().is_empty();
        if input_ended {
            self.notify_input_ended(session_id);
        }
        if !self.enabled || terminal_disabled || input_ended || cursor_is_inside_word(&buffer, cursor) {
            self.dismiss(cx);
            set_intercept(&figterm_state, session_id, false, false);
            return;
        }

        // Accepting a completion produces one shell buffer notification. The
        // legacy WebView kept itself in HIDDEN_BY_INSERTION for that update so
        // the just-accepted row did not immediately reappear. Consume this
        // one-shot marker before starting another engine request; the next
        // real keypress is then free to show fresh suggestions.
        let suppress_insert_update = self.state.update(cx, |overlay, cx| {
            let suppress = overlay.take_suppress_completion_for(&buffer, cursor);
            if suppress {
                overlay.hide();
                cx.notify();
            }
            suppress
        });
        if suppress_insert_update {
            self.hide(cx);
            self.sync_intercept(&figterm_state, cx);
            return;
        }

        // Terminal hooks can publish the same edit buffer repeatedly while a
        // command writes output. Invalidating and restarting the in-flight
        // completion each time can otherwise keep the UI in loading forever.
        // Explicit recomputation (history/fuzzy/settings changes) bypasses
        // this guard through `force`.
        if duplicate_input {
            return;
        }

        // Two legacy `HIDDEN_UNTIL_KEYPRESS` triggers. Backspacing a whole
        // token away should not pop the previous token's full option list back
        // up, and a paste or a history recall is not a request for
        // suggestions. Either way the next real keystroke shows the list again.
        // These also downgrade Esc's `HIDDEN_UNTIL_SHOWN` so a later keystroke
        // can show; leaving `suppress_until_shown` set would latch forever.
        if let Some(previous) = previous_buffer.as_deref()
            && (backspaced_to_new_token(previous, &buffer)
                || (!self_inserted && large_buffer_change(previous, &buffer)))
        {
            self.hide_until_keypress(cx);
            self.sync_intercept(&figterm_state, cx);
            return;
        }

        if !force
            && self.state.read(cx).only_show_on_tab
            && only_show_on_tab_hides_until_shown(previous_buffer.as_deref(), &buffer)
        {
            self.hide_until_shown(cx);
            self.sync_intercept(&figterm_state, cx);
        }
        if self.state.read(cx).suppress_until_shown && self.show_intent.is_none() {
            self.retire_completion();
            self.sync_intercept(&figterm_state, cx);
            return;
        }

        // A buffer can precede the caret event in the host queue. Keep only
        // its latest text while positioning is unavailable; applying a valid
        // caret resumes it once, without loading specs or running scripts here.
        if self
            .last_position
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .is_none()
        {
            if self.show_intent.is_none() {
                self.hide(cx);
            }
            self.completion_display = CompletionDisplayState::WaitingForCaret;
            self.sync_intercept(&figterm_state, cx);
            return;
        }

        let generation = self.bump_generation();
        self.request_input = Some((
            generation,
            LastInput {
                buffer: buffer.clone(),
                cwd: cwd.clone(),
                cursor,
                session_id,
            },
        ));
        self.completion_display = CompletionDisplayState::Requested;
        self.take_loading_owner();
        self.clear_stale_loading(cx);
        let (fuzzy, history_only, include_history, suggest_first_token) = {
            let overlay = self.state.read(cx);
            let history_only = overlay.history_mode || overlay.history_setting == "history_only";
            (
                overlay.fuzzy_search,
                history_only,
                history_only || overlay.history_setting != "off",
                overlay.first_token_completion,
            )
        };
        let (current_shell, current_process, environment_variables, alias) = figterm_state
            .with(&session_id, |session| {
                let context = session.context.as_ref();
                (
                    context.and_then(|context| context.shell_path.clone()),
                    context.and_then(|context| context.process_name.clone()),
                    session.flattened_env.clone(),
                    context.and_then(|context| context.alias.clone()),
                )
            })
            .unwrap_or_else(|| (None, None, Arc::new(Vec::new()), None));
        let request = CompleteRequest {
            buffer,
            cwd: cwd.clone(),
            cursor: Some(cursor),
            fuzzy,
            history_only,
            include_history,
            suggest_first_token,
            current_shell,
            current_process,
            environment_variables,
            alias,
            include_public_ai: self.jev.is_ready(),
            // `apply_completion` schedules the follow-up a pending
            // result asks for, so slow generators need not hold up the list.
            defer_slow_generators: true,
        };
        self.capture_jev_request_context(&request, session_id);
        // Submit before spawning: queue order is the input-event order, not
        // whichever GPUI future happens to be polled first.
        let complete = self
            .engine
            .complete_for_session(SessionId::new(session_id.as_u128()), request);
        crate::heap_trim::note_activity();
        let proxy = self.proxy.clone();
        let executor = cx.background_executor().clone();

        cx.spawn(async move |_cx| {
            futures::pin_mut!(complete);
            let timed = executor.timer(loading_threshold);
            futures::pin_mut!(timed);
            let result = match futures::future::select(complete, timed).await {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right((_, complete)) => {
                    let _ = proxy.send_event(Event::GpuiOverlayLoading { generation });
                    // The user's own script budget bounds how long `···` is
                    // worth showing, but not the request: the engine already
                    // has a supervisor watchdog, and a result that lands after
                    // the marker is retired is still the one the legacy
                    // WebView would eventually have rendered.
                    let deadline = executor.timer(ui_completion_deadline());
                    futures::pin_mut!(deadline);
                    match futures::future::select(complete, deadline).await {
                        futures::future::Either::Left((result, _)) => result,
                        futures::future::Either::Right((_, complete)) => {
                            let _ = proxy.send_event(Event::GpuiOverlayLoadingExpired { generation });
                            complete.await
                        },
                    }
                },
            };
            let _ = proxy.send_event(Event::GpuiOverlayComplete {
                generation,
                result,
                session_id,
                cwd,
            });
        })
        .detach();
    }

    /// The `···` marker outlived the user's script budget. Retire it and park
    /// an empty card, but leave the request running: the engine watchdog still
    /// bounds it, and a late result is better than none at all.
    pub fn expire_loading(&mut self, generation: u64, cx: &mut App) {
        if self.release_loading_owner(generation) {
            self.clear_stale_loading(cx);
        }
    }

    pub fn show_loading(&mut self, generation: u64, cx: &mut App) {
        if self.generation.load(Ordering::Relaxed) != generation {
            return;
        }
        self.loading_owner.store(generation, Ordering::Relaxed);
        let visible = self.state.update(cx, |overlay, cx| {
            overlay.loading = true;
            if !overlay.suppress_until_shown {
                overlay.visible = true;
            }
            cx.notify();
            overlay.visible
        });
        if !visible {
            return;
        }
        let positioned = self.ensure_window(cx).is_some_and(|handle| {
            layout_overlay(
                &self.state,
                &self.handle,
                handle,
                &self.last_position,
                &self.platform_state,
                cx,
            )
        });
        self.settle_show_layout(positioned);
        self.sync_own_intercept_for_layout(positioned, cx);
        self.update_layout_retry(positioned, cx);
    }

    pub fn apply_completion(
        &mut self,
        generation: u64,
        result: anyhow::Result<CompleteResult>,
        session_id: Uuid,
        cwd: &str,
        cx: &mut App,
    ) {
        // Release the latch before the currency check. A superseded result is
        // not worth rendering, but it is still the only signal that the request
        // which turned the marker on has finished — dropping it here is what
        // used to strand `···` on screen until the next keystroke.
        let released = self.release_loading_owner(generation);
        if !completion_is_current(
            generation,
            self.generation.load(Ordering::Relaxed),
            self.current_session(),
            session_id,
        ) {
            if released {
                self.clear_stale_loading(cx);
            }
            return;
        }
        self.rows_input = self
            .request_input
            .as_ref()
            .filter(|(requested, _)| *requested == generation)
            .map(|(_, input)| input.clone());
        // A current result answers the latest request. An action for any other
        // input will never see its own rows.
        if self
            .pending_insert_action
            .as_ref()
            .is_some_and(|(_, input, _)| self.rows_input.as_ref() != Some(input))
        {
            self.pending_insert_action = None;
        }
        if result.is_err() {
            self.cancel_show();
            // A failed attempt produced nothing to show, so the recorded input
            // must not keep the duplicate guard armed: the terminal
            // republishing the same buffer is then the only path back to a
            // suggestion. Superseded results never reach here, so this cannot
            // reopen the storm the guard exists to stop.
            self.forget_last_input();
        }
        let pending = result.as_ref().ok().and_then(|complete| {
            complete
                .pending_generators
                .then_some(complete.debounce_ms.unwrap_or(200).max(0) as u64)
        });
        let explicit_show = self.show_intent.is_some() || self.explicit_show_pending;
        let accept_single = result
            .as_ref()
            .is_ok_and(|complete| self.accept_single_show_result(complete));
        if accept_single {
            // Apply the fresh row without opening a native window just to
            // accept it. Retained rows are never used by the hidden Tab path.
            self.state.update(cx, |overlay, _| overlay.suppress_until_shown = true);
        }
        let ai_candidates = result.as_ref().ok().and_then(|complete| self.prepare_jev(complete));
        let positioned = self.file_icons.update(cx, |icons, cx| {
            icons.begin_batch();
            let positioned = apply_complete_result(
                self.state.clone(),
                &self.handle,
                result,
                session_id,
                &self.figterm_state,
                cwd,
                &self.last_position,
                &self.platform_state,
                !explicit_show,
                cx,
            );
            icons.finish_batch(&self.state, &self.handle, cx);
            if !self.state.read(cx).visible {
                icons.schedule_idle(self.state.downgrade(), self.handle.clone(), cx);
            }
            positioned
        });
        self.update_layout_retry(positioned, cx);
        if pending.is_none() {
            self.explicit_show_pending = false;
        }
        if accept_single {
            self.pending_insert_action = None;
            self.state.update(cx, |overlay, _| overlay.suppress_until_shown = false);
            self.insert_selected(false, &self.figterm_state.clone(), cx);
            return;
        }
        if let Some(intent) = &mut self.show_intent {
            // Fresh partial rows are already actionable once positioned.
            // Loading or an empty pending result still owes the original Tab.
            intent.result_ready = !self.state.read(cx).items.is_empty();
            if !intent.result_ready && pending.is_none() {
                self.cancel_show();
            }
        }
        self.settle_show_layout(positioned);
        self.sync_own_intercept_for_layout(positioned, cx);
        if self.state.read(cx).suppress_until_shown || (!self.state.read(cx).visible && pending.is_none()) {
            // Empty final results have no display consumer. An initially
            // empty pending result still belongs to the active demand.
            if let Some((_, input, key)) = self.pending_insert_action.take() {
                self.return_intercepted_key(&input, key);
            }
            self.retire_completion();
            self.cancel_jev(cx);
            return;
        }
        self.schedule_jev(ai_candidates, cx);
        if let Some(delay_ms) = pending {
            self.schedule_debounce(delay_ms, cx);
        }
        // Empty rows with generators still pending: the follow-up for the same
        // input may have something to act on.
        if pending.is_none() || !self.state.read(cx).items.is_empty() {
            if let Some((action, input, key)) = self.pending_insert_action.take() {
                if self.state.read(cx).items.is_empty() {
                    // Only an argument hint is left: nothing to insert.
                    self.return_intercepted_key(&input, key);
                } else {
                    let figterm_state = self.figterm_state.clone();
                    self.handle_action(&action, session_id, &figterm_state, cx);
                }
            }
        }
    }

    fn schedule_debounce(&mut self, delay_ms: u64, cx: &mut App) {
        let generation = self.generation.load(Ordering::Relaxed);
        let proxy = self.proxy.clone();
        let executor = cx.background_executor().clone();
        self.debounce_task = Some(cx.spawn(async move |_cx| {
            executor.timer(Duration::from_millis(delay_ms)).await;
            let _ = proxy.send_event(Event::GpuiOverlayDebouncedComplete {
                generation,
                waited_ms: delay_ms,
            });
        }));
    }

    /// The debounce elapsed: run the generators the last result deferred.
    /// Fig armed its `···` timer at the keystroke — a debounced generator was
    /// `loading` through the whole wait — so the follow-up only gets whatever
    /// is left of the threshold, not a fresh one.
    pub fn apply_debounced_complete(&mut self, generation: u64, waited_ms: u64, cx: &mut App) {
        if self.generation.load(Ordering::Relaxed) != generation
            || !self.wants_caret(cx)
            || self.state.read(cx).suppress_until_shown
        {
            return;
        }
        self.debounce_task = None;
        let loading_threshold = LOADING_THRESHOLD.saturating_sub(Duration::from_millis(waited_ms));
        self.recomplete_with(
            RequestOptions {
                force: true,
                loading_threshold,
            },
            cx,
        );
    }

    /// Re-run the current buffer even when its text is unchanged. Settings
    /// changes (notably the auto-execute visibility toggle) must update the
    /// retained rows immediately instead of waiting for another keystroke.
    pub fn recomplete(&mut self, cx: &mut App) {
        self.recomplete_with(RequestOptions::forced(), cx);
    }

    fn recomplete_with(&mut self, options: RequestOptions, cx: &mut App) {
        if self.completion_display == CompletionDisplayState::Idle
            && self.show_intent.is_none()
            && !self.state.read(cx).visible
        {
            return;
        }
        let Some(current_session) = self.current_session() else {
            return;
        };
        let Some(input) = self.last_input.lock().unwrap_or_else(|err| err.into_inner()).clone() else {
            return;
        };
        if input.session_id != current_session {
            return;
        }
        self.complete_buffer_inner(
            input.buffer,
            input.cwd,
            input.cursor,
            input.session_id,
            self.figterm_state.clone(),
            options,
            cx,
        );
    }

    pub fn handle_action(&mut self, action: &str, action_session_id: Uuid, figterm_state: &FigtermState, cx: &mut App) {
        self.handle_key_action(action, None, action_session_id, figterm_state, cx);
    }

    /// `key` is the raw key fterm intercepted for `action`, if it came from one.
    pub fn handle_key_action(
        &mut self,
        action: &str,
        key: Option<String>,
        action_session_id: Uuid,
        figterm_state: &FigtermState,
        cx: &mut App,
    ) {
        if !action_session_is_current(self.current_session(), action_session_id) {
            debug!(%action_session_id, current_session = ?self.current_session(), action, "ignoring stale overlay action");
            return;
        }
        self.reconcile_jev_context(cx);
        if self.show_intent.is_some()
            && matches!(
                action,
                "insertCommonPrefix" | "insertCommonPrefixOrInsertSelected" | "insertCommonPrefixOrNavigateDown"
            )
        {
            self.request_show(true, cx);
            return;
        }
        let (visible, loading, has_items) = {
            let overlay = self.state.read(cx);
            (overlay.visible, overlay.loading, !overlay.items.is_empty())
        };
        if !action_is_allowed(action, visible, loading, has_items) {
            debug!(action, loading, "ignoring overlay action without an actionable list");
            return;
        }
        if action_inserts_from_rows(action) {
            let current_input = self.last_input.lock().unwrap_or_else(|err| err.into_inner()).clone();
            if let Some(current_input) = current_input
                && rows_are_stale(self.rows_input.as_ref(), &current_input)
            {
                debug!(action, "deferring insert until rows for the current input arrive");
                self.pending_insert_action = Some((action.to_string(), current_input, key));
                return;
            }
        }
        if matches!(
            action,
            "navigateUp"
                | "navigateDown"
                | "insertSelected"
                | "insertSelectedAndExecute"
                | "insertCommonPrefix"
                | "insertCommonPrefixOrInsertSelected"
                | "insertCommonPrefixOrNavigateDown"
                | "execute"
                | "toggleHistoryMode"
                | "toggleFuzzySearch"
        ) || action.starts_with("selectSuggestion")
        {
            self.cancel_jev_request(cx);
        }
        match action {
            "navigateUp" => self.move_selection(-1, true, figterm_state, cx),
            "navigateDown" => self.move_selection(1, false, figterm_state, cx),
            "hideAutocomplete" => {
                self.hide_until_shown(cx);
                self.sync_intercept(figterm_state, cx);
            },
            // `showAutocomplete` is an explicit show action.  It must only
            // reveal the kept rows; accepting the sole row is reserved for
            // the hidden-overlay Tab shortcut below.
            "showAutocomplete" => self.show_kept_items(cx),
            "showAutocompleteFromTab" => self.request_show(true, cx),
            "toggleAutocomplete" => {
                let visible = self.state.read(cx).visible;
                if visible {
                    self.hide_until_shown(cx);
                    self.sync_intercept(figterm_state, cx);
                } else {
                    self.show_kept_items(cx);
                }
            },
            "relayoutOverlay" => {
                self.relayout_and_sync(cx);
            },
            "insertSelected" => self.insert_selected(false, figterm_state, cx),
            "insertCommonPrefixOrInsertSelected" => {
                if !self.insert_common_prefix(figterm_state, cx) {
                    // A multi-row action has no common prefix by design.
                    // Falling back to it would turn Tab into execution when
                    // an exact command's auto-execute row carries a newline.
                    let can_accept = self
                        .state
                        .read(cx)
                        .selected_item()
                        .is_some_and(|item| !matches!(item.kind.as_str(), "auto-execute" | "special"));
                    if can_accept {
                        self.insert_selected(false, figterm_state, cx);
                    } else {
                        self.shake(cx);
                    }
                }
            },
            "insertSelectedAndExecute" => self.insert_selected(true, figterm_state, cx),
            "execute" => {
                let _ = self.insert_text("\n", 0, true, figterm_state, cx);
            },
            "insertCommonPrefix" => {
                if !self.insert_common_prefix(figterm_state, cx) {
                    self.shake(cx);
                }
            },
            "insertCommonPrefixOrNavigateDown" => {
                if !self.insert_common_prefix(figterm_state, cx) {
                    self.move_selection(1, false, figterm_state, cx);
                }
            },
            "toggleDescription" => {
                if !fastab_settings::settings::get_bool_or("autocomplete.alwaysShowDescription", false) {
                    self.state.update(cx, |overlay, cx| {
                        overlay.description_popout = !overlay.description_popout;
                        if !overlay.description_popout {
                            overlay.is_above_cursor = false;
                        }
                        cx.notify();
                    });
                    self.relayout_and_sync(cx);
                }
            },
            "showDescription" => {
                self.state.update(cx, |overlay, cx| {
                    overlay.description_popout = true;
                    cx.notify();
                });
                self.relayout_and_sync(cx);
            },
            "hideDescription" => {
                self.state.update(cx, |overlay, cx| {
                    overlay.description_popout = false;
                    overlay.is_above_cursor = false;
                    cx.notify();
                });
                self.relayout_and_sync(cx);
            },
            "toggleHistoryMode" => {
                self.state.update(cx, |overlay, cx| {
                    overlay.history_mode = !overlay.history_mode;
                    cx.notify();
                });
                self.recomplete(cx);
            },
            "toggleFuzzySearch" => {
                self.state.update(cx, |overlay, cx| {
                    overlay.fuzzy_search = !overlay.fuzzy_search;
                    overlay.effective_fuzzy_search = overlay.fuzzy_search;
                    cx.notify();
                });
                // The keybinding is a temporary overlay-mode toggle in the
                // WebView. Only the settings UI persists the user default.
                self.recomplete(cx);
            },
            "increaseSize" => {
                self.state.update(cx, |overlay, cx| {
                    overlay.change_size(true);
                    cx.notify();
                });
                self.relayout_and_sync(cx);
            },
            "decreaseSize" => {
                self.state.update(cx, |overlay, cx| {
                    overlay.change_size(false);
                    cx.notify();
                });
                self.relayout_and_sync(cx);
            },
            other if other.starts_with("selectSuggestion") => {
                if let Ok(n) = other.trim_start_matches("selectSuggestion").parse::<usize>() {
                    if let Some(index) = select_suggestion_index(n, self.state.read(cx).items.len()) {
                        let previous_size = overlay_window_size_from(self.state.read(cx));
                        self.state.update(cx, |overlay, cx| {
                            overlay.selected = index;
                            overlay.has_changed_index = true;
                            cx.notify();
                        });
                        if previous_size != overlay_window_size_from(self.state.read(cx)) {
                            self.relayout_and_sync(cx);
                        }
                    }
                }
            },
            other => debug!(action = other, "unhandled overlay action"),
        }
    }

    pub fn handle_click(
        &mut self,
        click: ClickInsert,
        click_session_id: Uuid,
        click_generation: u64,
        figterm_state: &FigtermState,
        cx: &mut App,
    ) {
        self.reconcile_jev_context(cx);
        let click_is_current = {
            let overlay = self.state.read(cx);
            click_matches_current(&click, overlay)
        };
        if !completion_is_current(
            click_generation,
            self.generation.load(Ordering::Relaxed),
            self.current_session(),
            click_session_id,
        ) || !click_is_current
        {
            debug!(
                %click_session_id,
                current_session = ?self.current_session(),
                click_generation,
                current_generation = self.generation.load(Ordering::Relaxed),
                "ignoring stale overlay click"
            );
            return;
        }
        self.insert_item(click, false, figterm_state, cx);
    }

    fn shake(&self, cx: &mut App) {
        self.state.update(cx, |overlay, cx| {
            overlay.start_shake(cx);
        });
    }

    fn move_selection(&mut self, delta: i32, up_from_top: bool, figterm_state: &FigtermState, cx: &mut App) {
        let wrap = self.state.read(cx).scroll_wrap_around;
        let at_top = self.state.read(cx).selected == 0;
        let previous_size = overlay_window_size_from(self.state.read(cx));
        let still_visible = self.state.update(cx, |overlay, cx| {
            let visible = overlay.move_selection_with_wrap(delta, wrap);
            cx.notify();
            visible
        });
        if !still_visible {
            if up_from_top && at_top && self.state.read(cx).navigate_to_history {
                self.state.update(cx, |overlay, cx| {
                    overlay.history_mode = !overlay.history_mode;
                    cx.notify();
                });
                self.recomplete(cx);
                return;
            }
            // Up-from-top maps to the legacy HIDDEN_UNTIL_KEYPRESS state:
            // cancel the current generation, but allow the next real buffer
            // change to show suggestions again. `hide_until_shown` is the
            // stronger Esc/onlyShowOnTab state and would keep the list hidden.
            self.hide(cx);
            self.sync_intercept(figterm_state, cx);
        } else if previous_size != overlay_window_size_from(self.state.read(cx)) {
            // Expanded argument details can change the popout height; keep
            // the window anchored to the terminal caret.
            self.relayout_and_sync(cx);
        }
    }

    fn insert_common_prefix(&mut self, figterm_state: &FigtermState, cx: &mut App) -> bool {
        let decision = {
            let overlay = self.state.read(cx);
            tab_prefix_insertion(overlay.selected, &overlay.items, &overlay.search_term)
        };
        match decision {
            Some(TabPrefix::Full(_)) => {
                self.insert_selected(false, figterm_state, cx);
                true
            },
            Some(TabPrefix::Partial(shared)) => {
                let (raw_search, query_term, completes_the_row, kind, quote_aware) = {
                    let overlay = self.state.read(cx);
                    let Some(item) = overlay.selected_item() else {
                        return false;
                    };
                    (
                        overlay.search_term.clone(),
                        item.query_term.clone(),
                        prefix_completes_row(&shared, item),
                        item.kind.clone(),
                        item.insert_value.is_none() || matches!(item.kind.as_str(), "file" | "folder" | "dir"),
                    )
                };
                // The shared prefix can already spell out the whole selected
                // row. Fig treated that as a real acceptance so the trailing
                // space, separator and post-insert hide all still happen;
                // inserting it as a bare prefix would strand the caret.
                if completes_the_row {
                    self.insert_selected(false, figterm_state, cx);
                    return true;
                }
                // A prefix is typed bare, with only its spaces escaped, so one
                // that needs quoting cannot be typed in part: `a;b` from the
                // files `a;b1` and `a;b2` would leave `cat a;b` on the line.
                // Callers fall back to accepting the row, which quotes it. A
                // closed quote is the exception: the edit below re-encodes
                // the text for that quote.
                if quote_aware
                    && closed_shell_quote_inner(&raw_search).is_none()
                    && tab_prefix_needs_quoting(&shared, &raw_search, query_term.as_deref(), &kind)
                {
                    return false;
                }
                let shared = escape_tab_prefix(&shared, &kind);
                let search = prefix_insertion_search(&shared, &raw_search, query_term.as_deref());
                let (insertion, deletion) = insertion_for(&shared, &search);
                if insertion.is_empty() && deletion == 0 {
                    return false;
                }
                let (insertion, deletion, offset) =
                    insertion_edit_for_raw_search(&raw_search, insertion, deletion, quote_aware, false, false);
                self.insert_text_with_offset(&insertion, deletion, offset, false, figterm_state, cx);
                true
            },
            None => false,
        }
    }

    fn insert_selected(&mut self, execute: bool, figterm_state: &FigtermState, cx: &mut App) {
        let selected = {
            let overlay = self.state.read(cx);
            let Some(item) = overlay.selected_item() else {
                return;
            };
            ClickInsert {
                name: item.name.clone(),
                description: item.description.clone(),
                search: overlay.search_term.clone(),
                kind: item.kind.clone(),
                args_hint: item.args_hint.clone(),
                insert_value: item.insert_value.clone(),
                display_name: item.display_name.clone(),
                primary_name: item.primary_name.clone(),
                separator_to_add: item.separator_to_add.clone(),
                should_add_space: item.should_add_space,
                hidden: item.hidden,
                priority: item.priority,
                icon_identifier: item.icon_identifier.clone(),
                original_type: item.original_type.clone(),
                query_term: item.query_term.clone(),
                argument_value: item.argument_value,
                acceptance_scope: item.acceptance_scope.clone(),
            }
        };
        self.insert_item(selected, execute, figterm_state, cx);
    }

    fn insert_item(&mut self, item: ClickInsert, execute: bool, figterm_state: &FigtermState, cx: &mut App) {
        let ai_acceptance = self.take_jev_acceptance(&item);
        self.cancel_jev(cx);
        let input_before_accept = self.current_input_snapshot();
        let acceptance = (!item.argument_value)
            .then(|| accepted_suggestion_key(&item, input_before_accept.as_ref()))
            .flatten();
        let scoped_acceptance = accepted_scoped_suggestion_key(&item);
        let add_space = self.state.read(cx).insert_space_automatically;
        let text = resolve_cursor_marker(full_insertion_for_item(
            &item.name,
            item.insert_value.as_deref(),
            &item.kind,
            item.separator_to_add.as_deref(),
            item.should_add_space,
            add_space,
            execute,
        ));
        let search_term = insertion_search_term(&item.kind, &item.search, item.query_term.as_deref(), &text);
        let kind = item.kind;
        let opens_new_arg = opens_new_arg(add_space, item.should_add_space, item.separator_to_add.as_deref());
        // Auto-execute/special rows represent an action on the existing
        // buffer, not a replacement for the current query.  The old WebView
        // therefore sent `\n` with zero deletion; blindly using insertion_for
        // here would backspace `status` before executing it.
        let (insertion, deletion) = insertion_for_kind(&text, &search_term, &kind);
        let buffer = input_before_accept.as_ref().map_or("", |(buffer, _)| buffer.as_str());
        let (insertion, deletion) = apply_shortcut_palette_deletion(buffer, &search_term, insertion, deletion, &text);
        let outside_space = add_space && item.should_add_space && !text.ends_with('\n');
        let outside_newline = execute && kind != "auto-execute" && text.ends_with('\n');
        let (insertion, deletion, offset) = insertion_edit_for_kind(
            &item.search,
            insertion,
            deletion,
            &kind,
            item.insert_value.is_none() || matches!(kind.as_str(), "file" | "folder" | "dir"),
            outside_space,
            outside_newline,
        );
        let should_suppress = should_suppress_after_insert(execute, &kind, opens_new_arg, &text);
        let inserted = self.insert_text_with_offset(&insertion, deletion, offset, execute, figterm_state, cx);
        Self::finish_jev_acceptance(ai_acceptance, inserted);
        if inserted {
            if let Some((scope, accepted_name)) = scoped_acceptance {
                if let Err(err) = self.engine.record_scoped_acceptance(scope, accepted_name) {
                    debug!(%err, "failed to record scoped autocomplete acceptance");
                }
            } else if !item.argument_value
                && let Some((root_command, accepted_name)) = acceptance
                && let Err(err) = self.engine.record_acceptance(root_command, accepted_name)
            {
                debug!(%err, "failed to record autocomplete acceptance");
            }
        }
        let changed_buffer = insertion_changes_buffer(&insertion, deletion, execute);
        if inserted && changed_buffer && should_suppress {
            if let Some((expected_buffer, expected_cursor)) =
                input_before_accept.as_ref().and_then(|(buffer, cursor)| {
                    predicted_buffer_after_insert_with_offset(
                        buffer,
                        *cursor,
                        &insertion,
                        deletion,
                        offset.unwrap_or_default(),
                    )
                })
            {
                self.state.update(cx, |overlay, cx| {
                    // Bind suppression to the post-accept buffer. Cursor is
                    // recorded but not required to match: figterm/IME often
                    // ack the same text with a different caret, and treating
                    // that as a fresh complete would show the accepted row.
                    overlay.mark_suppress_unchanged_completion(expected_buffer, expected_cursor);
                    cx.notify();
                });
            } else {
                // Prediction needs a char-boundary cursor. If it fails, still
                // eat the next buffer tick rather than re-show the accepted row.
                self.state.update(cx, |overlay, cx| {
                    overlay.mark_suppress_next_completion();
                    cx.notify();
                });
            }
        } else if inserted && !changed_buffer {
            if let Some((buffer, cursor)) = input_before_accept {
                self.state.update(cx, |overlay, cx| {
                    overlay.mark_suppress_unchanged_completion(buffer, cursor);
                    cx.notify();
                });
            }
        }
        self.hide(cx);
        self.sync_intercept(figterm_state, cx);
    }

    fn insert_text(
        &mut self,
        insertion: &str,
        deletion: i64,
        execute: bool,
        figterm_state: &FigtermState,
        _cx: &mut App,
    ) -> bool {
        self.insert_text_with_offset(insertion, deletion, None, execute, figterm_state, _cx)
    }

    fn insert_text_with_offset(
        &mut self,
        insertion: &str,
        deletion: i64,
        offset: Option<i64>,
        execute: bool,
        figterm_state: &FigtermState,
        _cx: &mut App,
    ) -> bool {
        let Some(session_id) = self.current_session() else {
            return false;
        };
        let snapshot = self.current_input_snapshot();
        *self.self_insertion.lock().unwrap_or_else(|err| err.into_inner()) = snapshot
            .as_ref()
            .and_then(|(buffer, cursor)| {
                predicted_buffer_after_insert_with_offset(
                    buffer,
                    *cursor,
                    insertion,
                    deletion,
                    offset.unwrap_or_default(),
                )
            })
            .map(|(predicted, _)| predicted);
        let insertion_buffer = snapshot.map(|(buffer, _)| buffer);
        let sent = figterm_state.with(&session_id, |session| {
            session.send(FigtermCommand::InsertText {
                insertion: Some(insertion.to_string()),
                deletion: Some(deletion),
                offset,
                // An auto-execute suggestion already carries its `\n` insertValue;
                // adding the immediate carriage return would execute twice.
                immediate: Some(immediate_for_insert(execute, insertion)),
                insertion_buffer,
                insert_during_command: None,
            })
        });
        match sent {
            Some(Ok(())) => {
                // A successful user edit consumes the explicit request even
                // if its layout acknowledgement has not reached us yet.
                self.show_intent = None;
                self.explicit_show_pending = false;
                fastab_telemetry::count("autocomplete_accepted");
                true
            },
            error => {
                self.take_self_insertion();
                error!(?error, %session_id, "failed to insert suggestion");
                false
            },
        }
    }

    fn current_input_snapshot(&self) -> Option<(String, u32)> {
        let current_session = self.current_session()?;
        self.last_input
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .as_ref()
            .filter(|input| input.session_id == current_session)
            .map(|input| (input.buffer.clone(), input.cursor))
    }
}

fn apply_settings(overlay: &mut OverlayState, previous_fuzzy_search: Option<bool>) -> bool {
    let metrics = overlay_metrics();
    overlay.theme = resolve_overlay_theme();
    overlay.font_family = metrics.font_family;
    overlay.custom_font_family = metrics.custom_font_family;
    overlay.font_size = metrics.font_size;
    overlay.row_height = metrics.row_height;
    let configured_width = fastab_settings::settings::get_int("autocomplete.width").ok().flatten();
    let configured_history_mode = fastab_settings::settings::get_string("beta.history.mode")
        .ok()
        .flatten();
    overlay.history_setting = configured_history_mode.clone().unwrap_or_else(|| "show".into());
    overlay.list_width = legacy_list_width(configured_width, configured_history_mode.as_deref()).max(150) as f32;
    overlay.max_list_height =
        fastab_settings::settings::get_int_or("autocomplete.height", DEFAULT_MAX_LIST_HEIGHT as i64).max(40) as f32;
    let fuzzy_search = fastab_settings::settings::get_bool_or("autocomplete.fuzzySearch", true);
    // Presentation refreshes must not reset the shortcut's temporary mode or
    // the matching mode supplied by the current result. A changed saved
    // preference is separately classified by CompletionSettings for recompute.
    if previous_fuzzy_search != Some(fuzzy_search) {
        overlay.fuzzy_search = fuzzy_search;
        overlay.effective_fuzzy_search = fuzzy_search;
    }
    overlay.only_show_on_tab = fastab_settings::settings::get_bool_or("autocomplete.onlyShowOnTab", false);
    overlay.first_token_completion = fastab_settings::settings::get_bool_or("autocomplete.firstTokenCompletion", false);
    overlay.navigate_to_history = fastab_settings::settings::get_bool_or("autocomplete.navigateToHistory", false);
    overlay.scroll_wrap_around = resolve_scroll_wrap_around(
        fastab_settings::settings::get_bool("autocomplete.scrollWrapAround")
            .ok()
            .flatten(),
        overlay.navigate_to_history,
    );
    overlay.insert_space_automatically =
        fastab_settings::settings::get_bool_or("autocomplete.insertSpaceAutomatically", true);
    overlay.title_overflow = fastab_gpui::TitleOverflow::parse(&fastab_settings::settings::get_string_or(
        "autocomplete.overflow",
        "scroll".into(),
    ));
    let was_always_show = overlay.always_show_description;
    overlay.always_show_description =
        fastab_settings::settings::get_bool_or("autocomplete.alwaysShowDescription", false);
    if overlay.always_show_description {
        overlay.description_popout = true;
    } else if was_always_show {
        overlay.description_popout = false;
    }
    overlay.show_dev_banner = fastab_settings::settings::get_bool_or("autocomplete.developerModeNPM", false);
    overlay.description_hint = description_hint_from_settings();
    fuzzy_search
}

fn resolve_scroll_wrap_around(configured: Option<bool>, navigate_to_history: bool) -> bool {
    // An explicit wrap preference takes precedence. With no preference, keep
    // the Up-for-history setting useful while making the normal list cyclic.
    configured.unwrap_or(!navigate_to_history)
}

fn legacy_list_width(configured_width: Option<i64>, configured_history_mode: Option<&str>) -> i64 {
    configured_width.unwrap_or_else(|| {
        let history_extra = if matches!(configured_history_mode, Some("show" | "history_only")) {
            50
        } else {
            0
        };
        DEFAULT_WIDTH as i64 + history_extra
    })
}

fn description_hint_from_settings() -> String {
    let bindings = KeyBindings::load_from_settings("autocomplete").unwrap_or_else(|_| KeyBindings(Vec::new()));
    bindings
        .into_iter()
        .find(|binding| {
            matches!(
                binding.identifier.as_str(),
                "toggleDescription" | "showDescription" | "hideDescription"
            )
        })
        .map(|binding| format_keybinding(&binding.binding))
        .filter(|binding| !binding.is_empty())
        .unwrap_or_else(|| "⌃k".into())
}

fn format_keybinding(binding: &str) -> String {
    binding
        .split('+')
        .map(|token| match token.to_ascii_lowercase().as_str() {
            "cmd" | "command" | "meta" => "⌘".to_string(),
            "control" | "ctrl" => "⌃".to_string(),
            "shift" => "⇧".to_string(),
            "option" | "opt" | "alt" => "⌥".to_string(),
            "enter" | "return" => "↵".to_string(),
            "tab" => "⇥".to_string(),
            "escape" | "esc" => "⎋".to_string(),
            other => other.to_string(),
        })
        .collect()
}

struct OverlayMetrics {
    font_family: String,
    custom_font_family: bool,
    font_size: f32,
    row_height: f32,
}

fn overlay_metrics() -> OverlayMetrics {
    let configured_font = fastab_settings::settings::get_string("autocomplete.fontFamily")
        .ok()
        .flatten()
        .filter(|font| !font.trim().is_empty());
    let custom_font_family = configured_font.is_some();
    let font_family = configured_font.unwrap_or_else(|| "Monaco".into());
    let font_size = {
        let setting = fastab_settings::settings::get_int_or("autocomplete.fontSize", 0);
        if setting > 0 { setting as f32 } else { DEFAULT_FONT_SIZE }
    };
    OverlayMetrics {
        font_family,
        custom_font_family,
        font_size,
        row_height: font_size * 1.5625,
    }
}

fn overlay_window_size_from(overlay: &OverlayState) -> LogicalSize<f64> {
    let (width, height) = overlay_content_size_with_context(
        overlay.items.len(),
        overlay.effective_row_height(),
        overlay.effective_font_size(),
        overlay.effective_list_width(),
        overlay.effective_max_list_height(),
        overlay.description_popout && !overlay.items.is_empty() && !overlay.loading,
        overlay.show_dev_banner,
        overlay.loading,
        overlay.current_arg_rows(),
        overlay.selected_item().map_or("", |item| item.args_hint.as_str()),
    );
    LogicalSize::new(f64::from(width), f64::from(height + overlay.ai_height()))
}

pub fn resolve_overlay_theme() -> OverlayTheme {
    let name = fastab_settings::settings::get_string_or("autocomplete.theme", "github-dark".into());
    overlay_theme_by_name(&name)
}

/// Resolve colors for both the live overlay and its settings previews.
pub(crate) fn overlay_theme_by_name(name: &str) -> OverlayTheme {
    match name.to_ascii_lowercase().as_str() {
        "light" => OverlayTheme::light(),
        "dark" => OverlayTheme::dark(),
        "system" => {
            if fastab_gpui::system_appearance_is_dark() {
                OverlayTheme::dark()
            } else {
                OverlayTheme::light()
            }
        },
        other => load_named_theme(other).unwrap_or_else(OverlayTheme::dark),
    }
}

fn load_named_theme(name: &str) -> Option<OverlayTheme> {
    static CACHE: OnceLock<Mutex<HashMap<String, OverlayTheme>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(theme) = guard.get(name) {
            return Some(*theme);
        }
    }
    let ctx = fastab_os_shim::Context::new();
    let path = fastab_util::directories::themes_dir(&ctx)
        .ok()?
        .join(format!("{name}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    let theme = theme_from_json(&text)?;
    if let Ok(mut guard) = cache.lock() {
        guard.insert(name.to_string(), theme);
    }
    Some(theme)
}

#[allow(clippy::too_many_arguments)]
fn apply_complete_result(
    state: Entity<OverlayState>,
    window_slot: &Mutex<Option<OverlayHandle>>,
    result: anyhow::Result<CompleteResult>,
    session_id: Uuid,
    figterm_state: &FigtermState,
    _cwd: &str,
    last_position: &Mutex<Option<WindowPosition>>,
    platform_state: &PlatformState,
    allow_arg_hide: bool,
    cx: &mut App,
) -> bool {
    match result {
        Ok(result) => {
            let effective_fuzzy = result.fuzzy;
            let (current_arg_name, current_arg_description) = result
                .current_arg
                .as_ref()
                .map(|arg| (arg.name.clone(), arg.description.clone()))
                .unwrap_or_default();
            let mut hide_for_new_arg = false;
            let items = result
                .suggestions
                .into_iter()
                .map(|s| SuggestionItem {
                    icon_png: None,
                    name: s.name,
                    description: s.description,
                    kind: s.kind,
                    args_hint: s.args_hint,
                    insert_value: s.insert_value,
                    display_name: s.display_name,
                    primary_name: s.primary_name,
                    separator_to_add: s.separator_to_add,
                    should_add_space: s.should_add_space,
                    hidden: s.hidden,
                    priority: s.priority,
                    icon_identifier: s.icon,
                    original_type: s.original_type,
                    query_term: s.query_term,
                    argument_value: s.argument_value,
                    acceptance_scope: s.acceptance_scope,
                })
                .collect::<Vec<_>>();
            let empty = items.is_empty();
            let match_term = if result.match_term.is_empty() {
                result.search_term.clone()
            } else {
                result.match_term
            };
            state.update(cx, |overlay, cx| {
                overlay.effective_fuzzy_search = effective_fuzzy;
                if allow_arg_hide
                    && overlay.only_show_on_tab
                    && current_arg_identity_changed(&overlay.current_arg_name, &current_arg_name)
                {
                    overlay.hide_until_shown();
                    hide_for_new_arg = true;
                }
                overlay.set_current_arg(current_arg_name, current_arg_description);
                overlay.set_suggestions_with_match_term(items, result.search_term, match_term);
                cx.notify();
            });
            let (visible, has_items) = {
                let overlay = state.read(cx);
                (overlay.visible, !overlay.items.is_empty())
            };
            let positioned = if visible {
                if let Some(handle) = ensure_overlay_window(window_slot, &state, cx) {
                    layout_overlay(&state, window_slot, handle, last_position, platform_state, cx)
                } else {
                    false
                }
            } else {
                if empty || hide_for_new_arg {
                    let _ = park_overlay_slot(window_slot, cx);
                }
                false
            };
            let has_last_position = last_position.lock().unwrap_or_else(|err| err.into_inner()).is_some();
            let (visible_intercept, global_intercept) = positioned_intercept_flags(InterceptInputs {
                overlay_visible: visible,
                has_items,
                positioned,
                has_last_position,
            });
            set_intercept_flags(figterm_state, session_id, visible_intercept, global_intercept);
            if visible && positioned && has_items {
                fastab_telemetry::count("autocomplete_shown");
            }
            positioned
        },
        Err(err) => {
            warn!(%err, "completion engine failed");
            state.update(cx, |overlay, cx| {
                overlay.dismiss();
                cx.notify();
            });
            let _ = park_overlay_slot(window_slot, cx);
            set_intercept(figterm_state, session_id, false, false);
            false
        },
    }
}

fn layout_retry_delay(attempt: u8) -> Option<Duration> {
    LAYOUT_RETRY_DELAYS_MS
        .get(usize::from(attempt))
        .copied()
        .map(Duration::from_millis)
}

#[derive(Clone, Copy)]
struct LayoutRetryInputs {
    overlay_visible: bool,
    has_content: bool,
    has_position: bool,
    positioned: bool,
}

fn should_retry_layout(inputs: LayoutRetryInputs) -> bool {
    let LayoutRetryInputs {
        overlay_visible,
        has_content,
        has_position,
        positioned,
    } = inputs;
    overlay_visible && has_content && has_position && !positioned
}

fn layout_retry_is_current(generation: u64, current_generation: u64, token: u64, current_token: u64) -> bool {
    generation == current_generation && token == current_token
}

/// Controller/outbox tests exercise native-unavailable handling. GPUI's test
/// windows have no raw AppKit handle, so do not invoke the title-based fallback.
#[cfg(test)]
struct HeadlessOverlayWindows;

#[cfg(test)]
impl gpui::Global for HeadlessOverlayWindows {}

fn ensure_overlay_window(
    slot: &Mutex<Option<OverlayHandle>>,
    state: &Entity<OverlayState>,
    cx: &mut App,
) -> Option<OverlayHandle> {
    #[cfg(test)]
    if cx.has_global::<HeadlessOverlayWindows>() {
        return None;
    }
    if let Some(handle) = *slot.lock().unwrap_or_else(|err| err.into_inner()) {
        return Some(handle);
    }
    match open_overlay_window(cx, state.clone()) {
        Ok(handle) => {
            *slot.lock().unwrap_or_else(|err| err.into_inner()) = Some(handle);
            Some(handle)
        },
        Err(err) => {
            error!(%err, "Failed to open overlay window");
            None
        },
    }
}

fn clear_overlay_handle_if(slot: &Mutex<Option<OverlayHandle>>, handle: OverlayHandle) {
    let mut current = slot.lock().unwrap_or_else(|err| err.into_inner());
    if current.as_ref().is_some_and(|candidate| *candidate == handle) {
        *current = None;
    }
}

fn park_overlay_slot(slot: &Mutex<Option<OverlayHandle>>, cx: &mut App) -> bool {
    let Some(handle) = *slot.lock().unwrap_or_else(|err| err.into_inner()) else {
        return true;
    };
    match park_overlay_handle(&handle, cx) {
        Ok(()) => true,
        Err(err) => {
            warn!(%err, "overlay window update failed while parking; clearing stale handle");
            clear_overlay_handle_if(slot, handle);
            false
        },
    }
}

fn layout_overlay(
    state: &Entity<OverlayState>,
    window_slot: &Mutex<Option<OverlayHandle>>,
    handle: OverlayHandle,
    last_position: &Mutex<Option<WindowPosition>>,
    platform_state: &PlatformState,
    cx: &mut App,
) -> bool {
    let (overlay_size, popout, visible, empty, flip_height) = {
        let overlay = state.read(cx);
        (
            overlay_window_size_from(overlay),
            overlay.description_popout && !overlay.items.is_empty() && !overlay.loading,
            overlay.visible,
            overlay.items.is_empty() && !overlay.loading && !overlay.has_current_arg(),
            overlay.effective_max_list_height() as f64,
        )
    };
    if !visible || empty {
        return false;
    }
    if let Some(position) = *last_position.lock().unwrap_or_else(|err| err.into_inner()) {
        let (origin, size, on_left, is_above) =
            overlay_bounds(position, overlay_size, platform_state, popout, flip_height);
        state.update(cx, |overlay, cx| {
            // The legacy view also retained `isAboveCursor` for the developer
            // banner when no description popout was open. Keeping the real
            // placement flag here avoids rendering that banner below a panel
            // that has already flipped above the caret.
            if overlay.set_layout_flags(on_left, is_above) {
                cx.notify();
            }
        });
        match position_overlay(origin, size, &handle, cx) {
            Ok(positioned) => positioned,
            Err(err) => {
                warn!(%err, "overlay window update failed while positioning; clearing stale handle");
                clear_overlay_handle_if(window_slot, handle);
                false
            },
        }
    } else {
        // A caret we never received is not a usable screen-space fallback: a
        // window-relative guess lands the list away from the real cursor. Keep
        // the last valid position when there is one, otherwise stay hidden.
        let _ = park_overlay_slot(window_slot, cx);
        false
    }
}

fn session_changed(current: Option<Uuid>, next: Uuid) -> bool {
    current != Some(next)
}

fn should_skip_duplicate_input(previous: Option<&LastInput>, next: &LastInput, force: bool) -> bool {
    !force && previous == Some(next)
}

/// Sentinel for "no request owns the loading marker". Generations start at 1.
const NO_LOADING_OWNER: u64 = 0;

/// Whether a completion for `generation` must switch the loading marker off.
///
/// A request always releases its own latch. It also releases one left behind by
/// an older generation: generations only move forward, so an older owner has
/// already reported or been superseded and will never come back to clean up. An
/// owner newer than this result is left alone — that request is still running
/// and its marker is the one the user should be looking at.
fn loading_owner_is_released_by(owner: u64, generation: u64) -> bool {
    owner != NO_LOADING_OWNER && owner <= generation
}

fn completion_is_current(
    generation: u64,
    current_generation: u64,
    current_session: Option<Uuid>,
    result_session: Uuid,
) -> bool {
    generation == current_generation && current_session == Some(result_session)
}

fn action_session_is_current(current_session: Option<Uuid>, action_session: Uuid) -> bool {
    current_session == Some(action_session)
}

/// The legacy overlay did not offer completions while the caret was in the
/// middle of a token. It only completed the text to the left when the next
/// character was whitespace (or the caret was at the end of the buffer).
fn cursor_is_inside_word(buffer: &str, cursor: u32) -> bool {
    let Ok(mut cursor) = usize::try_from(cursor) else {
        return false;
    };
    cursor = cursor.min(buffer.len());
    while cursor < buffer.len() && !buffer.is_char_boundary(cursor) {
        cursor += 1;
    }
    buffer[cursor..].chars().next().is_some_and(|ch| !ch.is_whitespace())
}

fn insertion_changes_buffer(insertion: &str, deletion: i64, execute: bool) -> bool {
    deletion != 0 || !insertion.is_empty() || execute
}

fn immediate_for_insert(execute: bool, insertion: &str) -> bool {
    // `{cursor}` is resolved by appending `\x1b[D` after the rest of the
    // insertion, which can leave a newline that is no longer at the end.
    // Figterm would then send an extra `\r` on top of that `\n`.
    execute && !insertion.contains('\n')
}

fn accepted_suggestion_key(item: &ClickInsert, input: Option<&(String, u32)>) -> Option<(String, String)> {
    let (buffer, cursor) = input?;
    Some((
        ranking_root_command(buffer, Some(*cursor)),
        item.primary_name.clone().unwrap_or_else(|| item.name.clone()),
    ))
}

fn accepted_scoped_suggestion_key(item: &ClickInsert) -> Option<(String, String)> {
    if !item.argument_value {
        return None;
    }
    Some((
        item.acceptance_scope.clone()?,
        item.primary_name.clone().unwrap_or_else(|| item.name.clone()),
    ))
}

/// Predict the edit-buffer notification produced by a regular insertion.
///
/// `cursor` is a byte offset into the shell buffer, while `deletion` counts
/// shell characters (the same units used by the backspaces sent to figterm).
/// The only control sequences emitted in an insertion are cursor arrows; they
/// move the cursor without becoming buffer text.
#[cfg(test)]
fn predicted_buffer_after_insert(buffer: &str, cursor: u32, insertion: &str, deletion: i64) -> Option<(String, u32)> {
    predicted_buffer_after_insert_with_offset(buffer, cursor, insertion, deletion, 0)
}

fn predicted_buffer_after_insert_with_offset(
    buffer: &str,
    cursor: u32,
    insertion: &str,
    deletion: i64,
    offset: i64,
) -> Option<(String, u32)> {
    let cursor = usize::try_from(cursor).ok()?;
    if cursor > buffer.len() || !buffer.is_char_boundary(cursor) {
        return None;
    }
    let insertion_cursor = move_edit_cursor(buffer, cursor, offset);
    let deletion = usize::try_from(deletion).ok()?;
    let deletion_start = previous_edit_boundary(buffer, insertion_cursor, deletion);
    let mut result =
        String::with_capacity(buffer.len().saturating_sub(insertion_cursor - deletion_start) + insertion.len());
    result.push_str(&buffer[..deletion_start]);
    let mut predicted_cursor = result.len();
    // Keep the untouched tail in the simulated buffer while consuming the
    // insertion stream. This matters for a closed quote: the right-arrow
    // crossing from the newly completed value must be able to cross the
    // original closing quote before an automatic space/newline is appended.
    result.push_str(&buffer[insertion_cursor..]);

    let mut offset = 0;
    while offset < insertion.len() {
        let remaining = insertion.get(offset..)?;
        if remaining.starts_with("\x1b[D") {
            predicted_cursor = previous_edit_boundary(&result, predicted_cursor, 1);
            offset += "\x1b[D".len();
            continue;
        }
        if remaining.starts_with("\x1b[C") {
            predicted_cursor = next_edit_boundary(&result, predicted_cursor, 1);
            offset += "\x1b[C".len();
            continue;
        }
        let character = remaining.chars().next()?;
        result.insert(predicted_cursor, character);
        predicted_cursor += character.len_utf8();
        offset += character.len_utf8();
    }
    Some((result, u32::try_from(predicted_cursor).ok()?))
}

fn move_edit_cursor(buffer: &str, cursor: usize, offset: i64) -> usize {
    if offset < 0 {
        previous_edit_boundary(buffer, cursor, offset.unsigned_abs() as usize)
    } else {
        next_edit_boundary(buffer, cursor, offset as usize)
    }
}

/// A zero-width mark (combining accent, ZWJ, variation selector) rides on the
/// character before it. Readline, fish and zsh with `COMBINING_CHARS` (macOS's
/// `/etc/zshrc` sets it) move and delete over the whole cluster, so one `\x08`
/// or `\x1b[D` covers a character plus the marks that follow it. The WebView
/// counted JS `.length` (UTF-16 code units) instead, which made an emoji cost
/// two backspaces and ate the character before it.
fn is_attached_mark(ch: char) -> bool {
    !ch.is_control() && ch.width() == Some(0)
}

/// How many shell cursor positions `text` spans.
fn edit_units(text: &str) -> usize {
    text.chars().filter(|&ch| !is_attached_mark(ch)).count()
}

fn previous_edit_boundary(text: &str, from: usize, units: usize) -> usize {
    let prefix = text.get(..from).unwrap_or(text);
    let mut remaining = units;
    let mut boundary = from.min(prefix.len());
    for (index, ch) in prefix.char_indices().rev() {
        if remaining == 0 {
            break;
        }
        boundary = index;
        // Walking backwards meets a cluster's marks before its character.
        if !is_attached_mark(ch) {
            remaining -= 1;
        }
    }
    if remaining > 0 { 0 } else { boundary }
}

fn next_edit_boundary(text: &str, from: usize, units: usize) -> usize {
    let suffix = text.get(from..).unwrap_or("");
    let mut remaining = units;
    let mut boundary = from.min(text.len());
    for (index, ch) in suffix.char_indices() {
        if !is_attached_mark(ch) {
            if remaining == 0 {
                break;
            }
            remaining -= 1;
        }
        boundary = from + index + ch.len_utf8();
    }
    boundary
}

fn opens_new_arg(add_space: bool, should_add_space: bool, separator: Option<&str>) -> bool {
    (add_space && should_add_space) || separator.is_some()
}

fn should_suppress_after_insert(execute: bool, kind: &str, opens_new_arg: bool, text: &str) -> bool {
    !execute && !matches!(kind, "auto-execute" | "special") && !opens_new_arg && !text.ends_with([' ', '\n', '/'])
}

fn insertion_for(name: &str, search_term: &str) -> (String, i64) {
    if let Some(suffix) = name.strip_prefix(search_term) {
        (suffix.to_string(), 0)
    } else {
        (
            name.to_string(),
            edit_units(&deletion_term_for_search(search_term)) as i64,
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShellQuoteKind {
    Single,
    Double,
    AnsiC,
}

fn closed_shell_quote_inner(raw_search: &str) -> Option<(ShellQuoteKind, &str)> {
    let (kind, rest) = if let Some(rest) = raw_search.strip_prefix("$'") {
        (ShellQuoteKind::AnsiC, rest)
    } else if let Some(rest) = raw_search.strip_prefix('\'') {
        (ShellQuoteKind::Single, rest)
    } else if let Some(rest) = raw_search.strip_prefix('"') {
        (ShellQuoteKind::Double, rest)
    } else {
        return None;
    };
    let closer = match kind {
        ShellQuoteKind::Single | ShellQuoteKind::AnsiC => '\'',
        ShellQuoteKind::Double => '"',
    };
    let inner = rest.strip_suffix(closer)?;
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if kind != ShellQuoteKind::Single && ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == closer {
            // An unescaped closer before the final character means this is a
            // quoted fragment followed by more text, not a token whose caret
            // is immediately after its closing quote.
            return None;
        }
    }
    if escaped { None } else { Some((kind, inner)) }
}

fn escape_insertion_inside_quote(value: &str, kind: ShellQuoteKind) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match kind {
            ShellQuoteKind::Single if ch == '\'' => escaped.push_str("'\"'\"'"),
            ShellQuoteKind::Double if matches!(ch, '$' | '`' | '"' | '\\' | '\n') => {
                escaped.push('\\');
                escaped.push(ch);
            },
            ShellQuoteKind::AnsiC if matches!(ch, '\\' | '\'') => {
                escaped.push('\\');
                escaped.push(ch);
            },
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn split_closed_quote_suffix(text: &str, trailing_space: bool, trailing_newline: bool) -> (&str, &str) {
    let mut inside_end = text.len();
    if trailing_newline {
        inside_end = text
            .get(..inside_end)
            .and_then(|text| text.strip_suffix('\n'))
            .map_or(inside_end, str::len);
    }
    if trailing_space {
        inside_end = text
            .get(..inside_end)
            .and_then(|text| text.strip_suffix(' '))
            .map_or(inside_end, str::len);
    }
    (&text[..inside_end], &text[inside_end..])
}

/// Move only the completion's token text inside a closed quote. Automatic
/// spaces and execute newlines belong after that quote, so the insertion
/// stream crosses it with one right-arrow before appending those suffixes.
fn insertion_edit_for_raw_search(
    raw_search: &str,
    insertion: String,
    deletion: i64,
    quote_aware: bool,
    trailing_space: bool,
    trailing_newline: bool,
) -> (String, i64, Option<i64>) {
    let Some((quote_kind, _)) = closed_shell_quote_inner(raw_search) else {
        return (insertion, deletion, None);
    };
    // An explicit `insertValue` is already shell text, not necessarily the
    // plain value represented by this query. It may intentionally contain
    // quotes, separators, or a cursor marker, so moving its edit point inside
    // the existing quote can change the command's meaning. Keep the legacy
    // replacement protocol for that case; only plain-name/file insertions
    // have enough information for quote-native editing.
    if !quote_aware {
        return (insertion, deletion, None);
    }
    // A cursor marker is an explicit request to leave the caret at a position
    // inside the inserted text. Do not move that caret back across the quote;
    // the marker path has its own cursor semantics.
    if insertion.contains("\x1b[D") {
        return (insertion, deletion, None);
    }
    // `insertion_for` normally receives the unquoted value. For a replacement
    // inside a closed quote, however, backspaces must count the raw shell
    // characters (including escapes), not the decoded value, or they can eat
    // the opening quote as well.
    let deletion = if deletion > 0 {
        closed_shell_quote_inner(raw_search).map_or(deletion, |(_, inner)| edit_units(inner) as i64)
    } else {
        deletion
    };
    let offset = -1;
    // Keep automatic suffixes outside the quote before re-encoding the value:
    // a double-quoted newline is escaped, while an execute newline must stay
    // a literal line break after the closing quote.
    let (raw_inside, outside) = split_closed_quote_suffix(&insertion, trailing_space, trailing_newline);
    let inside = if quote_aware {
        let value = unquote_shell_token(raw_inside);
        escape_insertion_inside_quote(&value, quote_kind)
    } else {
        raw_inside.to_string()
    };
    if inside.is_empty() {
        // An exact quoted acceptance may only have an automatic suffix. It is
        // already at the correct position, after the closing quote.
        return (insertion, deletion, None);
    }
    let mut stream = String::with_capacity(insertion.len() + "\x1b[C".len());
    stream.push_str(&inside);
    stream.push_str("\x1b[C");
    stream.push_str(outside);
    (stream, deletion, Some(offset))
}

fn insertion_edit_for_kind(
    raw_search: &str,
    insertion: String,
    deletion: i64,
    kind: &str,
    quote_aware: bool,
    trailing_space: bool,
    trailing_newline: bool,
) -> (String, i64, Option<i64>) {
    if matches!(kind, "auto-execute" | "special") {
        // Actions operate on the current buffer rather than replacing the
        // typed token. They must remain at the caret even if that token
        // happens to end in a quote.
        (insertion, deletion, None)
    } else {
        insertion_edit_for_raw_search(
            raw_search,
            insertion,
            deletion,
            quote_aware,
            trailing_space,
            trailing_newline,
        )
    }
}

/// Count deletion in the same units as the typed token.
///
/// WebView reconstructed an escaped space from innerText (`my file` → `my\ file`).
/// Native `search_term` is already the raw token, so injecting `\ ` into a
/// still-quoted query (`'my file`) adds a backslash the user never typed.
/// A typed `\ ` is already two characters and must stay that way.
fn deletion_term_for_search(search_term: &str) -> String {
    if search_term.contains('\'') || search_term.contains('"') || search_term.contains(r"\ ") {
        search_term.to_string()
    } else {
        search_term.replacen(' ', r"\ ", 1)
    }
}

fn insertion_for_kind(name: &str, search_term: &str, kind: &str) -> (String, i64) {
    if matches!(kind, "auto-execute" | "special") {
        (name.to_string(), 0)
    } else {
        insertion_for(name, search_term)
    }
}

fn insertion_search_term(kind: &str, raw_search: &str, query_term: Option<&str>, insert_text: &str) -> String {
    // Shortcut matching strips the leading `?`, but the accepted edit must
    // replace that prefix too. The WebView added one deletion explicitly;
    // using the raw token here is the same operation and also handles Unicode
    // by letting `insertion_for` count shell characters.
    if kind == "shortcut" {
        raw_search.to_string()
    } else {
        // Same candidate order as Tab Partial: a stamped getQueryTerm tail
        // that does not prefix the row (`fo` vs `src/foo/` from `'src/fo`)
        // must not win, or Full/Enter/click delete two characters and
        // insert the whole name (`'src/src/foo/`).
        prefix_insertion_search(insert_text, raw_search, query_term)
    }
}

/// Search used to compute a Tab Partial suffix against the (possibly escaped) shared prefix.
fn prefix_insertion_search(shared: &str, raw_search: &str, query_term: Option<&str>) -> String {
    let unquoted_query = query_term.map(unquote_shell_token);
    let unquoted_raw = unquote_shell_token(raw_search);
    let candidates = [
        unquoted_query.clone(),
        Some(unquoted_raw.clone()),
        query_term.map(str::to_string),
        Some(raw_search.to_string()),
    ];
    for candidate in candidates.into_iter().flatten() {
        if candidate.is_empty() {
            continue;
        }
        if shared.starts_with(&candidate) || shared.to_ascii_lowercase().starts_with(&candidate.to_ascii_lowercase()) {
            return candidate;
        }
    }
    unquoted_raw
}

/// WebView `rootCommand === "?"` adds two deletions for `?` + space.
/// Glued `?la` already deletes the `?` via the raw token and must not get extra.
fn apply_shortcut_palette_deletion(
    buffer: &str,
    search_term: &str,
    insertion: String,
    deletion: i64,
    full_text: &str,
) -> (String, i64) {
    if search_term.starts_with('?') {
        return (insertion, deletion);
    }
    // WebView `rootCommand === "?"` is the current command, not the first
    // token of the whole buffer (`echo x && ? deploy`).
    if ranking_root_command(buffer, None) != "?" {
        return (insertion, deletion);
    }
    let query = edit_units(search_term) as i64;
    (full_text.to_string(), query + 2)
}

/// `onlyShowOnTab` hides until Tab on a new trailing-space argument, not
/// whenever the list is already hidden (that upgraded paste/Esc-keypress
/// hides into a latch). Extra spaces on the same token are not a new arg.
fn safe_tab_row(kind: &str, insert_value: Option<&str>) -> bool {
    !matches!(kind, "auto-execute" | "special") && insert_value.is_none_or(|value| !value.contains(['\n', '\r']))
}

fn only_show_on_tab_hides_until_shown(previous: Option<&str>, current: &str) -> bool {
    let Some(previous) = previous.filter(|previous| !previous.is_empty()) else {
        return !current.is_empty();
    };
    if !current.ends_with(' ') {
        return false;
    }
    if previous.ends_with(' ') {
        let (previous_tokens, _) = tokenize(previous);
        let (current_tokens, _) = tokenize(current);
        previous_tokens != current_tokens
    } else {
        true
    }
}

fn current_arg_identity_changed(previous: &str, next: &str) -> bool {
    !next.is_empty() && previous != next
}

#[cfg(test)]
fn full_insertion(name: &str, kind: &str, args_hint: &str, add_space: bool) -> String {
    let is_folder = matches!(kind, "folder" | "dir");
    let mut text = escape_insertion(name, is_folder);
    if add_space && should_add_space(kind, args_hint) && !text.ends_with([' ', '\n', '/']) {
        text.push(' ');
    }
    text
}

/// Build the accepted text using the metadata carried by a Fig suggestion.
/// `insert_value` wins over the display name for all non-file suggestions;
/// file/folder suggestions retain the shell escaping path from the old UI.
/// Did a backspace remove a whole trailing token? Fig compared the parsed
/// token arrays and kept the list down in that case, so deleting `-m` from
/// `git commit -m` does not immediately reopen every `git commit` option.
/// Removing the trailing space that had opened a new argument (`git commit `
/// → `git commit`) is the same hide: the WebView injected an empty token
/// when text ended with a space, so that backspace dropped the token count.
fn backspaced_to_new_token(previous: &str, current: &str) -> bool {
    if current.len() >= previous.len() {
        return false;
    }
    let (current_tokens, _) = tokenize(current);
    let (previous_tokens, _) = tokenize(previous);
    if current_tokens.is_empty() {
        return false;
    }
    if previous.ends_with(' ') && !current.ends_with(' ') && previous_tokens == current_tokens {
        return true;
    }
    if current_tokens.len() >= previous_tokens.len() {
        return false;
    }
    previous_tokens[current_tokens.len() - 1] == current_tokens[current_tokens.len() - 1]
}

/// The WebView's `isBufferDifferenceFromTyping`, inverted. Anything that is
/// not a single-character edit on a shared prefix reads as a paste or a shell
/// history recall, and Fig did not answer those with suggestions.
fn large_buffer_change(previous: &str, current: &str) -> bool {
    if !previous.starts_with(current) && !current.starts_with(previous) {
        return true;
    }
    let previous_len = previous.encode_utf16().count() as i64;
    let current_len = current.encode_utf16().count() as i64;
    (previous_len - current_len).abs() >= 2
}

/// Escape spaces in a Tab common prefix the way a shell path needs them.
/// History rows like `checkout main` / `checkout master` share `checkout `;
/// treating that token-boundary space as `\\ ` yields `git checkout\`.
fn escape_tab_prefix(shared: &str, kind: &str) -> String {
    if matches!(kind, "file" | "folder" | "dir") {
        shared.replace(' ', r"\ ")
    } else {
        shared.to_string()
    }
}

/// Does the common prefix already spell out the row's own insertion?
/// Fig compared the two before deciding whether Tab was a partial completion
/// or a full acceptance, using the bare insertion — without the trailing space
/// or newline an acceptance would append.
fn prefix_completes_row(shared: &str, item: &SuggestionItem) -> bool {
    if item.kind == "auto-execute" {
        return false;
    }
    escape_tab_prefix(shared, &item.kind)
        == full_insertion_for_item(
            &item.name,
            item.insert_value.as_deref(),
            &item.kind,
            item.separator_to_add.as_deref(),
            false,
            false,
            false,
        )
}

fn full_insertion_for_item(
    name: &str,
    insert_value: Option<&str>,
    kind: &str,
    separator: Option<&str>,
    should_add_space: bool,
    add_space: bool,
    execute: bool,
) -> String {
    let is_folder = matches!(kind, "folder" | "dir");
    let is_file_or_folder = is_folder || kind == "file";
    let mut text = if let Some(value) = insert_value.filter(|_| !is_file_or_folder) {
        value.to_string()
    } else {
        let mut value = name.to_string();
        if let Some(separator) = separator.filter(|separator| !separator.is_empty()) {
            value.push_str(separator);
            value.push_str("{cursor}");
        }
        // The WebView escaped the completed value, including the separator
        // and cursor marker. This keeps the closing quote after `{cursor}` so
        // resolving the marker leaves the caret inside a quoted value.
        escape_word(&value, is_folder, is_file_or_folder)
    };

    if execute && kind != "auto-execute" {
        text.push('\n');
    }
    if text.ends_with('\n') {
        let end = text.trim_end_matches('\n').len();
        text.truncate(end);
        text.push('\n');
    }
    if add_space && should_add_space && !text.ends_with('\n') {
        text.push(' ');
    }
    text
}

/// Fig's `{cursor}` marker is inserted as text followed by left-arrow bytes.
/// The PTY insertion request applies its `offset` before insertion, so putting
/// the movement in the insertion stream preserves the old post-insert cursor
/// position (notably for `--option={cursor}` and quoted snippets).
fn resolve_cursor_marker(mut text: String) -> String {
    const MARKER: &str = "{cursor}";
    let Some(marker) = text.find(MARKER) else {
        return text;
    };
    let after = edit_units(&text[marker + MARKER.len()..]);
    text.replace_range(marker..marker + MARKER.len(), "");
    text.push_str(&"\x1b[D".repeat(after));
    text
}

#[cfg(test)]
fn should_add_space(kind: &str, args_hint: &str) -> bool {
    match kind {
        "cmd" => true,
        "subcommand" | "option" => args_hint.contains('<'),
        _ => false,
    }
}

/// Characters that keep an unquoted word from reaching the command as written.
///
/// Fig's list (`insertion.ts`, which the WebView shipped), plus `;` and the
/// backquote. Without those two a file or a remote branch named
/// `a;sh${IFS}payload` was typed unquoted, and Enter ran what followed the
/// `;`. Braces are left out on purpose: they cannot run anything, and
/// `stash@{0}` has always been accepted bare.
const SHELL_SPECIAL: &[char] = &[
    '\\', '?', '*', '\'', '"', '#', '|', '<', '>', '(', ')', '[', ']', '!', '&', ';', '`',
];

/// The file or folder name itself, without the directories before it.
fn last_path_component(path: &str) -> &str {
    let path = path.strip_suffix('/').unwrap_or(path);
    path.rsplit('/').next().unwrap_or(path)
}

/// Whether `value` has to be quoted to stay one literal shell word.
///
/// A `$` counts only in a path's own name. Before the last `/` it is what the
/// user typed and the path generator expanded (`$HOME/Doc` lists `$HOME/Documents/`),
/// and other rows may name a variable to expand (`$PATH`). A file called
/// `price$5.txt` is neither: unquoted, the shell opens `price.txt`.
fn needs_quoting(value: &str, is_path: bool) -> bool {
    value.chars().any(|ch| SHELL_SPECIAL.contains(&ch)) || (is_path && last_path_component(value).contains('$'))
}

/// Whether a Tab common prefix would put a character on the command line that
/// accepting the row would have quoted. Only the part still to be typed
/// counts: what is already there is the user's own spelling.
fn tab_prefix_needs_quoting(shared: &str, raw_search: &str, query_term: Option<&str>, kind: &str) -> bool {
    let typed = prefix_insertion_search(shared, raw_search, query_term);
    let (untyped, _) = insertion_for(shared, &typed);
    needs_quoting(&untyped, matches!(kind, "file" | "folder" | "dir"))
}

#[cfg(test)]
fn escape_insertion(value: &str, is_folder: bool) -> String {
    escape_word(value, is_folder, is_folder)
}

/// `is_path` is true for file and folder rows; `is_folder` keeps the trailing
/// slash outside the quotes.
fn escape_word(value: &str, is_folder: bool, is_path: bool) -> String {
    if !needs_quoting(value, is_path) {
        // JavaScript's `str.replace(/\s/g, "\\ ")` normalizes every
        // whitespace character to an escaped ASCII space.
        let mut escaped = String::with_capacity(value.len());
        for ch in value.chars() {
            if ch.is_whitespace() {
                escaped.push_str(r"\ ");
            } else {
                escaped.push(ch);
            }
        }
        return escaped;
    }
    let quoted = |s: &str| s.replace('\'', r#"'"'"'"#);
    if is_folder {
        let body = value.strip_suffix('/').unwrap_or(value);
        format!("'{}'/", quoted(body))
    } else {
        format!("'{}'", quoted(value))
    }
}

fn intercept_flags(overlay_visible: bool, has_items: bool) -> (bool, bool) {
    // Keystrokes only while the list is on screen. Global intercept stays on when
    // items are kept (onlyShowOnTab / hideAutocomplete) so Tab can show the list.
    (overlay_visible && has_items, has_items)
}

/// Grouped so the four independent flags cannot be silently transposed at a
/// call site.
#[derive(Clone, Copy)]
struct InterceptInputs {
    overlay_visible: bool,
    has_items: bool,
    positioned: bool,
    has_last_position: bool,
}

/// Native key interception must additionally know whether a caret location is
/// usable. A hidden list with retained rows may keep global Tab interception
/// when it can be shown at the last caret; a visible list that failed layout
/// must not swallow terminal keys at all.
fn positioned_intercept_flags(inputs: InterceptInputs) -> (bool, bool) {
    let InterceptInputs {
        overlay_visible,
        has_items,
        positioned,
        has_last_position,
    } = inputs;
    if !has_items {
        return (false, false);
    }
    if overlay_visible {
        (positioned, positioned)
    } else {
        (false, has_last_position)
    }
}

fn set_intercept(figterm_state: &FigtermState, session_id: Uuid, overlay_visible: bool, has_items: bool) {
    let (intercept, intercept_global) = intercept_flags(overlay_visible, has_items);
    set_intercept_flags(figterm_state, session_id, intercept, intercept_global);
}

fn desired_intercept_modes(for_this: bool, intercept: bool, intercept_global: bool) -> (InterceptMode, InterceptMode) {
    (
        if intercept && for_this {
            InterceptMode::Locked
        } else {
            InterceptMode::Unlocked
        },
        if intercept_global && for_this {
            InterceptMode::Locked
        } else {
            InterceptMode::Unlocked
        },
    )
}

fn intercept_modes_need_ipc(current: (InterceptMode, InterceptMode), desired: (InterceptMode, InterceptMode)) -> bool {
    current != desired
}

fn set_intercept_flags(figterm_state: &FigtermState, session_id: Uuid, intercept: bool, intercept_global: bool) {
    for session in figterm_state.inner.lock().linked_sessions.values_mut() {
        let desired = desired_intercept_modes(session.id == session_id, intercept, intercept_global);
        if !intercept_modes_need_ipc((session.intercept, session.intercept_global), desired) {
            continue;
        }
        let (next_intercept, next_global) = desired;
        let enable = next_intercept == InterceptMode::Locked;
        let enable_global = next_global == InterceptMode::Locked;
        let actions = if enable || enable_global {
            overlay_actions()
        } else {
            vec![]
        };
        let admitted = session
            .send(FigtermCommand::InterceptFigJs {
                intercept_keystrokes: enable,
                intercept_global_keystrokes: enable_global,
                actions,
                override_actions: enable || enable_global,
            })
            .and_then(|()| session.send(FigtermCommand::InterceptFigJSVisible { visible: enable }));
        if admitted.is_ok() {
            session.intercept = next_intercept;
            session.intercept_global = next_global;
        } else {
            session.sender.close();
        }
    }
}

fn action_requires_visible(action: &str) -> bool {
    matches!(
        action,
        "navigateUp"
            | "navigateDown"
            | "insertSelected"
            | "insertCommonPrefixOrInsertSelected"
            | "insertSelectedAndExecute"
            | "execute"
            | "insertCommonPrefix"
            | "insertCommonPrefixOrNavigateDown"
            | "toggleDescription"
            | "showDescription"
            | "hideDescription"
            | "toggleHistoryMode"
            | "toggleFuzzySearch"
            | "increaseSize"
            | "decreaseSize"
    ) || action.starts_with("selectSuggestion")
}

fn action_inserts_from_rows(action: &str) -> bool {
    matches!(
        action,
        "insertSelected"
            | "insertSelectedAndExecute"
            | "insertCommonPrefix"
            | "insertCommonPrefixOrInsertSelected"
            | "insertCommonPrefixOrNavigateDown"
    )
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalDisable {
    /// The app's own state, e.g. an IME terminal without the input method.
    Confirmed(macos_utils::window_server::ApplicationSpecifier),
    /// Part of that state could not be read, so the disable is a guess.
    Unconfirmed,
}

/// An unknown frontmost app cannot confirm the sample either.
#[cfg(target_os = "macos")]
fn disable_sample_is_stale(
    disable: Option<&TerminalDisable>,
    frontmost: impl FnOnce() -> Option<macos_utils::window_server::ApplicationSpecifier>,
) -> bool {
    match disable {
        None => false,
        Some(TerminalDisable::Unconfirmed) => true,
        Some(TerminalDisable::Confirmed(app)) => frontmost().is_none_or(|frontmost| frontmost != *app),
    }
}

/// Rows whose request input is unknown keep the previous behaviour.
fn rows_are_stale(rows_input: Option<&LastInput>, current_input: &LastInput) -> bool {
    rows_input.is_some_and(|rows| rows != current_input)
}

fn action_is_allowed(action: &str, visible: bool, loading: bool, has_items: bool) -> bool {
    !action_requires_visible(action) || (visible && (!loading || has_items))
}

fn click_matches_current(click: &ClickInsert, overlay: &OverlayState) -> bool {
    overlay.visible
        && click.search == overlay.search_term
        && overlay.items.iter().any(|item| {
            item.name == click.name
                && item.description == click.description
                && item.kind == click.kind
                && item.insert_value == click.insert_value
                && item.primary_name == click.primary_name
                && item.separator_to_add == click.separator_to_add
                && item.should_add_space == click.should_add_space
                && item.query_term == click.query_term
                && item.argument_value == click.argument_value
                && item.acceptance_scope == click.acceptance_scope
        })
}

fn select_suggestion_index(n: usize, len: usize) -> Option<usize> {
    (n >= 1 && n <= len).then(|| n - 1)
}

const DEFAULT_OVERLAY_BINDINGS: &[(&str, &[&str])] = &[
    ("insertSelected", &["enter"]),
    ("insertCommonPrefixOrInsertSelected", &["tab"]),
    ("hideAutocomplete", &["esc"]),
    ("navigateUp", &["shift+tab", "up", "control+p"]),
    ("navigateDown", &["down", "control+n"]),
    ("selectSuggestion1", &["command+1"]),
    ("selectSuggestion2", &["command+2"]),
    ("selectSuggestion3", &["command+3"]),
    ("selectSuggestion4", &["command+4"]),
    ("selectSuggestion5", &["command+5"]),
    ("selectSuggestion6", &["command+6"]),
    ("selectSuggestion7", &["command+7"]),
    ("selectSuggestion8", &["command+8"]),
    ("selectSuggestion9", &["command+9"]),
    ("selectSuggestion10", &["command+0"]),
    ("toggleDescription", &["control+k"]),
    ("toggleHistoryMode", &["control+r"]),
];

fn overlay_actions() -> Vec<Action> {
    let user = KeyBindings::load_from_settings("autocomplete").unwrap_or_else(|_| KeyBindings(Vec::new()));
    merge_overlay_actions(DEFAULT_OVERLAY_BINDINGS, user)
}

fn merge_overlay_actions(defaults: &[(&str, &[&str])], user: KeyBindings) -> Vec<Action> {
    let mut actions: Vec<Action> = defaults
        .iter()
        .map(|(identifier, bindings)| Action {
            identifier: (*identifier).to_string(),
            bindings: bindings.iter().map(|binding| (*binding).to_string()).collect(),
        })
        .collect();
    for KeyBinding { identifier, binding } in user {
        actions.push(Action {
            identifier,
            bindings: vec![binding],
        });
    }
    actions
}

fn overlay_bounds(
    position: WindowPosition,
    overlay_size: LogicalSize<f64>,
    platform_state: &PlatformState,
    popout: bool,
    flip_height: f64,
) -> (Point<Pixels>, Size<Pixels>, bool, bool) {
    match position {
        WindowPosition::Absolute(pos) => {
            let logical: LogicalPosition<f64> = match pos {
                Position::Logical(p) => LogicalPosition::new(p.x, p.y),
                Position::Physical(p) => p.to_logical(1.0),
            };
            (
                gpui::point(px(logical.x as f32), px(logical.y as f32)),
                size(px(overlay_size.width as f32), px(overlay_size.height as f32)),
                false,
                false,
            )
        },
        WindowPosition::Centered => (
            gpui::point(px(120.), px(120.)),
            size(px(overlay_size.width as f32), px(overlay_size.height as f32)),
            false,
            false,
        ),
        WindowPosition::RelativeToCaret {
            caret_position,
            caret_size,
            origin,
        } => {
            let mut caret: LogicalPosition<f64> = match caret_position {
                Position::Logical(p) => LogicalPosition::new(p.x, p.y),
                Position::Physical(p) => p.to_logical(1.0),
            };
            let caret_size: LogicalSize<f64> = match caret_size {
                tao::dpi::Size::Logical(s) => LogicalSize::new(s.width, s.height),
                tao::dpi::Size::Physical(s) => s.to_logical(1.0),
            };
            let screens = screens_quartz();
            caret.y = caret_y_in_quartz_space(caret.y, caret_size.height, origin, screens.first().copied());
            let mut edges = screen_edges_containing(&screens, caret.x, caret.y);
            // The WebView clamped the window to the *monitor* and only consulted the
            // terminal window when deciding whether to flip above the caret. Keeping
            // these separate matters: a caret near the bottom of a short terminal must
            // not drag the overlay upwards while there is still screen below it.
            let mut flip_bottom = edges.map(|(_, _, _, bottom)| bottom);
            if let Some(active) = platform_state.get_active_window() {
                let (_, _, _, window_bottom) = window_edges(&active.rect);
                flip_bottom = Some(match flip_bottom {
                    Some(bottom) => bottom.min(window_bottom),
                    None => window_bottom,
                });
                if edges.is_none() {
                    edges = Some(window_edges(&active.rect));
                }
            }
            let (x, y, on_left, is_above) = place_overlay_at_caret(
                caret.x,
                caret.y,
                caret_size.height,
                overlay_size.width,
                overlay_size.height,
                flip_height,
                edges,
                flip_bottom,
                popout,
            );
            (
                gpui::point(px(x as f32), px(y as f32)),
                size(px(overlay_size.width as f32), px(overlay_size.height as f32)),
                on_left,
                is_above,
            )
        },
    }
}

/// Place the overlay below the caret, flipping the description to the left or the
/// window above the caret when it would leave `edges` (left, top, right, bottom).
///
/// `flip_bottom` is the lower of the screen and the terminal window; it only
/// decides whether to flip. Clamping always uses the screen `edges`.
#[allow(clippy::too_many_arguments)]
fn place_overlay_at_caret(
    caret_x: f64,
    caret_y: f64,
    caret_height: f64,
    overlay_width: f64,
    overlay_height: f64,
    flip_height: f64,
    edges: Option<(f64, f64, f64, f64)>,
    flip_bottom: Option<f64>,
    popout: bool,
) -> (f64, f64, bool, bool) {
    let mut x = caret_x;
    let mut y = caret_y + caret_height + 2.0;
    let mut on_left = false;
    let mut is_above = false;
    let constrained_to_screen = edges.is_some();
    if let Some((left, top, right, bottom)) = edges {
        if popout && x + overlay_width > right {
            let shift = fastab_gpui::POPOUT_WIDTH as f64;
            x = (caret_x - shift).max(left);
            on_left = true;
        }
        // The WebView decided whether to flip using the configured maxHeight,
        // not the momentary number of rendered rows. This keeps a short list
        // from jumping below the caret only to move above it as more generator
        // results arrive.
        let overflows_above = top >= caret_y - flip_height;
        let overflows_below = caret_y + caret_height + flip_height > flip_bottom.unwrap_or(bottom);
        if !overflows_above && overflows_below {
            y = caret_y - overlay_height - 2.0;
            is_above = true;
        }
        // Keep the whole window on the monitor, exactly as the WebView host did.
        x = x.min((right - overlay_width).max(left)).max(left);
        y = y.min((bottom - overlay_height).max(top)).max(top);
    }
    if !constrained_to_screen {
        x = x.max(0.0);
        y = y.max(0.0);
    }
    (x, y, on_left, is_above)
}

/// IME clients report Cocoa bottom-left coordinates, while AX and the overlay
/// placement code use Quartz top-left coordinates.
fn caret_y_in_quartz_space(
    caret_y: f64,
    caret_height: f64,
    origin: Origin,
    primary_screen: Option<(f64, f64, f64, f64)>,
) -> f64 {
    match (origin, primary_screen) {
        (Origin::BottomLeft, Some((_, primary_y, _, primary_height))) => {
            primary_y + primary_height - caret_y - caret_height
        },
        _ => caret_y,
    }
}

fn window_edges(rect: &crate::utils::Rect) -> (f64, f64, f64, f64) {
    let pos = match rect.position {
        Position::Logical(p) => (p.x, p.y),
        Position::Physical(p) => {
            let p = p.to_logical(1.0);
            (p.x, p.y)
        },
    };
    let size = match rect.size {
        tao::dpi::Size::Logical(s) => (s.width, s.height),
        tao::dpi::Size::Physical(s) => {
            let s = s.to_logical(1.0);
            (s.width, s.height)
        },
    };
    (pos.0, pos.1, pos.0 + size.0, pos.1 + size.1)
}

fn screen_edges_containing(screens: &[(f64, f64, f64, f64)], x: f64, y: f64) -> Option<(f64, f64, f64, f64)> {
    let screen = screens
        .iter()
        .copied()
        .find(|(sx, sy, sw, sh)| x >= *sx && x <= sx + sw && y >= *sy && y <= sy + sh)
        .or_else(|| screens.first().copied())?;
    Some((screen.0, screen.1, screen.0 + screen.2, screen.1 + screen.3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn insertion_prediction_retires_with_input_but_survives_hidden_echo(cx: &mut gpui::TestAppContext) {
        use fastab_remote_ipc::figterm::{EditBuffer, FigtermSession};

        let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::new_fake());
        let specs = tempfile::tempdir().unwrap();
        let engine = EngineClient::spawn(specs.path().to_path_buf()).unwrap();
        let (proxy, _events) = crate::event_loop::channel();
        let sessions = Arc::new(FigtermState::new());
        let platform = Arc::new(PlatformState::new(proxy.clone()));
        let session = Uuid::new_v4();
        let (sender, mut commands) = fastab_remote_ipc::outbox::channel();
        sessions.insert(FigtermSession {
            id: session,
            secret: String::new(),
            sender,
            dead_since: None,
            edit_buffer: EditBuffer::default(),
            last_receive: tokio::time::Instant::now(),
            context: None,
            flattened_env: Arc::new(Vec::new()),
            terminal_cursor_coordinates: None,
            current_session_metrics: None,
            intercept: InterceptMode::Unlocked,
            intercept_global: InterceptMode::Unlocked,
        });
        let mut overlay = cx.update(|cx| {
            let mut overlay = OverlayController::start(cx, engine.clone(), proxy, sessions.clone(), platform).unwrap();
            overlay.enabled = true;
            overlay.state.update(cx, |state, _| state.only_show_on_tab = false);
            overlay
        });
        let buffer = format!("tool {}", "x".repeat(64 * 1024));
        let predicted = format!("{buffer}_accepted");
        let mut insert = |overlay: &mut OverlayController, cx: &mut App| {
            overlay.complete_buffer(
                buffer.clone(),
                String::new(),
                buffer.len() as u32,
                session,
                sessions.clone(),
                cx,
            );
            assert!(overlay.insert_text("_accepted", 0, false, &sessions, cx));
            let mut inserts = Vec::new();
            while let Some(frame) = commands.try_recv() {
                let (_, message) = fastab_proto::FigMessage::parse(&mut frame.bytes()).unwrap();
                let message = message.decode::<fastab_proto::remote::Clientbound>().unwrap();
                if let Some(fastab_proto::remote::clientbound::Packet::Request(request)) = message.packet
                    && let Some(fastab_proto::remote::clientbound::request::Request::InsertText(insert)) =
                        request.request
                {
                    inserts.push(insert);
                }
            }
            assert_eq!(inserts.len(), 1, "exercise a real admitted InsertText");
            assert_eq!(inserts[0].insertion.as_deref(), Some("_accepted"));
            assert_eq!(inserts[0].insertion_buffer.as_deref(), Some(buffer.as_str()));
            assert_eq!(
                overlay.self_insertion.lock().unwrap().as_deref(),
                Some(predicted.as_str())
            );
        };

        cx.update(|cx| {
            insert(&mut overlay, cx);
            overlay.hide(cx);
            overlay.end_input(Uuid::new_v4(), cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(11));
        cx.run_until_parked();
        cx.update(|cx| {
            // A hidden window's grace does not end the terminal's input. Its
            // delayed echo still needs the paste-heuristic exemption.
            assert_eq!(
                overlay.self_insertion.lock().unwrap().as_deref(),
                Some(predicted.as_str())
            );
            overlay.complete_buffer(
                predicted.clone(),
                String::new(),
                predicted.len() as u32,
                session,
                sessions.clone(),
                cx,
            );
            assert!(overlay.self_insertion.lock().unwrap().is_none());
            assert!(matches!(
                overlay.completion_display,
                CompletionDisplayState::WaitingForCaret
            ));

            insert(&mut overlay, cx);
            overlay.end_input(session, cx);
            assert!(overlay.self_insertion.lock().unwrap().is_none());
            assert!(overlay.last_input.lock().unwrap().is_none());

            insert(&mut overlay, cx);
            overlay.invalidate_caret_for_focus_change(cx);
            assert!(overlay.self_insertion.lock().unwrap().is_none());
            assert!(overlay.last_input.lock().unwrap().is_none());
        });
        let diagnostics = futures::executor::block_on(engine.diagnostics()).unwrap();
        assert_eq!(
            diagnostics.requests.submitted, 0,
            "this no-caret fixture must not run the engine"
        );
    }

    #[gpui::test]
    fn delayed_input_cannot_restore_the_old_owner_after_host_accepts_focus(cx: &mut gpui::TestAppContext) {
        use crate::event_loop::OverlayInput;
        use crate::gpui_host::{dispatch_overlay_input, invalidate_overlay_input_focus};

        let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::new_fake());
        let specs = tempfile::tempdir().unwrap();
        let engine = EngineClient::spawn(specs.path().to_path_buf()).unwrap();
        let (proxy, events) = crate::event_loop::channel();
        let sessions = Arc::new(FigtermState::new());
        let platform = Arc::new(PlatformState::new(proxy.clone()));
        let mut overlay = cx.update(|cx| {
            let mut overlay =
                OverlayController::start(cx, engine, proxy.clone(), sessions.clone(), platform.clone()).unwrap();
            overlay.enabled = true;
            overlay
        });
        let old_session = Uuid::new_v4();
        let fresh_session = Uuid::new_v4();
        let input = |text: &str, session_id| OverlayInput {
            buffer: text.into(),
            cwd: String::new(),
            cursor: text.len() as u32,
            session_id,
            redraw: false,
        };
        cx.update(|cx| {
            overlay.complete_buffer("git ".into(), String::new(), 4, old_session, sessions.clone(), cx);
        });
        // The initial hide notifies the AI observer before this fixture's
        // ingress sequence begins. Account for that real host event explicitly.
        for event in events.try_iter() {
            assert!(matches!(event, Event::JevContextChanged));
        }
        let old_input = proxy.begin_overlay_input();
        let old_empty = proxy.begin_overlay_input();
        proxy
            .send_event(Event::PlatformBoundEvent(
                crate::platform::PlatformBoundEvent::ExternalWindowFocusChanged {
                    #[cfg(target_os = "macos")]
                    app: macos_utils::window_server::ApplicationSpecifier {
                        pid: 1,
                        bundle_id: "test.terminal".into(),
                    },
                    window: None,
                },
            ))
            .unwrap();
        futures::executor::block_on(async {
            proxy
                .send_overlay_input_with_sequence(input("git s", old_session), old_input)
                .await
                .unwrap();
            proxy
                .send_overlay_input_async(input("tool ", fresh_session))
                .await
                .unwrap();
            proxy
                .send_overlay_input_with_sequence(input("", old_session), old_empty)
                .await
                .unwrap();
        });
        // Exercise the host's post-validation focus handler; native identity
        // discovery itself requires a real frontmost terminal, not a test app.
        futures::executor::block_on(events.recv_queued())
            .unwrap()
            .dispatch(|_, sequence| {
                cx.update(|cx| invalidate_overlay_input_focus(&proxy, &mut overlay, sequence, cx));
            });
        futures::executor::block_on(events.recv_queued())
            .unwrap()
            .dispatch(|event, _| {
                let Event::GpuiOverlayInput(input) = event else {
                    panic!("old input")
                };
                cx.update(|cx| {
                    assert!(!dispatch_overlay_input(&proxy, &mut overlay, &input, &sessions, cx));
                    overlay.apply_position(WindowPosition::Centered, &platform, cx);
                    assert!(overlay.last_input.lock().unwrap().is_none());
                    assert!(matches!(overlay.completion_display, CompletionDisplayState::Idle));
                    assert!(!overlay.state.read(cx).visible);
                });
            });
        futures::executor::block_on(events.recv_queued())
            .unwrap()
            .dispatch(|event, _| {
                let Event::GpuiOverlayInput(input) = event else {
                    panic!("fresh input")
                };
                cx.update(|cx| {
                    assert!(dispatch_overlay_input(&proxy, &mut overlay, &input, &sessions, cx));
                    assert_eq!(overlay.current_session(), Some(fresh_session));
                    assert_eq!(overlay.last_input.lock().unwrap().as_ref().unwrap().buffer, "tool ");
                });
            });
        futures::executor::block_on(events.recv_queued())
            .unwrap()
            .dispatch(|event, _| {
                let Event::GpuiOverlayInput(input) = event else {
                    panic!("old empty input")
                };
                cx.update(|cx| {
                    assert!(!dispatch_overlay_input(&proxy, &mut overlay, &input, &sessions, cx));
                    assert_eq!(overlay.current_session(), Some(fresh_session));
                    assert_eq!(overlay.last_input.lock().unwrap().as_ref().unwrap().buffer, "tool ");
                    assert!(matches!(
                        overlay.completion_display,
                        CompletionDisplayState::WaitingForCaret
                    ));
                });
            });
    }

    #[gpui::test]
    fn a_background_redraw_of_an_unchanged_line_does_not_take_over(cx: &mut gpui::TestAppContext) {
        use crate::event_loop::OverlayInput;
        use crate::gpui_host::dispatch_overlay_input;

        let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::new_fake());
        let specs = tempfile::tempdir().unwrap();
        let engine = EngineClient::spawn(specs.path().to_path_buf()).unwrap();
        let (proxy, events) = crate::event_loop::channel();
        let sessions = Arc::new(FigtermState::new());
        let platform = Arc::new(PlatformState::new(proxy.clone()));
        let mut overlay = cx.update(|cx| {
            let mut overlay =
                OverlayController::start(cx, engine, proxy.clone(), sessions.clone(), platform.clone()).unwrap();
            overlay.enabled = true;
            overlay
        });
        let typing = Uuid::new_v4();
        let background = Uuid::new_v4();
        cx.update(|cx| {
            overlay.complete_buffer("git ".into(), String::new(), 4, typing, sessions.clone(), cx);
        });
        for event in events.try_iter() {
            assert!(matches!(event, Event::JevContextChanged));
        }

        let dispatch = |overlay: &mut OverlayController, text: &str, session_id, redraw| {
            futures::executor::block_on(proxy.send_overlay_input_async(OverlayInput {
                buffer: text.into(),
                cwd: String::new(),
                cursor: text.len() as u32,
                session_id,
                redraw,
            }))
            .unwrap();
            loop {
                let mut needs_caret = None;
                futures::executor::block_on(events.recv_queued())
                    .unwrap()
                    .dispatch(|event, _| {
                        if let Event::GpuiOverlayInput(input) = event {
                            needs_caret =
                                Some(cx.update(|cx| dispatch_overlay_input(&proxy, overlay, &input, &sessions, cx)));
                        }
                    });
                if let Some(needs_caret) = needs_caret {
                    return needs_caret;
                }
            }
        };

        assert!(!dispatch(&mut overlay, "cargo b", background, true));
        assert_eq!(overlay.current_session(), Some(typing));
        assert_eq!(overlay.last_input.lock().unwrap().as_ref().unwrap().buffer, "git ");
        // The current session's own redraw still completes.
        assert!(dispatch(&mut overlay, "git ", typing, true));
        assert_eq!(overlay.current_session(), Some(typing));
        // Typing in the background tab is a real switch.
        assert!(dispatch(&mut overlay, "cargo bu", background, false));
        assert_eq!(overlay.current_session(), Some(background));
        assert_eq!(overlay.last_input.lock().unwrap().as_ref().unwrap().buffer, "cargo bu");

        // Neither an ended input nor a focus change lets a repaint take over:
        // after a resize every pane repaints, and the one that wins would show
        // its list at the focused pane's caret.
        cx.update(|cx| overlay.end_input(background, cx));
        assert!(!dispatch(&mut overlay, "git ", typing, true));
        assert_eq!(overlay.current_session(), Some(background));
        cx.update(|cx| overlay.invalidate_caret_for_focus_change(cx));
        assert!(!dispatch(&mut overlay, "git ", typing, true));
        assert_eq!(overlay.current_session(), Some(background));
        // The first keystroke in the focused pane switches to it.
        assert!(dispatch(&mut overlay, "git s", typing, false));
        assert_eq!(overlay.current_session(), Some(typing));
        assert_eq!(overlay.last_input.lock().unwrap().as_ref().unwrap().buffer, "git s");
    }

    #[gpui::test]
    fn replaced_and_hidden_debounce_tasks_do_not_post_stale_events(cx: &mut gpui::TestAppContext) {
        let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::new_fake());
        let specs = tempfile::tempdir().unwrap();
        let engine = EngineClient::spawn(specs.path().to_path_buf()).unwrap();
        let (proxy, events) = crate::event_loop::channel();
        let platform = Arc::new(PlatformState::new(proxy.clone()));
        let mut overlay = cx
            .update(|cx| OverlayController::start(cx, engine, proxy, Arc::new(FigtermState::new()), platform).unwrap());
        cx.update(|cx| overlay.schedule_debounce(60_000, cx));
        cx.run_until_parked();
        cx.update(|cx| {
            overlay.bump_generation();
            overlay.schedule_debounce(100, cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        let generations: Vec<_> = events
            .try_iter()
            .filter_map(|event| match event {
                Event::GpuiOverlayDebouncedComplete { generation, .. } => Some(generation),
                _ => None,
            })
            .collect();
        assert_eq!(generations, [overlay.generation.load(Ordering::Relaxed)]);
        cx.update(|cx| {
            overlay.schedule_debounce(60_000, cx);
            overlay.hide(cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(61));
        cx.run_until_parked();
        assert!(
            events
                .try_iter()
                .all(|event| !matches!(event, Event::GpuiOverlayDebouncedComplete { .. }))
        );
        assert!(overlay.debounce_task.is_none());
    }

    #[gpui::test]
    fn hidden_argument_hints_do_not_recreate_an_idle_overlay(cx: &mut gpui::TestAppContext) {
        let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::new_fake());
        let specs = tempfile::tempdir().unwrap();
        let engine = EngineClient::spawn(specs.path().to_path_buf()).unwrap();
        let (proxy, _events) = crate::event_loop::channel();
        let platform = Arc::new(PlatformState::new(proxy.clone()));
        let mut overlay = cx.update(|cx| {
            let mut overlay =
                OverlayController::start(cx, engine, proxy, Arc::new(FigtermState::new()), platform.clone()).unwrap();
            overlay.state.update(cx, |state, _| {
                state.set_current_arg("path", "Choose a path");
            });
            overlay.hide_until_shown(cx);
            overlay
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(11));
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(overlay.handle.lock().unwrap().is_none());
            overlay.apply_position(
                WindowPosition::Absolute(Position::Logical(LogicalPosition::new(100.0, 100.0))),
                &platform,
                cx,
            );
            assert!(!overlay.relayout(cx));
            assert!(overlay.handle.lock().unwrap().is_none());
            assert!(
                !overlay.state.read(cx).has_current_arg(),
                "hidden payload retired with the window"
            );
        });
    }

    #[gpui::test]
    fn caret_and_input_lifecycle_release_engine_resources(cx: &mut gpui::TestAppContext) {
        // Settings overrides are thread-local, but EngineClient has real
        // worker threads. Isolate their process-wide settings from both the
        // user's custom history commands and concurrently running UI tests.
        const CHILD: &str = "FASTAB_END_INPUT_REGRESSION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "overlay::tests::caret_and_input_lifecycle_release_engine_resources",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .expect("isolated regression process");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                // A renamed test must not turn a stale --exact filter into
                // a successful child run with zero tests.
                output.status.success() && stdout.contains("test result: ok. 1 passed;"),
                "{}\n{}",
                stdout,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use fastab_settings::JsonStore as _;
        *fastab_settings::OldSettings::data_lock().write() = Some(
            [
                ("autocomplete.history.disableLoading".into(), serde_json::json!(true)),
                ("autocomplete.sortMethod".into(), serde_json::json!("default")),
            ]
            .into_iter()
            .collect(),
        );
        let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::from_slice(&[
            ("dashboard.theme", serde_json::json!("light")),
            ("beta.history.mode", serde_json::json!("off")),
        ]));
        let specs = tempfile::tempdir().expect("fixture directory");
        std::fs::write(
            specs.path().join("tool.json"),
            r#"{"names":["tool"],"options":[{"names":["--value"]}]}"#,
        )
        .expect("fixture spec");
        let engine = EngineClient::spawn_with_options(
            specs.path().to_path_buf(),
            fastab_engine::EngineClientOptions {
                spec_idle_grace: Duration::from_millis(50),
                ..fastab_engine::EngineClientOptions::default()
            },
        )
        .expect("engine");
        let (proxy, _events) = crate::event_loop::channel();
        // No linked sessions models the state after a terminal disconnects.
        // The controller must retire its input even though the worker can
        // still complete this buffer without a live remote connection.
        let figterm_state = Arc::new(FigtermState::new());
        let platform_state = Arc::new(PlatformState::new(proxy.clone()));
        let mut overlay = cx.update(|cx| {
            let mut overlay =
                OverlayController::start(cx, engine.clone(), proxy, figterm_state.clone(), platform_state.clone())
                    .expect("controller");
            // The headless test does not depend on the test binary's AX grant.
            overlay.enabled = true;
            overlay
        });
        let session = Uuid::new_v4();
        let complete_input = |overlay: &mut OverlayController, cx: &mut App| {
            overlay.complete_buffer("tool ".into(), String::new(), 5, session, figterm_state.clone(), cx);
        };
        let refresh_settings = |overlay: &mut OverlayController, cx: &mut App| {
            // The enabled branch of GpuiHost's ReloadCredentials handler.
            overlay.apply_theme(cx);
            overlay.set_enabled(true, cx);
            overlay.recomplete(cx);
        };
        let snapshot = || futures::executor::block_on(engine.diagnostics()).expect("diagnostics");
        let wait_for_release = || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while snapshot()
                .engine
                .expect("initialized engine")
                .registry
                .cached_file_count
                != 0
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "hidden input must release its spec"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let position = WindowPosition::Absolute(Position::Logical(LogicalPosition::new(100.0, 100.0)));

        cx.update(|cx| complete_input(&mut overlay, cx));
        cx.update(|cx| {
            refresh_settings(&mut overlay, cx);
            complete_input(&mut overlay, cx);
        });
        let waiting = snapshot();
        assert_eq!(waiting.requests.submitted, 0, "no caret must not start completion work");
        assert!(
            waiting.engine.is_none(),
            "waiting for a caret must not initialize the engine"
        );
        cx.update(|cx| {
            overlay.apply_position(position, &platform_state, cx);
            overlay.apply_position(position, &platform_state, cx);
        });
        assert_eq!(snapshot().requests.completed, 1);

        // Ending a background session and ordinary hiding must retain the
        // active input, so settings changes can still recompute it.
        cx.update(|cx| {
            overlay.end_input(Uuid::new_v4(), cx);
            refresh_settings(&mut overlay, cx);
        });
        assert_eq!(snapshot().requests.completed, 2);
        cx.update(|cx| {
            overlay.hide(cx);
            refresh_settings(&mut overlay, cx);
        });
        assert_eq!(
            snapshot().requests.completed,
            2,
            "hidden settings refresh must not complete"
        );

        cx.update(|cx| {
            overlay.end_input(session, cx);
            refresh_settings(&mut overlay, cx);
            overlay.end_input(session, cx);
            refresh_settings(&mut overlay, cx);
        });
        assert_eq!(snapshot().requests.submitted, 2, "ended input must not be resubmitted");
        wait_for_release();

        // pre-exec keeps the UUID: a new real buffer, even with identical
        // text, must be allowed to complete and establish the next input.
        cx.update(|cx| complete_input(&mut overlay, cx));
        let resumed = snapshot();
        assert_eq!(resumed.requests.submitted, 3);
        assert_eq!(resumed.requests.completed, 3);
        assert_eq!(
            resumed.engine.expect("initialized engine").registry.cached_file_count,
            1
        );

        // Losing the caret releases the same nonempty input. A new caret
        // resumes it exactly once, even if the terminal repeats its location.
        cx.update(|cx| overlay.clear_caret_position(cx));
        wait_for_release();
        cx.update(|cx| {
            overlay.apply_position(position, &platform_state, cx);
            overlay.apply_position(position, &platform_state, cx);
        });
        assert_eq!(snapshot().requests.completed, 4);

        // An explicit Esc cancels that automatic resume; only explicit Show
        // may recompute retained input after the spec has been released.
        cx.update(|cx| {
            overlay.clear_caret_position(cx);
            overlay.hide_until_shown(cx);
            overlay.apply_position(position, &platform_state, cx);
        });
        wait_for_release();
        assert_eq!(snapshot().requests.submitted, 4);
        cx.update(|cx| {
            overlay.show(cx);
            overlay.apply_position(position, &platform_state, cx);
        });
        assert_eq!(snapshot().requests.completed, 5);

        // Switching to a session with no caret must still retire the old
        // owner, even though no new request is submitted to the worker.
        let next_session = Uuid::new_v4();
        cx.update(|cx| {
            overlay.complete_buffer(
                "tool ".into(),
                String::new(),
                5,
                next_session,
                figterm_state.clone(),
                cx,
            );
        });
        wait_for_release();
        assert_eq!(snapshot().requests.submitted, 5);
        cx.update(|cx| overlay.apply_position(position, &platform_state, cx));
        assert_eq!(snapshot().requests.completed, 6);

        // A focus change is stronger than a transient missing caret. A caret
        // from another pane and a settings refresh cannot revive old input.
        cx.update(|cx| {
            overlay.invalidate_caret_for_focus_change(cx);
            overlay.apply_position(position, &platform_state, cx);
            refresh_settings(&mut overlay, cx);
        });
        wait_for_release();
        assert_eq!(snapshot().requests.submitted, 6);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_a_disable_sampled_for_the_frontmost_app_survives_a_keystroke() {
        use super::TerminalDisable::{Confirmed, Unconfirmed};
        use macos_utils::window_server::ApplicationSpecifier;
        let app = |pid, bundle_id: &str| ApplicationSpecifier {
            pid,
            bundle_id: bundle_id.to_owned(),
        };
        let ghostty = app(42, "com.mitchellh.ghostty");
        let stale = |disable, frontmost: Option<ApplicationSpecifier>| {
            super::disable_sample_is_stale(Some(&disable), || frontmost)
        };
        // An IME terminal without the input method stays off while it is frontmost.
        assert!(!stale(Confirmed(ghostty.clone()), Some(ghostty.clone())));
        // A sample taken for Safari, or an older Ghostty process, no longer applies.
        assert!(stale(Confirmed(app(7, "com.apple.Safari")), Some(ghostty.clone())));
        assert!(stale(
            Confirmed(app(41, "com.mitchellh.ghostty")),
            Some(ghostty.clone())
        ));
        assert!(stale(Confirmed(ghostty.clone()), None));
        // A disable guessed from an unreadable input method state never sticks.
        assert!(stale(Unconfirmed, Some(ghostty.clone())));
        assert!(!super::disable_sample_is_stale(None, || unreachable!()));
    }

    #[test]
    fn insert_keeps_directory_prefix() {
        assert_eq!(insertion_for("src/main.rs", "src/m"), ("ain.rs".into(), 0));
    }

    #[test]
    fn webview_enter_on_tilde_inserts_tilde_and_does_not_execute() {
        // WebView `getFullInsertion` uses insertValue for non-file rows, else
        // the matched name. The cd hidden `~` seed and a history suffix `~`
        // both insert `~` with no slash and no newline. Enter is
        // `insertSelected` (`fig.json`), not `insertSelectedAndExecute`.
        // Auto-execute (`\n`) is a separate row, hidden when
        // `hideAutoExecuteSuggestion` is on.
        assert_eq!(full_insertion_for_item("~", None, "arg", None, false, true, false), "~");
        assert_eq!(
            full_insertion_for_item("~", Some("~"), "history", None, false, true, false),
            "~"
        );
        assert!(!immediate_for_insert(false, "~"));
        assert!(should_suppress_after_insert(false, "arg", false, "~"));
        assert_eq!(
            full_insertion_for_item("~", Some("\n"), "auto-execute", None, false, true, false),
            "\n"
        );
        assert_eq!(
            full_insertion_for_item("~/Desktop/", None, "folder", None, false, true, false),
            "~/Desktop/"
        );
        assert!(!should_suppress_after_insert(false, "folder", false, "~/Desktop/"));
    }

    #[test]
    fn insert_appends_history_suffix() {
        assert_eq!(
            insertion_for("git checkout -b feature", "git"),
            (" checkout -b feature".into(), 0)
        );
    }

    #[test]
    fn insert_replaces_token_when_name_is_not_a_prefix() {
        assert_eq!(insertion_for("checkout", "git"), ("checkout".into(), 3));
    }

    #[test]
    fn shortcut_acceptance_replaces_the_raw_question_mark_query() {
        let search = insertion_search_term("shortcut", "?la", Some("la"), "last");
        assert_eq!(search, "?la");
        assert_eq!(insertion_for("last", &search), ("last".into(), 3));
        assert_eq!(insertion_search_term("arg", "scope@la", Some("la"), "last"), "la");
    }

    #[test]
    fn insert_deletion_counts_unicode_chars_not_bytes() {
        assert_eq!(insertion_for("文件.txt", "文"), ("件.txt".into(), 0));
        assert_eq!(insertion_for("src/", "文"), ("src/".into(), 1));
    }

    #[test]
    fn insert_replaces_token_when_case_differs() {
        assert_eq!(insertion_for("Checkout", "ch"), ("Checkout".into(), 2));
    }

    #[test]
    fn completion_is_hidden_when_cursor_is_inside_a_word() {
        assert!(cursor_is_inside_word("git status", 5));
        assert!(!cursor_is_inside_word("git status", 3));
        assert!(!cursor_is_inside_word("git status", 10));
        assert!(cursor_is_inside_word("git 文件", 5));
    }

    #[test]
    fn legacy_width_only_adds_history_space_for_an_unset_width() {
        assert_eq!(legacy_list_width(None, None), 320);
        assert_eq!(legacy_list_width(None, Some("show")), 370);
        assert_eq!(legacy_list_width(None, Some("history_only")), 370);
        assert_eq!(legacy_list_width(None, Some("off")), 320);
        assert_eq!(legacy_list_width(Some(300), Some("show")), 300);
    }

    #[test]
    fn auto_execute_does_not_delete_the_exact_query() {
        assert_eq!(insertion_for_kind("\n", "status", "auto-execute"), ("\n".into(), 0));
    }

    #[test]
    fn action_insertions_stay_at_the_caret_after_a_closed_quote() {
        let (insertion, deletion) = insertion_for_kind("\n", "ch", "auto-execute");
        let (insertion, deletion, offset) =
            insertion_edit_for_kind("'ch'", insertion, deletion, "auto-execute", false, false, false);
        assert_eq!(insertion, "\n");
        assert_eq!(deletion, 0);
        assert_eq!(offset, None);
    }

    #[test]
    fn explicit_insert_value_keeps_legacy_closed_quote_edit_position() {
        // `insertValue` is opaque shell text. In particular, it can be a
        // command fragment rather than the plain value named by the row, so
        // the closed-quote value edit must not move it before the closer.
        let raw_search = "'ch'";
        let insert_value =
            full_insertion_for_item("checkout", Some("checkout --detach"), "arg", None, false, false, false);
        let search = insertion_search_term("arg", raw_search, None, &insert_value);
        assert_eq!(search, "ch");
        let (insertion, deletion) = insertion_for_kind(&insert_value, &search, "arg");
        assert_eq!((insertion.as_str(), deletion), ("eckout --detach", 0));

        let (insertion, deletion, offset) =
            insertion_edit_for_kind(raw_search, insertion, deletion, "arg", false, false, false);
        assert_eq!(insertion, "eckout --detach");
        assert_eq!(deletion, 0);
        assert_eq!(offset, None);

        let buffer = "git 'ch'";
        let (predicted, cursor) = predicted_buffer_after_insert_with_offset(
            buffer,
            u32::try_from(buffer.len()).unwrap(),
            &insertion,
            deletion,
            offset.unwrap_or_default(),
        )
        .expect("explicit insertValue should retain the legacy insertion calculation");
        assert_eq!(predicted, "git 'ch'eckout --detach");
        assert_eq!(cursor, u32::try_from(predicted.len()).unwrap());
    }

    #[test]
    fn ordinary_enter_is_not_an_immediate_execute() {
        assert!(!immediate_for_insert(false, "checkout"));
        assert!(immediate_for_insert(true, "checkout"));
        assert!(!immediate_for_insert(true, "\n"));
        assert!(!immediate_for_insert(true, "commit -m ''\n\x1b[D"));
    }

    #[test]
    fn acceptance_uses_the_engine_root_and_primary_alias() {
        let item = ClickInsert {
            name: "co".into(),
            primary_name: Some("checkout".into()),
            ..ClickInsert::default()
        };
        assert_eq!(
            accepted_suggestion_key(&item, Some(&("git co ignored".into(), 6))),
            Some(("git".into(), "checkout".into()))
        );

        let item = ClickInsert {
            name: "status".into(),
            ..ClickInsert::default()
        };
        assert_eq!(
            accepted_suggestion_key(&item, Some(&("git status".into(), 10))),
            Some(("git".into(), "status".into()))
        );
        assert_eq!(accepted_suggestion_key(&item, None), None);
    }

    #[test]
    fn argument_acceptance_keeps_the_scope_from_the_selected_row() {
        let item = ClickInsert {
            name: "co".into(),
            primary_name: Some("checkout".into()),
            argument_value: true,
            acceptance_scope: Some("v1:stored-parser-scope".into()),
            ..ClickInsert::default()
        };
        assert_eq!(
            accepted_scoped_suggestion_key(&item),
            Some(("v1:stored-parser-scope".into(), "checkout".into()))
        );
        assert_eq!(
            accepted_scoped_suggestion_key(&ClickInsert {
                acceptance_scope: None,
                ..item.clone()
            }),
            None
        );
        assert_eq!(
            accepted_scoped_suggestion_key(&ClickInsert {
                argument_value: false,
                ..item
            }),
            None
        );
    }

    #[test]
    fn predicted_acceptance_tracks_buffer_and_cursor() {
        assert_eq!(
            predicted_buffer_after_insert("git co", 6, "mmit", 0),
            Some(("git commit".into(), 10))
        );
        assert_eq!(
            predicted_buffer_after_insert("git st", 6, "status", 2),
            Some(("git status".into(), 10))
        );
        assert_eq!(
            predicted_buffer_after_insert("--message", 9, "= \x1b[D", 0),
            Some(("--message= ".into(), 10))
        );
    }

    #[test]
    fn predicted_suppression_does_not_swallow_a_later_input_without_ack() {
        let Some((expected_buffer, expected_cursor)) = predicted_buffer_after_insert("git co", 6, "mmit", 0) else {
            panic!("expected a predictable insertion");
        };
        let mut overlay = OverlayState::new();
        overlay.mark_suppress_unchanged_completion(expected_buffer, expected_cursor);
        assert!(!overlay.take_suppress_completion_for("git commitx", 11));
        assert!(!overlay.take_suppress_completion_for("git commit", 10));
    }

    #[test]
    fn predicted_suppression_still_hides_when_the_caret_drifts() {
        let Some((expected_buffer, expected_cursor)) = predicted_buffer_after_insert("git ch", 6, "eckout", 0) else {
            panic!("expected a predictable insertion");
        };
        assert_eq!(expected_buffer, "git checkout");
        let mut overlay = OverlayState::new();
        overlay.mark_suppress_unchanged_completion(expected_buffer.clone(), expected_cursor);
        assert_ne!(expected_cursor, 8);
        assert!(overlay.take_suppress_completion_for("git checkout", 8));
    }

    #[test]
    fn session_switch_is_detected_for_generation_invalidation() {
        let first = Uuid::nil();
        let second = Uuid::from_u128(1);
        assert!(session_changed(None, first));
        assert!(!session_changed(Some(first), first));
        assert!(session_changed(Some(first), second));
    }

    #[test]
    fn completion_requires_current_generation_and_session() {
        let session = Uuid::from_u128(7);
        assert!(completion_is_current(4, 4, Some(session), session));
        assert!(!completion_is_current(3, 4, Some(session), session));
        assert!(!completion_is_current(4, 4, None, session));
        assert!(!completion_is_current(4, 4, Some(Uuid::from_u128(8)), session));
    }

    #[test]
    fn actions_only_apply_to_the_current_session() {
        let current = Uuid::from_u128(7);
        assert!(action_session_is_current(Some(current), current));
        assert!(!action_session_is_current(None, current));
        assert!(!action_session_is_current(Some(Uuid::from_u128(8)), current));
    }

    #[test]
    fn empty_exact_acceptance_does_not_claim_the_buffer_changed_or_execute() {
        assert!(!insertion_changes_buffer("", 0, false));
        assert!(insertion_changes_buffer("checkout", 0, false));
        assert!(insertion_changes_buffer("", 3, false));
        assert!(insertion_changes_buffer("", 0, true));
    }

    #[test]
    fn new_argument_depends_on_effective_space_setting_or_separator() {
        assert!(opens_new_arg(true, true, None));
        assert!(!opens_new_arg(false, true, None));
        assert!(opens_new_arg(false, false, Some("=")));
        assert!(!opens_new_arg(true, false, None));
    }

    #[test]
    fn suppress_predicate_excludes_actions_and_new_arguments() {
        assert!(should_suppress_after_insert(false, "subcommand", false, "checkout"));
        assert!(!should_suppress_after_insert(true, "subcommand", false, "checkout"));
        assert!(!should_suppress_after_insert(false, "auto-execute", false, "\n"));
        assert!(!should_suppress_after_insert(false, "subcommand", true, "checkout "));
        assert!(!should_suppress_after_insert(false, "subcommand", false, "checkout "));
    }

    #[test]
    fn common_prefix_of_nested_paths() {
        assert_eq!(
            fastab_gpui::longest_common_prefix(&["src/main.rs", "src/mod.rs"]),
            "src/m"
        );
    }

    #[test]
    fn escape_spaces_in_file_names() {
        assert_eq!(escape_insertion("my file.txt", false), r"my\ file.txt");
        assert_eq!(escape_insertion("My Folder/", true), r"My\ Folder/");
    }

    #[test]
    fn tab_prefix_does_not_escape_history_token_spaces() {
        // `git ch` + Tab with checkout history selected shares `checkout `,
        // which must stay a real space — not `checkout\`.
        assert_eq!(escape_tab_prefix("checkout ", "history"), "checkout ");
        assert_eq!(insertion_for("checkout ", "ch"), ("eckout ".into(), 0));
        assert_eq!(escape_tab_prefix("my file.", "file"), r"my\ file.");
        assert_eq!(escape_tab_prefix("my file.", "folder"), r"my\ file.");
        assert_eq!(escape_tab_prefix("checkout ", "subcommand"), "checkout ");
    }

    #[test]
    fn prefix_completes_row_uses_kind_aware_space_escaping() {
        let history = SuggestionItem {
            name: "checkout main".into(),
            kind: "history".into(),
            insert_value: Some("checkout main".into()),
            ..SuggestionItem::default()
        };
        assert!(!prefix_completes_row("checkout ", &history));
        assert!(prefix_completes_row("checkout main", &history));

        let file = SuggestionItem {
            name: "my file.txt".into(),
            kind: "file".into(),
            ..SuggestionItem::default()
        };
        assert!(prefix_completes_row("my file.txt", &file));
        assert!(!prefix_completes_row("my file.", &file));
    }

    #[test]
    fn escape_quotes_special_characters() {
        assert_eq!(escape_insertion("it's.txt", false), "'it'\"'\"'s.txt'");
        assert_eq!(escape_insertion("it's/", true), "'it'\"'\"'s'/");
    }

    /// Fig's list had neither `;` nor the backquote, so a name holding one was
    /// typed bare and Enter ran what followed it.
    #[test]
    fn command_separators_and_substitutions_are_quoted() {
        assert_eq!(escape_insertion("a;sh${IFS}payload", false), "'a;sh${IFS}payload'");
        assert_eq!(escape_insertion("a`id`b", false), "'a`id`b'");
        assert_eq!(escape_insertion("a;b/", true), "'a;b'/");
        let arg = |name| full_insertion_for_item(name, None, "arg", None, false, false, false);
        assert_eq!(arg("feature;rm"), "'feature;rm'");
        // Rows that are not paths keep a `$` and braces, as they always have.
        assert_eq!(arg("$PATH"), "$PATH");
        assert_eq!(arg("stash@{0}"), "stash@{0}");
    }

    #[test]
    fn a_dollar_in_a_file_name_is_quoted_but_not_in_the_typed_directory() {
        let file = |name| full_insertion_for_item(name, None, "file", None, false, false, false);
        let folder = |name| full_insertion_for_item(name, None, "folder", None, false, false, false);
        assert_eq!(file("price$5.txt"), "'price$5.txt'");
        assert_eq!(file("src/price$5.txt"), "'src/price$5.txt'");
        assert_eq!(folder("a$b/"), "'a$b'/");
        // `$HOME/Doc` lists `$HOME/Documents/`: that `$` is the user's.
        assert_eq!(folder("$HOME/Documents/"), "$HOME/Documents/");
        assert_eq!(file("$HOME/notes.txt"), "$HOME/notes.txt");
        assert_eq!(file("~/notes.txt"), "~/notes.txt");
    }

    #[test]
    fn a_tab_prefix_that_needs_quoting_is_not_typed_in_part() {
        assert!(tab_prefix_needs_quoting("a;b", "a", None, "file"));
        assert!(tab_prefix_needs_quoting("a(b", "a", None, "file"));
        assert!(tab_prefix_needs_quoting("pri$ce", "pr", None, "file"));
        assert!(tab_prefix_needs_quoting("feature;x", "fea", None, "arg"));
        // A replacement types the whole prefix.
        assert!(tab_prefix_needs_quoting("a;b", "A", None, "file"));
        // Spaces are escaped, not quoted.
        assert!(!tab_prefix_needs_quoting("my file", "my", None, "file"));
        // What is already on the line is the user's own spelling.
        assert!(!tab_prefix_needs_quoting(
            "Screenshot (1",
            r"Screenshot\ \(",
            None,
            "file"
        ));
        assert!(!tab_prefix_needs_quoting(
            "$HOME/Documents",
            "$HOME/Doc",
            None,
            "folder"
        ));
        assert!(!tab_prefix_needs_quoting("$PATH", "$PA", None, "arg"));
    }

    #[test]
    fn space_after_first_token_commands_and_mandatory_args() {
        assert!(should_add_space("cmd", ""));
        assert!(should_add_space("subcommand", "<branch>"));
        assert!(!should_add_space("subcommand", "[branch]"));
        assert!(!should_add_space("file", "<unused>"));
        assert!(!should_add_space("history", ""));
    }

    #[test]
    fn full_insertion_appends_space_to_commands() {
        assert_eq!(full_insertion("git", "cmd", "", true), "git ");
        assert_eq!(full_insertion("checkout", "subcommand", "[branch]", true), "checkout");
        assert_eq!(full_insertion("src/main.rs", "file", "", true), "src/main.rs");
        assert_eq!(full_insertion("git", "cmd", "", false), "git");
    }

    #[test]
    fn metadata_insert_value_is_used_verbatim() {
        assert_eq!(
            full_insertion_for_item("co", Some("commit -m '{cursor}'"), "arg", None, false, true, false,),
            "commit -m '{cursor}'"
        );
    }

    #[test]
    fn metadata_separator_places_cursor_marker() {
        assert_eq!(
            full_insertion_for_item("--message", None, "option", Some("="), true, true, false),
            "--message={cursor} "
        );
    }

    #[test]
    fn separator_is_escaped_with_the_name_and_keeps_cursor_inside_quotes() {
        let text = full_insertion_for_item("it's", None, "option", Some("="), false, true, false);
        assert_eq!(resolve_cursor_marker(text), "'it'\"'\"'s='\x1b[D");
    }

    #[test]
    fn execute_appends_one_newline_and_never_an_automatic_space() {
        assert_eq!(
            full_insertion_for_item("checkout", None, "subcommand", None, true, true, true),
            "checkout\n"
        );
        assert_eq!(
            full_insertion_for_item("ignored", Some("status\n\n"), "arg", None, true, true, true),
            "status\n"
        );
        assert_eq!(
            full_insertion_for_item("execute", Some("\n"), "auto-execute", None, true, true, true),
            "\n"
        );
    }

    #[test]
    fn should_add_space_matches_the_webview_without_extra_suffix_guards() {
        assert_eq!(
            full_insertion_for_item("ignored", Some("value "), "arg", None, true, true, false),
            "value  "
        );
        assert_eq!(
            full_insertion_for_item("folder/", None, "folder", None, true, true, false),
            "folder/ "
        );
    }

    #[test]
    fn cursor_marker_moves_after_trailing_quote() {
        assert_eq!(
            resolve_cursor_marker("commit -m '{cursor}'".into()),
            "commit -m ''\x1b[D"
        );
    }

    #[test]
    fn cursor_marker_counts_characters_not_utf16_units() {
        // The shell moves over U+1F600 with one left arrow.
        assert_eq!(resolve_cursor_marker("{cursor}😀".into()), "😀\x1b[D");
    }

    #[test]
    fn predicted_buffer_deletes_characters_not_utf16_units() {
        let (buffer, cursor) = predicted_buffer_after_insert("ok😀", 6, "x", 1).expect("predict");
        assert_eq!(buffer, "okx");
        assert_eq!(cursor, 3);
        let (buffer, _) = predicted_buffer_after_insert("ok😀", 6, "x", 2).expect("predict");
        assert_eq!(buffer, "ox");
    }

    #[test]
    fn deletion_of_an_emoji_query_is_one_backspace_per_character() {
        assert_eq!(insertion_for("😀.txt", "😀x"), ("😀.txt".into(), 2));
        assert_eq!(insertion_for("src/", "😀"), ("src/".into(), 1));
    }

    #[test]
    fn combining_marks_ride_on_the_character_before_them() {
        // NFD `café` is five scalars but four shell positions.
        assert_eq!(insertion_for("src/", "cafe\u{301}"), ("src/".into(), 4));
        assert_eq!(edit_units("👍\u{1F3FD}"), 2);
        assert_eq!(edit_units("👨\u{200D}👩"), 2);
        assert_eq!(edit_units("❤\u{FE0F}"), 1);
        assert_eq!(
            resolve_cursor_marker("{cursor}e\u{301}'".into()),
            "e\u{301}'\x1b[D\x1b[D"
        );

        let buffer = "ls cafe\u{301}";
        let (predicted, cursor) =
            predicted_buffer_after_insert(buffer, u32::try_from(buffer.len()).unwrap(), "x", 1).expect("predict");
        assert_eq!(predicted, "ls cafx");
        assert_eq!(cursor, u32::try_from("ls cafx".len()).unwrap());

        assert_eq!(next_edit_boundary("e\u{301}x", 0, 1), "e\u{301}".len());
        assert_eq!(previous_edit_boundary("xe\u{301}", "xe\u{301}".len(), 1), 1);
    }

    #[test]
    fn history_insert_value_keeps_literal_spaces() {
        assert_eq!(
            full_insertion_for_item(
                "git checkout -b feature",
                Some("git checkout -b feature"),
                "history",
                None,
                false,
                true,
                false,
            ),
            "git checkout -b feature"
        );
    }

    #[test]
    fn plain_insertion_normalizes_all_whitespace_like_javascript() {
        assert_eq!(escape_insertion("one\ttwo\nthree", false), r"one\ two\ three");
    }

    #[test]
    fn hide_turns_off_keystroke_intercept_but_keeps_tab_when_items_remain() {
        assert_eq!(intercept_flags(true, true), (true, true));
        assert_eq!(intercept_flags(false, true), (false, true));
        assert_eq!(intercept_flags(false, false), (false, false));
        assert_eq!(intercept_flags(true, false), (false, false));
    }

    #[test]
    fn up_from_first_row_respects_default_wrap_and_existing_history_preferences() {
        for (configured_wrap, navigate_to_history, expected_visible, expected_selected) in [
            (None, false, true, 1),
            (None, true, false, 0),
            (Some(false), false, false, 0),
            (Some(true), true, true, 1),
        ] {
            let mut overlay = OverlayState::new();
            overlay.set_suggestions(
                vec![
                    SuggestionItem {
                        name: "first".into(),
                        ..SuggestionItem::default()
                    },
                    SuggestionItem {
                        name: "last".into(),
                        ..SuggestionItem::default()
                    },
                ],
                "".into(),
            );
            overlay.navigate_to_history = navigate_to_history;
            overlay.scroll_wrap_around = resolve_scroll_wrap_around(configured_wrap, navigate_to_history);
            assert_eq!(
                overlay.move_selection_with_wrap(-1, overlay.scroll_wrap_around),
                expected_visible
            );
            assert_eq!(overlay.selected, expected_selected);
        }
    }

    #[test]
    fn other_tabs_unlock_and_unchanged_sessions_need_no_ipc() {
        assert_eq!(
            desired_intercept_modes(true, true, true),
            (InterceptMode::Locked, InterceptMode::Locked)
        );
        assert_eq!(
            desired_intercept_modes(true, true, false),
            (InterceptMode::Locked, InterceptMode::Unlocked)
        );
        assert_eq!(
            desired_intercept_modes(true, false, false),
            (InterceptMode::Unlocked, InterceptMode::Unlocked)
        );
        assert_eq!(
            desired_intercept_modes(false, true, true),
            (InterceptMode::Unlocked, InterceptMode::Unlocked)
        );
        let already_unlocked = desired_intercept_modes(false, true, true);
        assert!(!intercept_modes_need_ipc(already_unlocked, already_unlocked));
        assert!(intercept_modes_need_ipc(
            already_unlocked,
            desired_intercept_modes(true, true, true)
        ));
    }

    #[test]
    fn native_intercept_never_swallows_keys_without_a_usable_caret() {
        let inputs = |overlay_visible, positioned, has_last_position| InterceptInputs {
            overlay_visible,
            has_items: true,
            positioned,
            has_last_position,
        };
        assert_eq!(positioned_intercept_flags(inputs(true, false, false)), (false, false));
        assert_eq!(positioned_intercept_flags(inputs(false, false, false)), (false, false));
        assert_eq!(positioned_intercept_flags(inputs(false, false, true)), (false, true));
        assert_eq!(positioned_intercept_flags(inputs(true, true, true)), (true, true));
    }

    #[test]
    fn a_visible_fast_completion_retries_a_failed_native_layout() {
        let retryable = LayoutRetryInputs {
            overlay_visible: true,
            has_content: true,
            has_position: true,
            positioned: false,
        };
        assert!(should_retry_layout(retryable));
        assert!(!should_retry_layout(LayoutRetryInputs {
            positioned: true,
            ..retryable
        }));
        assert!(!should_retry_layout(LayoutRetryInputs {
            overlay_visible: false,
            ..retryable
        }));
        assert!(!should_retry_layout(LayoutRetryInputs {
            has_content: false,
            ..retryable
        }));
        assert!(!should_retry_layout(LayoutRetryInputs {
            has_position: false,
            ..retryable
        }));
    }

    #[test]
    fn native_layout_retries_are_short_and_bounded() {
        assert_eq!(layout_retry_delay(0), Some(Duration::from_millis(16)));
        assert_eq!(layout_retry_delay(1), Some(Duration::from_millis(32)));
        assert_eq!(layout_retry_delay(2), Some(Duration::from_millis(64)));
        assert_eq!(layout_retry_delay(3), Some(Duration::from_millis(128)));
        assert_eq!(layout_retry_delay(4), None);
    }

    #[test]
    fn a_new_completion_or_layout_cancels_an_old_retry_chain() {
        assert!(layout_retry_is_current(7, 7, 12, 12));
        assert!(!layout_retry_is_current(6, 7, 12, 12));
        assert!(!layout_retry_is_current(7, 7, 11, 12));
    }

    #[test]
    fn explicit_recompute_bypasses_identical_buffer_deduplication() {
        let input = LastInput {
            buffer: "git add .".into(),
            cwd: "/tmp/project".into(),
            cursor: 9,
            session_id: Uuid::nil(),
        };
        assert!(should_skip_duplicate_input(Some(&input), &input, false));
        assert!(!should_skip_duplicate_input(Some(&input), &input, true));

        let mut changed = input.clone();
        changed.cursor += 1;
        assert!(!should_skip_duplicate_input(Some(&input), &changed, false));
    }

    #[test]
    fn hidden_overlay_rejects_delayed_row_actions() {
        assert!(!action_is_allowed("insertSelected", false, false, false));
        assert!(!action_is_allowed("navigateDown", false, false, false));
        assert!(!action_is_allowed("selectSuggestion1", false, false, false));
        assert!(action_is_allowed("showAutocompleteFromTab", false, false, false));
        assert!(action_is_allowed("toggleAutocomplete", false, false, false));
    }

    #[test]
    fn a_completion_releases_its_own_loading_marker() {
        assert!(loading_owner_is_released_by(7, 7));
    }

    #[test]
    fn a_completion_releases_a_marker_stranded_by_an_older_request() {
        // The old WebView derived its loading flag from the live generator set,
        // so it could not be stranded. Here generation 6 turned the marker on
        // and its result was dropped as stale; generation 7 has to clean up or
        // `···` stays on screen until the next keystroke.
        assert!(loading_owner_is_released_by(6, 7));
    }

    #[test]
    fn a_completion_leaves_a_newer_requests_marker_alone() {
        assert!(!loading_owner_is_released_by(8, 7));
    }

    #[test]
    fn nothing_to_release_when_no_request_owns_the_marker() {
        assert!(!loading_owner_is_released_by(NO_LOADING_OWNER, 7));
    }

    #[test]
    fn deleting_a_whole_token_reads_as_a_new_token() {
        assert!(backspaced_to_new_token("git commit -m", "git commit"));
        assert!(backspaced_to_new_token("git c", "git "));
        assert!(backspaced_to_new_token(r#"git commit "msg""#, "git commit"));
    }

    #[test]
    fn ordinary_backspaces_within_a_token_keep_the_list_up() {
        assert!(!backspaced_to_new_token("git com", "git co"));
        assert!(!backspaced_to_new_token("git", "gi"));
        // Typing forward never counts, even when it adds a token.
        assert!(!backspaced_to_new_token("git ", "git c"));
    }

    #[test]
    fn backspacing_a_trailing_space_reads_as_a_new_token() {
        assert!(backspaced_to_new_token("git commit ", "git commit"));
        assert!(backspaced_to_new_token("git ", "git"));
    }

    #[test]
    fn typing_one_character_is_not_a_large_buffer_change() {
        assert!(!large_buffer_change("git", "git "));
        assert!(!large_buffer_change("git ", "git"));
        assert!(!large_buffer_change("git", "git"));
    }

    #[test]
    fn pasting_or_recalling_history_is_a_large_buffer_change() {
        // Up-arrow at an empty prompt.
        assert!(large_buffer_change("", "git commit -m 'wip'"));
        // A paste that shares no prefix with what was there.
        assert!(large_buffer_change("git st", "npm run build"));
        // Deleting a word.
        assert!(large_buffer_change("git commit", "git "));
    }

    #[test]
    fn multibyte_edits_are_counted_in_characters_not_bytes() {
        // One deleted CJK character is three bytes but a single UTF-16 unit.
        assert!(!large_buffer_change("echo 中文", "echo 中"));
        // One emoji is two UTF-16 units, matching JavaScript string.length.
        assert!(large_buffer_change("echo 😀", "echo "));
    }

    #[test]
    fn a_common_prefix_that_spells_out_the_row_is_a_full_acceptance() {
        // `getFullInsertion` omits the trailing space an acceptance appends,
        // so `should_add_space` must not break the comparison.
        let item = SuggestionItem {
            name: "checkout".into(),
            kind: "subcommand".into(),
            should_add_space: true,
            ..SuggestionItem::default()
        };
        assert!(prefix_completes_row("checkout", &item));
        assert!(!prefix_completes_row("check", &item));
    }

    #[test]
    fn a_common_prefix_never_triggers_an_auto_execute_row() {
        let item = SuggestionItem {
            name: "status".into(),
            kind: "auto-execute".into(),
            insert_value: Some("status".into()),
            ..SuggestionItem::default()
        };
        assert!(!prefix_completes_row("status", &item));
    }

    #[test]
    fn loading_overlay_rejects_actions_for_hidden_stale_rows() {
        assert!(!action_is_allowed("insertSelected", true, true, false));
        assert!(action_is_allowed("insertSelected", true, true, true));
        assert!(action_is_allowed("insertCommonPrefix", true, true, true));
        assert!(action_is_allowed("navigateDown", true, true, true));
        assert!(action_is_allowed("showAutocompleteFromTab", true, true, true));
        assert!(action_is_allowed("showAutocompleteFromTab", false, true, true));
        assert!(action_is_allowed("showAutocompleteFromTab", false, true, false));
        assert!(action_is_allowed("hideAutocomplete", true, true, false));
    }

    #[test]
    fn click_must_match_the_visible_result_and_raw_search() {
        let item = SuggestionItem {
            name: "checkout".into(),
            description: "Switch branches".into(),
            kind: "subcommand".into(),
            insert_value: Some("checkout".into()),
            query_term: Some("co".into()),
            ..SuggestionItem::default()
        };
        let mut overlay = OverlayState::new();
        overlay.set_suggestions(vec![item.clone()], "'co".into());
        let mut click = ClickInsert {
            name: item.name,
            description: item.description,
            search: "'co".into(),
            kind: item.kind,
            args_hint: item.args_hint,
            insert_value: item.insert_value,
            display_name: item.display_name,
            primary_name: item.primary_name,
            separator_to_add: item.separator_to_add,
            should_add_space: item.should_add_space,
            hidden: item.hidden,
            priority: item.priority,
            icon_identifier: item.icon_identifier,
            original_type: item.original_type,
            query_term: item.query_term,
            argument_value: item.argument_value,
            acceptance_scope: item.acceptance_scope,
        };
        assert!(click_matches_current(&click, &overlay));
        click.argument_value = true;
        assert!(!click_matches_current(&click, &overlay));
        click.argument_value = false;
        click.acceptance_scope = Some("stale-scope".into());
        assert!(!click_matches_current(&click, &overlay));
        click.acceptance_scope = None;
        click.search = "co".into();
        assert!(!click_matches_current(&click, &overlay));
        click.search = "'co".into();
        overlay.hide();
        assert!(!click_matches_current(&click, &overlay));

        overlay.visible = true;
        overlay.loading = true;
        assert!(click_matches_current(&click, &overlay));
    }

    #[test]
    fn no_popout_clamps_to_the_right_edge_instead_of_shifting_by_popout_width() {
        let (x, _, on_left, _) = place_overlay_at_caret(
            350.0,
            100.0,
            16.0,
            328.0,
            80.0,
            140.0,
            Some((0.0, 0.0, 400.0, 800.0)),
            None,
            false,
        );
        assert!(!on_left);
        assert_eq!(x, 72.0);
    }

    #[test]
    fn popout_flips_to_the_left_when_the_combined_width_overflows() {
        let popout_shift = fastab_gpui::POPOUT_WIDTH as f64;
        let width = 328.0 + popout_shift;
        let (x, _, on_left, _) = place_overlay_at_caret(
            400.0,
            100.0,
            16.0,
            width,
            80.0,
            140.0,
            Some((0.0, 0.0, 800.0, 800.0)),
            None,
            true,
        );
        assert!(on_left);
        assert_eq!(x, 400.0 - popout_shift);
    }

    #[test]
    fn no_overflow_keeps_the_overlay_at_the_caret() {
        let (x, y, on_left, is_above) = place_overlay_at_caret(
            40.0,
            80.0,
            16.0,
            328.0,
            88.0,
            140.0,
            Some((0.0, 0.0, 800.0, 600.0)),
            None,
            false,
        );
        assert_eq!(x, 40.0);
        assert_eq!(y, 80.0 + 16.0 + 2.0);
        assert!(!on_left);
        assert!(!is_above);
    }

    #[test]
    fn negative_secondary_screen_coordinates_are_preserved() {
        let (x, y, _, _) = place_overlay_at_caret(
            -1200.0,
            80.0,
            16.0,
            328.0,
            88.0,
            140.0,
            Some((-1440.0, 0.0, 0.0, 900.0)),
            None,
            false,
        );
        assert_eq!(x, -1200.0);
        assert_eq!(y, 98.0);
    }

    /// 1440x900 laptop as the primary display with a 2560x1440 monitor to its
    /// right, bottom-aligned. In caret (Quartz) space the taller screen starts
    /// 540px above the primary's top edge.
    const DUAL_DISPLAY: [(f64, f64, f64, f64); 2] = [(0.0, 0.0, 1440.0, 900.0), (1440.0, -540.0, 2560.0, 1440.0)];

    #[test]
    fn a_caret_on_the_external_monitor_selects_that_monitor() {
        assert_eq!(
            screen_edges_containing(&DUAL_DISPLAY, 2000.0, -200.0),
            Some((1440.0, -540.0, 4000.0, 900.0))
        );
    }

    #[test]
    fn a_caret_on_the_primary_display_still_selects_the_primary() {
        assert_eq!(
            screen_edges_containing(&DUAL_DISPLAY, 400.0, 300.0),
            Some((0.0, 0.0, 1440.0, 900.0))
        );
    }

    #[test]
    fn an_external_monitor_caret_is_not_dragged_onto_the_primary() {
        // Above the primary's top edge and far to its right: clamping against
        // the primary would yank the overlay to y = 0 and x <= 1112.
        let edges = screen_edges_containing(&DUAL_DISPLAY, 2000.0, -200.0);
        let (x, y, _, is_above) = place_overlay_at_caret(2000.0, -200.0, 16.0, 328.0, 88.0, 140.0, edges, None, false);
        assert_eq!(x, 2000.0);
        assert_eq!(y, -182.0);
        assert!(!is_above);
    }

    #[test]
    fn bottom_overflow_places_overlay_two_points_above_caret() {
        let (_, y, _, is_above) = place_overlay_at_caret(
            40.0,
            560.0,
            16.0,
            328.0,
            88.0,
            140.0,
            Some((0.0, 0.0, 800.0, 600.0)),
            None,
            false,
        );
        assert_eq!(y, 560.0 - 88.0 - 2.0);
        assert!(is_above);
    }

    #[test]
    fn overlay_below_the_caret_is_clamped_onto_the_screen() {
        // A short flip budget keeps the overlay below the caret, but the window
        // itself must still fit: the WebView clamped it up to the screen edge.
        let (_, y, _, is_above) = place_overlay_at_caret(
            40.0,
            560.0,
            16.0,
            328.0,
            88.0,
            10.0,
            Some((0.0, 0.0, 800.0, 600.0)),
            None,
            false,
        );
        assert!(!is_above);
        assert_eq!(y, 600.0 - 88.0);
    }

    #[test]
    fn terminal_bottom_only_decides_the_flip_and_never_clamps() {
        // Short terminal on a tall screen: flip above the caret, but keep the
        // window positioned against the screen rather than the terminal.
        let (_, y, _, is_above) = place_overlay_at_caret(
            40.0,
            300.0,
            16.0,
            328.0,
            88.0,
            140.0,
            Some((0.0, 0.0, 800.0, 900.0)),
            Some(200.0),
            false,
        );
        assert!(is_above);
        assert_eq!(y, 300.0 - 88.0 - 2.0);
    }

    #[test]
    fn short_list_uses_configured_height_for_stable_flip_decision() {
        let (_, y, _, is_above) = place_overlay_at_caret(
            40.0,
            470.0,
            16.0,
            328.0,
            28.0,
            140.0,
            Some((0.0, 0.0, 800.0, 600.0)),
            None,
            false,
        );
        assert_eq!(y, 470.0 - 28.0 - 2.0);
        assert!(is_above);
    }

    #[test]
    fn ime_bottom_left_caret_is_converted_to_quartz_top_left() {
        assert_eq!(
            caret_y_in_quartz_space(120.0, 18.0, Origin::BottomLeft, Some((0.0, 0.0, 1440.0, 900.0)),),
            762.0
        );
        assert_eq!(
            caret_y_in_quartz_space(120.0, 18.0, Origin::TopLeft, Some((0.0, 0.0, 1440.0, 900.0))),
            120.0
        );
        // Cocoa coordinates remain global across vertically arranged displays:
        // values above the primary screen map to negative Quartz Y, while
        // values below it map past the primary screen height.
        assert_eq!(
            caret_y_in_quartz_space(1_000.0, 18.0, Origin::BottomLeft, Some((0.0, 0.0, 1440.0, 900.0))),
            -118.0
        );
        assert_eq!(
            caret_y_in_quartz_space(-200.0, 18.0, Origin::BottomLeft, Some((0.0, 0.0, 1440.0, 900.0))),
            1_082.0
        );
    }

    #[test]
    fn select_suggestion_ignores_out_of_range_indices() {
        assert_eq!(select_suggestion_index(1, 2), Some(0));
        assert_eq!(select_suggestion_index(2, 2), Some(1));
        assert_eq!(select_suggestion_index(5, 2), None);
        assert_eq!(select_suggestion_index(0, 2), None);
        assert_eq!(select_suggestion_index(1, 0), None);
        assert_eq!(select_suggestion_index(10, 10), Some(9));
    }

    #[test]
    fn user_keybindings_are_appended_after_defaults() {
        // Desktop replaces the terminal's initial key map when the list is
        // shown. Both defaults must agree or Tab silently changes behavior.
        let defaults = merge_overlay_actions(DEFAULT_OVERLAY_BINDINGS, KeyBindings(Vec::new()));
        let desktop_tab = defaults
            .iter()
            .rev()
            .find(|action| action.bindings.iter().any(|binding| binding == "tab"))
            .expect("desktop Tab binding");
        let terminal_tab = KeyBindings::load_hardcoded()
            .into_iter()
            .rev()
            .find(|binding| binding.binding == "tab")
            .expect("terminal Tab binding");
        assert_eq!(desktop_tab.identifier, "insertCommonPrefixOrInsertSelected");
        assert_eq!(desktop_tab.identifier, terminal_tab.identifier);
        let user = KeyBindings(vec![
            KeyBinding {
                identifier: "increaseSize".into(),
                binding: "control+=".into(),
            },
            KeyBinding {
                identifier: "insertSelected".into(),
                binding: "tab".into(),
            },
        ]);
        let actions = merge_overlay_actions(DEFAULT_OVERLAY_BINDINGS, user);
        assert!(
            actions
                .iter()
                .any(|action| action.identifier == "insertSelected" && action.bindings.iter().any(|b| b == "enter"))
        );
        assert!(
            actions
                .iter()
                .any(|action| { action.identifier == "increaseSize" && action.bindings == ["control+=".to_string()] })
        );
        let last = actions.last().expect("user binding appended");
        assert_eq!(last.identifier, "insertSelected");
        assert_eq!(last.bindings, vec!["tab".to_string()]);
    }

    #[test]
    fn description_hint_symbolizes_modifier_and_uses_unicode_keys() {
        assert_eq!(format_keybinding("command+i"), "⌘i");
        assert_eq!(format_keybinding("control+shift+k"), "⌃⇧k");
        assert_eq!(format_keybinding("option+return"), "⌥↵");
    }

    #[test]
    fn quoted_tab_prefix_inserts_only_the_unquoted_suffix() {
        let search = prefix_insertion_search("src/foo/", "'src/fo", None);
        assert_eq!(search, "src/fo");
        assert_eq!(insertion_for("src/foo/", &search), ("o/".into(), 0));
        let search = prefix_insertion_search("src/foo/", "'src/fo", Some("fo"));
        assert_eq!(search, "src/fo");
        assert_eq!(insertion_for("src/foo/", &search), ("o/".into(), 0));
    }

    #[test]
    fn quoted_path_full_accept_does_not_double_the_directory() {
        let search = insertion_search_term("folder", "'src/fo", Some("fo"), "src/foo/");
        assert_eq!(search, "src/fo");
        assert_eq!(insertion_for("src/foo/", &search), ("o/".into(), 0));
        let search = insertion_search_term("folder", r#""src/fo"#, Some("fo"), "src/foo/");
        assert_eq!(search, "src/fo");
        assert_eq!(insertion_for("src/foo/", &search), ("o/".into(), 0));
    }

    #[test]
    fn ansi_c_quoted_acceptance_keeps_the_dollar_and_opening_quote() {
        let search = insertion_search_term("arg", "$'fo", None, "foo");
        assert_eq!(search, "fo");
        let (insertion, deletion) = insertion_for("foo", &search);
        assert_eq!((insertion, deletion), ("o".into(), 0));
        let Some((buffer, cursor)) = predicted_buffer_after_insert("$'fo", 4, "o", 0) else {
            panic!("expected a predictable ANSI-C insertion");
        };
        assert_eq!(buffer, "$'foo");
        assert_eq!(cursor, 5);
    }

    #[test]
    fn quoted_and_escaped_path_acceptance_preserves_typed_shell_syntax() {
        let cases = [
            (r"cat my\ d", r"my\ d", "my dir", r"cat my\ dir"),
            ("cat 'src/m", "'src/m", "src/main.rs", "cat 'src/main.rs"),
            (r#"cat "src/m"#, r#""src/m"#, "src/main.rs", r#"cat "src/main.rs"#),
            ("cat $'src/m", "$'src/m", "src/main.rs", "cat $'src/main.rs"),
        ];

        for (buffer, raw_search, name, expected) in cases {
            let insert_text = full_insertion_for_item(name, None, "file", None, false, false, false);
            let search = insertion_search_term("file", raw_search, None, &insert_text);
            let (insertion, deletion) = insertion_for_kind(&insert_text, &search, "file");
            let Some((predicted, cursor)) =
                predicted_buffer_after_insert(buffer, u32::try_from(buffer.len()).unwrap(), &insertion, deletion)
            else {
                panic!("expected a predictable insertion for {buffer:?}");
            };
            assert_eq!(predicted, expected, "raw search={raw_search:?}");
            assert_eq!(
                cursor,
                u32::try_from(expected.len()).unwrap(),
                "raw search={raw_search:?}"
            );
        }
    }

    #[test]
    fn closed_quote_acceptance_inserts_before_the_closer_for_enter_and_tab() {
        let cases = [
            ("git 'ch'", "'ch'", "git 'checkout'"),
            (r#"git "ch""#, r#""ch""#, r#"git "checkout""#),
            ("git $'ch'", "$'ch'", "git $'checkout'"),
        ];

        for (buffer, raw_search, expected) in cases {
            let insert_text = full_insertion_for_item("checkout", None, "subcommand", None, false, false, false);
            let search = insertion_search_term("subcommand", raw_search, None, &insert_text);
            let (insertion, deletion) = insertion_for_kind(&insert_text, &search, "subcommand");
            let (insertion, deletion, offset) =
                insertion_edit_for_raw_search(raw_search, insertion, deletion, true, false, false);
            assert_eq!(offset, Some(-1), "raw search={raw_search:?}");
            let Some((predicted, cursor)) = predicted_buffer_after_insert_with_offset(
                buffer,
                u32::try_from(buffer.len()).unwrap(),
                &insertion,
                deletion,
                offset.unwrap_or_default(),
            ) else {
                panic!("expected a predictable closed-quote insertion for {raw_search:?}");
            };
            assert_eq!(predicted, expected, "raw search={raw_search:?}");
            assert_eq!(
                cursor,
                u32::try_from(expected.len()).unwrap(),
                "raw search={raw_search:?}"
            );

            // Tab Partial uses the same raw-search edit, with only the shared
            // prefix as its insertion. Keep this as a separate calculation so
            // the regression covers both acceptance paths rather than only the
            // Enter/full-row path.
            let shared = "check";
            let tab_search = prefix_insertion_search(shared, raw_search, None);
            let (tab_insertion, tab_deletion) = insertion_for(shared, &tab_search);
            let (tab_insertion, tab_deletion, tab_offset) =
                insertion_edit_for_raw_search(raw_search, tab_insertion, tab_deletion, true, false, false);
            assert_eq!(tab_offset, Some(-1), "raw search={raw_search:?}");
            let Some((tab_predicted, tab_cursor)) = predicted_buffer_after_insert_with_offset(
                buffer,
                u32::try_from(buffer.len()).unwrap(),
                &tab_insertion,
                tab_deletion,
                tab_offset.unwrap_or_default(),
            ) else {
                panic!("expected a predictable closed-quote Tab insertion for {raw_search:?}");
            };
            assert_eq!(
                tab_predicted,
                expected.replace("checkout", "check"),
                "raw search={raw_search:?}"
            );
            assert_eq!(
                tab_cursor,
                u32::try_from(expected.replace("checkout", "check").len()).unwrap()
            );
        }
    }

    #[test]
    fn closed_quote_acceptance_keeps_automatic_suffixes_outside() {
        for raw_search in ["'ch'", r#""ch""#, "$'ch'"] {
            let spaced_text = full_insertion_for_item("checkout", None, "subcommand", None, true, true, false);
            let search = insertion_search_term("subcommand", raw_search, None, &spaced_text);
            let (insertion, deletion) = insertion_for_kind(&spaced_text, &search, "subcommand");
            let (insertion, deletion, offset) =
                insertion_edit_for_raw_search(raw_search, insertion, deletion, true, true, false);
            let buffer = format!("git {raw_search}");
            let expected = format!("git {} ", raw_search.replace("ch", "checkout"));
            let Some((predicted, cursor)) = predicted_buffer_after_insert_with_offset(
                &buffer,
                u32::try_from(buffer.len()).unwrap(),
                &insertion,
                deletion,
                offset.unwrap_or_default(),
            ) else {
                panic!("expected a predictable spaced insertion for {raw_search:?}");
            };
            assert_eq!(predicted, expected, "raw search={raw_search:?}");
            assert_eq!(
                cursor,
                u32::try_from(expected.len()).unwrap(),
                "raw search={raw_search:?}"
            );

            let executed_text = full_insertion_for_item("checkout", None, "subcommand", None, true, true, true);
            let search = insertion_search_term("subcommand", raw_search, None, &executed_text);
            let (insertion, deletion) = insertion_for_kind(&executed_text, &search, "subcommand");
            let (insertion, deletion, offset) =
                insertion_edit_for_raw_search(raw_search, insertion, deletion, true, false, true);
            let Some((predicted, cursor)) = predicted_buffer_after_insert_with_offset(
                &buffer,
                u32::try_from(buffer.len()).unwrap(),
                &insertion,
                deletion,
                offset.unwrap_or_default(),
            ) else {
                panic!("expected a predictable executed insertion for {raw_search:?}");
            };
            assert_eq!(
                predicted,
                format!("git {}\n", raw_search.replace("ch", "checkout")),
                "raw search={raw_search:?}"
            );
            assert_eq!(
                cursor,
                u32::try_from(predicted.len()).unwrap(),
                "raw search={raw_search:?}"
            );
        }
    }

    #[test]
    fn closed_quoted_file_acceptance_uses_quote_native_space_escaping() {
        for raw_search in ["'my f'", r#""my f""#, "$'my f'"] {
            let text = full_insertion_for_item("my file.txt", None, "file", None, false, false, false);
            assert_eq!(text, r"my\ file.txt");
            let search = insertion_search_term("file", raw_search, None, &text);
            let (insertion, deletion) = insertion_for_kind(&text, &search, "file");
            let (insertion, deletion, offset) =
                insertion_edit_for_raw_search(raw_search, insertion, deletion, true, false, false);
            let buffer = format!("cat {raw_search}");
            let expected = format!("cat {}", raw_search.replace("my f", "my file.txt"));
            let Some((predicted, cursor)) = predicted_buffer_after_insert_with_offset(
                &buffer,
                u32::try_from(buffer.len()).unwrap(),
                &insertion,
                deletion,
                offset.unwrap_or_default(),
            ) else {
                panic!("expected a predictable quoted file insertion for {raw_search:?}");
            };
            assert_eq!(predicted, expected, "raw search={raw_search:?}");
            assert_eq!(
                cursor,
                u32::try_from(expected.len()).unwrap(),
                "raw search={raw_search:?}"
            );
        }
    }

    #[test]
    fn closed_quoted_file_tab_prefix_uses_quote_native_space_escaping() {
        for raw_search in ["'my f'", r#""my f""#, "$'my f'"] {
            let shared = escape_tab_prefix("my file", "file");
            let search = prefix_insertion_search(&shared, raw_search, None);
            let (insertion, deletion) = insertion_for(&shared, &search);
            let (insertion, deletion, offset) =
                insertion_edit_for_raw_search(raw_search, insertion, deletion, true, false, false);
            let buffer = format!("cat {raw_search}");
            let expected = format!("cat {}", raw_search.replace("my f", "my file"));
            let Some((predicted, cursor)) = predicted_buffer_after_insert_with_offset(
                &buffer,
                u32::try_from(buffer.len()).unwrap(),
                &insertion,
                deletion,
                offset.unwrap_or_default(),
            ) else {
                panic!("expected a predictable quoted Tab insertion for {raw_search:?}");
            };
            assert_eq!(predicted, expected, "raw search={raw_search:?}");
            assert_eq!(
                cursor,
                u32::try_from(expected.len()).unwrap(),
                "raw search={raw_search:?}"
            );
        }
    }

    #[test]
    fn closed_quoted_plain_name_acceptance_uses_quote_native_space_escaping() {
        for raw_search in ["'my f'", r#""my f""#, "$'my f'"] {
            let text = full_insertion_for_item("my file", None, "arg", None, false, false, false);
            assert_eq!(text, r"my\ file");
            let search = insertion_search_term("arg", raw_search, None, &text);
            let (insertion, deletion) = insertion_for_kind(&text, &search, "arg");
            let (insertion, deletion, offset) =
                insertion_edit_for_kind(raw_search, insertion, deletion, "arg", true, false, false);
            let buffer = format!("echo {raw_search}");
            let expected = format!("echo {}", raw_search.replace("my f", "my file"));
            let Some((predicted, cursor)) = predicted_buffer_after_insert_with_offset(
                &buffer,
                u32::try_from(buffer.len()).unwrap(),
                &insertion,
                deletion,
                offset.unwrap_or_default(),
            ) else {
                panic!("expected a predictable quoted argument insertion for {raw_search:?}");
            };
            assert_eq!(predicted, expected, "raw search={raw_search:?}");
            assert_eq!(
                cursor,
                u32::try_from(expected.len()).unwrap(),
                "raw search={raw_search:?}"
            );
        }
    }

    #[test]
    fn closed_quote_replacement_counts_raw_escaped_characters() {
        let cases = [
            (r#"'my file'"#, "my-other", "git 'my-other'", 7),
            (r#""a\"b""#, "aXb", r#"git "aXb""#, 4),
            (r#"$'a\'b'"#, "aXb", r#"git $'aXb'"#, 4),
        ];
        for (raw_search, replacement, expected, raw_inner_units) in cases {
            let (insertion, deletion) = insertion_for(replacement, &unquote_shell_token(raw_search));
            assert!(deletion > 0, "the replacement should not be treated as a prefix");
            let (insertion, deletion, offset) =
                insertion_edit_for_raw_search(raw_search, insertion, deletion, true, false, false);
            assert_eq!(offset, Some(-1), "raw search={raw_search:?}");
            assert_eq!(deletion, raw_inner_units, "raw search={raw_search:?}");
            let buffer = format!("git {raw_search}");
            let Some((predicted, cursor)) = predicted_buffer_after_insert_with_offset(
                &buffer,
                u32::try_from(buffer.len()).unwrap(),
                &insertion,
                deletion,
                offset.unwrap_or_default(),
            ) else {
                panic!("expected a predictable replacement");
            };
            assert_eq!(predicted, expected, "raw search={raw_search:?}");
            assert_eq!(
                cursor,
                u32::try_from(predicted.len()).unwrap(),
                "raw search={raw_search:?}"
            );
        }
    }

    #[test]
    fn tab_prefix_search_keeps_query_term_when_it_prefixes_the_shared_name() {
        let search = prefix_insertion_search("fooba", "scope@foo", Some("foo"));
        assert_eq!(search, "foo");
        assert_eq!(insertion_for("fooba", &search), ("ba".into(), 0));
    }

    #[test]
    fn tab_prefix_search_keeps_typed_escaped_spaces() {
        let shared = escape_tab_prefix("my file.", "file");
        let search = prefix_insertion_search(&shared, r"my\ file", None);
        assert_eq!(search, r"my\ file");
        assert_eq!(insertion_for(&shared, &search), (".".into(), 0));
    }

    #[test]
    fn quoted_search_deletion_does_not_inject_escaped_spaces() {
        assert_eq!(insertion_for("my file.txt", "'my file"), ("my file.txt".into(), 8));
        assert_eq!(insertion_for("my file.txt", r#""my file"#), ("my file.txt".into(), 8));
        assert_eq!(insertion_for("my file.txt", r"my\ file"), ("my file.txt".into(), 8));
    }

    #[test]
    fn unquoted_inner_text_space_still_counts_the_escape() {
        assert_eq!(insertion_for("checkout", "git"), ("checkout".into(), 3));
        assert_eq!(deletion_term_for_search("my file"), r"my\ file");
    }

    #[test]
    fn shortcut_palette_deletes_question_mark_and_space() {
        let (insertion, deletion) =
            apply_shortcut_palette_deletion("? deploy", "deploy", "deploy".into(), 0, "deploy ");
        assert_eq!(insertion, "deploy ");
        assert_eq!(deletion, 8);
        let (insertion, deletion) =
            apply_shortcut_palette_deletion("? deploy", "deploy", "kubectl apply".into(), 6, "kubectl apply");
        assert_eq!(insertion, "kubectl apply");
        assert_eq!(deletion, 8);
        let (insertion, deletion) = apply_shortcut_palette_deletion("? ", "", "deploy ".into(), 0, "deploy ");
        assert_eq!(insertion, "deploy ");
        assert_eq!(deletion, 2);
        let (insertion, deletion) =
            apply_shortcut_palette_deletion("echo x && ? deploy", "deploy", "deploy".into(), 0, "deploy ");
        assert_eq!(insertion, "deploy ");
        assert_eq!(deletion, 8);
        let (insertion, deletion) =
            apply_shortcut_palette_deletion("FOO=1 ? deploy", "deploy", "kubectl apply".into(), 6, "kubectl apply");
        assert_eq!(insertion, "kubectl apply");
        assert_eq!(deletion, 8);
    }

    #[test]
    fn glued_shortcut_query_is_not_double_deleted() {
        let (insertion, deletion) = apply_shortcut_palette_deletion("?la", "?la", "last".into(), 3, "last");
        assert_eq!(insertion, "last");
        assert_eq!(deletion, 3);
        let (insertion, deletion) =
            apply_shortcut_palette_deletion("git checkout", "checkout", "checkout".into(), 0, "checkout ");
        assert_eq!(insertion, "checkout");
        assert_eq!(deletion, 0);
    }

    #[test]
    fn esc_then_type_keeps_suppress_until_shown() {
        let mut overlay = OverlayState::new();
        overlay.hide_until_shown();
        assert!(overlay.suppress_until_shown);
        assert!(!overlay.visible);
    }

    #[test]
    fn only_show_on_tab_hides_on_a_new_trailing_space_not_when_already_hidden() {
        assert!(only_show_on_tab_hides_until_shown(None, "git"));
        assert!(only_show_on_tab_hides_until_shown(Some(""), "git"));
        assert!(only_show_on_tab_hides_until_shown(Some("git"), "git "));
        assert!(!only_show_on_tab_hides_until_shown(Some("git "), "git  "));
        assert!(!only_show_on_tab_hides_until_shown(Some("git"), "git c"));
        assert!(!only_show_on_tab_hides_until_shown(Some("git commit "), "git commit"));
    }

    #[test]
    fn current_arg_identity_changes_when_the_name_changes() {
        assert!(current_arg_identity_changed("", "branch"));
        assert!(current_arg_identity_changed("command", "branch"));
        assert!(!current_arg_identity_changed("branch", "branch"));
        assert!(!current_arg_identity_changed("branch", ""));
    }

    #[test]
    fn cursor_marker_after_execute_newline_is_not_an_extra_immediate_return() {
        let text = full_insertion_for_item("co", Some("commit -m '{cursor}'"), "arg", None, false, true, true);
        assert!(text.contains("{cursor}"));
        assert!(text.contains('\n'));
        assert!(!immediate_for_insert(true, &text));
        let resolved = resolve_cursor_marker(text);
        assert!(!resolved.ends_with('\n'));
        assert!(resolved.contains('\n'));
        assert!(!immediate_for_insert(true, &resolved));
    }
}

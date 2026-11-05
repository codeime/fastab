use std::borrow::Cow;

use fastab_proto::local::caret_position_hook::Origin;
use tao::dpi::{LogicalSize, Position, Size};
use tao::event_loop::ControlFlow;
use tao::window::Theme;
use tokio::sync::mpsc::UnboundedSender;

use crate::platform::PlatformBoundEvent;
use crate::webview::WindowId;

#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum Event {
    WindowEvent {
        window_id: WindowId,
        window_event: WindowEvent,
    },
    WindowEventAll {
        window_event: WindowEvent,
    },

    PlatformBoundEvent(PlatformBoundEvent),
    ControlFlow(ControlFlow),
    SetTrayVisible(bool),

    ReloadCredentials,
    GpuiOverlayVisibleRows {
        generation: u64,
        revision: (u64, u64),
        range: std::ops::Range<usize>,
    },
    JevSettingsChanged,
    JevCredentialsLoaded {
        epoch: u64,
        credentials: crate::overlay::ai::LoadedCredentials,
    },
    JevDebounced(crate::overlay::ai::RequestToken),
    JevComplete {
        token: crate::overlay::ai::RequestToken,
        result: crate::overlay::ai::Completed,
    },
    JevContextChanged,
    ReloadAccessibility,
    /// Someone asked for an update check. Sparkle lives in the desktop app, so
    /// the Settings process passes this on.
    CheckForUpdates,
    ReloadTray {
        is_logged_in: bool,
    },

    /// Menu bar or tray item activated. Delivered by `muda::MenuEvent::set_event_handler`.
    MenuClicked(String),
    /// Fresh permission check for the native settings gate.
    PermissionSnapshot(crate::permissions::PermissionSnapshot),

    /// Read-only resource counters from the existing completion worker.
    EngineDiagnostics {
        reply: futures::channel::oneshot::Sender<anyhow::Result<fastab_engine::EngineClientDiagnostics>>,
    },

    /// Headless engine input for the GPUI overlay.
    GpuiOverlayInput(std::sync::Arc<crate::event_loop::QueuedOverlayInput>),
    /// The shell executed its input. Background sessions must not end the
    /// active terminal's request or hide its overlay.
    GpuiOverlayEndInput {
        session_id: uuid::Uuid,
    },
    /// The current completion request exceeded the loading threshold. This is
    /// routed through the host event queue so GPUI state is only touched from
    /// the top-level dispatcher.
    GpuiOverlayLoading {
        generation: u64,
    },
    /// The request outlived the user's script budget. Only the `···` marker is
    /// retired; the request itself keeps running under the engine watchdog.
    GpuiOverlayLoadingExpired {
        generation: u64,
    },
    /// An explicit Show/Tab could not resolve before its bounded deadline.
    GpuiOverlayShowExpired {
        token: u64,
    },
    /// A completion request finished on the engine thread.
    GpuiOverlayComplete {
        generation: u64,
        result: anyhow::Result<fastab_engine::CompleteResult>,
        session_id: uuid::Uuid,
        cwd: String,
    },
    /// Generators were delayed by Fig `debounce`. Re-run the same buffer once
    /// the delay elapses, unless a newer generation has already replaced it.
    GpuiOverlayDebouncedComplete {
        generation: u64,
        /// The debounce that just elapsed, already spent from the `···`
        /// threshold.
        waited_ms: u64,
    },
    /// A visible overlay failed to reach its native window. Retry on a later
    /// event-loop turn without waiting for another terminal input event.
    GpuiOverlayRelayoutRetry {
        generation: u64,
        token: u64,
        attempt: u8,
    },
    /// Figterm intercepted a key bound to an autocomplete action.
    AutocompleteAction {
        action: String,
        session_id: uuid::Uuid,
        /// The raw key fterm handed over, so it can still reach the shell.
        key: Option<String>,
    },
    /// A mouse click carries the exact row that was clicked. Keeping this in
    /// the event avoids a shared pending slot being overwritten by a second
    /// click before the host event loop handles the first one.
    AutocompleteClick {
        click: fastab_gpui::ClickInsert,
        session_id: uuid::Uuid,
        generation: u64,
    },

    ShowMessageNotification(ShowMessageNotification),
}

impl Event {
    /// Approximate owned Rust payload for ingress admission. Native objects,
    /// allocator overhead, shared caches and downstream work are not physical
    /// memory measurements and are deliberately not inferred from this count.
    pub(crate) fn ingress_payload_bytes(&self) -> usize {
        let heap = match self {
            Self::GpuiOverlayInput(input) => return input.payload_bytes(),
            Self::WindowEvent {
                window_id,
                window_event,
            } => cow_capacity(&window_id.0).saturating_add(window_event.payload_bytes()),
            Self::WindowEventAll { window_event } => window_event.payload_bytes(),
            Self::AutocompleteAction { action, key, .. } => action
                .capacity()
                .saturating_add(key.as_ref().map_or(0, String::capacity)),
            Self::MenuClicked(action) => action.capacity(),
            Self::GpuiOverlayComplete {
                result: Ok(result),
                cwd,
                ..
            } => {
                let rows = result.suggestions.iter().fold(
                    result
                        .suggestions
                        .capacity()
                        .saturating_mul(std::mem::size_of::<fastab_engine::Suggestion>()),
                    |bytes, row| {
                        let strings = [&row.name, &row.description, &row.kind, &row.args_hint]
                            .into_iter()
                            .map(|value| value.capacity())
                            .sum::<usize>();
                        let optional = [
                            &row.acceptance_scope,
                            &row.insert_value,
                            &row.display_name,
                            &row.primary_name,
                            &row.separator_to_add,
                            &row.icon,
                            &row.original_type,
                            &row.query_term,
                        ]
                        .into_iter()
                        .flatten()
                        .map(String::capacity)
                        .sum::<usize>();
                        let aliases = row
                            .alias_names
                            .capacity()
                            .saturating_mul(std::mem::size_of::<String>())
                            .saturating_add(row.alias_names.iter().map(String::capacity).sum::<usize>());
                        let public = row
                            .public_ai_candidate
                            .as_ref()
                            .map_or(0, |row| row.name.capacity().saturating_add(row.description.capacity()));
                        bytes
                            .saturating_add(strings)
                            .saturating_add(optional)
                            .saturating_add(aliases)
                            .saturating_add(public)
                    },
                );
                let context = result.public_ai_context.as_ref().map_or(0, |context| {
                    context
                        .command_path
                        .capacity()
                        .saturating_mul(std::mem::size_of::<String>())
                        .saturating_add(context.command_path.iter().map(String::capacity).sum::<usize>())
                        .saturating_add(context.token_prefix.capacity())
                });
                let arg = result
                    .current_arg
                    .as_ref()
                    .map_or(0, |arg| arg.name.capacity().saturating_add(arg.description.capacity()));
                rows.saturating_add(context)
                    .saturating_add(arg)
                    .saturating_add(cwd.capacity())
                    .saturating_add(result.search_term.capacity())
                    .saturating_add(result.match_term.capacity())
            },
            Self::GpuiOverlayComplete { cwd, .. } => cwd.capacity(),
            Self::AutocompleteClick { click, .. } => [
                &click.name,
                &click.description,
                &click.search,
                &click.kind,
                &click.args_hint,
            ]
            .into_iter()
            .map(|value| value.capacity())
            .sum::<usize>()
            .saturating_add(
                [
                    &click.insert_value,
                    &click.display_name,
                    &click.primary_name,
                    &click.separator_to_add,
                    &click.icon_identifier,
                    &click.original_type,
                    &click.query_term,
                    &click.acceptance_scope,
                ]
                .into_iter()
                .flatten()
                .map(String::capacity)
                .sum::<usize>(),
            ),
            Self::ShowMessageNotification(notification) => cow_capacity(&notification.title)
                .saturating_add(cow_capacity(&notification.body))
                .saturating_add(notification.parent.as_ref().map_or(0, |id| cow_capacity(&id.0))),
            _ => 0,
        };
        std::mem::size_of::<Self>().saturating_add(heap)
    }
}

// Admission needs the owned allocation's capacity and charges borrowed text
// zero bytes; a &str would erase both ownership and spare-capacity information.
#[allow(clippy::ptr_arg)]
fn cow_capacity(value: &Cow<'static, str>) -> usize {
    match value {
        Cow::Borrowed(_) => 0,
        Cow::Owned(value) => value.capacity(),
    }
}

impl From<PlatformBoundEvent> for Event {
    fn from(event: PlatformBoundEvent) -> Self {
        Self::PlatformBoundEvent(event)
    }
}

impl From<ShowMessageNotification> for Event {
    fn from(event: ShowMessageNotification) -> Self {
        Self::ShowMessageNotification(event)
    }
}

#[derive(Debug, Default)]
pub struct ShowMessageNotification {
    pub title: Cow<'static, str>,
    pub body: Cow<'static, str>,
    pub parent: Option<WindowId>,
    pub buttons: Option<rfd::MessageButtons>,
    pub buttons_result: Option<tokio::sync::mpsc::Sender<rfd::MessageDialogResult>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowPosition {
    Absolute(Position),
    Centered,
    RelativeToCaret {
        caret_position: Position,
        caret_size: Size,
        origin: Origin,
    },
}

/// The client a local caret hook was read from, when its sender reported it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaretSource {
    pub bundle_id: Option<String>,
    /// `CGWindowID` of the client's window.
    pub window_id: Option<u32>,
}

pub struct WindowGeometryResult {
    pub is_above: bool,
    pub is_clipped: bool,
}

#[derive(Debug, Clone)]
pub enum WindowEvent {
    /// Sets the window to be enabled or disabled
    ///
    /// This will cause events to be ignored other than [`WindowEvent::Hide`] and
    /// [`WindowEvent::SetEnabled(true)`]
    SetEnabled(bool),
    /// Sets the theme of the window (light, dark, or system if None)
    SetTheme(Option<Theme>),
    /// A local caret hook, kept distinct from native AX geometry until consumed.
    CaretPositionHook {
        position: WindowPosition,
        source: CaretSource,
    },
    #[cfg(target_os = "macos")]
    TerminalEnabled {
        app: macos_utils::window_server::ApplicationSpecifier,
        cache_identity: Option<(i32, u32)>,
        epoch: u64,
        enabled: bool,
        /// Sampled together with `enabled`.
        accessibility_missing: bool,
        /// The input method state could not be read, so `enabled` is a guess.
        sample_uncertain: bool,
    },
    #[cfg(target_os = "macos")]
    TerminalCaret {
        app: macos_utils::window_server::ApplicationSpecifier,
        cache_identity: Option<(i32, u32)>,
        epoch: u64,
        position: Option<WindowPosition>,
        /// The owning window was destroyed; a later caret must not resume its input.
        invalidate_input: bool,
    },
    UpdateWindowGeometry {
        position: Option<WindowPosition>,
        size: Option<LogicalSize<f64>>,
        anchor: Option<LogicalSize<f64>>,
        dry_run: bool,
        tx: Option<UnboundedSender<WindowGeometryResult>>,
    },
    /// Hides the window.
    Hide,
    /// Closes the window.
    Close,
    Show,
    NavigateRelative {
        path: Cow<'static, str>,
    },

    Event {
        event_name: Cow<'static, str>,
        payload: Option<Cow<'static, str>>,
    },

    Reload,

    Devtools,
    DebugMode(bool),

    Batch(Vec<WindowEvent>),
}

impl WindowEvent {
    fn payload_bytes(&self) -> usize {
        match self {
            Self::NavigateRelative { path } => cow_capacity(path),
            Self::CaretPositionHook { source, .. } => source.bundle_id.as_ref().map_or(0, String::capacity),
            Self::Event { event_name, payload } => {
                cow_capacity(event_name).saturating_add(payload.as_ref().map_or(0, cow_capacity))
            },
            Self::Batch(events) => events
                .capacity()
                .saturating_mul(std::mem::size_of::<Self>())
                .saturating_add(events.iter().map(Self::payload_bytes).sum::<usize>()),
            #[cfg(target_os = "macos")]
            Self::TerminalEnabled { app, .. } | Self::TerminalCaret { app, .. } => app.bundle_id.capacity(),
            _ => 0,
        }
    }

    pub fn is_allowed_while_disabled(&self) -> bool {
        matches!(
            self,
            WindowEvent::Hide | WindowEvent::Close | WindowEvent::SetEnabled(_)
        )
    }
}

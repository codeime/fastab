// This is needed for objc
#![allow(unexpected_cfgs)]
#![allow(deprecated)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use accessibility_sys::{
    AXError, AXIsProcessTrusted, AXUIElement, AXUIElementCreateSystemWide, AXUIElementSetMessagingTimeout, pid_t,
};
use anyhow::Context;
use cocoa::base::{YES, id};
use core_foundation::base::TCFType;
use core_graphics::display::CGRect;
use core_graphics::window::CGWindowID;
use fastab_integrations::input_method::InputMethod;
use fastab_proto::local::caret_position_hook::Origin;
use fastab_util::Terminal;
use macos_utils::accessibility::accessibility_is_enabled;
use macos_utils::caret_position::{CaretPosition, CaretQueryError, get_caret_position, get_terminal_caret_position};
use macos_utils::window_server::{AX_MESSAGING_TIMEOUT_SECONDS, ApplicationSpecifier, CGWindowLevelForKey, UIElement};
use macos_utils::{NotificationCenter, WindowServer, WindowServerEvent};
use objc::runtime::{BOOL, Class};
use objc::{msg_send, sel, sel_impl};
use objc2_foundation::{NSDictionary, NSOperationQueue, ns_string};
use serde::Serialize;
use tao::dpi::{LogicalPosition, LogicalSize, Position};
use tao::platform::macos::ActivationPolicy;
use tracing::{debug, error, trace, warn};

use super::{PlatformBoundEvent, PlatformWindow};
use crate::event::{Event, WindowEvent, WindowPosition};
use crate::utils::Rect;
use crate::webview::{FigIdMap, WindowId};
use crate::{AUTOCOMPLETE_ID, AUTOCOMPLETE_WINDOW_TITLE, EventLoopProxy};

pub const DEFAULT_CARET_WIDTH: f64 = 10.0;

pub(crate) fn prefers_ax_caret_for_app(app: &ApplicationSpecifier) -> bool {
    let Some(terminal @ Terminal::Otty) = Terminal::from_bundle_id(&app.bundle_id) else {
        return false;
    };
    terminal.prefers_macos_accessibility(macos_utils::applications::running_application_version(app.pid).as_deref())
}

fn should_refresh_x_term_cache(bundle_id: &str) -> bool {
    Terminal::from_bundle_id(bundle_id).is_some_and(|terminal| terminal.is_xterm())
}

/// IME-only terminals (Ghostty, Kitty, …) report the caret through IMK,
/// not AX, so an in-window focused-element change is noise we cannot follow
/// rather than a pane switch we should park the list for. No built-in terminal
/// is both IME and xterm; that guard is for a custom terminal declaring both,
/// where the AX pane switch is the real signal.
pub(crate) fn hide_overlay_on_element_change(bundle_id: &str) -> bool {
    !matches!(
        Terminal::from_bundle_id(bundle_id),
        Some(terminal) if terminal.supports_macos_input_method() && !terminal.is_xterm()
    )
}

fn window_focus_events(
    is_frontmost: bool,
    app: ApplicationSpecifier,
    make_window: impl FnOnce() -> Result<PlatformWindowImpl, AXError>,
) -> Vec<Event> {
    // Activation discovery is delayed; reject an old app before either AX work
    // or Hide can cancel the currently focused terminal's completion request.
    if !is_frontmost {
        return Vec::new();
    }
    vec![Event::PlatformBoundEvent(
        PlatformBoundEvent::ExternalWindowFocusChanged {
            app,
            window: make_window().ok(),
        },
    )]
}

const WINDOW_RECOVERY_BACKOFF: Duration = Duration::from_millis(750);

fn recovery_is_throttled(
    failure: Option<&(ApplicationSpecifier, Instant)>,
    app: &ApplicationSpecifier,
    now: Instant,
) -> bool {
    failure.is_some_and(|(failed_app, failed_at)| {
        failed_app == app && now.duration_since(*failed_at) < WINDOW_RECOVERY_BACKOFF
    })
}

// See for other window level keys
// https://github.com/phracker/MacOSX-SDKs/blob/master/MacOSX10.8.sdk/System/Library/Frameworks/CoreGraphics.framework/Versions/A/Headers/CGWindowLevel.h
#[allow(non_upper_case_globals)]
const kCGFloatingWindowLevelKey: i32 = 5;

static UNMANAGED: Unmanaged = Unmanaged {
    event_sender: RwLock::new(Option::<EventLoopProxy>::None),
    window_server: RwLock::new(Option::<Arc<Mutex<WindowServer>>>::None),
};

static ACCESSIBILITY_ENABLED: LazyLock<AtomicBool> = LazyLock::new(|| AtomicBool::new(accessibility_is_enabled()));

static MACOS_VERSION: LazyLock<semver::Version> = LazyLock::new(|| {
    let version = macos_utils::os::OperatingSystemVersion::get();
    semver::Version::new(version.major() as u64, version.minor() as u64, version.patch() as u64)
});

#[allow(dead_code)]
pub fn activate_app() {
    // LSUIElement/menu-bar apps can switch back from Accessory to Regular without
    // AppKit automatically bringing the process forward. Explicit activation keeps
    // dashboard opens from being treated like a wallpaper/desktop click on macOS.
    unsafe {
        let Some(application) = Class::get("NSApplication") else {
            return;
        };
        let app: id = msg_send![application, sharedApplication];
        let _: () = msg_send![app, activateIgnoringOtherApps: YES];
    }
}

#[allow(dead_code)]
pub fn is_ventura() -> bool {
    MACOS_VERSION.major >= 13
}

/// Check the Apple Event that launched the app for the Login Item marker.
///
/// `SMAppService.mainAppService` does not support custom arguments. Launch
/// Services instead marks its open-application event with
/// `keyAELaunchedAsLogInItem` (`'lgit'`).
pub fn launched_as_login_item() -> bool {
    const KEY_AE_LAUNCHED_AS_LOGIN_ITEM: u32 = u32::from_be_bytes(*b"lgit");

    unsafe {
        let Some(manager_class) = Class::get("NSAppleEventManager") else {
            return false;
        };
        let manager: id = msg_send![manager_class, sharedAppleEventManager];
        let event: id = msg_send![manager, currentAppleEvent];
        if event.is_null() {
            return false;
        }
        let attribute: id = msg_send![event, attributeDescriptorForKeyword: KEY_AE_LAUNCHED_AS_LOGIN_ITEM];
        !attribute.is_null()
    }
}

struct Unmanaged {
    event_sender: RwLock<Option<EventLoopProxy>>,
    window_server: RwLock<Option<Arc<Mutex<WindowServer>>>>,
}

/// Caps every AX request at 250 ms so a hung tracked application cannot hang us with it.
/// The system-wide element is created under the create rule and released on return.
fn set_global_ax_messaging_timeout() {
    unsafe {
        let system_wide = AXUIElement::wrap_under_create_rule(AXUIElementCreateSystemWide());
        AXUIElementSetMessagingTimeout(system_wide.as_concrete_TypeRef(), AX_MESSAGING_TIMEOUT_SECONDS);
    }
}

#[derive(Debug, Serialize)]
pub struct PlatformStateImpl {
    ax_caret: Mutex<AxCaretDiagnostics>,
    #[serde(skip)]
    proxy: EventLoopProxy,
    #[serde(skip)]
    focused_window: Mutex<Option<PlatformWindowImpl>>,
    #[serde(skip)]
    last_window_recovery_failure: Mutex<Option<(ApplicationSpecifier, Instant)>>,
    #[serde(skip)]
    last_ax_caret_failure: Mutex<Option<(ApplicationSpecifier, Instant)>>,
    #[serde(skip)]
    caret_epoch: AtomicU64,
    #[serde(skip)]
    enabled_epoch: AtomicU64,
    #[serde(skip)]
    caret_demand: AtomicBool,
}

/// Returned by the existing platform state dump without performing an AX query.
/// Values are bounded metadata; no AX text, descriptions, or geometry is retained.
#[derive(Debug, Serialize)]
struct AxCaretDiagnostics {
    position_refreshes: u64,
    last_route: &'static str,
    queries: u64,
    successes: u64,
    failures: u64,
    throttled: u64,
    missing_window: u64,
    discarded: u64,
    last_query: Option<AxCaretQueryDiagnostic>,
}

impl Default for AxCaretDiagnostics {
    fn default() -> Self {
        Self {
            position_refreshes: 0,
            last_route: "not_requested",
            queries: 0,
            successes: 0,
            failures: 0,
            throttled: 0,
            missing_window: 0,
            discarded: 0,
            last_query: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct AxCaretQueryDiagnostic {
    pid: pid_t,
    window_id: CGWindowID,
    epoch: u64,
    elapsed_us: u64,
    disposition: &'static str,
    failure_stage: Option<&'static str>,
    failure_reason: Option<&'static str>,
    ax_error: Option<AXError>,
}

impl AxCaretDiagnostics {
    fn record_query(
        &mut self,
        query: CaretQueryIdentity,
        elapsed: Duration,
        result: &Result<CaretPosition, CaretQueryError>,
        disposition: &'static str,
    ) {
        self.queries = self.queries.saturating_add(1);
        if result.is_ok() {
            self.successes = self.successes.saturating_add(1);
        } else {
            self.failures = self.failures.saturating_add(1);
        }
        if disposition != "emitted" {
            self.discarded = self.discarded.saturating_add(1);
        }
        let failure = result.as_ref().err();
        self.last_query = Some(AxCaretQueryDiagnostic {
            pid: query.pid,
            window_id: query.window_id,
            epoch: query.epoch,
            elapsed_us: elapsed.as_micros().min(u64::MAX as u128) as u64,
            disposition,
            failure_stage: failure.map(|error| error.stage),
            failure_reason: failure.map(|error| error.failure.as_str()),
            ax_error: failure.and_then(|error| error.ax_error),
        });
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PlatformWindowImpl {
    window_id: CGWindowID,
    ui_element: UIElement,
    x_term_tree_cache: Option<Vec<UIElement>>,
    x_term_last_failure: Option<Instant>,
    pub bundle_id: String,
    pub pid: pid_t,
}

impl From<CGRect> for Rect {
    fn from(cgr: CGRect) -> Rect {
        Rect {
            position: LogicalPosition::new(cgr.origin.x, cgr.origin.y).into(),
            size: LogicalSize::new(cgr.size.width, cgr.size.height).into(),
        }
    }
}

impl PlatformWindowImpl {
    pub(crate) fn is_current_focused_window(&self) -> Result<bool, AXError> {
        UIElement::application(self.pid)
            .focused_window()
            .map(|window| window == self.ui_element)
    }

    pub fn new(bundle_id: String, pid: pid_t, ui_element: UIElement) -> Result<Self, AXError> {
        let window_id = unsafe { ui_element.get_window_id()? };
        Ok(Self {
            window_id,
            ui_element,
            pid,
            x_term_tree_cache: None,
            x_term_last_failure: None,
            bundle_id,
        })
    }

    pub fn get_window_id(&self) -> CGWindowID {
        self.window_id
    }

    pub fn bundle_id(&self) -> &str {
        self.bundle_id.as_str()
    }

    pub fn get_bounds(&self) -> Option<CGRect> {
        let info = self.ui_element.window_info(false)?;
        Some(info.bounds)
    }

    pub fn get_level(&self) -> Option<i64> {
        // We grab all the windows since we don't want this to fail and are more fine if it's slow
        let info = self.ui_element.window_info(true)?;
        Some(info.level)
    }

    pub fn get_x_term_cursor_frame(&mut self) -> Option<CGRect> {
        if xterm_retry_is_throttled(self.x_term_last_failure, Instant::now()) {
            return None;
        }
        let deadline = Instant::now() + Duration::from_millis(250);
        let cached_leaf = self
            .x_term_tree_cache
            .as_ref()
            .and_then(|tree| tree.first())
            .filter(|leaf| leaf.is_xterm_helper_textarea_before(deadline).unwrap_or(false));
        let tree = cached_leaf
            .map(|leaf| vec![leaf.clone()])
            .or_else(|| self.ui_element.find_x_term_caret_tree_before(deadline).ok());
        let frame = tree
            .as_ref()
            .and_then(|tree| tree.first())
            .and_then(|leaf| leaf.frame_before(deadline).ok());
        if frame.is_some() {
            self.x_term_tree_cache = tree;
            self.x_term_last_failure = None;
        } else {
            self.x_term_tree_cache = None;
            self.x_term_last_failure = Some(Instant::now());
        }
        frame
    }

    fn apply_xterm_query(&mut self, query: CaretQueryIdentity, epoch: u64, result: Self) -> bool {
        if !caret_query_matches(query, Some((self.pid, self.window_id)), epoch) {
            return false;
        }
        self.x_term_tree_cache = result.x_term_tree_cache;
        self.x_term_last_failure = result.x_term_last_failure;
        true
    }

    /// The cached tree points at one specific terminal pane's caret element. Once focus moves it
    /// is stale, and reusing it would anchor the overlay to the pane the user just left.
    pub fn invalidate_x_term_cache(&mut self) {
        self.x_term_tree_cache = None;
        self.x_term_last_failure = None;
    }

    /// Apply the AX-derived cache decision after its queries run outside the
    /// focused-window mutex.
    fn apply_x_term_cache_update(&mut self, element: Option<UIElement>, update: XTermCacheUpdate) {
        match update {
            XTermCacheUpdate::Retarget => {
                self.x_term_tree_cache = element.map(|element| vec![element]);
                self.x_term_last_failure = None;
            },
            XTermCacheUpdate::Invalidate => self.invalidate_x_term_cache(),
            XTermCacheUpdate::Leave => {},
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CaretQueryIdentity {
    pid: i32,
    window_id: u32,
    epoch: u64,
}

fn caret_query_matches(query: CaretQueryIdentity, current: Option<(i32, u32)>, epoch: u64) -> bool {
    current == Some((query.pid, query.window_id)) && epoch == query.epoch
}

fn xterm_retry_is_throttled(failed_at: Option<Instant>, now: Instant) -> bool {
    failed_at.is_some_and(|failed| now.saturating_duration_since(failed) < WINDOW_RECOVERY_BACKOFF)
}

fn same_frame(a: CGRect, b: CGRect) -> bool {
    a.origin.x == b.origin.x
        && a.origin.y == b.origin.y
        && a.size.width == b.size.width
        && a.size.height == b.size.height
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XTermCacheUpdate {
    Retarget,
    Invalidate,
    Leave,
}

/// `same_window` is `None` when `_AXUIElementGetWindow` fails: drop the cache
/// rather than guess. A definite other window leaves the tracked pane alone.
fn x_term_cache_update_for_focused_element(
    is_xterm_helper_textarea: bool,
    same_window: Option<bool>,
) -> XTermCacheUpdate {
    match same_window {
        Some(false) => XTermCacheUpdate::Leave,
        Some(true) if is_xterm_helper_textarea => XTermCacheUpdate::Retarget,
        _ => XTermCacheUpdate::Invalidate,
    }
}

/// Pins the autocomplete overlay above the focused terminal.
///
/// `terminal_level` is the window level of the terminal the overlay is following. Handles iTerm
/// Quake mode by explicitly setting the window level, see
/// <https://github.com/gnachman/iTerm2/blob/1a5a09f02c62afcc70a647603245e98862e51911/sources/iTermProfileHotKey.m#L276-L310>
/// for more on window levels.
pub fn set_activation_policy(policy: ActivationPolicy) {
    let ns_policy: i64 = match policy {
        ActivationPolicy::Regular => 0,
        ActivationPolicy::Accessory => 1,
        ActivationPolicy::Prohibited => 2,
        _ => 1,
    };
    unsafe {
        let Some(application) = Class::get("NSApplication") else {
            return;
        };
        let app: id = msg_send![application, sharedApplication];
        let _: BOOL = msg_send![app, setActivationPolicy: ns_policy];
    }
}

fn apply_autocomplete_window_level(_window_map: &FigIdMap, terminal_level: Option<i64>) {
    let above = match terminal_level {
        None | Some(0) => unsafe { CGWindowLevelForKey(kCGFloatingWindowLevelKey) as i64 },
        Some(level) => level,
    };
    debug!("Setting overlay window level to {terminal_level:?}");
    fastab_gpui::set_overlay_window_level_for_title(AUTOCOMPLETE_WINDOW_TITLE, above);
}

impl PlatformStateImpl {
    pub(crate) fn pending_ax_events() -> usize {
        UNMANAGED
            .window_server
            .read()
            .unwrap()
            .as_ref()
            .map_or(0, |server| server.lock().unwrap().pending_events())
    }

    pub(super) fn new(proxy: EventLoopProxy) -> Self {
        let focused_window: Option<PlatformWindowImpl> = None;
        Self {
            ax_caret: Mutex::new(AxCaretDiagnostics::default()),
            proxy,
            focused_window: Mutex::new(focused_window),
            last_window_recovery_failure: Mutex::new(None),
            last_ax_caret_failure: Mutex::new(None),
            caret_epoch: AtomicU64::new(0),
            enabled_epoch: AtomicU64::new(0),
            caret_demand: AtomicBool::new(false),
        }
    }

    pub(super) fn handle(self: &Arc<Self>, event: PlatformBoundEvent, window_map: &FigIdMap) -> anyhow::Result<()> {
        match &event {
            PlatformBoundEvent::FocusedElementChanged { app, .. } => {
                // UIElement's Debug implementation asks AX for its role and frame. Keep
                // this event's log metadata-only until after the frontmost-app filter.
                debug!(pid = app.pid, bundle_id = %app.bundle_id, "Handling focused element event");
            },
            PlatformBoundEvent::ExternalWindowFocusChanged { app, .. } => {
                // PlatformWindowImpl derives Debug through UIElement, which would issue
                // AX requests before the stale-activation check below.
                debug!(pid = app.pid, bundle_id = %app.bundle_id, "Handling external window focus event");
            },
            _ => debug!("Handling platform event: {:?}", event),
        }
        match event {
            PlatformBoundEvent::Initialize => {
                if unsafe { AXIsProcessTrusted() } {
                    set_global_ax_messaging_timeout();
                }

                UNMANAGED.event_sender.write().unwrap().replace(self.proxy.clone());
                // Native AX/NSWorkspace callbacks cannot wait on the GPUI
                // consumer. This remains a separately diagnosed FIFO until
                // a source protocol can preserve every focus/input barrier;
                // bounded send would deadlock and try_send would lose focus.
                let (tx, rx) = flume::unbounded::<WindowServerEvent>();

                UNMANAGED
                    .window_server
                    .write()
                    .unwrap()
                    .replace(Arc::new(Mutex::new(WindowServer::new(tx))));

                let accessibility_proxy = self.proxy.clone();
                let mut distributed = NotificationCenter::distributed_center();
                let ax_notification_name = ns_string!("com.apple.accessibility.api");
                let queue = unsafe { NSOperationQueue::new() };
                distributed.subscribe(ax_notification_name, Some(&queue), move |_| {
                    accessibility_proxy
                        .clone()
                        .send_event(Event::PlatformBoundEvent(
                            PlatformBoundEvent::AccessibilityUpdateRequested,
                        ))
                        .ok();
                });

                let observer_proxy = self.proxy.clone();
                tokio::runtime::Handle::current().spawn(async move {
                    while let std::result::Result::Ok(result) = rx.recv_async().await {
                        let mut events: Vec<Event> = vec![];

                        match result {
                            WindowServerEvent::FocusChanged { window, app } => {
                                events.extend(window_focus_events(
                                    macos_utils::window_server::is_frontmost_application(&app),
                                    app.clone(),
                                    || PlatformWindowImpl::new(app.bundle_id, app.pid, window),
                                ));
                            },
                            WindowServerEvent::FocusedElementChanged { element, app } => {
                                events.push(Event::PlatformBoundEvent(PlatformBoundEvent::FocusedElementChanged {
                                    element,
                                    app,
                                }));
                            },
                            WindowServerEvent::WindowDestroyed { app } => {
                                events.push(Event::PlatformBoundEvent(PlatformBoundEvent::WindowDestroyed { app }));
                            },
                            WindowServerEvent::ActiveSpaceChanged { .. } => {
                                events.push(Event::WindowEvent {
                                    window_id: AUTOCOMPLETE_ID.clone(),
                                    window_event: WindowEvent::Hide,
                                });
                            },
                            WindowServerEvent::RequestCaretPositionUpdate { app, window_id } => {
                                events.push(Event::PlatformBoundEvent(
                                    PlatformBoundEvent::CaretPositionUpdateRequested { app, window_id },
                                ));
                            },
                        };

                        for event in events {
                            if let Err(e) = observer_proxy.send_event(event) {
                                warn!("Error sending event: {e:?}");
                            }
                        }
                    }
                });

                Ok(())
            },
            PlatformBoundEvent::InitializePostRun => {
                // GPUI registers the macOS app delegate and owns reopen
                // dispatch. The callback is installed in `gpui_host` once
                // the desktop event proxy exists.
                Ok(())
            },
            PlatformBoundEvent::EditBufferChanged => {
                if let Err(err) = self.refresh_window_position() {
                    error!(%err, "Failed to refresh window position");
                }
                Ok(())
            },
            PlatformBoundEvent::ExternalWindowFocusChanged { app, window } => {
                if !macos_utils::window_server::is_frontmost_application(&app) {
                    trace!(pid = app.pid, bundle_id = %app.bundle_id, "Ignoring stale external app activation");
                    return Ok(());
                }
                // Invalidate cached identity and its epoch under the same lock.
                {
                    let mut focused = self.focused_window.lock().unwrap();
                    self.caret_epoch.fetch_add(1, Ordering::Relaxed);
                    focused.take();
                }

                // A same-process window switch whose AX lookup failed must not
                // leave the previous window eligible for the app-level fast path.
                self.last_window_recovery_failure.lock().unwrap().take();
                self.last_ax_caret_failure.lock().unwrap().take();
                let Some(window) = window else {
                    *self.last_window_recovery_failure.lock().unwrap() = Some((app, Instant::now()));
                    return Ok(());
                };
                let level = window.get_level();
                if !macos_utils::window_server::is_frontmost_application(&app) {
                    return Ok(());
                }

                if level == Some(0) {
                    let mut focused = self.focused_window.lock().unwrap();
                    focused.replace(window);
                    drop(focused);
                    self.refresh_autocomplete_enabled(&app.bundle_id);
                } else {
                    *self.last_window_recovery_failure.lock().unwrap() = Some((app, Instant::now()));
                }

                apply_autocomplete_window_level(window_map, level);

                Ok(())
            },
            PlatformBoundEvent::AutocompleteWindowLevelUpdateRequested => {
                let window = self.focused_window.lock().unwrap().clone();
                let level = window.as_ref().and_then(|window| window.get_level());
                apply_autocomplete_window_level(window_map, level);

                Ok(())
            },
            PlatformBoundEvent::CaretPositionUpdateRequested { app, window_id } => {
                if !macos_utils::window_server::is_frontmost_application(&app) {
                    return Ok(());
                }
                {
                    let mut focused = self.focused_window.lock().unwrap();
                    let Some(window) = focused.as_mut().filter(|window| {
                        window.pid == app.pid && window.bundle_id == app.bundle_id && window.window_id == window_id
                    }) else {
                        return Ok(());
                    };
                    window.invalidate_x_term_cache();
                    self.caret_epoch.fetch_add(1, Ordering::Relaxed);
                }
                self.last_ax_caret_failure.lock().unwrap().take();
                // Movement invalidates older geometry, not the completion list.
                // The refresh emits None only if the new caret cannot be found.
                if let Err(e) = self.refresh_window_position() {
                    debug!(%e, "Failed to refresh window position");
                }
                Ok(())
            },
            PlatformBoundEvent::AccessibilityUpdateRequested => {
                let proxy = self.proxy.clone();
                tokio::runtime::Handle::current().spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let enabled = accessibility_is_enabled();
                    proxy
                        .send_event(Event::PlatformBoundEvent(PlatformBoundEvent::AccessibilityUpdated {
                            enabled,
                        }))
                        .ok();
                    if enabled {
                        set_global_ax_messaging_timeout();
                    }
                });

                Ok(())
            },
            PlatformBoundEvent::AccessibilityUpdated { enabled } => {
                let _was_enabled = ACCESSIBILITY_ENABLED.swap(enabled, Ordering::SeqCst);

                // Recorded here as well as at launch: a user who grants the permission and never
                // restarts before reinstalling would otherwise look like one who never granted it,
                // and the reinstall that invalidates the grant would pass unnoticed.
                if enabled {
                    crate::install::record_accessibility_grant();
                }
                // if enabled && !was_enabled {
                //     tokio::runtime::Handle::current().spawn(async move {
                //         fastab_telemetry::emit_track(fastab_telemetry::TrackEvent::new(
                //             fastab_telemetry::TrackEventType::GrantedAXPermission,
                //             fastab_telemetry::TrackSource::Desktop,
                //             env!("CARGO_PKG_VERSION").into(),
                //             std::iter::empty::<(&str, &str)>(),
                //         ))
                //         .await
                //         .ok();
                //     });
                // }

                self.proxy.send_event(Event::ReloadAccessibility).ok();

                Ok(())
            },
            PlatformBoundEvent::FocusedElementChanged { element, app } => {
                if prefers_ax_caret_for_app(&app) {
                    // The host clears the position synchronously when accepting
                    // this event. Invalidate queued results from another pane or
                    // the terminal before a Find/command-palette field took focus.
                    if macos_utils::window_server::is_frontmost_application(&app) {
                        let _focused = self.focused_window.lock().unwrap();
                        self.caret_epoch.fetch_add(1, Ordering::Relaxed);
                        self.last_ax_caret_failure.lock().unwrap().take();
                    }
                    return Ok(());
                }
                if !macos_utils::window_server::is_frontmost_application(&app) {
                    trace!(pid = app.pid, bundle_id = %app.bundle_id, "Ignoring stale focused-element event");
                    return Ok(());
                }

                // Snapshot identity under the lock, then make potentially blocking AX
                // requests after releasing it.
                let tracked_window = {
                    let focused = self.focused_window.lock().unwrap();
                    focused.as_ref().and_then(|focused_window| {
                        (focused_window.pid == app.pid && focused_window.bundle_id() == app.bundle_id).then_some((
                            focused_window.window_id,
                            should_refresh_x_term_cache(&app.bundle_id),
                            self.caret_epoch(),
                        ))
                    })
                };
                let Some((window_id, is_xterm, epoch)) = tracked_window else {
                    return Ok(());
                };

                // The overlay is anchored to one pane. VS Code / Cursor /
                // Windsurf: a focused helper textarea is that pane's caret —
                // keep / retarget the cache instead of a ~60 ms window walk
                // on the next key. Anything else in that window still drops
                // it. IME and other AX terminals never use this cache, so
                // skip the extra AX queries and just clear it.
                let cache_update = if is_xterm {
                    let deadline = Instant::now() + Duration::from_millis(250);
                    let same_window = element
                        .window_id_before(deadline)
                        .ok()
                        .map(|element_window| element_window == window_id);
                    let is_helper = matches!(same_window, Some(true))
                        && element.is_xterm_helper_textarea_before(deadline).unwrap_or(false);
                    x_term_cache_update_for_focused_element(is_helper, same_window)
                } else {
                    XTermCacheUpdate::Invalidate
                };
                let cache_element = (cache_update == XTermCacheUpdate::Retarget).then(|| element.clone());

                // Commit cache state and advance the pane epoch together.
                let query = {
                    let mut focused = self.focused_window.lock().unwrap();
                    let Some(window) = focused.as_mut() else {
                        return Ok(());
                    };
                    if window.pid != app.pid
                        || window.bundle_id != app.bundle_id
                        || window.window_id != window_id
                        || self.caret_epoch() != epoch
                        || !macos_utils::window_server::is_frontmost_application(&app)
                    {
                        return Ok(());
                    }
                    if cache_update == XTermCacheUpdate::Leave {
                        return Ok(());
                    }
                    let epoch = self.caret_epoch.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                    window.apply_x_term_cache_update(cache_element, cache_update);
                    CaretQueryIdentity {
                        pid: window.pid,
                        window_id: window.window_id,
                        epoch,
                    }
                };
                if hide_overlay_on_element_change(&app.bundle_id) {
                    self.send_terminal_caret_for_query(app, query, None);
                }

                Ok(())
            },
            PlatformBoundEvent::WindowDestroyed { app } => {
                let tracked_window_id = self.focused_window.lock().unwrap().as_ref().and_then(|window| {
                    (window.pid == app.pid && window.bundle_id == app.bundle_id).then_some(window.window_id)
                });
                let Some(tracked_window_id) = tracked_window_id else {
                    return Ok(());
                };
                // The event has no window ID and may predate a newly focused
                // window. Recheck absence (with the normal AX timeout) before
                // clearing, even if this app has since moved to the background.
                if !matches!(
                    UIElement::application(app.pid).focused_window(),
                    Err(accessibility_sys::kAXErrorNoValue)
                ) {
                    return Ok(());
                }
                let is_frontmost = macos_utils::window_server::is_frontmost_application(&app);
                let mut focused = self.focused_window.lock().unwrap();
                let mut cleared_epoch = None;
                if let Some(focused_window) = focused.as_ref() {
                    if focused_window.pid == app.pid
                        && focused_window.bundle_id() == app.bundle_id
                        && focused_window.window_id == tracked_window_id
                    {
                        focused.take();
                        cleared_epoch = Some(self.caret_epoch.fetch_add(1, Ordering::Relaxed).wrapping_add(1));
                    }
                }
                drop(focused);
                if let Some(epoch) = cleared_epoch.filter(|_| is_frontmost) {
                    // Capture the invalidated identity under the same lock as
                    // the clear; a later focus must not stamp this old event
                    // with its new window/epoch and revive the closed input.
                    self.send_terminal_caret_snapshot(app, None, epoch, None, true);
                }
                Ok(())
            },
        }
    }

    fn refresh_autocomplete_enabled(&self, bundle_id: &str) {
        let cache_identity = self.caret_cache_identity();
        let Some((pid, _)) = cache_identity else {
            return;
        };
        let app = ApplicationSpecifier {
            pid,
            bundle_id: bundle_id.to_owned(),
        };
        let epoch = self.enabled_epoch.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let proxy = self.proxy.clone();
        // IME state reads acquire SQLite connections and can wait for seconds.
        // Keep them off the UI thread; bind the reply to this accepted focus.
        tokio::task::spawn_blocking(move || {
            if !macos_utils::window_server::is_frontmost_application(&app) {
                return;
            }
            let terminal = Terminal::from_bundle_id(&app.bundle_id);
            let accessibility_missing = !accessibility_is_enabled();
            let mut sample_uncertain = false;
            let enabled = terminal.as_ref().is_some_and(|terminal| {
                !fastab_settings::settings::get_bool_or(
                    format!("integrations.{}.disabled", terminal.internal_id()),
                    false,
                ) && (prefers_ax_caret_for_app(&app)
                    || !terminal.supports_macos_input_method()
                    || match InputMethod::default().read_is_enabled() {
                        Ok(enabled) => enabled.unwrap_or(false),
                        Err(err) => {
                            warn!(%err, "Failed to read the input method state");
                            sample_uncertain = true;
                            false
                        },
                    })
            }) && !fastab_settings::settings::get_bool_or("autocomplete.disable", false)
                && !accessibility_missing;
            proxy
                .send_event(Event::WindowEvent {
                    window_id: AUTOCOMPLETE_ID,
                    window_event: WindowEvent::TerminalEnabled {
                        app,
                        cache_identity,
                        epoch,
                        enabled,
                        accessibility_missing,
                        sample_uncertain,
                    },
                })
                .ok();
        });
    }

    fn recover_focused_terminal_window(&self) -> bool {
        let Some(app) = macos_utils::window_server::frontmost_application() else {
            return false;
        };
        // Hooks can arrive after focus moved away from a terminal. This is a
        // normal skip, not an AX failure or a reason to query the background app.
        if Terminal::from_bundle_id(&app.bundle_id).is_none() {
            return false;
        }
        let already_tracked = self
            .focused_window
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|window| window.pid == app.pid && window.bundle_id == app.bundle_id);
        if already_tracked {
            return true;
        }
        if recovery_is_throttled(
            self.last_window_recovery_failure.lock().unwrap().as_ref(),
            &app,
            Instant::now(),
        ) {
            return false;
        }

        // Only cache loss/app mismatch queries AX. Back off after a failure so
        // an unresponsive terminal cannot consume a 250 ms AX timeout per key.
        let recovered = (|| -> anyhow::Result<PlatformWindowImpl> {
            let element = UIElement::application(app.pid)
                .focused_window()
                .map_err(|err| anyhow::anyhow!("Failed to recover focused terminal window: AX error {err}"))?;
            let window = PlatformWindowImpl::new(app.bundle_id.clone(), app.pid, element)
                .map_err(|err| anyhow::anyhow!("Failed to identify focused terminal window: AX error {err}"))?;
            anyhow::ensure!(
                window.get_level() == Some(0),
                "Focused terminal window is not at the normal level"
            );
            anyhow::ensure!(
                macos_utils::window_server::is_frontmost_application(&app),
                "Frontmost terminal changed during window recovery"
            );
            Ok(window)
        })();
        match recovered {
            Ok(window) => {
                self.last_window_recovery_failure.lock().unwrap().take();
                {
                    let mut focused = self.focused_window.lock().unwrap();
                    self.caret_epoch.fetch_add(1, Ordering::Relaxed);
                    focused.replace(window);
                }
                self.refresh_autocomplete_enabled(&app.bundle_id);
                debug!(pid = app.pid, bundle_id = %app.bundle_id, "Recovered focused terminal window");
                true
            },
            Err(err) => {
                debug!(%err, pid = app.pid, bundle_id = %app.bundle_id, "Backing off focused terminal window recovery");
                *self.last_window_recovery_failure.lock().unwrap() = Some((app, Instant::now()));
                false
            },
        }
    }

    pub(crate) fn caret_cache_identity(&self) -> Option<(i32, u32)> {
        self.focused_window
            .lock()
            .unwrap()
            .as_ref()
            .map(|window| (window.pid, window.window_id))
    }

    /// Whether a local caret read from window `window_id` of `app` may be
    /// placed. Only a tracked window that AX still confirms as focused can
    /// reject another one, and only when `app` owns the reported window: a
    /// number that names no window of this app is no evidence, so it can never
    /// hide the list for good. Without that, the sender's report is all there is.
    pub(crate) fn local_caret_window_is_focused(&self, app: &ApplicationSpecifier, window_id: CGWindowID) -> bool {
        let Some(tracked) = self
            .focused_window
            .lock()
            .unwrap()
            .as_ref()
            .filter(|window| {
                window.pid == app.pid && window.bundle_id == app.bundle_id && window.window_id != window_id
            })
            .cloned()
        else {
            return true;
        };
        if macos_utils::window_server::window_owner_pid(window_id) != Some(app.pid) {
            return true;
        }
        // Do not hold the cache mutex across AX; this runs on the main thread.
        let deadline = Instant::now() + Duration::from_millis(100);
        let tracked_is_focused = UIElement::application(tracked.pid)
            .focused_window_before(deadline)
            .is_ok_and(|focused| focused == tracked.ui_element);
        if tracked_is_focused {
            debug!(
                reported = window_id,
                tracked = tracked.window_id,
                "Dropping a local caret from a window that is not focused"
            );
        }
        !tracked_is_focused
    }

    pub(crate) fn cached_window_is_current(&self, app: &ApplicationSpecifier) -> bool {
        let cached = self
            .focused_window
            .lock()
            .unwrap()
            .as_ref()
            .filter(|window| window.pid == app.pid && window.bundle_id == app.bundle_id)
            .cloned();
        // A failed old discovery need not invalidate a newer cache that AX can
        // positively identify now. Do not hold the cache mutex across AX.
        cached.is_some_and(|window| window.is_current_focused_window() == Ok(true))
    }

    /// VS Code / Cursor / Windsurf re-post focus for the helper textarea that
    /// already has it, e.g. when the overlay window appears. That is not a pane
    /// switch. The tracked caret element, or a helper textarea at its frame in
    /// the same window while the tracked one still holds focus, keeps the pane
    /// and its completion session. Two terminal tabs can put their textareas at
    /// the same frame; only the tab the user left loses focus.
    pub(crate) fn focused_element_keeps_x_term_pane(&self, app: &ApplicationSpecifier, element: &UIElement) -> bool {
        if !should_refresh_x_term_cache(&app.bundle_id) {
            return false;
        }
        let tracked = self.focused_window.lock().unwrap().as_ref().and_then(|window| {
            (window.pid == app.pid && window.bundle_id == app.bundle_id)
                .then(|| Some((window.window_id, window.x_term_tree_cache.as_ref()?.first()?.clone())))
                .flatten()
        });
        let Some((window_id, leaf)) = tracked else {
            return false;
        };
        if leaf == *element {
            return true;
        }
        // Frames first: a real pane switch fails here, before repeating the
        // window and helper queries the platform handler is about to make.
        let deadline = Instant::now() + Duration::from_millis(250);
        matches!(
            (element.frame_before(deadline), leaf.frame_before(deadline)),
            (Ok(focused), Ok(tracked)) if same_frame(focused, tracked)
        ) && element.window_id_before(deadline).is_ok_and(|id| id == window_id)
            && element.is_xterm_helper_textarea_before(deadline).unwrap_or(false)
            && leaf.is_xterm_helper_textarea_before(deadline).unwrap_or(false)
    }

    pub(crate) fn caret_epoch(&self) -> u64 {
        self.caret_epoch.load(Ordering::Relaxed)
    }

    pub(crate) fn enabled_epoch(&self) -> u64 {
        self.enabled_epoch.load(Ordering::Relaxed)
    }

    fn send_terminal_caret(&self, app: ApplicationSpecifier, position: Option<WindowPosition>) {
        let (identity, epoch) = {
            let focused = self.focused_window.lock().unwrap();
            (
                focused.as_ref().map(|window| (window.pid, window.window_id)),
                self.caret_epoch(),
            )
        };
        self.send_terminal_caret_snapshot(app, identity, epoch, position, false);
    }

    fn send_terminal_caret_snapshot(
        &self,
        app: ApplicationSpecifier,
        cache_identity: Option<(i32, u32)>,
        epoch: u64,
        position: Option<WindowPosition>,
        invalidate_input: bool,
    ) {
        self.proxy
            .send_event(Event::WindowEvent {
                window_id: AUTOCOMPLETE_ID,
                window_event: WindowEvent::TerminalCaret {
                    app,
                    cache_identity,
                    epoch,
                    position,
                    invalidate_input,
                },
            })
            .ok();
    }

    fn send_terminal_caret_for_query(
        &self,
        app: ApplicationSpecifier,
        query: CaretQueryIdentity,
        position: Option<WindowPosition>,
    ) {
        self.send_terminal_caret_snapshot(app, Some((query.pid, query.window_id)), query.epoch, position, false);
    }

    fn refresh_ax_terminal_caret(&self, app: ApplicationSpecifier) {
        let query = {
            let focused = self.focused_window.lock().unwrap();
            focused
                .as_ref()
                .filter(|window| window.pid == app.pid && window.bundle_id == app.bundle_id)
                .map(|window| CaretQueryIdentity {
                    pid: window.pid,
                    window_id: window.window_id,
                    epoch: self.caret_epoch(),
                })
        };
        let Some(query) = query else {
            let mut diagnostics = self.ax_caret.lock().unwrap();
            diagnostics.missing_window = diagnostics.missing_window.saturating_add(1);
            drop(diagnostics);
            self.send_terminal_caret(app, None);
            return;
        };
        if recovery_is_throttled(
            self.last_ax_caret_failure.lock().unwrap().as_ref(),
            &app,
            Instant::now(),
        ) {
            let mut diagnostics = self.ax_caret.lock().unwrap();
            diagnostics.throttled = diagnostics.throttled.saturating_add(1);
            drop(diagnostics);
            self.send_terminal_caret_for_query(app, query, None);
            return;
        }
        let started = Instant::now();
        let caret = unsafe { get_terminal_caret_position(query.pid, query.window_id) };
        let elapsed = started.elapsed();
        if !macos_utils::window_server::is_frontmost_application(&app) {
            self.ax_caret
                .lock()
                .unwrap()
                .record_query(query, elapsed, &caret, "stale_application");
            return;
        }
        let (identity, epoch, position) = {
            let mut focused = self.focused_window.lock().unwrap();
            let identity = focused.as_ref().map(|window| (window.pid, window.window_id));
            if !caret_query_matches(query, identity, self.caret_epoch()) {
                self.ax_caret
                    .lock()
                    .unwrap()
                    .record_query(query, elapsed, &caret, "stale_window");
                return;
            }
            if let Ok(caret) = &caret {
                self.last_ax_caret_failure.lock().unwrap().take();
                (
                    identity,
                    query.epoch,
                    Some(WindowPosition::RelativeToCaret {
                        caret_position: LogicalPosition::new(caret.x, caret.y).into(),
                        caret_size: LogicalSize::new(DEFAULT_CARET_WIDTH, caret.height).into(),
                        origin: Origin::TopLeft,
                    }),
                )
            } else {
                *self.last_ax_caret_failure.lock().unwrap() = Some((app.clone(), Instant::now()));
                focused.take();
                let epoch = self.caret_epoch.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                (None, epoch, None)
            }
        };
        self.ax_caret
            .lock()
            .unwrap()
            .record_query(query, elapsed, &caret, "emitted");
        self.send_terminal_caret_snapshot(app, identity, epoch, position, false);
    }

    pub(crate) fn terminal_completion_disabled(&self) -> bool {
        self.focused_window
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|window| Terminal::from_bundle_id(&window.bundle_id))
            .is_some_and(|terminal| {
                fastab_settings::settings::get_bool_or(
                    format!("integrations.{}.disabled", terminal.internal_id()),
                    false,
                )
            })
    }

    pub(crate) fn set_caret_demand(&self, demand: bool) {
        self.caret_demand.store(demand, Ordering::Relaxed);
    }

    fn refresh_window_position(&self) -> anyhow::Result<()> {
        if !self.caret_demand.load(Ordering::Relaxed)
            || fastab_settings::settings::get_bool_or("autocomplete.disable", false)
        {
            return Ok(());
        }
        // Checking the frontmost app does not walk its AX tree. Keep this
        // ahead of recovery and of both the AX and IME query routes.
        let frontmost = macos_utils::window_server::frontmost_application();
        if frontmost
            .as_ref()
            .and_then(|app| Terminal::from_bundle_id(&app.bundle_id))
            .is_none_or(|terminal| {
                fastab_settings::settings::get_bool_or(
                    format!("integrations.{}.disabled", terminal.internal_id()),
                    false,
                )
            })
        {
            return Ok(());
        }
        let ax_app = frontmost.filter(prefers_ax_caret_for_app);
        {
            let mut diagnostics = self.ax_caret.lock().unwrap();
            diagnostics.position_refreshes = diagnostics.position_refreshes.saturating_add(1);
            diagnostics.last_route = if ax_app.is_some() { "otty_ax" } else { "unresolved" };
        }
        if !self.recover_focused_terminal_window() {
            if let Some(app) = ax_app {
                let mut diagnostics = self.ax_caret.lock().unwrap();
                diagnostics.missing_window = diagnostics.missing_window.saturating_add(1);
                drop(diagnostics);
                self.send_terminal_caret(app, None);
            }
            return Ok(());
        }
        if let Some(app) = ax_app {
            self.refresh_ax_terminal_caret(app);
            return Ok(());
        }
        let (mut active_window, query) = {
            let focused = self.focused_window.lock().unwrap();
            let window = focused.as_ref().context("No active window")?;
            (
                window.clone(),
                CaretQueryIdentity {
                    pid: window.pid,
                    window_id: window.window_id,
                    epoch: self.caret_epoch(),
                },
            )
        };
        let current_terminal = Terminal::from_bundle_id(active_window.bundle_id());

        let supports_ime = current_terminal
            .clone()
            .is_some_and(|t| t.supports_macos_input_method());

        let is_xterm = current_terminal.is_some_and(|t| t.is_xterm());
        self.ax_caret.lock().unwrap().last_route = if is_xterm {
            "xterm_ax"
        } else if supports_ime {
            "ime"
        } else {
            "ax"
        };

        if is_xterm {
            let app = ApplicationSpecifier {
                pid: query.pid,
                bundle_id: active_window.bundle_id.clone(),
            };
            let frame = active_window.get_x_term_cursor_frame();
            if !macos_utils::window_server::is_frontmost_application(&app) {
                return Ok(());
            }
            {
                let mut focused = self.focused_window.lock().unwrap();
                let identity = focused.as_ref().map(|window| (window.pid, window.window_id));
                if !caret_query_matches(query, identity, self.caret_epoch()) {
                    return Ok(());
                }
                let window = focused.as_mut().expect("matching window");
                if !window.apply_xterm_query(query, self.caret_epoch(), active_window) {
                    return Ok(());
                }
            }
            self.send_terminal_caret_for_query(
                app,
                query,
                frame.map(|frame| WindowPosition::RelativeToCaret {
                    caret_position: LogicalPosition::new(frame.origin.x, frame.origin.y).into(),
                    caret_size: LogicalSize::new(frame.size.width, frame.size.height).into(),
                    origin: Origin::TopLeft,
                }),
            );
            return Ok(());
        }

        if !is_xterm && supports_ime {
            tracing::debug!("Sending notif {}", fastab_util::macos::EDIT_BUFFER_UPDATED_NOTIFICATION);
            NotificationCenter::distributed_center().post_notification(
                // Literal must stay equal to `fastab_util::macos::EDIT_BUFFER_UPDATED_NOTIFICATION`.
                // `ns_string!` needs a compile-time literal. Do not post the Amazon Q /
                // Easy Complete caret name — a sibling IME must not hear this.
                ns_string!("app.fastab.edit_buffer_updated"),
                &NSDictionary::new(),
            );
        } else {
            let caret = self.get_cursor_position();

            let caret = caret.context("Failed to get cursor position")?;
            debug!("Sending caret update {:?}", caret);

            UNMANAGED
                .event_sender
                .read()
                .unwrap()
                .clone()
                .unwrap()
                .send_event(Event::WindowEvent {
                    window_id: AUTOCOMPLETE_ID,
                    window_event: WindowEvent::UpdateWindowGeometry {
                        position: Some(WindowPosition::RelativeToCaret {
                            caret_position: caret.position,
                            caret_size: caret.size,
                            origin: Origin::TopLeft,
                        }),
                        size: None,
                        anchor: None,
                        tx: None,
                        dry_run: false,
                    },
                })
                .ok();
        }

        Ok(())
    }

    #[allow(clippy::unused_self)]
    pub(super) fn position_window(&self, _window_id: &WindowId, _position: Position) -> anyhow::Result<()> {
        Ok(())
    }

    #[allow(clippy::unused_self)]
    pub(super) fn get_cursor_position(&self) -> Option<Rect> {
        let caret: CaretPosition = unsafe { get_caret_position(true) };

        if caret.valid {
            Some(Rect {
                position: LogicalPosition::new(caret.x, caret.y).into(),
                size: LogicalSize::new(DEFAULT_CARET_WIDTH, caret.height).into(),
            })
        } else {
            None
        }
    }

    /// Gets the currently active window on the platform
    pub(super) fn get_active_window(&self) -> Option<PlatformWindow> {
        let active_window = self.focused_window.lock().unwrap().as_ref()?.clone();
        Some(PlatformWindow {
            rect: active_window.get_bounds()?.into(),
            inner: active_window,
        })
    }

    pub(super) fn accessibility_is_enabled() -> Option<bool> {
        Some(ACCESSIBILITY_ENABLED.load(Ordering::SeqCst))
    }
}

pub const fn autocomplete_active() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::{
        ApplicationSpecifier, Instant, WINDOW_RECOVERY_BACKOFF, XTermCacheUpdate, hide_overlay_on_element_change,
        recovery_is_throttled, should_refresh_x_term_cache, window_focus_events,
        x_term_cache_update_for_focused_element,
    };

    #[test]
    fn suspended_or_disabled_refresh_does_not_enter_any_caret_route() {
        let (proxy, _events) = crate::event_loop::channel();
        let state = super::PlatformStateImpl::new(proxy);
        state.set_caret_demand(false);
        state.refresh_window_position().unwrap();
        let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::from_slice(&[(
            "autocomplete.disable",
            serde_json::json!(true),
        )]));
        state.set_caret_demand(true);
        state.refresh_window_position().unwrap();
        let diagnostics = state.ax_caret.lock().unwrap();
        assert_eq!(diagnostics.position_refreshes, 0);
        assert_eq!(diagnostics.queries, 0);
    }

    #[test]
    fn platform_dump_reports_ax_query_failure_without_requerying() {
        let (proxy, _events) = crate::event_loop::channel();
        let state = super::PlatformStateImpl::new(proxy);
        let error = super::CaretQueryError {
            stage: "range_bounds",
            failure: macos_utils::caret_position::CaretQueryFailure::AxError,
            ax_error: Some(accessibility_sys::kAXErrorParameterizedAttributeUnsupported),
        };
        state.ax_caret.lock().unwrap().record_query(
            super::CaretQueryIdentity {
                pid: 42,
                window_id: 7,
                epoch: 3,
            },
            std::time::Duration::from_micros(150),
            &Err(error),
            "emitted",
        );
        let dump = serde_json::to_value(&state).unwrap();
        assert_eq!(dump["ax_caret"]["queries"], 1);
        assert_eq!(dump["ax_caret"]["failures"], 1);
        assert_eq!(dump["ax_caret"]["last_query"]["failure_stage"], "range_bounds");
        assert_eq!(dump["ax_caret"]["last_query"]["failure_reason"], "ax_error");
        assert_eq!(dump["ax_caret"]["last_query"]["ax_error"], error.ax_error.unwrap());
        assert_eq!(
            state.ax_caret.lock().unwrap().queries,
            1,
            "reading the dump must not query AX"
        );
    }

    fn test_app() -> ApplicationSpecifier {
        ApplicationSpecifier {
            pid: 42,
            bundle_id: "io.appmakes.otty".into(),
        }
    }

    #[test]
    fn stale_activation_neither_hides_overlay_nor_queries_ax() {
        let events = window_focus_events(false, test_app(), || panic!("stale activation must not query AX"));
        assert!(events.is_empty());
    }

    #[test]
    fn failed_window_discovery_preserves_identity_for_consumer_invalidation() {
        let expected = test_app();
        let events = window_focus_events(true, expected.clone(), || {
            Err(accessibility_sys::kAXErrorCannotComplete)
        });
        assert!(matches!(
            events.as_slice(),
            [crate::event::Event::PlatformBoundEvent(
                crate::platform::PlatformBoundEvent::ExternalWindowFocusChanged { app, window: None }
            )] if app == &expected
        ));
    }

    #[test]
    fn window_recovery_backoff_expires_and_does_not_block_another_app() {
        let app = test_app();
        let failed_at = Instant::now();
        let failure = (app.clone(), failed_at);
        assert!(recovery_is_throttled(Some(&failure), &app, failed_at));
        assert!(!recovery_is_throttled(
            Some(&failure),
            &app,
            failed_at + WINDOW_RECOVERY_BACKOFF
        ));
        assert!(!recovery_is_throttled(None, &app, failed_at));
        let mut other = app.clone();
        other.pid += 1;
        assert!(!recovery_is_throttled(Some(&failure), &other, failed_at));
        other.pid = app.pid;
        other.bundle_id = "com.mitchellh.ghostty".into();
        assert!(!recovery_is_throttled(Some(&failure), &other, failed_at));
    }

    fn xterm_window() -> super::PlatformWindowImpl {
        super::PlatformWindowImpl {
            window_id: 7,
            ui_element: macos_utils::window_server::UIElement::application(std::process::id() as i32),
            x_term_tree_cache: None,
            x_term_last_failure: None,
            bundle_id: "com.microsoft.VSCode".into(),
            pid: std::process::id() as i32,
        }
    }

    #[test]
    fn old_pane_success_and_failure_cannot_change_current_cache_or_backoff() {
        let mut current = xterm_window();
        let query = super::CaretQueryIdentity {
            pid: current.pid,
            window_id: current.window_id,
            epoch: 1,
        };
        let mut old_success = current.clone();
        old_success.x_term_tree_cache = Some(vec![current.ui_element.clone()]);
        let mut old_failure = current.clone();
        old_failure.x_term_last_failure = Some(Instant::now());
        assert!(!current.apply_xterm_query(query, 2, old_success));
        assert!(!current.apply_xterm_query(query, 2, old_failure));
        assert!(current.x_term_tree_cache.is_none());
        assert!(current.x_term_last_failure.is_none());

        let mut latest_success = current.clone();
        latest_success.x_term_tree_cache = Some(vec![current.ui_element.clone()]);
        assert!(current.apply_xterm_query(super::CaretQueryIdentity { epoch: 2, ..query }, 2, latest_success));
        let mut late_failure = current.clone();
        late_failure.x_term_tree_cache = None;
        late_failure.x_term_last_failure = Some(Instant::now());
        assert!(!current.apply_xterm_query(query, 2, late_failure));
        assert!(current.x_term_tree_cache.is_some());
        assert!(current.x_term_last_failure.is_none());
    }

    #[test]
    fn xterm_backoff_expires_and_focus_invalidation_allows_immediate_retry() {
        let mut window = xterm_window();
        let failed_at = Instant::now();
        window.x_term_last_failure = Some(failed_at);
        assert!(super::xterm_retry_is_throttled(window.x_term_last_failure, failed_at));
        assert!(!super::xterm_retry_is_throttled(
            window.x_term_last_failure,
            failed_at + WINDOW_RECOVERY_BACKOFF
        ));
        window.invalidate_x_term_cache();
        assert!(!super::xterm_retry_is_throttled(window.x_term_last_failure, failed_at));
        let other = xterm_window();
        assert!(!super::xterm_retry_is_throttled(other.x_term_last_failure, failed_at));
    }

    #[test]
    fn moved_or_replaced_windows_reject_older_geometry() {
        let query = super::CaretQueryIdentity {
            pid: 42,
            window_id: 7,
            epoch: 3,
        };
        assert!(super::caret_query_matches(query, Some((42, 7)), 3));
        for (identity, epoch) in [(Some((42, 7)), 4), (Some((42, 8)), 3), (Some((43, 7)), 3), (None, 3)] {
            assert!(!super::caret_query_matches(query, identity, epoch));
        }
    }

    #[test]
    fn xterm_helper_textarea_keeps_the_caret_cache() {
        assert_eq!(
            x_term_cache_update_for_focused_element(true, Some(true)),
            XTermCacheUpdate::Retarget
        );
        assert_eq!(
            x_term_cache_update_for_focused_element(false, Some(true)),
            XTermCacheUpdate::Invalidate
        );
        assert_eq!(
            x_term_cache_update_for_focused_element(true, Some(false)),
            XTermCacheUpdate::Leave
        );
        assert_eq!(
            x_term_cache_update_for_focused_element(false, Some(false)),
            XTermCacheUpdate::Leave
        );
        assert_eq!(
            x_term_cache_update_for_focused_element(true, None),
            XTermCacheUpdate::Invalidate
        );
    }

    #[test]
    fn a_reposted_focus_on_the_tracked_caret_keeps_the_xterm_pane() {
        let (proxy, _events) = crate::event_loop::channel();
        let state = super::PlatformStateImpl::new(proxy);
        let pid = std::process::id() as i32;
        let app = ApplicationSpecifier {
            pid,
            bundle_id: "com.microsoft.VSCode".into(),
        };
        // `AXUIElementCreateApplication` needs no Accessibility grant.
        let leaf = super::UIElement::application(pid);
        assert!(!state.focused_element_keeps_x_term_pane(&app, &leaf));
        *state.focused_window.lock().unwrap() = Some(super::PlatformWindowImpl {
            window_id: 7,
            ui_element: leaf.clone(),
            x_term_tree_cache: Some(vec![leaf.clone()]),
            x_term_last_failure: None,
            bundle_id: app.bundle_id.clone(),
            pid,
        });
        assert!(state.focused_element_keeps_x_term_pane(&app, &leaf));
        let otty = ApplicationSpecifier {
            pid,
            bundle_id: "io.appmakes.otty".into(),
        };
        assert!(!state.focused_element_keeps_x_term_pane(&otty, &leaf));
        state
            .focused_window
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .invalidate_x_term_cache();
        assert!(!state.focused_element_keeps_x_term_pane(&app, &leaf));
    }

    #[test]
    fn only_xterm_terminals_refresh_the_caret_cache() {
        assert!(should_refresh_x_term_cache("com.microsoft.VSCode"));
        assert!(should_refresh_x_term_cache("com.todesktop.230313mzl4w4u92"));
        assert!(!should_refresh_x_term_cache("io.appmakes.otty"));
        assert!(!should_refresh_x_term_cache("com.mitchellh.ghostty"));
        assert!(!should_refresh_x_term_cache("com.googlecode.iterm2"));
    }

    #[test]
    fn ime_terminals_keep_the_overlay_on_element_change() {
        assert!(!hide_overlay_on_element_change("com.mitchellh.ghostty"));
        assert!(!hide_overlay_on_element_change("net.kovidgoyal.kitty"));
    }

    #[test]
    fn ax_terminals_still_hide_on_element_change() {
        // Version-aware Otty AX focus handling runs before this legacy helper.
        assert!(!hide_overlay_on_element_change("io.appmakes.otty"));
        assert!(hide_overlay_on_element_change("com.googlecode.iterm2"));
        assert!(hide_overlay_on_element_change("com.apple.Terminal"));
        assert!(hide_overlay_on_element_change("com.microsoft.VSCode"));
    }

    #[test]
    fn caret_request_notification_is_fastab_only() {
        let sibling = ["com.amazon.", "codewhisperer", ".edit_buffer_updated"].concat();
        assert_eq!(
            fastab_util::macos::EDIT_BUFFER_UPDATED_NOTIFICATION,
            "app.fastab.edit_buffer_updated"
        );
        let production = include_str!("macos.rs").split("#[cfg(test)]").next().unwrap();
        assert!(
            production.contains("ns_string!(\"app.fastab.edit_buffer_updated\")"),
            "desktop must post the Fastab IME notification"
        );
        assert!(
            !production.contains(&sibling),
            "desktop must not post the sibling Amazon Q notification"
        );
    }
}

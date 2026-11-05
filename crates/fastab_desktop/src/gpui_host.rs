//! GPUI process host. Replaces tao's `EventLoop::run` so the overlay and the tray share one
//! `NSApplication`. Settings runs elsewhere: see `settings_process`.

use std::sync::Arc;
use std::{cell::RefCell, rc::Rc};

use fastab_engine::{EngineClient, default_specs_dir};
#[cfg(target_os = "macos")]
use fastab_integrations::input_method::InputMethod;
use gpui::{App, Application, Entity};
use muda::MenuEvent;
use tao::event_loop::ControlFlow;
#[cfg(target_os = "macos")]
use tracing::warn;
use tracing::{debug, error, info, trace};
use tray_icon::TrayIcon;

use crate::EventLoopProxy;
use crate::event::{Event, ShowMessageNotification, WindowEvent};
use crate::event_loop::{InputSequence, OverlayInputDispatch, QueuedOverlayInput};
use crate::overlay::OverlayController;
use crate::platform::{PlatformBoundEvent, PlatformState};
use crate::settings_process::{HostMessage, SettingsProcess};
use crate::tray::{self, get_context_menu, get_icon};
use crate::webview::{DASHBOARD_ID, FigIdMap, WindowId};
use fastab_os_shim::Context;
use fastab_remote_ipc::figterm::FigtermState;

#[cfg(target_os = "macos")]
/// Only the IME sends `BottomLeft`, and it serves IME terminals alone. An older
/// IME's frame carries no source, so one queued before focus moved to an
/// AX-caret terminal or to another app would place the list over the wrong
/// window. `ime_terminal` is `None` when the frontmost app is unknown.
fn accept_local_caret_position(
    position: crate::event::WindowPosition,
    prefers_ax: bool,
    ime_terminal: Option<bool>,
) -> bool {
    !matches!(
        position,
        crate::event::WindowPosition::RelativeToCaret {
            origin: fastab_proto::local::caret_position_hook::Origin::BottomLeft,
            ..
        }
    ) || (!prefers_ax && ime_terminal != Some(false))
}

#[cfg(target_os = "macos")]
/// A caret queued before focus moved to another window of the same app, or to
/// another app, names the client it came from. `window_is_focused` decides a
/// reported window against the tracked one; an unknown frontmost app or a
/// source without that field changes nothing.
fn local_caret_source_is_frontmost(
    source: &crate::event::CaretSource,
    frontmost: Option<&macos_utils::window_server::ApplicationSpecifier>,
    window_is_focused: impl FnOnce(&macos_utils::window_server::ApplicationSpecifier, u32) -> bool,
) -> bool {
    let Some(app) = frontmost else {
        return true;
    };
    if source
        .bundle_id
        .as_deref()
        .is_some_and(|bundle_id| bundle_id != app.bundle_id)
    {
        return false;
    }
    source
        .window_id
        .is_none_or(|window_id| window_is_focused(app, window_id))
}

/// The caller must first validate the native focus event against the current
/// application/window/element. Merely receiving a caret or stale focus is not
/// sufficient to invalidate an input that is waiting for queue admission.
pub(crate) fn invalidate_overlay_input_focus(
    proxy: &EventLoopProxy,
    overlay: &mut OverlayController,
    sequence: InputSequence,
    cx: &mut App,
) {
    proxy.accept_input_focus(sequence);
    overlay.invalidate_caret_for_focus_change(cx);
}

/// Returns whether this input may refresh the current terminal's caret.
pub(crate) fn dispatch_overlay_input(
    proxy: &EventLoopProxy,
    overlay: &mut OverlayController,
    input: &QueuedOverlayInput,
    figterm_state: &Arc<FigtermState>,
    cx: &mut App,
) -> bool {
    match proxy.take_overlay_input(input) {
        Some(OverlayInputDispatch::Complete(input)) => {
            let needs_caret = !input.buffer.trim().is_empty();
            if needs_caret && input.redraw && overlay.is_background_session(input.session_id) {
                // A background tab repainting its unchanged command line.
                return false;
            }
            overlay.complete_buffer(
                input.buffer,
                input.cwd,
                input.cursor,
                input.session_id,
                figterm_state.clone(),
                cx,
            );
            needs_caret
        },
        Some(OverlayInputDispatch::EndInput(session_id)) => {
            overlay.end_input(session_id, cx);
            false
        },
        None => false,
    }
}

pub struct DesktopHost {
    pub fig_id_map: FigIdMap,
    pub figterm_state: Arc<FigtermState>,
    pub platform_state: Arc<PlatformState>,
    #[allow(dead_code)]
    pub context: Arc<Context>,
    /// The launch flow deferred its normal settings/dashboard open until the
    /// GPUI host exists (the modern Login Item path). `--no-dashboard` and
    /// `app.silentLaunch` clear this upstream; a missing Accessibility grant
    /// remains an explicit exception in [`start_application`], because the
    /// native settings gate is the in-app path that explains and repairs it.
    pub show_dashboard_after_normal_launch: bool,
    pub proxy: EventLoopProxy,
    pub overlay: OverlayController,
    /// The last Settings process started. It may have exited since.
    pub settings: Option<SettingsProcess>,
    pub completion_settings: crate::settings_refresh::CompletionSettings,
    pub tray: TrayIcon,
}

impl DesktopHost {
    pub(crate) fn dispatch(&mut self, event: Event, sequence: InputSequence, cx: &mut App) {
        match event {
            Event::GpuiOverlayVisibleRows {
                generation,
                revision,
                range,
            } => {
                self.overlay.request_visible_icons(generation, revision, range, cx);
            },
            Event::JevSettingsChanged => self.overlay.reload_jev(cx),
            Event::JevCredentialsLoaded { epoch, credentials } => {
                self.overlay.jev_credentials_loaded(epoch, credentials);
            },
            Event::JevDebounced(token) => self.overlay.start_jev_request(token, cx),
            Event::JevComplete { token, result } => self.overlay.apply_jev(token, result, cx),
            Event::JevContextChanged => self.overlay.reconcile_jev_context(cx),
            Event::WindowEvent {
                window_id,
                window_event,
            } => self.dispatch_window_event(window_id, window_event, sequence, cx),
            Event::WindowEventAll { window_event } => {
                if matches!(window_event, WindowEvent::SetTheme(_)) {
                    self.overlay.apply_theme(cx);
                }
            },
            Event::ControlFlow(ControlFlow::Exit) => cx.quit(),
            Event::ControlFlow(_) => {},
            Event::ReloadTray { is_logged_in } => {
                self.tray
                    .set_icon(Some(get_icon(is_logged_in)))
                    .map_err(|err| error!(?err))
                    .ok();
                self.tray.set_icon_as_template(true);
                self.tray.set_menu(Some(Box::new(get_context_menu(is_logged_in))));
            },
            Event::ReloadCredentials => {
                let needs_completion = self
                    .completion_settings
                    .refresh(&fastab_settings::settings::Settings::new());
                let autocomplete_enabled = autocomplete_should_run();
                self.overlay.apply_theme(cx);
                self.overlay.set_enabled(autocomplete_enabled, cx);
                if autocomplete_enabled && needs_completion {
                    self.overlay.recomplete(cx);
                }
                self.overlay.relayout_and_sync(cx);
            },
            Event::ReloadAccessibility => {
                self.tray.set_menu(Some(Box::new(get_context_menu(true))));
                let autocomplete_enabled = autocomplete_should_run();
                self.overlay.apply_theme(cx);
                self.overlay.set_enabled(autocomplete_enabled, cx);
                if let Some(settings) = &self.settings {
                    settings.send(HostMessage::RefreshPermissions).ok();
                }
            },
            Event::CheckForUpdates => {
                tokio::spawn(async {
                    crate::update::check_for_update(true, false).await;
                });
            },
            Event::MenuClicked(id) => {
                info!(%id, "Menu Event");
                let menu_event = MenuEvent { id: muda::MenuId(id) };
                tray::handle_event(&menu_event, &self.proxy);
            },
            // Only the Settings process checks permissions, and it keeps the result.
            Event::PermissionSnapshot(_) => {},
            Event::EngineDiagnostics { mut reply } => {
                if reply.is_canceled() {
                    return;
                }
                let diagnostics = self.overlay.engine_diagnostics();
                let mut host = self.overlay.resource_diagnostics(cx);
                host.gpui_windows = cx.windows().len();
                let events = self.proxy.diagnostics();
                host.host_async_events = events.async_events;
                host.host_async_payload_bytes = events.async_payload_bytes;
                host.host_async_waiters = events.async_waiters;
                host.host_async_waiting_bytes = events.async_waiting_bytes;
                host.host_sync_events = events.sync_events;
                host.host_sync_payload_bytes = events.sync_payload_bytes;
                host.ipc_remote_connections = fastab_remote_ipc::remote::active_connections();
                host.ipc_local_connections = crate::local_ipc::active_connections();
                #[cfg(target_os = "macos")]
                {
                    host.host_ax_events = crate::platform::PlatformStateImpl::pending_ax_events();
                }
                {
                    let sessions = self.figterm_state.inner.lock();
                    host.ipc_sessions = sessions.linked_sessions.len();
                    for session in sessions.linked_sessions.values() {
                        host.ipc_session_edit_bytes = host
                            .ipc_session_edit_bytes
                            .saturating_add(session.edit_buffer.text.len());
                        host.ipc_session_edit_capacity = host
                            .ipc_session_edit_capacity
                            .saturating_add(session.edit_buffer.text.capacity());
                        let pending = session.sender.diagnostics();
                        host.ipc_pending_messages = host.ipc_pending_messages.saturating_add(pending.pending_messages);
                        host.ipc_pending_bytes = host.ipc_pending_bytes.saturating_add(pending.pending_bytes);
                        host.ipc_pending_responses =
                            host.ipc_pending_responses.saturating_add(pending.pending_responses);
                    }
                }
                // GPUI only submits the read-only job. Wait off the foreground
                // executor and stop waiting if the IPC deadline expires.
                tokio::spawn(async move {
                    let snapshot = tokio::select! {
                        snapshot = diagnostics => snapshot,
                        _ = reply.cancellation() => return,
                    };
                    let _ = reply.send(snapshot.map(|mut snapshot| {
                        snapshot.host = Some(host);
                        snapshot
                    }));
                });
            },
            Event::SetTrayVisible(visible) => {
                if let Err(err) = self.tray.set_visible(visible) {
                    error!(%err, "Failed to set tray visible");
                }
            },
            Event::PlatformBoundEvent(native_event) => {
                #[cfg(target_os = "macos")]
                self.platform_state
                    .inner()
                    .set_caret_demand(self.overlay.wants_caret(cx));
                #[cfg(target_os = "macos")]
                if let crate::platform::PlatformBoundEvent::ExternalWindowFocusChanged { app, window } = &native_event {
                    // AX discovery may have blocked since this event was produced.
                    // Reject a superseded app/window before clearing its caret.
                    if !macos_utils::window_server::is_frontmost_application(app) {
                        return;
                    }
                    if window.is_none() && self.platform_state.inner().cached_window_is_current(app) {
                        return;
                    }
                    if window
                        .as_ref()
                        .is_some_and(|window| window.is_current_focused_window() == Ok(false))
                        || !macos_utils::window_server::is_frontmost_application(app)
                    {
                        return;
                    }
                    invalidate_overlay_input_focus(&self.proxy, &mut self.overlay, sequence, cx);
                }
                #[cfg(target_os = "macos")]
                if let crate::platform::PlatformBoundEvent::FocusedElementChanged { app, element } = &native_event {
                    if (crate::platform::prefers_ax_caret_for_app(app)
                        || crate::platform::hide_overlay_on_element_change(&app.bundle_id))
                        && macos_utils::window_server::is_frontmost_application(app)
                    {
                        // A same-process notification can outlive its window or
                        // pane. Only the element that is still focused may clear
                        // the current terminal's caret and completion request.
                        if macos_utils::window_server::UIElement::application(app.pid)
                            .focused_element()
                            .as_ref()
                            .ok()
                            != Some(element)
                            || !macos_utils::window_server::is_frontmost_application(app)
                        {
                            return;
                        }
                        // Same pane: the platform would bump the caret epoch and
                        // send an empty caret, which hides the list just the same.
                        if self
                            .platform_state
                            .inner()
                            .focused_element_keeps_x_term_pane(app, element)
                        {
                            return;
                        }
                        invalidate_overlay_input_focus(&self.proxy, &mut self.overlay, sequence, cx);
                    }
                }
                if let Err(err) = self.platform_state.handle(native_event, &self.fig_id_map) {
                    debug!(%err, "Failed to handle native event");
                }
            },
            Event::ShowMessageNotification(ShowMessageNotification {
                title,
                body,
                parent,
                buttons,
                buttons_result,
            }) => {
                let dialog = rfd::AsyncMessageDialog::new().set_title(title).set_description(body);
                let _ = parent;
                let dialog = match (buttons, buttons_result.as_ref()) {
                    (Some(buttons), Some(_)) => dialog.set_buttons(buttons),
                    _ => dialog,
                };
                tokio::spawn(async move {
                    let res = dialog.show().await;
                    if let Some(buttons_result) = buttons_result {
                        buttons_result
                            .send(res)
                            .await
                            .map_err(|err| error!(?err, "Failed to send dialog result"))
                            .ok();
                    }
                });
            },
            Event::GpuiOverlayInput(input) => {
                if dispatch_overlay_input(&self.proxy, &mut self.overlay, &input, &self.figterm_state, cx) {
                    // Establish the input/session before querying AX/IME. Both
                    // halves occupy one queue slot and cannot interleave.
                    self.dispatch(
                        Event::PlatformBoundEvent(PlatformBoundEvent::EditBufferChanged),
                        sequence,
                        cx,
                    );
                }
            },
            Event::GpuiOverlayLoading { generation } => {
                self.overlay.show_loading(generation, cx);
            },
            Event::GpuiOverlayEndInput { session_id } => {
                self.overlay.end_input(session_id, cx);
            },
            Event::GpuiOverlayShowExpired { token } => {
                self.overlay.expire_show(token, cx);
            },
            Event::GpuiOverlayLoadingExpired { generation } => {
                self.overlay.expire_loading(generation, cx);
            },
            Event::GpuiOverlayComplete {
                generation,
                result,
                session_id,
                cwd,
            } => {
                self.overlay.apply_completion(generation, result, session_id, &cwd, cx);
            },
            Event::GpuiOverlayDebouncedComplete { generation, waited_ms } => {
                self.overlay.apply_debounced_complete(generation, waited_ms, cx);
            },
            Event::GpuiOverlayRelayoutRetry {
                generation,
                token,
                attempt,
            } => {
                self.overlay.retry_layout(generation, token, attempt, cx);
            },
            Event::AutocompleteAction {
                action,
                session_id,
                key,
            } => {
                self.overlay
                    .handle_key_action(&action, key, session_id, &self.figterm_state, cx);
            },
            Event::AutocompleteClick {
                click,
                session_id,
                generation,
            } => {
                self.overlay
                    .handle_click(click, session_id, generation, &self.figterm_state, cx);
            },
        }
    }

    fn dispatch_window_event(
        &mut self,
        window_id: WindowId,
        window_event: WindowEvent,
        sequence: InputSequence,
        cx: &mut App,
    ) {
        match window_id {
            id if id == DASHBOARD_ID => self.dispatch_settings_event(window_event),
            id if id == crate::webview::AUTOCOMPLETE_ID => match window_event {
                WindowEvent::Hide | WindowEvent::Close => self.overlay.hide(cx),
                WindowEvent::SetEnabled(enabled) => self.overlay.set_enabled(enabled, cx),
                #[cfg(target_os = "macos")]
                WindowEvent::TerminalEnabled {
                    app,
                    cache_identity,
                    epoch,
                    enabled,
                    accessibility_missing,
                    sample_uncertain,
                } => {
                    if macos_utils::window_server::is_frontmost_application(&app)
                        && self.platform_state.inner().caret_cache_identity() == cache_identity
                        && self.platform_state.inner().enabled_epoch() == epoch
                    {
                        self.overlay
                            .set_terminal_enabled(app, enabled, accessibility_missing, sample_uncertain, cx);
                    }
                },
                WindowEvent::CaretPositionHook { position, source } => {
                    #[cfg(not(target_os = "macos"))]
                    let _ = source;
                    #[cfg(target_os = "macos")]
                    {
                        let frontmost = macos_utils::window_server::frontmost_application();
                        let prefers_ax = frontmost
                            .as_ref()
                            .is_some_and(crate::platform::prefers_ax_caret_for_app);
                        let ime_terminal = frontmost.as_ref().map(|app| {
                            fastab_util::Terminal::from_bundle_id(&app.bundle_id)
                                .is_some_and(|terminal| terminal.supports_macos_input_method())
                        });
                        // The IME wire contract always uses BottomLeft. Keep the
                        // local hook distinct so native AX geometry is unaffected,
                        // including a late IME reply queued before focus changed.
                        if !accept_local_caret_position(position, prefers_ax, ime_terminal)
                            || !local_caret_source_is_frontmost(&source, frontmost.as_ref(), |app, window_id| {
                                self.platform_state
                                    .inner()
                                    .local_caret_window_is_focused(app, window_id)
                            })
                        {
                            return;
                        }
                    }
                    self.overlay.apply_position(position, &self.platform_state, cx);
                },
                #[cfg(target_os = "macos")]
                WindowEvent::TerminalCaret {
                    app,
                    cache_identity,
                    epoch,
                    position,
                    invalidate_input,
                } => {
                    if !macos_utils::window_server::is_frontmost_application(&app)
                        || self.platform_state.inner().caret_cache_identity() != cache_identity
                        || self.platform_state.inner().caret_epoch() != epoch
                    {
                        return;
                    }
                    if invalidate_input {
                        invalidate_overlay_input_focus(&self.proxy, &mut self.overlay, sequence, cx);
                    } else if let Some(position) = position {
                        self.overlay.apply_position(position, &self.platform_state, cx);
                    } else {
                        self.overlay.clear_caret_position(cx);
                    }
                },
                WindowEvent::UpdateWindowGeometry { position, .. } => {
                    if let Some(position) = position {
                        self.overlay.apply_position(position, &self.platform_state, cx);
                    }
                },
                WindowEvent::Show => self.overlay.show(cx),
                WindowEvent::Event { event_name, .. } if event_name == "clear-cache" => self.overlay.clear_caches(),
                other => trace!(?other, "Ignoring settings-only event on GPUI overlay"),
            },
            other => trace!(%other, ?window_event, "Ignoring event for unknown window"),
        }
    }

    fn dispatch_settings_event(&mut self, window_event: WindowEvent) {
        match window_event {
            WindowEvent::Show | WindowEvent::Devtools => self.show_settings(None),
            WindowEvent::Hide | WindowEvent::Close => {
                if let Some(settings) = &self.settings {
                    settings.send(HostMessage::Close).ok();
                }
            },
            WindowEvent::NavigateRelative { path } => self.show_settings(Some(path.into_owned())),
            WindowEvent::Batch(events) => {
                for event in events {
                    self.dispatch_settings_event(event);
                }
            },
            other => trace!(?other, "Ignoring webview-only event on native settings"),
        }
    }

    fn show_settings(&mut self, path: Option<String>) {
        let mut message = HostMessage::Show { path };
        if let Some(settings) = &self.settings {
            message = match settings.send(message) {
                Ok(()) => return,
                Err(message) => message,
            };
        }
        match SettingsProcess::launch(self.proxy.clone()) {
            Ok(settings) => {
                settings.send(message).ok();
                self.settings = Some(settings);
            },
            Err(err) => {
                error!(%err, "Failed to start the Settings process");
                self.settings = None;
            },
        }
    }
}

fn autocomplete_should_run() -> bool {
    if fastab_settings::settings::get_bool_or("autocomplete.disable", false) {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        macos_utils::accessibility::accessibility_is_enabled()
    }
    #[cfg(not(target_os = "macos"))]
    {
        PlatformState::accessibility_is_enabled().unwrap_or(true)
    }
}

pub fn run(host: Entity<DesktopHost>, event_rx: crate::event_loop::EventLoopReceiver, cx: &mut App) {
    cx.spawn(async move |cx| {
        let executor = cx.background_executor().clone();
        let mut dispatched = 0;
        while let Ok(event) = event_rx.recv_queued().await {
            let result = event.dispatch(|event, sequence| {
                cx.update(|cx| {
                    host.update(cx, |host, cx| host.dispatch(event, sequence, cx));
                })
            });
            if let Err(err) = result {
                error!(%err, "GPUI app released while dispatching desktop event");
                break;
            }
            dispatched += 1;
            if dispatched == 64 {
                // A zero-duration GPUI timer is immediately ready. A real
                // timer turn lets AppKit paint and handle native callbacks
                // even while IPC keeps the queue continuously readable.
                executor.timer(std::time::Duration::from_millis(1)).await;
                dispatched = 0;
            }
        }
    })
    .detach();
}

/// GPUI subclasses `NSApplication` as `GPUIApplication` and stores ivars on that
/// instance. The first `sharedApplication` call wins, so tray-icon / settings
/// windows must not create a plain `NSApplication` first — that aborts in `set_ivar`.
#[cfg(target_os = "macos")]
pub fn ensure_gpui_ns_application() {
    #[allow(unexpected_cfgs, unused_unsafe)]
    unsafe {
        use objc::{class, msg_send, sel, sel_impl};
        let _: cocoa::base::id = msg_send![class!(GPUIApplication), sharedApplication];
    }
}

/// Builds the host once GPUI's `App` exists. Produced by the async prelude and
/// consumed by [`start_application`] from `main`, outside `block_on`.
pub type Setup =
    Box<dyn FnOnce(&mut App) -> anyhow::Result<(Entity<DesktopHost>, crate::event_loop::EventLoopReceiver)>>;

#[cfg(target_os = "macos")]
fn should_show_settings_after_launch(
    deferred_normal_launch: bool,
    launched_as_login_item: bool,
    accessibility_missing: bool,
) -> bool {
    accessibility_missing || (deferred_normal_launch && !launched_as_login_item)
}

pub fn start_application(
    setup: impl FnOnce(&mut App) -> anyhow::Result<(Entity<DesktopHost>, crate::event_loop::EventLoopReceiver)> + 'static,
) -> anyhow::Result<()> {
    let application = Application::new();
    let reopen_proxy: Rc<RefCell<Option<EventLoopProxy>>> = Rc::new(RefCell::new(None));
    let reopen_proxy_for_callback = reopen_proxy.clone();
    application.on_reopen(move |_cx| {
        if let Some(proxy) = reopen_proxy_for_callback.borrow().as_ref() {
            proxy
                .send_event(Event::WindowEvent {
                    window_id: DASHBOARD_ID,
                    window_event: WindowEvent::Show,
                })
                .ok();
        }
    });
    application.run(move |cx: &mut App| match setup(cx) {
        Ok((host, event_rx)) => {
            *reopen_proxy.borrow_mut() = Some(host.read(cx).proxy.clone());
            fastab_gpui::install_idle_cache_release(cx, crate::heap_trim::note_activity);
            run(host.clone(), event_rx, cx);
            #[cfg(target_os = "macos")]
            {
                let proxy = host.read(cx).proxy.clone();
                // This foreground task runs after GPUI has entered its event
                // loop. Only a previously chosen IME is restored, and its
                // repair runs away from the UI thread.
                cx.spawn(async move |_| {
                    tokio::spawn(async move {
                        let input_method = InputMethod::default();
                        // An open Settings window shows the input method's state.
                        match input_method.restore_if_chosen().await {
                            Ok(false) => {},
                            Ok(true) => {
                                proxy.send_event(Event::ReloadAccessibility).ok();
                            },
                            Err(err) => {
                                warn!(%err, "Failed to restore the previously installed input method");
                                proxy.send_event(Event::ReloadAccessibility).ok();
                            },
                        }
                        // The launch-time restore is a single check; keep the
                        // chosen IME running after it, too.
                        crate::ime_watch::run(input_method).await;
                    });
                })
                .detach();
            }
            host.update(cx, |host, _cx| {
                info!("{} has started", fastab_util::PRODUCT_NAME);
                #[cfg(target_os = "macos")]
                {
                    // GPUI makes every app Regular as it finishes launching. This
                    // process has no window that needs a Dock icon or a menu.
                    crate::platform::set_activation_policy(tao::platform::macos::ActivationPolicy::Accessory);
                    // `--no-dashboard` and `app.silentLaunch` suppress only the
                    // normal dashboard request. Missing Accessibility is an
                    // intentional exception: without opening native settings,
                    // the user has no in-app path to repair the required grant.
                    let show_settings = should_show_settings_after_launch(
                        host.show_dashboard_after_normal_launch,
                        crate::platform::launched_as_login_item(),
                        crate::permissions::accessibility_is_missing(),
                    );
                    if show_settings {
                        host.proxy
                            .send_event(Event::WindowEvent {
                                window_id: DASHBOARD_ID,
                                window_event: WindowEvent::Show,
                            })
                            .ok();
                    }
                }
            });
        },
        Err(err) => {
            error!(%err, "Failed to start GPUI host");
            cx.quit();
        },
    });
    Ok(())
}

pub fn spawn_engine() -> anyhow::Result<EngineClient> {
    let dir = default_specs_dir();
    if !dir.is_dir() {
        tracing::warn!(
            path = %dir.display(),
            "specs IR directory missing; completions fall back to cobra and history"
        );
    }
    EngineClient::spawn(dir)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::should_show_settings_after_launch;

    #[cfg(target_os = "macos")]
    #[test]
    fn ax_terminal_rejects_queued_ime_carets_but_preserves_top_left_hooks() {
        use fastab_proto::local::caret_position_hook::Origin;
        use tao::dpi::{LogicalPosition, LogicalSize};
        let position = |origin| crate::event::WindowPosition::RelativeToCaret {
            caret_position: LogicalPosition::new(100.0, 200.0).into(),
            caret_size: LogicalSize::new(0.0, 18.0).into(),
            origin,
        };
        let accept = |origin, prefers_ax, ime_terminal| {
            super::accept_local_caret_position(position(origin), prefers_ax, ime_terminal)
        };
        assert!(!accept(Origin::BottomLeft, true, Some(true)));
        assert!(accept(Origin::TopLeft, true, Some(true)));
        assert!(accept(Origin::BottomLeft, false, Some(true)));
        // A frame queued in Ghostty must not land on iTerm2 or Finder.
        assert!(!accept(Origin::BottomLeft, false, Some(false)));
        assert!(accept(Origin::TopLeft, false, Some(false)));
        assert!(accept(Origin::BottomLeft, false, None));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_queued_ime_caret_from_another_window_or_app_is_rejected() {
        use crate::event::CaretSource;
        use macos_utils::window_server::ApplicationSpecifier;
        let ghostty = ApplicationSpecifier {
            pid: 42,
            bundle_id: "com.mitchellh.ghostty".to_owned(),
        };
        let source = |bundle_id: Option<&str>, window_id| CaretSource {
            bundle_id: bundle_id.map(str::to_owned),
            window_id,
        };
        // Window 7 is focused.
        let accept = |source: &CaretSource, frontmost| {
            super::local_caret_source_is_frontmost(source, frontmost, |app, window_id| {
                assert_eq!(app.pid, 42);
                window_id == 7
            })
        };

        assert!(accept(&source(Some("com.mitchellh.ghostty"), Some(7)), Some(&ghostty)));
        assert!(!accept(&source(Some("com.mitchellh.ghostty"), Some(8)), Some(&ghostty)));
        assert!(!accept(&source(Some("net.kovidgoyal.kitty"), Some(7)), Some(&ghostty)));
        assert!(accept(&source(Some("com.mitchellh.ghostty"), None), Some(&ghostty)));
        // Frames from an older IME carry no source.
        assert!(accept(&CaretSource::default(), Some(&ghostty)));
        assert!(accept(&source(Some("net.kovidgoyal.kitty"), Some(8)), None));
    }

    #[test]
    fn accessibility_gate_overrides_silent_or_no_dashboard_launch() {
        assert!(should_show_settings_after_launch(false, false, true));
        assert!(should_show_settings_after_launch(false, true, true));
        assert!(!should_show_settings_after_launch(false, false, false));
    }

    #[test]
    fn deferred_dashboard_only_opens_for_a_normal_non_login_launch() {
        assert!(should_show_settings_after_launch(true, false, false));
        assert!(!should_show_settings_after_launch(true, true, false));
    }
}

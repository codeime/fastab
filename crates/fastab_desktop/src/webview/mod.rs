pub mod menu;
pub mod window_id;

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use fastab_os_shim::Context;
use fastab_remote_ipc::figterm::FigtermState;
use fnv::FnvBuildHasher;
use gpui::AppContext as _;
use tao::dpi::LogicalSize;
use tao::window::Theme as TaoTheme;
use tracing::{debug, error, warn};

use crate::event::{Event, WindowEvent};
use crate::notification_bus::{JsonNotification, NOTIFICATION_BUS};
use crate::platform::{PlatformBoundEvent, PlatformState};
use crate::remote_ipc::RemoteHook;
use crate::tray::build_tray;
pub use crate::webview::window_id::{AUTOCOMPLETE_ID, DASHBOARD_ID, WindowId};
use crate::{EventLoopProxy, file_watcher, local_ipc};

pub const DASHBOARD_SIZE: LogicalSize<f64> = LogicalSize::new(820.0, 640.0);

pub const AUTOCOMPLETE_WINDOW_TITLE: &str = "FasTab Autocomplete";

fn map_theme(theme: &str) -> Option<TaoTheme> {
    match theme {
        "dark" => Some(TaoTheme::Dark),
        "light" => Some(TaoTheme::Light),
        _ => None,
    }
}

pub type FigIdMap = HashSet<WindowId, FnvBuildHasher>;

pub struct WebviewManager {
    pub(crate) fig_id_map: FigIdMap,
    pub(crate) event_rx: crate::event_loop::EventLoopReceiver,
    pub(crate) proxy: EventLoopProxy,
    pub(crate) figterm_state: Arc<FigtermState>,
    pub(crate) platform_state: Arc<PlatformState>,
    pub(crate) context: Arc<Context>,
    pub(crate) show_dashboard_after_normal_launch: bool,
}

/// The platform layer reaches for this from callbacks that carry no state of
/// their own. Everything else is threaded through [`WebviewManager`].
pub static GLOBAL_PROXY: OnceLock<EventLoopProxy> = OnceLock::new();

impl WebviewManager {
    #[allow(unused_variables)]
    #[allow(unused_mut)]
    pub fn new(context: Arc<Context>, show_dashboard_after_normal_launch: bool) -> Self {
        let (proxy, event_rx) = crate::event_loop::channel();
        GLOBAL_PROXY.set(proxy.clone()).unwrap();

        let figterm_state = Arc::new(FigtermState::default());
        let platform_state = Arc::new(PlatformState::new(proxy.clone()));

        Self {
            fig_id_map: Default::default(),
            event_rx,
            proxy,
            figterm_state,
            platform_state,
            context,
            show_dashboard_after_normal_launch,
        }
    }

    /// Start the IPC servers, tray and engine, and return the closure that
    /// builds the GPUI host. Starting GPUI is left to `main`: the async prelude
    /// runs inside `block_on`, and the UI loop must not (see `main.rs`).
    #[allow(unused_mut)]
    pub async fn prepare(mut self) -> anyhow::Result<crate::gpui_host::Setup> {
        self.platform_state
            .handle(PlatformBoundEvent::Initialize, &self.fig_id_map)
            .expect("Failed to initialize platform state");

        {
            let platform_state = self.platform_state.clone();
            let figterm_state = self.figterm_state.clone();
            let proxy = self.proxy.clone();
            tokio::spawn(async move {
                match local_ipc::start_local_ipc(platform_state, figterm_state, proxy).await {
                    Ok(_) => (),
                    Err(err) => error!("Unable to start local ipc: {:?}", err),
                }
            });
        }

        tokio::spawn(fastab_remote_ipc::remote::start_remote_ipc(
            fastab_util::directories::local_remote_socket_path().unwrap(),
            self.figterm_state.clone(),
            RemoteHook {
                proxy: self.proxy.clone(),
            },
        ));

        file_watcher::setup_listeners(self.proxy.clone()).await;

        init_webview_notification_listeners(self.proxy.clone()).await;

        let menu_proxy = self.proxy.clone();
        muda::MenuEvent::set_event_handler(Some({
            let menu_proxy = menu_proxy.clone();
            move |event: muda::MenuEvent| {
                let _ = menu_proxy.send_event(Event::MenuClicked(event.id.0));
            }
        }));
        // Handler install is OnceCell: if anything primed it to `None` first, new events stay
        // on the muda channel. Drain leftovers and keep a fallback listener so tray clicks
        // cannot go silent again.
        while let Ok(event) = muda::MenuEvent::receiver().try_recv() {
            let _ = menu_proxy.send_event(Event::MenuClicked(event.id.0));
        }
        std::thread::Builder::new()
            .name("ec-menu-events".into())
            .spawn({
                let menu_proxy = menu_proxy.clone();
                move || {
                    let rx = muda::MenuEvent::receiver();
                    while let Ok(event) = rx.recv() {
                        let _ = menu_proxy.send_event(Event::MenuClicked(event.id.0));
                    }
                }
            })
            .ok();

        let tray_visible = !fastab_settings::settings::get_bool_or("app.hideMenubarIcon", false);
        let tray = build_tray(&self.figterm_state).await.unwrap();
        if let Err(err) = tray.set_visible(tray_visible) {
            error!(%err, "Failed to set tray visible");
        }

        self.proxy
            .send_event(Event::PlatformBoundEvent(PlatformBoundEvent::InitializePostRun))
            .expect("Failed to send post init event");

        let engine = crate::gpui_host::spawn_engine().expect("completion engine");
        let event_rx = self.event_rx;
        let proxy = self.proxy.clone();
        let fig_id_map = self.fig_id_map;
        let figterm_state = self.figterm_state;
        let platform_state = self.platform_state;
        let context = self.context;
        let show_dashboard_after_normal_launch = self.show_dashboard_after_normal_launch;

        Ok(Box::new(move |cx| {
            let overlay = crate::overlay::OverlayController::start(
                cx,
                engine,
                proxy.clone(),
                figterm_state.clone(),
                platform_state.clone(),
            )?;
            let host = cx.new(|_| crate::gpui_host::DesktopHost {
                fig_id_map,
                figterm_state,
                platform_state,
                context,
                show_dashboard_after_normal_launch,
                proxy,
                overlay,
                settings: None,
                completion_settings: crate::settings_refresh::CompletionSettings::capture(
                    &fastab_settings::settings::Settings::new(),
                ),
                tray,
            });
            Ok((host, event_rx))
        }))
    }
}

async fn init_webview_notification_listeners(proxy: EventLoopProxy) {
    #[allow(unused_macros)]
    macro_rules! watcher {
        ($type:ident, $name:expr, $on_update:expr) => {{
            paste::paste! {
                let proxy = proxy.clone();
                tokio::spawn(async move {
                    let mut rx = NOTIFICATION_BUS.[<subscribe_ $type>]($name.into());
                    loop {
                        let res = rx.recv().await;
                        match res {
                            Ok(val) => {
                                #[allow(clippy::redundant_closure_call)]
                                ($on_update)(val, &proxy);
                            },
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Notification bus '{}' lagged by {n} messages", $name);
                            },
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });
            }
        };};
    }

    #[cfg(target_os = "linux")]
    {
        use fastab_integrations::Integration;
        use fastab_integrations::desktop_entry::{AutostartIntegration, should_install_autostart_entry};
        use fastab_settings::{Settings, State};

        use crate::notification_bus::JsonNotification;
        watcher!(
            settings,
            "autocomplete.disable",
            |notification: JsonNotification, proxy: &EventLoopProxy| {
                let enabled = !notification.into_bool().unwrap_or(false);
                debug!(%enabled, "Autocomplete");
                proxy
                    .send_event(Event::WindowEvent {
                        window_id: AUTOCOMPLETE_ID,
                        window_event: WindowEvent::SetEnabled(enabled),
                    })
                    .unwrap();
            }
        );
        watcher!(
            settings,
            "app.launchOnStartup",
            |notification: JsonNotification, _proxy: &EventLoopProxy| {
                let enabled = notification.into_bool().unwrap_or(false);
                debug!(%enabled, "app.launchOnStartup");
                tokio::spawn(async move {
                    let ctx = Context::new();
                    let settings = Settings::new();
                    let state = State::new();
                    let autostart = match AutostartIntegration::new(&ctx) {
                        Ok(autostart) => autostart,
                        Err(err) => {
                            error!(
                                ?err,
                                "failed to update the autostart integration installed status to {}", enabled
                            );
                            return;
                        },
                    };
                    if should_install_autostart_entry(&ctx, &settings, &state) {
                        autostart
                            .install()
                            .await
                            .map_err(|err| warn!(?err, "unable to install autostart integration"))
                            .ok();
                    } else {
                        autostart
                            .uninstall()
                            .await
                            .map_err(|err| warn!(?err, "unable to uninstall autostart integration"))
                            .ok();
                    }
                });
            }
        );
    }

    watcher!(
        settings,
        "app.theme",
        |notification: JsonNotification, proxy: &EventLoopProxy| {
            let theme = notification.into_string().as_deref().and_then(map_theme);
            debug!(?theme, "Theme changed");
            proxy
                .send_event(Event::WindowEventAll {
                    window_event: WindowEvent::SetTheme(theme),
                })
                .unwrap();
        }
    );

    watcher!(
        settings,
        "app.hideMenubarIcon",
        |notification: JsonNotification, proxy: &EventLoopProxy| {
            let enabled = !notification.into_bool().unwrap_or(false);
            debug!(%enabled, "Tray icon");
            proxy.send_event(Event::SetTrayVisible(enabled)).unwrap();
        }
    );
}

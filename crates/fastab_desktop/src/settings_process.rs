//! Settings runs in a process of its own, which exits when its window closes.
//!
//! Opening the window leaves 9 to 12 MB in whichever process hosted it, and
//! almost none of that is ours: AppKit, CoreText, CoreGraphics and the framework
//! data pages they dirty never give it back (measured in AGENTS.md). So the
//! desktop app opens no window but the overlay. It asks for Settings here, and
//! everything the window allocated leaves with the process.
//!
//! The desktop app starts the process through LaunchServices (`open -n`), the
//! only launch a GUI app is known to behave under on macOS 27, and which also
//! activates it and applies `LSEnvironment`. Such a process inherits no
//! descriptors, so the two talk over a private socket, one JSON value per line.
//! That connection is also the lifetime: when the desktop app goes away for any
//! reason, the Settings process sees EOF and exits.
//!
//! Settings need no message. The process writes `settings.json`, and the file
//! watcher each side already runs picks the change up.

use std::borrow::Cow;
use std::io::{BufRead as _, BufReader, IsTerminal as _, Write as _};
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use fastab_log::{LogArgs, initialize_logging};
use fastab_util::directories;
use gpui::{App, AppContext as _, Application, Entity};
use serde::{Deserialize, Serialize};
use tao::event_loop::ControlFlow;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader as AsyncBufReader};
use tokio::net::{UnixListener, UnixStream as AsyncUnixStream};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

use crate::EventLoopProxy;
use crate::event::{Event, WindowEvent};
use crate::event_loop::EventLoopReceiver;
use crate::settings_ui::{self, SettingsHandle};
use crate::webview::DASHBOARD_ID;

pub(crate) const SETTINGS_WINDOW_FLAG: &str = "--settings-window";
pub(crate) const LOG_FILE_NAME: &str = "fastab-settings.log";

/// How long a launched process has to connect before the desktop app gives up
/// on it. It connects before it opens anything, so this only bounds a launch
/// that failed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// These writes are a few dozen bytes on the UI thread. This only bounds a
/// desktop app that has stopped reading with the socket buffer already full.
const HOST_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
/// EOF quits through AppKit. A wedged UI thread must not outlive the desktop app.
const FORCED_EXIT_GRACE: Duration = Duration::from_secs(2);
const BACKGROUND_WORK_POLL: Duration = Duration::from_millis(250);

/// From the desktop app to the Settings process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum HostMessage {
    /// Bring the window forward, on `path`'s page when one is named.
    Show {
        path: Option<String>,
    },
    Close,
    /// A permission may have changed outside the window.
    RefreshPermissions,
}

impl HostMessage {
    fn into_window_event(self) -> WindowEvent {
        match self {
            Self::Show { path: None } => WindowEvent::Show,
            Self::Show { path: Some(path) } => WindowEvent::Batch(vec![
                WindowEvent::NavigateRelative { path: Cow::Owned(path) },
                WindowEvent::Show,
            ]),
            Self::Close => WindowEvent::Close,
            Self::RefreshPermissions => WindowEvent::Reload,
        }
    }
}

/// From the Settings process to the desktop app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum HelperMessage {
    /// A `Show` was carried out.
    Shown,
    /// The tray and the overlay depend on what the window just repaired.
    ReloadAccessibility,
    /// Sparkle lives in the desktop app, which is what it replaces.
    CheckForUpdates,
    /// "Quit FasTab" in the window's menu means the whole app.
    Quit,
}

fn encode(message: &impl Serialize) -> Vec<u8> {
    let mut line = serde_json::to_vec(message).expect("a message is plain data");
    line.push(b'\n');
    line
}

// The desktop app's side.

/// The desktop app's handle on a running Settings process.
pub(crate) struct SettingsProcess {
    commands: mpsc::UnboundedSender<HostMessage>,
}

impl SettingsProcess {
    /// Start a process. Messages sent before it connects wait for it. Has to be
    /// called with the tokio runtime entered, which the UI thread always is.
    pub(crate) fn launch(proxy: EventLoopProxy) -> anyhow::Result<Self> {
        let dir = directories::host_sockets_dir()?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let socket = dir.join(format!("settings-{}.sock", std::process::id()));
        std::fs::remove_file(&socket).ok();
        let listener = UnixListener::bind(&socket)?;

        let launcher = launcher_command(&socket).and_then(|mut command| command.spawn());
        let mut launcher = match launcher {
            Ok(launcher) => launcher,
            Err(err) => {
                std::fs::remove_file(&socket).ok();
                return Err(err.into());
            },
        };
        tokio::spawn(async move {
            match launcher.wait().await {
                Ok(status) if status.success() => {},
                Ok(status) => warn!(%status, "The Settings process launcher failed"),
                Err(err) => warn!(%err, "Failed to wait for the Settings process launcher"),
            }
        });

        let (commands, inbox) = mpsc::unbounded_channel();
        tokio::spawn(serve(listener, socket, inbox, proxy));
        Ok(Self { commands })
    }

    /// Hands `message` back once the process has gone, for the caller to start
    /// another.
    pub(crate) fn send(&self, message: HostMessage) -> Result<(), HostMessage> {
        self.commands.send(message).map_err(|err| err.0)
    }
}

/// The `.app` that holds the executable at `exe`, whatever it is called.
///
/// `fastab_util::app_bundle_path_opt` only knows a bundle named `Fastab.app`.
/// A renamed copy fell through to the direct spawn below, which is the launch
/// macOS 27 does not treat as a GUI app.
fn enclosing_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

fn launcher_command(socket: &Path) -> std::io::Result<tokio::process::Command> {
    let bundle = fastab_util::app_bundle_path_opt().or_else(|| {
        fastab_util::current_exe_origin()
            .ok()
            .as_deref()
            .and_then(enclosing_bundle)
    });
    let mut command = match bundle {
        Some(bundle) => {
            let mut open = tokio::process::Command::new("/usr/bin/open");
            open.arg("-n").arg(bundle).arg("--args");
            open
        },
        // A dev build outside a bundle has nothing for LaunchServices to open.
        None => tokio::process::Command::new(std::env::current_exe()?),
    };
    command
        .arg(SETTINGS_WINDOW_FLAG)
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

async fn serve(
    listener: UnixListener,
    socket: PathBuf,
    inbox: mpsc::UnboundedReceiver<HostMessage>,
    proxy: EventLoopProxy,
) {
    let accepted = tokio::time::timeout(CONNECT_TIMEOUT, listener.accept()).await;
    // One process per listener, and nothing else ever needs the name.
    drop(listener);
    std::fs::remove_file(&socket).ok();
    match accepted {
        Ok(Ok((stream, _))) => serve_connection(stream, inbox, proxy).await,
        Ok(Err(err)) => warn!(%err, "Failed to accept the Settings process"),
        Err(_) => warn!("The Settings process did not connect"),
    }
}

async fn serve_connection(
    stream: AsyncUnixStream,
    mut inbox: mpsc::UnboundedReceiver<HostMessage>,
    proxy: EventLoopProxy,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = AsyncBufReader::new(reader).lines();
    let send = |event| {
        proxy.send_event(event).ok();
    };
    // A `Show` written while the window closes reaches a process that is already
    // leaving, and nothing would open. It is repeated, but only after an earlier
    // one was answered: a process that dies before answering its first would
    // otherwise be restarted forever.
    let mut answered_before = false;
    let mut unanswered = None;
    loop {
        tokio::select! {
            message = inbox.recv() => {
                let Some(message) = message else { return };
                if let HostMessage::Show { path } = &message {
                    unanswered = Some(path.clone());
                }
                if let Err(err) = writer.write_all(&encode(&message)).await {
                    debug!(%err, "The Settings process stopped reading");
                    break;
                }
            },
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                match serde_json::from_str(&line) {
                    Ok(HelperMessage::Shown) => {
                        answered_before = true;
                        unanswered = None;
                    },
                    Ok(HelperMessage::ReloadAccessibility) => send(Event::ReloadAccessibility),
                    Ok(HelperMessage::CheckForUpdates) => send(Event::CheckForUpdates),
                    Ok(HelperMessage::Quit) => send(Event::ControlFlow(ControlFlow::Exit)),
                    Err(err) => warn!(%err, %line, "Ignoring an unreadable message from the Settings process"),
                }
            },
        }
    }
    // Close the inbox first, so that the repeat starts a new process.
    drop(inbox);
    if let Some(path) = unanswered.filter(|_| answered_before) {
        send(Event::WindowEvent {
            window_id: DASHBOARD_ID,
            window_event: HostMessage::Show { path }.into_window_event(),
        });
    }
}

// The Settings process's side. It uses blocking I/O: it lives only as long as
// its window, and a thread beats an async writer that `cx.quit()` could cut off
// before "Quit" reached the desktop app.

/// The whole of `main` for a process started with [`SETTINGS_WINDOW_FLAG`].
pub(crate) fn run_window_process(socket: &Path, runtime: &tokio::runtime::Runtime) -> ExitCode {
    #[cfg(target_os = "macos")]
    objc2::rc::autoreleasepool(|_| crate::gpui_host::ensure_gpui_ns_application());

    let _log_guard = initialize_logging(LogArgs {
        log_level: None,
        log_to_stdout: std::io::stdout().is_terminal(),
        log_file_path: directories::logs_dir().ok().map(|dir| dir.join(LOG_FILE_NAME)),
        delete_old_log_file: false,
    })
    .inspect_err(|err| eprintln!("Failed to start logging: {err:?}"))
    .ok();

    if let Err(err) = fastab_settings::settings::init_global() {
        error!(%err, "failed to init global settings");
    }

    let to_host = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(err) => {
            error!(%err, socket = %socket.display(), "Failed to reach the desktop app");
            return ExitCode::FAILURE;
        },
    };
    to_host.set_write_timeout(Some(HOST_WRITE_TIMEOUT)).ok();

    let (proxy, event_rx) = crate::event_loop::channel();
    let started = to_host.try_clone().and_then(|from_host| {
        let proxy = proxy.clone();
        std::thread::Builder::new().name("settings-host".into()).spawn(move || {
            read_host_messages(from_host, &proxy);
            info!("The desktop app has gone; leaving");
            std::thread::sleep(FORCED_EXIT_GRACE);
            #[allow(clippy::exit)]
            std::process::exit(0);
        })
    });
    if let Err(err) = started {
        error!(%err, "Failed to start reading from the desktop app");
        return ExitCode::FAILURE;
    }

    // Another process can write settings too, and a write from here saves this
    // process's whole copy, which therefore has to stay current.
    runtime.block_on(crate::file_watcher::setup_listeners(proxy.clone()));

    // Entered rather than `block_on`, for the reason in `main`.
    let _runtime = runtime.enter();
    let application = Application::new();
    let reopen = proxy.clone();
    application.on_reopen(move |_cx| {
        reopen
            .send_event(Event::WindowEvent {
                window_id: DASHBOARD_ID,
                window_event: WindowEvent::Show,
            })
            .ok();
    });
    application.run(move |cx: &mut App| {
        #[cfg(target_os = "macos")]
        {
            crate::webview::menu::install_actions(cx, proxy.clone());
            crate::webview::menu::install_native(cx);
        }
        cx.on_window_closed(|cx| {
            // GPUI notifies before the removed window finishes dropping.
            cx.defer(exit_when_idle);
        })
        .detach();
        let host = cx.new(|_| SettingsHost {
            proxy,
            window: None,
            to_host,
        });
        pump_events(host, event_rx, cx);
    });
    ExitCode::SUCCESS
}

/// Returns at EOF, having asked the UI thread to quit.
fn read_host_messages(stream: UnixStream, proxy: &EventLoopProxy) {
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        match serde_json::from_str::<HostMessage>(&line) {
            Ok(message) => {
                let event = Event::WindowEvent {
                    window_id: DASHBOARD_ID,
                    window_event: message.into_window_event(),
                };
                if proxy.send_event(event).is_err() {
                    return;
                }
            },
            Err(err) => warn!(%err, %line, "Ignoring an unreadable message from the desktop app"),
        }
    }
    proxy.send_event(Event::ControlFlow(ControlFlow::Exit)).ok();
}

fn pump_events(host: Entity<SettingsHost>, event_rx: EventLoopReceiver, cx: &mut App) {
    cx.spawn(async move |cx| {
        while let Ok(event) = event_rx.recv_queued().await {
            let result = event.dispatch(|event, _sequence| {
                cx.update(|cx| {
                    host.update(cx, |host, cx| host.dispatch(event, cx));
                })
            });
            if let Err(err) = result {
                error!(%err, "GPUI app released while dispatching a Settings event");
                break;
            }
        }
    })
    .detach();
}

/// Leave once no window is left. A repair, or the Accessibility guide one
/// started, keeps going without the window, as it did when closing Settings left
/// the app running: an install must not stop halfway.
fn exit_when_idle(cx: &mut App) {
    if !cx.windows().is_empty() {
        return;
    }
    cx.spawn(async move |cx| {
        while crate::permissions::repair_is_running() {
            cx.background_executor().timer(BACKGROUND_WORK_POLL).await;
        }
        cx.update(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .ok();
    })
    .detach();
}

struct SettingsHost {
    proxy: EventLoopProxy,
    window: Option<SettingsHandle>,
    to_host: UnixStream,
}

impl SettingsHost {
    fn dispatch(&mut self, event: Event, cx: &mut App) {
        match event {
            Event::WindowEvent {
                window_id,
                window_event,
            } if window_id == DASHBOARD_ID => self.dispatch_window_event(window_event, cx),
            Event::PermissionSnapshot(snapshot) => {
                if let Some(window) = &self.window {
                    settings_ui::apply_permission_snapshot(window, snapshot, cx);
                }
            },
            Event::ReloadCredentials => {
                if let Some(window) = &self.window {
                    window.update(cx, |_view, _window, cx| cx.notify()).ok();
                }
            },
            Event::ReloadAccessibility => self.tell_host(HelperMessage::ReloadAccessibility),
            Event::CheckForUpdates => self.tell_host(HelperMessage::CheckForUpdates),
            Event::ControlFlow(ControlFlow::Exit) => {
                // Goes nowhere when EOF is what brought us here.
                self.tell_host(HelperMessage::Quit);
                cx.quit();
            },
            other => trace!(?other, "Ignoring an event meant for the desktop app"),
        }
    }

    fn dispatch_window_event(&mut self, event: WindowEvent, cx: &mut App) {
        match event {
            WindowEvent::Show => self.show(cx),
            WindowEvent::NavigateRelative { path } => {
                self.show(cx);
                if let Some(window) = &self.window {
                    settings_ui::set_settings_section(window, path.as_ref(), cx);
                }
            },
            WindowEvent::Close => {
                if let Some(window) = self.window.take() {
                    settings_ui::close_settings(&window, cx);
                }
            },
            WindowEvent::Reload => {
                if let Some(window) = &self.window {
                    settings_ui::refresh_permission_check(window, &self.proxy, cx);
                }
            },
            WindowEvent::Batch(events) => {
                for event in events {
                    self.dispatch_window_event(event, cx);
                }
            },
            other => trace!(?other, "Ignoring a window event Settings has no use for"),
        }
    }

    fn show(&mut self, cx: &mut App) {
        if let Some(window) = &self.window {
            if settings_ui::focus_settings(window, cx) {
                settings_ui::refresh_permission_check(window, &self.proxy, cx);
                self.tell_host(HelperMessage::Shown);
                return;
            }
        }
        match settings_ui::open_settings_window(cx, self.proxy.clone()) {
            Ok(window) => {
                settings_ui::refresh_permission_check(&window, &self.proxy, cx);
                self.window = Some(window);
                self.tell_host(HelperMessage::Shown);
            },
            Err(err) => {
                error!(%err, "Failed to open native settings");
                self.window = None;
                exit_when_idle(cx);
            },
        }
    }

    fn tell_host(&mut self, message: HelperMessage) {
        if let Err(err) = self.to_host.write_all(&encode(&message)) {
            debug!(%err, ?message, "The desktop app stopped reading");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::time::Duration;

    use clap::Parser as _;
    use tao::event_loop::ControlFlow;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader as AsyncBufReader};
    use tokio::net::UnixStream as AsyncUnixStream;
    use tokio::sync::mpsc;

    use super::{
        DASHBOARD_ID, Event, EventLoopReceiver, HelperMessage, HostMessage, SETTINGS_WINDOW_FLAG, SettingsProcess,
        UnixStream, WindowEvent, encode, read_host_messages, serve_connection,
    };

    async fn next_event(events: &EventLoopReceiver) -> Event {
        tokio::time::timeout(Duration::from_secs(5), events.recv_async())
            .await
            .expect("an event arrives")
            .expect("the channel is open")
    }

    /// The channel disconnects once the served connection has ended, which also
    /// means nothing is coming.
    async fn assert_no_event(events: &EventLoopReceiver) {
        let event = tokio::time::timeout(Duration::from_millis(100), events.recv_async()).await;
        assert!(!matches!(event, Ok(Ok(_))), "unexpected event: {event:?}");
    }

    /// The channel's ends, plus the far end of a served connection.
    fn serve_pair() -> (SettingsProcess, EventLoopReceiver, AsyncUnixStream) {
        let (proxy, events) = crate::event_loop::channel();
        let (ours, theirs) = AsyncUnixStream::pair().expect("socket pair");
        let (commands, inbox) = mpsc::unbounded_channel();
        tokio::spawn(serve_connection(ours, inbox, proxy));
        (SettingsProcess { commands }, events, theirs)
    }

    async fn wait_until_gone(process: &SettingsProcess) {
        tokio::time::timeout(Duration::from_secs(5), process.commands.closed())
            .await
            .expect("the connection ended");
    }

    /// `scripts/settings-process-smoke.py` speaks this too.
    #[test]
    fn a_message_is_one_json_line_whatever_its_path_holds() {
        assert_eq!(
            encode(&HostMessage::Show {
                path: Some("/a\nb".into())
            }),
            b"{\"Show\":{\"path\":\"/a\\nb\"}}\n"
        );
        assert_eq!(
            encode(&HostMessage::Show { path: None }),
            b"{\"Show\":{\"path\":null}}\n"
        );
        assert_eq!(encode(&HostMessage::Close), b"\"Close\"\n");
        assert_eq!(encode(&HostMessage::RefreshPermissions), b"\"RefreshPermissions\"\n");
        assert_eq!(encode(&HelperMessage::Shown), b"\"Shown\"\n");
        assert_eq!(
            serde_json::from_str::<HostMessage>("{\"Show\":{\"path\":\"/a\\nb\"}}").unwrap(),
            HostMessage::Show {
                path: Some("/a\nb".into())
            }
        );
    }

    #[test]
    fn a_host_message_becomes_the_window_event_settings_handles() {
        assert!(matches!(
            HostMessage::Show { path: None }.into_window_event(),
            WindowEvent::Show
        ));
        let WindowEvent::Batch(events) = HostMessage::Show {
            path: Some("/about".into()),
        }
        .into_window_event() else {
            panic!("a path navigates and then shows");
        };
        assert!(matches!(
            events.as_slice(),
            [WindowEvent::NavigateRelative { path }, WindowEvent::Show] if path == "/about"
        ));
        assert!(matches!(HostMessage::Close.into_window_event(), WindowEvent::Close));
        assert!(matches!(
            HostMessage::RefreshPermissions.into_window_event(),
            WindowEvent::Reload
        ));
    }

    #[test]
    fn a_renamed_bundle_is_still_opened_as_a_bundle() {
        use std::path::Path;

        let bundle = |exe: &str| super::enclosing_bundle(Path::new(exe));
        assert_eq!(
            bundle("/Applications/Fastab.app/Contents/MacOS/fastab"),
            Some(PathBuf::from("/Applications/Fastab.app"))
        );
        assert_eq!(
            bundle("/Users/me/Apps/Fastab 2.app/Contents/MacOS/fastab"),
            Some(PathBuf::from("/Users/me/Apps/Fastab 2.app"))
        );
        // A development build has no bundle to open.
        assert_eq!(bundle("/repo/target/debug/fastab"), None);
        assert_eq!(bundle("/repo/Contents/MacOS/fastab"), None);
        assert_eq!(bundle("/Fastab.app/Contents/Helpers/fastab"), None);
    }

    #[test]
    fn the_launcher_flag_is_the_one_the_cli_parses() {
        let cli = crate::cli::Cli::try_parse_from(["fastab", SETTINGS_WINDOW_FLAG, "/tmp/settings.sock"]).unwrap();
        assert_eq!(cli.settings_window, Some(PathBuf::from("/tmp/settings.sock")));
        assert_eq!(
            crate::cli::Cli::try_parse_from(["fastab"]).unwrap().settings_window,
            None
        );
    }

    /// Opening Settings in the desktop app is what this module exists to stop:
    /// a window, or a main menu, costs it memory it never gets back.
    #[test]
    fn the_desktop_app_hosts_no_settings_window_and_no_menu() {
        let production = include_str!("gpui_host.rs")
            .split("#[cfg(all(test")
            .next()
            .expect("production source");
        for forbidden in ["open_settings_window", "install_native", "install_actions", "set_menus"] {
            assert!(!production.contains(forbidden), "gpui_host.rs must not use {forbidden}");
        }
        assert!(production.contains("ActivationPolicy::Accessory"));
    }

    #[tokio::test]
    async fn what_the_settings_process_asks_for_reaches_the_event_loop() {
        let (_process, events, mut helper) = serve_pair();
        helper
            .write_all(b"\"ReloadAccessibility\"\nnot json\n\"CheckForUpdates\"\n\"Quit\"\n")
            .await
            .unwrap();

        assert!(matches!(next_event(&events).await, Event::ReloadAccessibility));
        assert!(matches!(next_event(&events).await, Event::CheckForUpdates));
        assert!(matches!(
            next_event(&events).await,
            Event::ControlFlow(ControlFlow::Exit)
        ));
    }

    #[tokio::test]
    async fn messages_sent_before_it_connects_arrive_in_order() {
        let (proxy, _events) = crate::event_loop::channel();
        let (ours, theirs) = AsyncUnixStream::pair().unwrap();
        let (commands, inbox) = mpsc::unbounded_channel();
        let process = SettingsProcess { commands };
        process
            .send(HostMessage::Show {
                path: Some("/about".into()),
            })
            .unwrap();
        process.send(HostMessage::RefreshPermissions).unwrap();
        tokio::spawn(serve_connection(ours, inbox, proxy));

        let mut lines = AsyncBufReader::new(theirs).lines();
        assert_eq!(
            lines.next_line().await.unwrap().unwrap(),
            "{\"Show\":{\"path\":\"/about\"}}"
        );
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "\"RefreshPermissions\"");
    }

    #[tokio::test]
    async fn a_show_lost_to_a_closing_window_is_repeated_for_a_new_process() {
        let (process, events, helper) = serve_pair();
        let (reader, mut writer) = helper.into_split();
        let mut lines = AsyncBufReader::new(reader).lines();

        process.send(HostMessage::Show { path: None }).unwrap();
        lines.next_line().await.unwrap().unwrap();
        writer.write_all(b"\"Shown\"\n").await.unwrap();
        // Answered, so the next one is known to reach a working process. Give
        // the answer time to be read before sending.
        tokio::time::sleep(Duration::from_millis(50)).await;

        process
            .send(HostMessage::Show {
                path: Some("/about".into()),
            })
            .unwrap();
        lines.next_line().await.unwrap().unwrap();
        drop((lines, writer));

        wait_until_gone(&process).await;
        assert!(process.send(HostMessage::Close).is_err());
        let Event::WindowEvent {
            window_id,
            window_event,
        } = next_event(&events).await
        else {
            panic!("the show is repeated");
        };
        assert!(window_id == DASHBOARD_ID);
        let WindowEvent::Batch(repeated) = window_event else {
            panic!("the repeat keeps its page");
        };
        assert!(matches!(
            repeated.as_slice(),
            [WindowEvent::NavigateRelative { path }, WindowEvent::Show] if path == "/about"
        ));
    }

    /// Otherwise a process that crashes on startup is restarted forever.
    #[tokio::test]
    async fn a_process_that_never_answered_is_not_started_again() {
        let (process, events, helper) = serve_pair();
        process.send(HostMessage::Show { path: None }).unwrap();
        let mut lines = AsyncBufReader::new(helper).lines();
        lines.next_line().await.unwrap().unwrap();
        drop(lines);

        wait_until_gone(&process).await;
        assert_no_event(&events).await;
    }

    #[tokio::test]
    async fn a_window_closed_by_hand_opens_nothing_and_the_next_show_starts_over() {
        let (process, events, helper) = serve_pair();
        drop(helper);

        wait_until_gone(&process).await;
        assert_no_event(&events).await;
        assert_eq!(
            process.send(HostMessage::Show { path: None }),
            Err(HostMessage::Show { path: None })
        );
    }

    #[tokio::test]
    async fn the_settings_process_quits_when_the_desktop_app_goes_away() {
        let (proxy, events) = crate::event_loop::channel();
        let (mut host, helper) = UnixStream::pair().unwrap();
        host.write_all(b"\"Close\"\nnot json\n{\"Show\":{\"path\":null}}\n")
            .unwrap();
        drop(host);

        read_host_messages(helper, &proxy);

        let close = next_event(&events).await;
        assert!(matches!(
            close,
            Event::WindowEvent {
                window_event: WindowEvent::Close,
                ..
            }
        ));
        let show = next_event(&events).await;
        assert!(matches!(
            show,
            Event::WindowEvent {
                window_event: WindowEvent::Show,
                ..
            }
        ));
        assert!(matches!(
            next_event(&events).await,
            Event::ControlFlow(ControlFlow::Exit)
        ));
    }
}

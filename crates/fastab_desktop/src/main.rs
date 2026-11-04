mod cli;
mod event;
mod event_loop;
mod gpui_host;
mod heap_trim;
#[cfg(target_os = "macos")]
mod ime_watch;
mod jev;
mod overlay;
#[cfg(test)]
mod overlay_session;
mod permissions;
mod settings_refresh;
mod settings_ui;
// mod figterm;
mod file_watcher;
mod install;
mod local_ipc;
mod notification_bus;
mod platform;
mod remote_ipc;
mod tray;
mod update;
mod utils;
mod webview;

use std::io::IsTerminal as _;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use event::Event;
use fastab_log::{LogArgs, initialize_logging};
use fastab_os_shim::Context;
#[cfg(target_os = "linux")]
use fastab_util::consts::APP_PROCESS_NAME;
use fastab_util::consts::PRODUCT_NAME;
use fastab_util::{directories, is_product_url_scheme};
#[cfg(target_os = "linux")]
use sysinfo::get_current_pid;
#[cfg(target_os = "linux")]
use sysinfo::{ProcessRefreshKind, RefreshKind, System};
use tracing::{error, warn};
use url::Url;
use webview::WebviewManager;
pub use webview::{AUTOCOMPLETE_ID, AUTOCOMPLETE_WINDOW_TITLE, DASHBOARD_ID};

pub use event_loop::{EventLoopClosed, EventLoopProxy, EventLoopWindowTarget};

/// What the async prelude hands back so GPUI can be started from plain `main`.
struct Launch {
    setup: gpui_host::Setup,
    /// Flushes the log file on drop, so it has to outlive `NSApplication::run`.
    _log_guard: fastab_log::LogGuard,
}

fn main() -> ExitCode {
    fastab_util::malloc::start_space_efficient();
    fastab_util::build_identity::initialize();
    // The desktop process is I/O bound: GPUI owns the UI thread, and the
    // completion engine has its own worker. A worker per core just parks
    // stacks — the same waste ecterm already stopped.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .thread_name("ec-tokio")
        .enable_all()
        .build()
        .expect("tokio runtime");

    // The prelude runs on this thread before AppKit starts managing event
    // pools. Drain its temporaries before entering the long-lived UI loop.
    // `block_on` polls its root future here; the pool never migrates with a
    // spawned worker future or spans `NSApplication::run`.
    #[cfg(target_os = "macos")]
    let launch = objc2::rc::autoreleasepool(|_| runtime.block_on(async_main()));
    #[cfg(not(target_os = "macos"))]
    let launch = runtime.block_on(async_main());
    let Launch { setup, _log_guard } = match launch {
        Ok(launch) => launch,
        Err(exit_code) => return exit_code,
    };

    // Stay *entered* in the runtime so `Handle::current()` and `tokio::spawn`
    // keep working from AppKit callbacks, but do not run the UI loop inside
    // `block_on`. `block_on` hands the future it polls a fixed cooperative
    // budget — 128 units — that is only refilled when that poll returns, and
    // `NSApplication::run` never returned from it. Every tokio channel that
    // resolved on this thread spent a unit; once they were gone each further
    // poll reported Pending, woke itself, and GPUI's executor re-queued it
    // onto the main queue it was already draining. That livelock pinned the
    // desktop process at 100% CPU with the overlay frozen on `···`.
    let _runtime = runtime.enter();
    heap_trim::start();
    match gpui_host::start_application(setup) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(%err, "GPUI host failed");
            ExitCode::FAILURE
        },
    }
}

/// launchd starts the app with a 256-descriptor soft limit. One remote IPC
/// connection per terminal pane (up to 128), local connections (up to 64),
/// SQLite, logs, Metal and the pipes of running hook scripts share it, so a
/// large session could hit EMFILE and lose completions in new panes. Raise
/// the soft limit toward the hard one; `OPEN_MAX` is the most macOS accepts.
#[cfg(target_os = "macos")]
fn raise_open_file_limit() {
    const OPEN_MAX: libc::rlim_t = 10_240;

    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit only read or write the struct passed in.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        warn!(err = %std::io::Error::last_os_error(), "Failed to read the open file limit");
        return;
    }
    let target = OPEN_MAX.min(limit.rlim_max);
    if limit.rlim_cur >= target {
        return;
    }
    let previous = limit.rlim_cur;
    limit.rlim_cur = target;
    // SAFETY: as above; the soft limit stays at or below the hard one.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        warn!(err = %std::io::Error::last_os_error(), previous, target, "Failed to raise the open file limit");
    } else {
        tracing::info!(previous, target, "Raised the open file limit");
    }
}

/// Everything that has to happen before the UI loop starts. `Err` carries the
/// exit code for the paths that stop short of launching.
async fn async_main() -> Result<Launch, ExitCode> {
    #[cfg(target_os = "macos")]
    gpui_host::ensure_gpui_ns_application();

    let cli = cli::Cli::parse();

    #[cfg(target_os = "macos")]
    if cli.unregister_login_item {
        return Err(match fastab_integrations::login_item::set_enabled(false) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("Failed to unregister launch at login: {err}");
                ExitCode::FAILURE
            },
        });
    }

    let log_guard = initialize_logging(LogArgs {
        log_level: None,
        // A LaunchServices launch points stdout at /dev/null, where the stdout
        // layer formatted every event twice and kept its own writer thread.
        log_to_stdout: std::io::stdout().is_terminal(),
        log_file_path: Some(
            directories::logs_dir()
                .expect("home dir must be set")
                .join("fastab.log"),
        ),
        delete_old_log_file: false,
    })
    .expect("Failed to init logging");

    #[cfg(target_os = "macos")]
    raise_open_file_limit();

    // macOS Tahoe's autofill heuristic controller attaches an "AutoFill (…)" helper to
    // any app with text input. SMS / contact autofill is not useful here, and the helper
    // is pure overhead — same rationale as Ghostty.
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSUserDefaults, ns_string};
        // SAFETY: standardUserDefaults and setBool_forKey are thread-safe Foundation APIs.
        unsafe {
            NSUserDefaults::standardUserDefaults()
                .setBool_forKey(false, ns_string!("NSAutoFillHeuristicControllerEnabled"));
        }
    }

    fastab_telemetry::init(
        option_env!("POSTHOG_ENDPOINT").unwrap_or(""),
        option_env!("POSTHOG_API_KEY").unwrap_or(""),
    );

    if let Err(err) = fastab_settings::settings::init_global() {
        error!(%err, "failed to init global settings");
    }

    let mut launch_on_startup = fastab_settings::settings::get_bool_or("app.launchOnStartup", false);
    #[cfg(target_os = "macos")]
    {
        const LOGIN_ITEM_MIGRATED_KEY: &str = "desktop.loginItemMigratedToSMAppService";

        if fastab_integrations::login_item::supports_modern_login_item() {
            let migrated = fastab_settings::state::get_bool_or(LOGIN_ITEM_MIGRATED_KEY, false);
            let result = if migrated {
                // Once migrated, System Settings is authoritative. This prevents
                // the app from silently re-registering after the user disables it there.
                fastab_integrations::login_item::is_enabled().and_then(|enabled| {
                    launch_on_startup = enabled;
                    fastab_integrations::login_item::reconcile(enabled)
                })
            } else {
                fastab_integrations::login_item::reconcile(launch_on_startup)
            };

            if let Err(err) = result {
                warn!(%err, "failed to migrate launch-at-login registration");
                launch_on_startup = false;
            } else if let Ok(enabled) = fastab_integrations::login_item::is_enabled() {
                launch_on_startup = enabled;
            }

            fastab_settings::settings::set_value("app.launchOnStartup", launch_on_startup).ok();
            fastab_settings::state::set_value(LOGIN_ITEM_MIGRATED_KEY, true).ok();
        } else if let Err(err) = fastab_integrations::login_item::reconcile(launch_on_startup) {
            warn!(%err, "failed to reconcile legacy launch-at-login registration");
        }
    }

    if cli.is_startup && !launch_on_startup {
        return Err(ExitCode::SUCCESS);
    }

    let page = parse_url_page(cli.url_link.as_deref())?;

    if !cli.allow_multiple {
        #[cfg(target_os = "macos")]
        if let Some(exit_code) = allow_multiple_running_check(std::process::id(), cli.kill_old, page.clone()).await {
            return Err(exit_code);
        }
        #[cfg(target_os = "linux")]
        match get_current_pid() {
            Ok(current_pid) => {
                if let Some(exit_code) = allow_multiple_running_check(current_pid, cli.kill_old, page.clone()).await {
                    return Err(exit_code);
                }
            },
            Err(err) => warn!(%err, "Failed to get pid"),
        }
    }

    #[cfg(target_os = "macos")]
    if let Ok(current_exe) = fastab_util::current_exe_origin() {
        if let Ok(statvfs) = nix::sys::statvfs::statvfs(&current_exe) {
            if statvfs.flags().contains(nix::sys::statvfs::FsFlags::ST_RDONLY) {
                rfd::MessageDialog::new()
                    .set_title("Error")
                    .set_description(
                        format!("Cannot execute {PRODUCT_NAME} from within a readonly volume. Please move {PRODUCT_NAME} to your applications folder and try again.")
                    )
                    .show();

                return Err(ExitCode::FAILURE);
            }
        }
    }

    let ctx = Context::new();
    install::run_install(
        Arc::clone(&ctx),
        cli.ignore_immediate_update,
        !cli.no_dashboard,
        cli.is_startup,
    )
    .await;

    // Daily active-device heartbeat; also flushes locally aggregated counters
    // (autocomplete_shown/accepted etc.) as properties of the heartbeat event.
    // Unconfigured builds never send, so do not keep an hourly sleeper around.
    if fastab_telemetry::is_configured() {
        tokio::spawn(async {
            loop {
                fastab_telemetry::maybe_send_daily_heartbeat().await;
                tokio::time::sleep(std::time::Duration::from_secs(60 * 60)).await;
            }
        });
    }

    #[cfg(target_os = "linux")]
    {
        match fastab_os_shim::Env::new().q_backend().ok().as_deref() {
            Some("default") => {},
            // SAFETY: we are calling set_var in a single-threaded context.
            Some(backend) => unsafe { std::env::set_var("GDK_BACKEND", backend) },
            None => unsafe { std::env::set_var("GDK_BACKEND", "x11") },
        }

        platform::gtk::init().expect("Failed initializing GTK");
    }

    // A deep link names the page to open, so it outranks the setting.
    let silent_launch = page.is_none() && fastab_settings::settings::get_bool_or("app.silentLaunch", false);

    #[cfg(target_os = "macos")]
    let defer_dashboard_for_modern_login_item = !cli.no_dashboard
        && !silent_launch
        && launch_on_startup
        && fastab_integrations::login_item::supports_modern_login_item();
    #[cfg(not(target_os = "macos"))]
    let defer_dashboard_for_modern_login_item = false;
    let visible = !cli.no_dashboard && !silent_launch && !defer_dashboard_for_modern_login_item;

    let webview_manager = WebviewManager::new(ctx, visible, defer_dashboard_for_modern_login_item);
    let auto_updates_enabled = !fastab_settings::settings::get_bool_or("app.disableAutoupdates", false);
    if auto_updates_enabled {
        // start_automatic_checks dispatches to the main thread asynchronously,
        // so the controller is created once the event loop is running.
        update::start_automatic_checks();
        // Delay the explicit background check so the event loop is already
        // processing the main GCD queue before exec_sync is called.
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let _ = update::check_for_update(false, false).await;
        });
    }
    let setup = webview_manager.prepare().await.expect("desktop services");
    Ok(Launch {
        setup,
        _log_guard: log_guard,
    })
}

fn parse_url_page(url: Option<&str>) -> Result<Option<String>, ExitCode> {
    let Some(url) = url else {
        return Ok(None);
    };

    let url = match Url::parse(url) {
        Ok(url) => url,
        Err(err) => {
            error!(%err, %url, "Failed to parse url");
            return Err(ExitCode::FAILURE);
        },
    };

    if !is_product_url_scheme(url.scheme()) {
        error!(scheme = %url.scheme(), %url, "Invalid scheme");
        return Err(ExitCode::FAILURE);
    }

    Ok(url.host_str().and_then(|s| match s {
        "dashboard" => Some(url.path().to_owned()),
        _ => {
            error!("Invalid deep link");
            None
        },
    }))
}

#[cfg(target_os = "linux")]
#[must_use]
async fn allow_multiple_running_check(
    current_pid: sysinfo::Pid,
    kill_old: bool,
    page: Option<String>,
) -> Option<ExitCode> {
    use std::ffi::OsString;

    use tracing::debug;

    if kill_old {
        eprintln!("Option kill-old is not supported on Linux.");
        return Some(ExitCode::SUCCESS);
    }

    let system = System::new_with_specifics(
        RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing().with_user(sysinfo::UpdateKind::Always)),
    );
    let app_process_name = OsString::from(APP_PROCESS_NAME);
    let processes = system.processes_by_exact_name(&app_process_name);

    let processes = processes.collect::<Vec<_>>();
    debug!("Checking for already running desktop instance: {:?}", processes);

    let current_user_id = nix::unistd::getuid().as_raw();
    for process in processes {
        let pid = process.pid();
        let uid = process.user_id().map(|uid| uid as &u32);
        match (process.parent(), uid) {
            // The Linux desktop app returns multiple processes with the same name for some reason.
            (Some(parent_pid), Some(uid))
                if pid != current_pid && parent_pid != current_pid && *uid == current_user_id =>
            {
                let exe = process.exe().unwrap_or(Path::new("")).display();
                eprintln!("{PRODUCT_NAME} is already running: {exe} (pid={pid}, uid={uid})");

                match &page {
                    Some(page) => {
                        eprintln!("Opening /{page}...");
                        Some(page)
                    },
                    None => {
                        eprintln!("Opening {PRODUCT_NAME} Window...");
                        None
                    },
                };

                if let Err(err) =
                    fastab_ipc::local::open_ui_element(fastab_proto::local::UiElement::MissionControl, page).await
                {
                    eprintln!("Failed to open {PRODUCT_NAME}: {err}");
                }

                return Some(ExitCode::SUCCESS);
            },
            _ => (),
        }
    }
    None
}

/// True once every one of `pids` is gone, false if `timeout` runs out first.
///
/// `kill(pid, 0)` for liveness: these are other app instances, not children, so
/// there is nothing to reap and no zombie to mistake for a live process.
#[cfg(target_os = "macos")]
async fn wait_for_exit(pids: &[i32], timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let alive = pids
            .iter()
            .any(|&pid| nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok());
        if !alive {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// SIGTERM the old instances and return only once they are actually gone,
/// SIGKILL-ing whatever ignored the first signal.
///
/// `--kill-old` is immediately followed by this process binding the desktop
/// socket, and the old instance holds it until it exits. Signalling without
/// waiting reads as "it killed the old app but the new one never came up".
#[cfg(target_os = "macos")]
async fn stop_instances(pids: &[i32]) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    for &pid in pids {
        kill(Pid::from_raw(pid), Signal::SIGTERM).ok();
        eprintln!("Killing instance: pid={pid}");
    }

    if wait_for_exit(pids, std::time::Duration::from_secs(2)).await {
        return;
    }

    eprintln!("Instance ignored SIGTERM; sending SIGKILL");
    for &pid in pids {
        kill(Pid::from_raw(pid), Signal::SIGKILL).ok();
    }
    wait_for_exit(pids, std::time::Duration::from_millis(500)).await;
}

#[cfg(target_os = "macos")]
#[must_use]
async fn allow_multiple_running_check(current_pid: u32, kill_old: bool, page: Option<String>) -> Option<ExitCode> {
    // AppKit already knows our bundle. Enumerating every process through
    // sysinfo was loading a full process table on every launch.
    let current = i32::try_from(current_pid).ok()?;
    let others: Vec<i32> = macos_utils::applications::running_application_pids(fastab_util::consts::APP_BUNDLE_ID)
        .into_iter()
        .filter(|&pid| pid != current && pid > 0)
        .collect();

    if others.is_empty() {
        return None;
    }

    if kill_old {
        stop_instances(&others).await;
        return None;
    }

    let pid = others[0];
    eprintln!("{PRODUCT_NAME} is already running: pid={pid}");
    match &page {
        Some(page) => eprintln!("Opening /{page}..."),
        None => eprintln!("Opening {PRODUCT_NAME} Window..."),
    }
    if let Err(err) = fastab_ipc::local::open_ui_element(fastab_proto::local::UiElement::MissionControl, page).await {
        eprintln!("Failed to open {PRODUCT_NAME}: {err}");
    }
    Some(ExitCode::SUCCESS)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::time::Duration;

    use super::wait_for_exit;

    #[test]
    fn macos_does_not_pull_sysinfo_just_to_read_our_pid() {
        let manifest = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
        let macos = manifest
            .split("[target.'cfg(target_os=\"macos\")'.dependencies]")
            .nth(1)
            .unwrap_or(manifest);
        assert!(
            !macos.contains("sysinfo"),
            "sysinfo on macOS used to load a process table on every launch"
        );
        assert!(
            manifest.contains("target_os = \"linux\"") && manifest.contains("sysinfo.workspace"),
            "Linux still needs sysinfo to find the other desktop instance"
        );
    }

    /// `--kill-old` binds the desktop socket as soon as this returns, so the
    /// wait has to outlive the process and not merely the signal.
    #[tokio::test]
    async fn a_live_process_is_waited_for_and_a_reaped_one_is_not() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = i32::try_from(child.id()).expect("pid fits");

        assert!(!wait_for_exit(&[pid], Duration::from_millis(50)).await);

        child.kill().expect("kill");
        // Reaped here only because this one *is* our child; a zombie answers
        // `kill(pid, 0)` and would read as still running.
        child.wait().expect("reap");
        assert!(wait_for_exit(&[pid], Duration::from_millis(500)).await);
    }
}

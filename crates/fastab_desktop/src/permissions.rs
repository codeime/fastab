//! Native permission checks used by the settings gate (replaces the dashboard WebView gate).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use fastab_desktop_api::requests::install::install;
use fastab_os_shim::{Context, ContextArcProvider, ContextProvider};
use fastab_proto::fig::install_response::{InstallationStatus, Response};
use fastab_proto::fig::result::Result as ProtoResultEnum;
use fastab_proto::fig::server_originated_message::Submessage as ServerOriginatedSubMessage;
use fastab_proto::fig::{InstallAction, InstallComponent, InstallRequest};
use fastab_settings::State;
use fastab_settings::settings::{Settings, SettingsProvider};
use fastab_settings::state::StateProvider;
use tracing::warn;

use crate::EventLoopProxy;
use crate::event::Event;

static NEXT_PERMISSION_GENERATION: AtomicU64 = AtomicU64::new(1);
static RUNNING_REPAIRS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermId {
    Accessibility,
    Shell,
    InputMethod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermReady {
    Checking,
    Ready,
    Missing,
    Error,
}

#[derive(Clone, Debug, Default)]
pub struct PermissionSnapshot {
    pub accessibility: PermReady,
    pub shell: PermReady,
    pub input_method: PermReady,
    pub error: Option<String>,
    /// Identifies one required check and its optional input-method enrichment.
    /// A newer generation supersedes every older snapshot, even when all
    /// readiness values happen to be identical.
    pub generation: u64,
    /// Set on the post-repair required snapshot so a late IME fill-in cannot
    /// clear an in-flight Grant / Fix All.
    pub completes_repair: bool,
}

impl Default for PermReady {
    fn default() -> Self {
        Self::Checking
    }
}

impl PermissionSnapshot {
    pub fn checking() -> Self {
        Self {
            accessibility: PermReady::Checking,
            shell: PermReady::Checking,
            input_method: PermReady::Checking,
            error: None,
            generation: 0,
            completes_repair: false,
        }
    }

    /// Accessibility and Shell. Input Method is optional and does not block settings.
    pub fn all_ready(&self) -> bool {
        self.accessibility == PermReady::Ready && self.shell == PermReady::Ready
    }

    pub fn still_checking(&self) -> bool {
        matches!(self.accessibility, PermReady::Checking) || matches!(self.shell, PermReady::Checking)
    }
}

struct InstallCtx {
    settings: Settings,
    state: State,
    ctx: Arc<Context>,
}

impl SettingsProvider for InstallCtx {
    fn settings(&self) -> &Settings {
        &self.settings
    }
}

impl StateProvider for InstallCtx {
    fn state(&self) -> &State {
        &self.state
    }
}

impl ContextProvider for InstallCtx {
    fn context(&self) -> &Context {
        self.ctx.as_ref()
    }
}

impl ContextArcProvider for InstallCtx {
    fn context_arc(&self) -> Arc<Context> {
        Arc::clone(&self.ctx)
    }
}

fn install_ctx() -> InstallCtx {
    InstallCtx {
        settings: Settings::new(),
        state: State::new(),
        ctx: Context::new(),
    }
}

fn component(id: PermId) -> InstallComponent {
    match id {
        PermId::Accessibility => InstallComponent::Accessibility,
        PermId::Shell => InstallComponent::Dotfiles,
        PermId::InputMethod => InstallComponent::InputMethod,
    }
}

fn status_from_message(msg: ServerOriginatedSubMessage) -> Result<bool, String> {
    match msg {
        ServerOriginatedSubMessage::InstallResponse(response) => match response.response {
            Some(Response::InstallationStatus(status)) => {
                let installed: i32 = InstallationStatus::Installed.into();
                Ok(status == installed)
            },
            Some(Response::Result(result)) => {
                let ok: i32 = ProtoResultEnum::Ok.into();
                if result.result == ok {
                    Ok(true)
                } else {
                    Err(result.error.unwrap_or_else(|| "Install failed".into()))
                }
            },
            None => Err("Empty install response".into()),
        },
        ServerOriginatedSubMessage::Error(err) => Err(err),
        other => Err(format!("Unexpected install response: {other:?}")),
    }
}

async fn query(id: PermId, action: InstallAction) -> Result<bool, String> {
    let ctx = install_ctx();
    let request = InstallRequest {
        component: component(id).into(),
        action: action.into(),
    };
    match install(request, &ctx).await {
        Ok(msg) => status_from_message(*msg),
        Err(err) => Err(err.to_string()),
    }
}

fn ready_from(result: Result<bool, String>) -> (PermReady, Option<String>) {
    match result {
        Ok(true) => (PermReady::Ready, None),
        Ok(false) => (PermReady::Missing, None),
        Err(err) => (PermReady::Error, Some(err)),
    }
}

async fn check_required() -> PermissionSnapshot {
    let (ax, ax_err) = ready_from(query(PermId::Accessibility, InstallAction::Status).await);
    let (shell, shell_err) = ready_from(query(PermId::Shell, InstallAction::Status).await);
    PermissionSnapshot {
        accessibility: ax,
        shell,
        input_method: PermReady::Checking,
        error: ax_err.or(shell_err),
        generation: 0,
        completes_repair: false,
    }
}

async fn with_input_method(mut snapshot: PermissionSnapshot) -> PermissionSnapshot {
    let (ime, ime_err) = ready_from(query(PermId::InputMethod, InstallAction::Status).await);
    snapshot.input_method = ime;
    if snapshot.error.is_none() {
        snapshot.error = ime_err;
    }
    snapshot
}

#[cfg(target_os = "macos")]
fn dashboard_language_zh() -> Option<bool> {
    match fastab_settings::settings::get_string_or("dashboard.language", "system".into()).as_str() {
        "zh-CN" | "zh" => Some(true),
        "en" => Some(false),
        _ => None,
    }
}

pub async fn repair(id: PermId) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if id == PermId::Accessibility {
        if macos_utils::accessibility::accessibility_is_enabled() {
            return Ok(());
        }
        macos_utils::accessibility::begin_accessibility_guide(dashboard_language_zh());
        let start_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if macos_utils::accessibility::accessibility_is_enabled() {
                return Ok(());
            }
            if macos_utils::accessibility::accessibility_guide_is_active() {
                break;
            }
            if std::time::Instant::now() >= start_deadline {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            if macos_utils::accessibility::accessibility_is_enabled() {
                return Ok(());
            }
            if !macos_utils::accessibility::accessibility_guide_is_active() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        return Ok(());
    }
    let _ = query(id, InstallAction::Install).await?;
    Ok(())
}

pub async fn repair_all() -> Result<(), String> {
    for id in [PermId::Accessibility, PermId::Shell] {
        if let Err(err) = repair(id).await {
            warn!(?id, %err, "permission repair failed");
            return Err(err);
        }
    }
    Ok(())
}

pub fn spawn_check(proxy: &EventLoopProxy) {
    let proxy = proxy.clone();
    let generation = next_permission_generation();
    tokio::spawn(async move {
        publish_check(&proxy, false, generation, None).await;
    });
}

fn next_permission_generation() -> u64 {
    NEXT_PERMISSION_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Counts a repair for [`repair_is_running`] for as long as it is held.
struct RunningRepair;

impl RunningRepair {
    fn begin() -> Self {
        RUNNING_REPAIRS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for RunningRepair {
    fn drop(&mut self) {
        RUNNING_REPAIRS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Whether a repair, or the Accessibility guide one started, still needs this
/// process. Closing the window does not stop either.
pub fn repair_is_running() -> bool {
    if RUNNING_REPAIRS.load(Ordering::Relaxed) > 0 {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        macos_utils::accessibility::accessibility_guide_is_active()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

async fn publish_check(proxy: &EventLoopProxy, completes_repair: bool, generation: u64, repair_error: Option<String>) {
    let mut required = check_required().await;
    required.generation = generation;
    required.completes_repair = completes_repair;
    if !required.all_ready() {
        required.error = repair_error.or(required.error);
    }
    if proxy.send_event(Event::PermissionSnapshot(required.clone())).is_err() {
        warn!("failed to deliver permission snapshot");
        return;
    }
    let mut snapshot = with_input_method(required).await;
    snapshot.completes_repair = false;
    if proxy.send_event(Event::PermissionSnapshot(snapshot)).is_err() {
        warn!("failed to deliver permission snapshot");
    }
}

pub fn spawn_repair(proxy: &EventLoopProxy, id: PermId) -> u64 {
    let proxy = proxy.clone();
    let generation = next_permission_generation();
    let running = RunningRepair::begin();
    tokio::spawn(async move {
        let _running = running;
        let repair_error = repair(id).await.err();
        if let Some(err) = &repair_error {
            warn!(?id, %err, "permission repair failed");
        }
        publish_check(&proxy, true, generation, repair_error).await;
        proxy.send_event(Event::ReloadAccessibility).ok();
    });
    generation
}

pub fn spawn_repair_all(proxy: &EventLoopProxy) -> u64 {
    let proxy = proxy.clone();
    let generation = next_permission_generation();
    let running = RunningRepair::begin();
    tokio::spawn(async move {
        let _running = running;
        let repair_error = repair_all().await.err();
        if let Some(err) = &repair_error {
            warn!(%err, "permission repair-all failed");
        }
        publish_check(&proxy, true, generation, repair_error).await;
        proxy.send_event(Event::ReloadAccessibility).ok();
    });
    generation
}

pub fn accessibility_is_missing() -> bool {
    #[cfg(target_os = "macos")]
    {
        !macos_utils::accessibility::accessibility_is_enabled()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(accessibility: PermReady, shell: PermReady, input_method: PermReady) -> PermissionSnapshot {
        PermissionSnapshot {
            accessibility,
            shell,
            input_method,
            error: None,
            generation: 1,
            completes_repair: false,
        }
    }

    #[test]
    fn input_method_does_not_block_settings() {
        assert!(snapshot(PermReady::Ready, PermReady::Ready, PermReady::Missing).all_ready());
        assert!(snapshot(PermReady::Ready, PermReady::Ready, PermReady::Checking).all_ready());
        assert!(!snapshot(PermReady::Ready, PermReady::Ready, PermReady::Checking).still_checking());
        assert!(!snapshot(PermReady::Missing, PermReady::Ready, PermReady::Ready).all_ready());
        assert!(!snapshot(PermReady::Ready, PermReady::Missing, PermReady::Ready).all_ready());
    }

    #[test]
    fn repair_all_skips_optional_input_method() {
        let production = include_str!("permissions.rs")
            .rsplit_once("mod tests {")
            .map(|(src, _)| src)
            .expect("production source");
        let start = production.find("pub async fn repair_all").expect("repair_all");
        let body = production[start..]
            .split("pub fn spawn_check")
            .next()
            .expect("repair_all body");
        assert!(body.contains("PermId::Accessibility"));
        assert!(body.contains("PermId::Shell"));
        assert!(
            !body.contains("PermId::InputMethod"),
            "Fix All must not install the optional input method"
        );
    }

    #[test]
    fn accessibility_repair_checks_trust_before_opening_settings() {
        let production = include_str!("permissions.rs")
            .rsplit_once("mod tests {")
            .map(|(src, _)| src)
            .expect("production source");
        let start = production.find("pub async fn repair(").expect("repair");
        let body = production[start..]
            .split("pub async fn repair_all")
            .next()
            .expect("repair body");
        let guide = body.find("begin_accessibility_guide").expect("guide");
        assert!(
            body[..guide].contains("accessibility_is_enabled"),
            "must not open System Settings until Accessibility is known to be missing"
        );
        let after = &body[guide..];
        let wait_for_start = after
            .find("accessibility_guide_is_active()")
            .expect("wait until the guide has started");
        let treat_as_dismissed = after
            .find("!macos_utils::accessibility::accessibility_guide_is_active()")
            .expect("dismissed only after start");
        assert!(
            wait_for_start < treat_as_dismissed,
            "Fix All must not treat a guide that has not started yet as dismissed"
        );
    }

    #[test]
    fn required_check_does_not_query_input_method() {
        let production = include_str!("permissions.rs")
            .rsplit_once("mod tests {")
            .map(|(src, _)| src)
            .expect("production source");
        let start = production.find("async fn check_required").expect("check_required");
        let body = production[start..]
            .split("async fn with_input_method")
            .next()
            .expect("check_required body");
        assert!(body.contains("PermId::Accessibility"));
        assert!(body.contains("PermId::Shell"));
        assert!(
            !body.contains("PermId::InputMethod"),
            "the settings spinner must not wait on optional IME status"
        );
    }

    #[test]
    fn spawn_check_publishes_required_permissions_before_input_method() {
        let production = include_str!("permissions.rs")
            .rsplit_once("mod tests {")
            .map(|(src, _)| src)
            .expect("production source");
        let start = production.find("async fn publish_check").expect("publish_check");
        let body = production[start..]
            .split("pub fn spawn_repair")
            .next()
            .expect("publish_check body");
        let first = body.find("send_event").expect("first snapshot");
        let ime = body.find("with_input_method").expect("IME fill-in");
        assert!(
            first < ime,
            "required Accessibility/Shell snapshot must reach the UI before the IME probe"
        );
    }
}

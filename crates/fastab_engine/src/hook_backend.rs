//! Native hook backend: typed IR + named adapters.
//!
//! Runtime JavaScript is gone (T4.1). A miss records a [`HookDiagnostic`]
//! and returns empty — there is no QuickJS fallback.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;

use crate::hook_types::{HookContext, HookDiagnostic, HookDiagnosticRecord, ScriptCommand, ShellContext};
use crate::ir::Spec;
use crate::process::{self, CommandError};
use crate::runtime::Suggestion;
use crate::snapshot::DirectorySnapshot;
use crate::typed_hook::{
    LazyTypedHookIr, RuntimeTypedHookAdapter, RuntimeTypedHookCatalog, TypedExecRequest, TypedExecResult,
    TypedHookCatalogIndex, TypedHookContext, TypedHookError, TypedHookIr, evaluate_typed_alias, evaluate_typed_custom,
    evaluate_typed_filter_template_suggestions, evaluate_typed_generate_spec, evaluate_typed_get_query_term,
    evaluate_typed_load_spec, evaluate_typed_post_process, evaluate_typed_script, evaluate_typed_trigger,
    index_typed_hook_catalog_bytes, parse_typed_hook_block, parse_typed_hook_catalog_bytes,
    suggestions_from_typed_json,
};

const TYPED_HOOKS_FILE: &str = "typed-hooks.json";

pub struct NativeHooks {
    source: Option<NativeHookSource>,
    // None inside the initialized cell means missing/rejected, not unloaded.
    catalog: OnceLock<Option<CatalogView>>,
}

enum CatalogView {
    /// The whole sidecar, parsed at once: no snapshot, or no usable index.
    Whole(RuntimeTypedHookCatalog),
    /// One command's block at a time, from the generation's pinned sidecar.
    Blocks(BlockCatalog),
}

struct BlockCatalog {
    sidecar: Arc<IndexedSidecar>,
    /// Parallel to the index's blocks. `None` inside a cell: unreadable or rejected.
    blocks: Vec<OnceLock<Option<RuntimeTypedHookCatalog>>>,
    snapshot: DirectorySnapshot,
}

/// Built once per snapshot generation and kept by the snapshot, so the
/// engine's idle reset of this owner costs a block read, not a rescan.
#[cfg(unix)]
struct IndexedSidecar {
    file: crate::snapshot::PinnedFile,
    index: TypedHookCatalogIndex,
}

#[cfg(not(unix))]
struct IndexedSidecar {
    index: TypedHookCatalogIndex,
}

impl IndexedSidecar {
    #[cfg(unix)]
    fn is_intact(&self) -> bool {
        self.file.is_intact()
    }

    #[cfg(not(unix))]
    fn is_intact(&self) -> bool {
        true
    }
}

impl BlockCatalog {
    fn block(&self, hook_id: &str) -> Option<&RuntimeTypedHookCatalog> {
        let position = self.sidecar.index.position(hook_id)?;
        self.blocks[position].get_or_init(|| self.load(position)).as_ref()
    }

    #[cfg(unix)]
    fn load(&self, position: usize) -> Option<RuntimeTypedHookCatalog> {
        let (hooks, adapters) = self.sidecar.index.ranges(position);
        let read = |range: Option<std::ops::Range<usize>>| match range {
            None => Ok(None),
            Some(range) => self.sidecar.file.read_range(range).map(Some),
        };
        let members = read(hooks).and_then(|hooks| read(adapters).map(|adapters| (hooks, adapters)));
        let (hooks, adapters) = match members {
            Ok(members) => members,
            Err(error) => {
                // The pinned inode moved: this generation is no longer intact.
                self.snapshot.mark_stale();
                tracing::warn!(%error, "typed hook block unreadable");
                return None;
            },
        };
        match parse_typed_hook_block(hooks.as_deref(), adapters.as_deref()) {
            Ok(block) => Some(block),
            Err(error) => {
                tracing::warn!(%error, "typed hook block rejected");
                None
            },
        }
    }

    #[cfg(not(unix))]
    fn load(&self, _position: usize) -> Option<RuntimeTypedHookCatalog> {
        None
    }

    fn descriptor_counts(&self) -> (usize, usize) {
        self.blocks
            .iter()
            .filter_map(|block| block.get()?.as_ref())
            .map(RuntimeTypedHookCatalog::descriptor_counts)
            .fold((0, 0), |(all, parsed), (more, more_parsed)| {
                (all + more, parsed + more_parsed)
            })
    }
}

impl CatalogView {
    fn descriptor(&self, hook_id: &str) -> Option<&LazyTypedHookIr> {
        match self {
            Self::Whole(catalog) => catalog.hooks.get(hook_id),
            Self::Blocks(catalog) => catalog.block(hook_id)?.hooks.get(hook_id),
        }
    }

    fn adapter(&self, hook_id: &str) -> Option<&RuntimeTypedHookAdapter> {
        match self {
            Self::Whole(catalog) => catalog.adapters.get(hook_id),
            Self::Blocks(catalog) => catalog.block(hook_id)?.adapters.get(hook_id),
        }
    }
}

/// The generation's block index over `typed-hooks.json`, if it has one. The
/// first build still consumes the bytes captured while the snapshot hashed
/// the tree; it reads the file again through a descriptor it keeps.
#[cfg(unix)]
fn indexed_sidecar(snapshot: &DirectorySnapshot) -> Option<Arc<IndexedSidecar>> {
    let path = Path::new(TYPED_HOOKS_FILE);
    if !snapshot.is_file(path) {
        return None;
    }
    snapshot.sidecar_index(|| {
        drop(snapshot.take_captured_file(path));
        let (file, bytes) = snapshot.pin_file(path).ok()?;
        match index_typed_hook_catalog_bytes(&bytes) {
            Ok(index) => Some(IndexedSidecar { file, index }),
            Err(error) => {
                tracing::debug!(%error, "typed hook catalog has no block index; parsing it whole");
                None
            },
        }
    })
}

#[cfg(not(unix))]
fn indexed_sidecar(_snapshot: &DirectorySnapshot) -> Option<Arc<IndexedSidecar>> {
    None
}

struct NativeHookSource {
    specs_dir: PathBuf,
    snapshot: Option<DirectorySnapshot>,
}

impl NativeHooks {
    #[cfg(test)]
    pub(crate) fn from_catalog(catalog: crate::typed_hook::TypedHookCatalog) -> Self {
        Self {
            source: None,
            catalog: OnceLock::from(Some(CatalogView::Whole(catalog.into_runtime()))),
        }
    }

    /// Bind one generation without reading or parsing its sidecar. Only a
    /// real dispatch calls `catalog`; static completion and diagnostics do not.
    pub(crate) fn new(specs_dir: &Path, snapshot: Option<DirectorySnapshot>) -> Self {
        Self {
            source: Some(NativeHookSource {
                specs_dir: specs_dir.to_path_buf(),
                snapshot,
            }),
            catalog: OnceLock::new(),
        }
    }

    fn catalog(&self) -> Option<&CatalogView> {
        self.catalog
            .get_or_init(|| {
                let source = self.source.as_ref()?;
                if let Some(snapshot) = source.snapshot.as_ref()
                    && let Some(sidecar) = indexed_sidecar(snapshot)
                {
                    // As a whole-file read would: a sidecar changed since it
                    // was verified leaves the catalog unloaded and the
                    // generation stale.
                    if !sidecar.is_intact() {
                        snapshot.mark_stale();
                        tracing::warn!("typed hook catalog changed after its generation was opened");
                        return None;
                    }
                    let blocks = (0..sidecar.index.len()).map(|_| OnceLock::new()).collect();
                    return Some(CatalogView::Blocks(BlockCatalog {
                        sidecar,
                        blocks,
                        snapshot: snapshot.clone(),
                    }));
                }
                let typed_bytes = read_sidecar(&source.specs_dir, source.snapshot.as_ref(), TYPED_HOOKS_FILE)?;
                match parse_typed_hook_catalog_bytes(&typed_bytes) {
                    Ok(catalog) => Some(CatalogView::Whole(catalog.into_runtime())),
                    Err(error) => {
                        tracing::warn!(%error, "typed hook catalog rejected");
                        None
                    },
                }
            })
            .as_ref()
    }

    pub(crate) fn diagnostics(&self) -> crate::diagnostics::HookCatalogDiagnostics {
        let captured_bytes = self
            .source
            .as_ref()
            .and_then(|source| source.snapshot.as_ref())
            .map_or(0, DirectorySnapshot::captured_bytes);
        let Some(catalog) = self.catalog.get().and_then(Option::as_ref) else {
            return crate::diagnostics::HookCatalogDiagnostics {
                load_attempted: self.catalog.get().is_some(),
                captured_bytes,
                ..Default::default()
            };
        };
        let ((descriptor_count, parsed_descriptor_count), typed_entries, adapter_entries) = match catalog {
            CatalogView::Whole(catalog) => (catalog.descriptor_counts(), catalog.hooks.len(), catalog.adapters.len()),
            CatalogView::Blocks(catalog) => (
                catalog.descriptor_counts(),
                catalog.sidecar.index.hook_count(),
                catalog.sidecar.index.adapter_count(),
            ),
        };
        crate::diagnostics::HookCatalogDiagnostics {
            load_attempted: true,
            loaded: true,
            typed_entries,
            adapter_entries,
            descriptor_count,
            parsed_descriptor_count,
            captured_bytes,
        }
    }
}

impl Drop for NativeHooks {
    fn drop(&mut self) {
        if let Some(snapshot) = self.source.as_ref().and_then(|source| source.snapshot.as_ref()) {
            // The worker's pristine Registry shares this snapshot; dropping
            // just this owner would leave its unconsumed bytes there forever.
            snapshot.release_captured_files();
        }
    }
}

fn read_sidecar(specs_dir: &Path, snapshot: Option<&DirectorySnapshot>, relative: &str) -> Option<Vec<u8>> {
    if let Some(snapshot) = snapshot {
        let path = Path::new(relative);
        if let Some(bytes) = snapshot.take_captured_file(path) {
            return Some(bytes);
        }
        return snapshot.read_optional_file(path).ok().flatten();
    }
    let path = specs_dir.join(relative);
    path.is_file().then(|| std::fs::read(path).ok()).flatten()
}

thread_local! {
    static SKIP_HOOKS: Cell<bool> = const { Cell::new(false) };
    static NATIVE: RefCell<Option<Arc<NativeHooks>>> = const { RefCell::new(None) };
    static CWD: RefCell<Option<String>> = const { RefCell::new(None) };
    static SHELL: RefCell<Option<ShellContext>> = const { RefCell::new(None) };
    static LAST_DIAGNOSTIC: RefCell<Option<HookDiagnosticRecord>> = const { RefCell::new(None) };
}

pub fn bind_native(native: Arc<NativeHooks>) -> BoundNative {
    NATIVE.with(|cell| {
        let previous = cell.replace(Some(native));
        BoundNative { previous }
    })
}

pub struct BoundNative {
    previous: Option<Arc<NativeHooks>>,
}

impl Drop for BoundNative {
    fn drop(&mut self) {
        NATIVE.with(|cell| {
            *cell.borrow_mut() = self.previous.take();
        });
    }
}

pub fn enter_context<R>(cwd: &str, shell: &ShellContext, f: impl FnOnce() -> R) -> R {
    CWD.with(|cell| {
        let previous_cwd = cell.replace(Some(cwd.to_string()));
        let result = SHELL.with(|shell_cell| {
            let previous_shell = shell_cell.replace(Some(shell.clone()));
            let result = f();
            *shell_cell.borrow_mut() = previous_shell;
            result
        });
        *cell.borrow_mut() = previous_cwd;
        result
    })
}

pub fn without_hooks<R>(f: impl FnOnce() -> R) -> R {
    SKIP_HOOKS.with(|cell| {
        let previous = cell.replace(true);
        let result = f();
        cell.set(previous);
        result
    })
}

#[allow(dead_code)]
pub fn last_diagnostic() -> Option<HookDiagnosticRecord> {
    LAST_DIAGNOSTIC.with(|cell| cell.borrow().clone())
}

fn record(hook_id: &str, outcome: HookDiagnostic) {
    if crate::cancellation::is_cancelled() {
        return;
    }
    LAST_DIAGNOSTIC.with(|cell| {
        *cell.borrow_mut() = Some(HookDiagnosticRecord {
            hook_id: hook_id.to_string(),
            outcome,
        });
    });
}

fn hooks_skipped() -> bool {
    SKIP_HOOKS.get() || crate::cancellation::is_cancelled()
}

/// Inside [`without_hooks`]: no hook runs, and no cached hook result applies.
pub(crate) fn hooks_suspended() -> bool {
    SKIP_HOOKS.get()
}

fn native() -> Option<Arc<NativeHooks>> {
    NATIVE.with(|cell| cell.borrow().clone())
}

fn session_shell() -> ShellContext {
    SHELL.with(|cell| cell.borrow().clone().unwrap_or_default())
}

pub fn current_cwd() -> Option<String> {
    CWD.with(|cell| cell.borrow().clone())
}

pub fn current_shell() -> ShellContext {
    session_shell()
}

fn finish_option<T>(hook_id: &str, value: Option<T>, empty: impl Fn(&T) -> bool) -> Option<T> {
    match value {
        Some(value) if empty(&value) => {
            record(hook_id, HookDiagnostic::EmptyResult);
            Some(value)
        },
        Some(value) => {
            record(hook_id, HookDiagnostic::Success);
            Some(value)
        },
        None => {
            record(hook_id, HookDiagnostic::EmptyResult);
            None
        },
    }
}

fn missing(hook_id: &str) -> Option<std::convert::Infallible> {
    record(hook_id, HookDiagnostic::SourceMissing);
    None
}

fn invoke_err<T>(hook_id: &str) -> Option<T> {
    record(hook_id, HookDiagnostic::InvokeError);
    None
}

/// A hook that ran out of its `autocomplete.scriptTimeout` budget is not a
/// broken hook. Keep the two apart in the diagnostic, as the QuickJS
/// interrupt handler did.
fn eval_err<T>(hook_id: &str, error: &crate::typed_hook::TypedHookError) -> Option<T> {
    if error.is_cancelled() {
        return None;
    }
    if error.is_timed_out() {
        record(hook_id, HookDiagnostic::Timeout);
        return None;
    }
    invoke_err(hook_id)
}

/// Every effectful hook runs under its script budget plus this margin, the
/// same window the QuickJS interrupt handler enforced.
const HOOK_DEADLINE_MARGIN: Duration = Duration::from_secs(2);

fn hook_deadline(timeout: Duration) -> Instant {
    Instant::now() + timeout + HOOK_DEADLINE_MARGIN
}

fn deadline_expired(deadline: Instant) -> bool {
    deadline.checked_duration_since(Instant::now()).is_none()
}

/// The old host's `hook_error`: a failure reported after the deadline has
/// passed is a timeout, whatever the hook threw on its way out. A named
/// adapter's exec throws a catchable `Error` on timeout, exactly as
/// `executeCommand` rejected, so this is the only place that can tell the
/// two apart.
fn adapter_err<T>(hook_id: &str, deadline: Option<Instant>) -> Option<T> {
    if deadline.is_some_and(deadline_expired) {
        record(hook_id, HookDiagnostic::Timeout);
        return None;
    }
    invoke_err(hook_id)
}

// Native adapters may catch executeCommand errors. The cancellation token
// remains sticky, so even a successful fallback cannot escape this boundary.
fn active_hook<T>(run: impl FnOnce() -> Option<T>) -> Option<T> {
    crate::cancellation::check().ok()?;
    let value = run();
    crate::cancellation::check().ok()?;
    value
}

pub fn dispatch_trigger(hook_id: &str, search_term: &str, previous: &str) -> Option<bool> {
    if hooks_skipped() {
        return None;
    }
    active_hook(|| native_trigger(hook_id, search_term, previous))
}

pub fn dispatch_get_query_term(hook_id: &str, search_term: &str) -> Option<String> {
    if hooks_skipped() {
        return None;
    }
    active_hook(|| native_get_query_term(hook_id, search_term))
}

/// `script` is the generator's own command line — the one whose output
/// `stdout` is — so a shared body can tell `cf orgs` from `cf spaces`.
pub fn dispatch_post_process(
    hook_id: &str,
    stdout: &str,
    tokens: &[String],
    script: &[String],
) -> Option<Vec<Suggestion>> {
    if hooks_skipped() {
        return None;
    }
    active_hook(|| native_post_process(hook_id, stdout, tokens, script))
}

pub fn dispatch_script_command(hook_id: &str, tokens: &[String]) -> Option<ScriptCommand> {
    if hooks_skipped() {
        return None;
    }
    active_hook(|| native_script(hook_id, tokens))
}

pub fn dispatch_filter_template_suggestions(
    hook_id: &str,
    suggestions: &[Suggestion],
    tokens: &[String],
) -> Option<Vec<Suggestion>> {
    if hooks_skipped() {
        return None;
    }
    active_hook(|| native_filter(hook_id, suggestions, tokens))
}

pub fn dispatch_custom(
    hook_id: &str,
    tokens: &[String],
    cwd: &str,
    search_term: &str,
    timeout: Duration,
    is_dangerous: bool,
) -> Option<Vec<Suggestion>> {
    if hooks_skipped() {
        return None;
    }
    active_hook(|| native_custom(hook_id, tokens, cwd, search_term, timeout, is_dangerous))
}

pub fn dispatch_alias(hook_id: &str, token: &str, cwd: &str, timeout: Duration) -> Option<String> {
    if hooks_skipped() {
        return None;
    }
    active_hook(|| native_alias(hook_id, token, cwd, timeout))
}

pub fn dispatch_load_spec(hook_id: &str, token: &str, cwd: &str, timeout: Duration) -> Option<Spec> {
    if hooks_skipped() {
        return None;
    }
    active_hook(|| native_load_spec(hook_id, token, cwd, timeout))
}

pub fn dispatch_generate_spec(hook_id: &str, tokens: &[String], cwd: &str, timeout: Duration) -> Option<Spec> {
    if hooks_skipped() {
        return None;
    }
    active_hook(|| native_generate_spec(hook_id, tokens, cwd, timeout))
}

fn typed_entry<'a>(native: &'a NativeHooks, hook_id: &str) -> Result<Option<&'a TypedHookIr>, TypedHookError> {
    let Some(catalog) = native.catalog() else {
        return Ok(None);
    };
    match catalog.descriptor(hook_id) {
        Some(descriptor) => descriptor.get().map(Some),
        None => Ok(None),
    }
}

fn adapter_sha<'a>(native: &'a NativeHooks, hook_id: &str, field: &str) -> Option<&'a str> {
    let entry = native.catalog()?.adapter(hook_id)?;
    (entry.source_field == field).then_some(entry.function_body_sha256.as_str())
}

fn finish_typed<T>(
    native: &NativeHooks,
    hook_id: &str,
    run: impl FnOnce(&TypedHookIr) -> Result<T, TypedHookError>,
    empty: impl Fn(&T) -> bool,
) -> Option<T> {
    match typed_entry(native, hook_id) {
        Ok(Some(descriptor)) => match run(descriptor) {
            Ok(value) => finish_option(hook_id, Some(value), empty),
            Err(error) => eval_err(hook_id, &error),
        },
        Ok(None) => {
            let _ = missing(hook_id);
            None
        },
        Err(error) => eval_err(hook_id, &error),
    }
}

fn native_trigger(hook_id: &str, search_term: &str, previous: &str) -> Option<bool> {
    finish_typed(
        native()?.as_ref(),
        hook_id,
        |descriptor| evaluate_typed_trigger(descriptor, search_term, previous),
        |_| false,
    )
}

fn native_get_query_term(hook_id: &str, search_term: &str) -> Option<String> {
    finish_typed(
        native()?.as_ref(),
        hook_id,
        |descriptor| evaluate_typed_get_query_term(descriptor, search_term),
        String::is_empty,
    )
}

fn native_post_process(hook_id: &str, stdout: &str, tokens: &[String], script: &[String]) -> Option<Vec<Suggestion>> {
    let native = native()?;
    // Named adapters own their body SHA (T2.4). A typed compile of the same
    // body is a fallback, not a shadow — yarn's package.json parser is the
    // case that made this order load-bearing.
    if let Some(sha) = adapter_sha(&native, hook_id, "postProcess")
        && let Some(json) = crate::native_adapters::evaluate_post_process(
            sha,
            stdout,
            crate::native_adapters::HookSite { tokens, script },
        )
    {
        return json_suggestions(hook_id, json, None);
    }
    finish_typed(
        &native,
        hook_id,
        |descriptor| evaluate_typed_post_process(descriptor, stdout, tokens),
        Vec::is_empty,
    )
}

fn native_script(hook_id: &str, tokens: &[String]) -> Option<ScriptCommand> {
    finish_typed(
        native()?.as_ref(),
        hook_id,
        |descriptor| evaluate_typed_script(descriptor, tokens),
        |command| command.command.is_empty(),
    )
}

fn native_filter(hook_id: &str, suggestions: &[Suggestion], tokens: &[String]) -> Option<Vec<Suggestion>> {
    let native = native()?;
    if let Some(sha) = adapter_sha(&native, hook_id, "filterTemplateSuggestions") {
        let payload: Vec<JsonValue> = suggestions
            .iter()
            .map(|item| {
                serde_json::json!({
                    "name": item.name,
                    "description": item.description,
                    "type": item.kind,
                    "insertValue": item.insert_value,
                    "displayName": item.display_name,
                    "icon": item.icon,
                    "hidden": item.hidden,
                    "isDangerous": item.is_dangerous,
                    "priority": item.priority,
                    "shouldAddSpace": item.should_add_space,
                })
            })
            .collect();
        if let Some(json) =
            crate::native_adapters::evaluate_filter(sha, &payload, crate::native_adapters::HookSite::tokens(tokens))
        {
            return json_suggestions(hook_id, json, None);
        }
    }
    finish_typed(
        &native,
        hook_id,
        |descriptor| evaluate_typed_filter_template_suggestions(descriptor, suggestions),
        Vec::is_empty,
    )
}

fn native_custom(
    hook_id: &str,
    tokens: &[String],
    cwd: &str,
    search_term: &str,
    timeout: Duration,
    is_dangerous: bool,
) -> Option<Vec<Suggestion>> {
    let native = native()?;
    let shell = session_shell();
    let context = HookContext::from_shell(cwd, &shell, search_term, is_dangerous);
    let deadline = hook_deadline(timeout);
    if let Some(sha) = adapter_sha(&native, hook_id, "custom") {
        let exec = live_adapter_exec(cwd, timeout, deadline);
        if let Some(json) = crate::native_adapters::evaluate_custom(sha, tokens, &exec, &context) {
            return json_suggestions(hook_id, json, Some(deadline));
        }
    }
    finish_typed(
        &native,
        hook_id,
        |descriptor| {
            let typed_ctx = typed_context(&context);
            let exec = live_typed_exec(cwd, timeout);
            evaluate_typed_custom(descriptor, tokens, &typed_ctx, &exec, Some(deadline))
        },
        Vec::is_empty,
    )
}

fn native_alias(hook_id: &str, token: &str, cwd: &str, timeout: Duration) -> Option<String> {
    let native = native()?;
    let context = HookContext::from_shell(cwd, &session_shell(), token, false);
    finish_typed(
        &native,
        hook_id,
        |descriptor| {
            let typed_ctx = typed_context(&context);
            let exec = live_typed_exec(cwd, timeout);
            evaluate_typed_alias(descriptor, token, &typed_ctx, &exec, Some(hook_deadline(timeout)))
        },
        String::is_empty,
    )
}

fn native_load_spec(hook_id: &str, token: &str, cwd: &str, timeout: Duration) -> Option<Spec> {
    let native = native()?;
    let context = HookContext::from_shell(cwd, &session_shell(), token, false);
    match typed_entry(&native, hook_id) {
        Ok(Some(descriptor)) => {
            let typed_ctx = typed_context(&context);
            let exec = live_typed_exec(cwd, timeout);
            match evaluate_typed_load_spec(descriptor, token, &typed_ctx, &exec, Some(hook_deadline(timeout))) {
                Ok(json) => spec_from_json(hook_id, json),
                Err(error) => eval_err(hook_id, &error),
            }
        },
        Ok(None) => {
            let _ = missing(hook_id);
            None
        },
        Err(error) => eval_err(hook_id, &error),
    }
}

fn native_generate_spec(hook_id: &str, tokens: &[String], cwd: &str, timeout: Duration) -> Option<Spec> {
    let native = native()?;
    let context = HookContext::from_shell(cwd, &session_shell(), tokens.last().map_or("", String::as_str), false);
    let deadline = hook_deadline(timeout);
    if let Some(sha) = adapter_sha(&native, hook_id, "generateSpec") {
        let exec = live_adapter_exec(cwd, timeout, deadline);
        if let Some(json) = crate::native_adapters::evaluate_generate_spec(sha, tokens, &exec, &context) {
            return spec_from_json_result(hook_id, json, Some(deadline));
        }
    }
    match typed_entry(&native, hook_id) {
        Ok(Some(descriptor)) => {
            let typed_ctx = typed_context(&context);
            let exec = live_typed_exec(cwd, timeout);
            match evaluate_typed_generate_spec(descriptor, tokens, &typed_ctx, &exec, Some(deadline)) {
                Ok(json) => spec_from_json(hook_id, json),
                Err(error) => eval_err(hook_id, &error),
            }
        },
        Ok(None) => {
            let _ = missing(hook_id);
            None
        },
        Err(error) => eval_err(hook_id, &error),
    }
}

fn json_suggestions(
    hook_id: &str,
    result: Result<JsonValue, String>,
    deadline: Option<Instant>,
) -> Option<Vec<Suggestion>> {
    match result {
        Ok(json) => match suggestions_from_typed_json(&json) {
            Ok(value) => finish_option(hook_id, Some(value), Vec::is_empty),
            Err(_) => {
                record(hook_id, HookDiagnostic::ResultConversionError);
                None
            },
        },
        Err(_) => adapter_err(hook_id, deadline),
    }
}

fn spec_from_json_result(hook_id: &str, result: Result<JsonValue, String>, deadline: Option<Instant>) -> Option<Spec> {
    match result {
        Ok(json) => spec_from_json(hook_id, json),
        Err(_) => adapter_err(hook_id, deadline),
    }
}

fn spec_from_json(hook_id: &str, json: JsonValue) -> Option<Spec> {
    let spec = fig_spec_from_json(&json);
    match spec {
        Some(spec) => {
            record(hook_id, HookDiagnostic::Success);
            Some(spec)
        },
        None => {
            record(hook_id, HookDiagnostic::ResultConversionError);
            None
        },
    }
}

fn fig_spec_from_json(value: &JsonValue) -> Option<Spec> {
    crate::fig_spec::spec_from_fig_json(value)
}

fn typed_context(context: &HookContext) -> TypedHookContext {
    TypedHookContext {
        current_working_directory: context.current_working_directory.clone(),
        current_process: context.current_process.clone(),
        ssh_prefix: context.ssh_prefix.clone(),
        environment_variables: context.environment_variables.clone(),
        search_term: context.search_term.clone(),
        is_dangerous: context.is_dangerous,
    }
}

fn live_typed_exec(
    cwd: &str,
    timeout: Duration,
) -> impl Fn(TypedExecRequest) -> Result<TypedExecResult, crate::typed_hook::TypedHookError> + '_ {
    move |request| {
        let command_cwd = request.cwd.as_deref().filter(|value| !value.is_empty()).unwrap_or(cwd);
        let command_timeout = request.timeout_ms.map_or(timeout, Duration::from_millis);
        match process::execute_full(
            &request.command,
            &request.args,
            command_cwd,
            &request.env,
            command_timeout,
        ) {
            Ok(output) => Ok(TypedExecResult {
                stdout: output.stdout,
                stderr: output.stderr,
                status: i64::from(output.status),
            }),
            Err(CommandError::Cancelled) => Err(crate::typed_hook::TypedHookError::cancelled()),
            Err(CommandError::TimedOut) => Err(crate::typed_hook::TypedHookError::timed_out("timeout")),
            Err(CommandError::Failed) => Err(crate::typed_hook::TypedHookError::new("exec failed")),
        }
    }
}

/// The adapter counterpart of the typed evaluator's exec clamp. A blocking
/// subprocess cannot be interrupted at the hook deadline, so each call gets
/// only the time the hook has left, and a call scheduled after the deadline
/// throws without running — `make`'s two commands and nx's `cat` per
/// project must not add up to N × `scriptTimeout`.
fn live_adapter_exec(
    cwd: &str,
    timeout: Duration,
    deadline: Instant,
) -> impl Fn(
    crate::native_adapters::AdapterExecRequest,
) -> Result<crate::native_adapters::AdapterExecResult, crate::native_adapters::AdapterError>
+ '_ {
    move |request| {
        if crate::cancellation::is_cancelled() {
            return Err(crate::native_adapters::adapter_throw("Error"));
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(crate::native_adapters::adapter_throw("Error"));
        };
        if remaining.is_zero() {
            return Err(crate::native_adapters::adapter_throw("Error"));
        }
        let command_cwd = request.cwd.as_deref().filter(|value| !value.is_empty()).unwrap_or(cwd);
        let command_timeout = request
            .timeout
            .and_then(|ms| u64::try_from(ms).ok())
            .map_or(timeout, Duration::from_millis)
            .min(remaining);
        let args: Vec<String> = request
            .args
            .iter()
            .filter_map(JsonValue::as_str)
            .map(ToOwned::to_owned)
            .collect();
        let env = match request.env.as_ref() {
            Some(JsonValue::Object(fields)) => fields
                .iter()
                .filter_map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
                .collect(),
            _ => Vec::new(),
        };
        match process::execute_full(&request.command, &args, command_cwd, &env, command_timeout) {
            Ok(output) => Ok(crate::native_adapters::AdapterExecResult {
                stdout: output.stdout,
                stderr: output.stderr,
                status: i64::from(output.status),
            }),
            Err(_) => Err(crate::native_adapters::adapter_throw("Error")),
        }
    }
}

pub fn merge_generated_spec(wrapper: &Spec, generated: Spec) -> Spec {
    crate::fig_spec::merge_generated_spec(wrapper, generated)
}

pub use crate::hook_types::clean_output;

#[cfg(test)]
pub(crate) fn test_sidecar_contracts() -> JsonValue {
    serde_json::json!({
        "trigger": {"irVersion": 1, "params": ["string", "string"], "resultType": "bool"},
        "getQueryTerm": {"irVersion": 1, "params": ["string"], "resultType": "string"},
        "postProcess": {"irVersion": 1, "params": ["string", "string-array"], "resultType": "suggestion-array"},
        "script": {"irVersion": 1, "params": ["string-array"], "resultType": "string-array"},
        "filterTemplateSuggestions": {"irVersion": 1, "params": ["suggestion-array"], "resultType": "suggestion-array"},
        "custom": {"irVersion": 1, "params": ["string-array", "exec", "context"], "resultType": "suggestion-array"},
        "alias": {"irVersion": 1, "params": ["string", "exec"], "resultType": "string"},
        "loadSpec": {"irVersion": 1, "params": ["string", "exec"], "resultType": "spec"},
        "generateSpec": {"irVersion": 1, "params": ["string-array", "exec"], "resultType": "spec"}
    })
}

#[cfg(test)]
pub(crate) fn test_typed_entry(id: &str, field: &str, expr: JsonValue) -> (String, JsonValue) {
    let (params, result_type) = match field {
        "trigger" => (
            serde_json::json!([{"index": 0, "type": "string"}, {"index": 1, "type": "string"}]),
            "bool",
        ),
        "getQueryTerm" => (serde_json::json!([{"index": 0, "type": "string"}]), "string"),
        "postProcess" => (
            serde_json::json!([{"index": 0, "type": "string"}, {"index": 1, "type": "string-array"}]),
            "suggestion-array",
        ),
        "script" => (
            serde_json::json!([{"index": 0, "type": "string-array"}]),
            "string-array",
        ),
        "filterTemplateSuggestions" => (
            serde_json::json!([{"index": 0, "type": "suggestion-array"}]),
            "suggestion-array",
        ),
        "custom" => (
            serde_json::json!([
                {"index": 0, "type": "string-array"},
                {"index": 1, "type": "exec"},
                {"index": 2, "type": "context"}
            ]),
            "suggestion-array",
        ),
        "alias" => (
            serde_json::json!([{"index": 0, "type": "string"}, {"index": 1, "type": "exec"}]),
            "string",
        ),
        "loadSpec" => (
            serde_json::json!([{"index": 0, "type": "string"}, {"index": 1, "type": "exec"}]),
            "spec",
        ),
        "generateSpec" => (
            serde_json::json!([{"index": 0, "type": "string-array"}, {"index": 1, "type": "exec"}]),
            "spec",
        ),
        other => panic!("unsupported test hook field {other}"),
    };
    let mut sha = String::new();
    for byte in id.as_bytes() {
        sha.push_str(&format!("{byte:02x}"));
    }
    while sha.len() < 64 {
        sha.push('0');
    }
    sha.truncate(64);
    (
        id.to_string(),
        serde_json::json!({
            "module": "test.js",
            "moduleSha256": "0".repeat(64),
            "path": "root.args[0]",
            "sourceField": field,
            "functionBodySha256": sha,
            "descriptor": {
                "version": 1,
                "kind": "typed-hook-expression",
                "sourceField": field,
                "resultType": result_type,
                "params": params,
                "expr": expr
            }
        }),
    )
}

#[cfg(test)]
pub(crate) fn test_native_hooks(entries: Vec<(String, JsonValue)>) -> NativeHooks {
    use crate::typed_hook::parse_typed_hook_catalog;
    let hooks = JsonValue::Object(entries.into_iter().collect());
    let catalog = serde_json::json!({
        "version": 1,
        "kind": "typed-hook-expressions",
        "contracts": test_sidecar_contracts(),
        "hooks": hooks
    });
    let catalog = parse_typed_hook_catalog(&catalog).expect("test typed catalog");
    NativeHooks::from_catalog(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_adapters::AdapterExecRequest;
    use crate::process::mock::{self, ExecRule};

    const MAKE_TARGETS_SHA: &str = "03b126c52218b618b258b4113e23cf47ef5a0197646c1f48f7983f6bf146ead8";

    #[test]
    fn cancellation_rejects_a_native_adapters_caught_error_fallback() {
        let token = crate::cancellation::CancellationToken::new();
        let _scope = crate::cancellation::enter(token.clone());
        let exec = |_request| {
            token.cancel();
            Err(crate::native_adapters::adapter_throw("Error"))
        };
        let context = HookContext::from_shell("/", &ShellContext::default(), "", false);
        let result = active_hook(|| {
            // The real deno adapter catches both failed cat commands and
            // produces a successful empty fallback. It cannot clear the token.
            let fallback = crate::native_adapters::evaluate_custom(
                "20e727a6b059f8d895ae13dcba72e4308f8935db13897c58057e7805f0182e1e",
                &[],
                &exec,
                &context,
            )
            .unwrap()
            .unwrap();
            assert_eq!(fallback, serde_json::json!([]));
            Some(fallback)
        });
        assert!(result.is_none());
        assert!(crate::cancellation::is_cancelled());
    }

    fn adapter_only_hooks(id: &str, field: &str, sha: &str) -> NativeHooks {
        use crate::typed_hook::parse_typed_hook_catalog;
        NativeHooks::from_catalog(
            parse_typed_hook_catalog(&adapter_only_catalog(id, field, sha)).expect("adapter catalog"),
        )
    }

    fn adapter_only_catalog(id: &str, field: &str, sha: &str) -> JsonValue {
        serde_json::json!({
            "version": 1,
            "kind": "typed-hook-expressions",
            "contracts": test_sidecar_contracts(),
            "hooks": {},
            "adapters": {
                id: {
                    "path": "root.args[0].generators[0]",
                    "sourceField": field,
                    "functionBodySha256": sha,
                }
            }
        })
    }

    fn sleep_request(timeout_ms: i64) -> AdapterExecRequest {
        AdapterExecRequest {
            command: "sleep".into(),
            args: vec![JsonValue::String("5".into())],
            cwd: None,
            env: None,
            timeout: Some(timeout_ms),
        }
    }

    #[test]
    fn adapter_exec_is_clamped_to_the_time_the_hook_has_left() {
        // The mock times out any rule whose delay exceeds the timeout the
        // caller hands it, without sleeping. A 200ms command under a 5s
        // request timeout only fails if the exec clamped that request to the
        // ~100ms the hook had left.
        let _guard = mock::install(vec![ExecRule {
            command: Some("sleep".into()),
            delay_ms: Some(200),
            ..ExecRule::default()
        }]);
        let deadline = Instant::now() + Duration::from_millis(100);
        let exec = live_adapter_exec("/", Duration::from_secs(5), deadline);
        assert!(
            exec(sleep_request(5_000)).is_err(),
            "exec must be clamped to the deadline"
        );
        assert_eq!(mock::calls().len(), 1);
    }

    #[test]
    fn adapter_exec_after_the_deadline_throws_without_running() {
        let _guard = mock::install(vec![ExecRule {
            command: Some("sleep".into()),
            ..ExecRule::default()
        }]);
        let deadline = Instant::now() - Duration::from_millis(1);
        let exec = live_adapter_exec("/", Duration::from_secs(5), deadline);
        assert!(exec(sleep_request(5_000)).is_err());
        assert!(
            mock::calls().is_empty(),
            "a command scheduled after the deadline must not spawn"
        );
    }

    #[test]
    fn adapter_exec_within_budget_runs_with_its_own_timeout() {
        let _guard = mock::install(vec![ExecRule {
            command: Some("sleep".into()),
            stdout: "done".into(),
            delay_ms: Some(200),
            ..ExecRule::default()
        }]);
        let deadline = Instant::now() + Duration::from_secs(10);
        let exec = live_adapter_exec("/", Duration::from_secs(5), deadline);
        let result = exec(sleep_request(5_000)).expect("a command inside the budget runs");
        assert_eq!(result.stdout, "done");
    }

    #[test]
    fn adapter_failure_after_the_deadline_is_a_timeout() {
        let expired = Some(Instant::now() - Duration::from_millis(1));
        assert!(adapter_err::<()>("make#custom#0", expired).is_none());
        assert_eq!(last_diagnostic().map(|r| r.outcome), Some(HookDiagnostic::Timeout));

        let live = Some(Instant::now() + Duration::from_secs(10));
        assert!(adapter_err::<()>("make#custom#0", live).is_none());
        assert_eq!(last_diagnostic().map(|r| r.outcome), Some(HookDiagnostic::InvokeError));

        assert!(adapter_err::<()>("direnv#filterTemplateSuggestions#0", None).is_none());
        assert_eq!(last_diagnostic().map(|r| r.outcome), Some(HookDiagnostic::InvokeError));
    }

    #[test]
    fn a_named_adapter_whose_command_fails_inside_its_budget_is_an_invoke_error() {
        // Both of make's commands are unmocked, so the adapter's first exec
        // throws with the whole budget still ahead of it.
        let _guard = mock::install(Vec::new());
        let native = Arc::new(adapter_only_hooks("make#custom#0", "custom", MAKE_TARGETS_SHA));
        let _bound = bind_native(Arc::clone(&native));
        let rows = dispatch_custom(
            "make#custom#0",
            &["make".into()],
            "/",
            "",
            Duration::from_secs(5),
            false,
        );
        assert!(rows.is_none());
        let record = last_diagnostic().expect("adapter failure records a diagnostic");
        assert_eq!(record.hook_id, "make#custom#0");
        assert_eq!(record.outcome, HookDiagnostic::InvokeError);
        assert_eq!(mock::calls().len(), 1, "the adapter stops at its first failing command");
    }

    #[test]
    fn make_targets_keep_the_order_the_makefile_listed_them_in() {
        // `make` collects targets in a JS `Map`; its `values()` are insertion
        // ordered and the ranker's stable sort preserves generator order on
        // equal priority, so a HashMap here would reshuffle the list per run.
        let _guard = mock::install(vec![
            ExecRule {
                command: Some("bash".into()),
                stdout: "zeta\nalpha\nmid\n".into(),
                ..ExecRule::default()
            },
            ExecRule {
                command: Some("cat".into()),
                stdout: "alpha: ## first\nzeta:\n\tgo\nomega: ## last\n".into(),
                ..ExecRule::default()
            },
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(TYPED_HOOKS_FILE),
            adapter_only_catalog("make#custom#0", "custom", MAKE_TARGETS_SHA).to_string(),
        )
        .unwrap();
        let snapshot = DirectorySnapshot::open(dir.path()).unwrap();
        let native = Arc::new(NativeHooks::new(dir.path(), Some(snapshot)));
        assert!(!native.diagnostics().load_attempted);
        let _bound = bind_native(Arc::clone(&native));
        let rows = dispatch_custom(
            "make#custom#0",
            &["make".into()],
            "/",
            "",
            Duration::from_secs(5),
            false,
        )
        .expect("make targets");
        assert_eq!(native.diagnostics().adapter_entries, 1);
        assert_eq!(native.diagnostics().captured_bytes, 0);
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["zeta", "alpha", "mid", "omega"]);
        assert_eq!(
            rows[1].description, "first",
            "a re-set key keeps its slot and takes the new value"
        );
    }

    #[test]
    fn skipped_or_cancelled_dispatch_does_not_initialize_catalog() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(TYPED_HOOKS_FILE), b"invalid JSON").unwrap();
        let snapshot = DirectorySnapshot::open(dir.path()).unwrap();
        let native = Arc::new(NativeHooks::new(dir.path(), Some(snapshot)));
        let _bound = bind_native(Arc::clone(&native));
        assert!(without_hooks(|| dispatch_trigger("demo#trigger#0", "a", "b")).is_none());
        {
            let token = crate::cancellation::CancellationToken::new();
            token.cancel();
            let _scope = crate::cancellation::enter(token);
            assert!(dispatch_trigger("demo#trigger#0", "a", "b").is_none());
        }
        assert!(!native.diagnostics().load_attempted);
        assert!(native.diagnostics().captured_bytes > 0);
        assert!(dispatch_trigger("demo#trigger#0", "a", "b").is_none());
        assert!(native.diagnostics().load_attempted);
        assert!(!native.diagnostics().loaded);
        assert_eq!(native.diagnostics().captured_bytes, 0);
    }

    #[test]
    fn native_miss_records_source_missing_and_returns_empty() {
        let native = Arc::new(NativeHooks {
            source: None,
            catalog: OnceLock::from(None),
        });
        let _bound = bind_native(Arc::clone(&native));
        assert!(dispatch_trigger("missing#trigger#0", "a", "b").is_none());
        let record = last_diagnostic().expect("native miss records a diagnostic");
        assert_eq!(record.hook_id, "missing#trigger#0");
        assert_eq!(record.outcome, HookDiagnostic::SourceMissing);
    }

    /// Two git triggers with distinct bodies, then npm's hook with a third.
    fn block_entries() -> [(String, JsonValue); 3] {
        [
            test_typed_entry(
                "git#trigger#0",
                "trigger",
                serde_json::json!({"op": "bool", "value": true}),
            ),
            test_typed_entry(
                "git#trigger#1",
                "trigger",
                serde_json::json!({"op": "bool", "value": false}),
            ),
            test_typed_entry(
                "npm#getQueryTerm#0",
                "getQueryTerm",
                serde_json::json!({"op": "string", "value": "x"}),
            ),
        ]
    }

    fn write_catalog(dir: &Path, entries: Vec<(String, JsonValue)>) {
        let catalog = serde_json::json!({
            "version": 1,
            "kind": "typed-hook-expressions",
            "contracts": test_sidecar_contracts(),
            "hooks": JsonValue::Object(entries.into_iter().collect()),
        });
        std::fs::write(dir.join(TYPED_HOOKS_FILE), catalog.to_string()).unwrap();
    }

    #[test]
    fn a_snapshot_parses_only_the_dispatched_commands_block() {
        let dir = tempfile::tempdir().unwrap();
        write_catalog(dir.path(), block_entries().into());
        let snapshot = DirectorySnapshot::open(dir.path()).unwrap();
        let native = Arc::new(NativeHooks::new(dir.path(), Some(snapshot.clone())));
        {
            let _bound = bind_native(Arc::clone(&native));
            assert_eq!(dispatch_trigger("git#trigger#0", "a", "b"), Some(true));
            assert_eq!(dispatch_trigger("git#trigger#1", "a", "b"), Some(false));
            assert!(dispatch_trigger("missing#trigger#0", "a", "b").is_none());
        }
        let diagnostics = native.diagnostics();
        assert!(diagnostics.loaded);
        assert_eq!(diagnostics.typed_entries, 3, "the index counts every hook");
        assert_eq!(diagnostics.descriptor_count, 2, "only git's block was parsed");
        assert_eq!(diagnostics.captured_bytes, 0, "the first index build consumes them");
        // The engine replaces its owner after every idle grace; the next one
        // reads its block through the index the snapshot kept.
        let next = Arc::new(NativeHooks::new(dir.path(), Some(snapshot)));
        let _bound = bind_native(Arc::clone(&next));
        assert_eq!(
            dispatch_get_query_term("npm#getQueryTerm#0", "abc").as_deref(),
            Some("x")
        );
        assert_eq!(next.diagnostics().descriptor_count, 1);
        assert_eq!(next.diagnostics().typed_entries, 3);
    }

    #[test]
    fn a_catalog_without_contiguous_blocks_is_parsed_whole() {
        let dir = tempfile::tempdir().unwrap();
        let [git_true, git_false, npm] = block_entries();
        write_catalog(dir.path(), vec![git_true, npm, git_false]);
        let snapshot = DirectorySnapshot::open(dir.path()).unwrap();
        let native = Arc::new(NativeHooks::new(dir.path(), Some(snapshot)));
        let _bound = bind_native(Arc::clone(&native));
        assert_eq!(dispatch_trigger("git#trigger#1", "a", "b"), Some(false));
        assert_eq!(native.diagnostics().descriptor_count, 3, "the whole catalog was parsed");
    }

    #[test]
    fn an_escaped_key_cannot_be_indexed_and_is_parsed_whole() {
        let dir = tempfile::tempdir().unwrap();
        write_catalog(dir.path(), block_entries().into());
        let path = dir.path().join(TYPED_HOOKS_FILE);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("\"git#trigger#1\"", "\"git\\u0023trigger#1\"")).unwrap();
        let snapshot = DirectorySnapshot::open(dir.path()).unwrap();
        let native = Arc::new(NativeHooks::new(dir.path(), Some(snapshot)));
        let _bound = bind_native(Arc::clone(&native));
        assert_eq!(dispatch_trigger("git#trigger#1", "a", "b"), Some(false));
        assert_eq!(native.diagnostics().descriptor_count, 3, "the whole catalog was parsed");
    }
}

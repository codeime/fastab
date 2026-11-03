//! Completion engine: load spec IR and run lookup.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::hook_backend::{self, NativeHooks};
use crate::hook_types::ShellContext;
use crate::ir::Registry;
use crate::lookup;
use crate::rank::{self, Frecency};

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn history_loading_enabled(disabled: bool) -> bool {
    !disabled
}

fn should_merge_history(include_history: bool, disabled: bool) -> bool {
    include_history && history_loading_enabled(disabled)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteRequest {
    /// Desktop-only opt-in. Never enabled by serialized CLI/protocol input.
    #[serde(skip)]
    pub include_public_ai: bool,
    /// Desktop-only: the caller re-requests on `pending_generators`, so a
    /// generator that ran slow last time can wait for that follow-up.
    #[serde(skip)]
    pub defer_slow_generators: bool,
    pub buffer: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub cursor: Option<u32>,
    #[serde(default)]
    pub fuzzy: bool,
    #[serde(default)]
    pub history_only: bool,
    #[serde(default = "default_true")]
    pub include_history: bool,
    #[serde(default = "default_false")]
    pub suggest_first_token: bool,
    /// Shell executable/path for selecting shell-specific history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_shell: Option<String>,
    /// Current process name/path, used when the integration does not expose a
    /// dedicated shell field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_process: Option<String>,
    /// Shell environment reported by the terminal integration. Fig `custom`
    /// generators read it through `context.environmentVariables`.
    ///
    /// Shared across the overlay request and native hooks so a keystroke does
    /// not clone every `KEY=value` pair.
    #[serde(default, skip_serializing_if = "empty_env")]
    pub environment_variables: Arc<Vec<(String, String)>>,
    /// Raw `alias` output from the shell integration. Fig expanded argv0
    /// from this map before walking specs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

fn empty_env(value: &Arc<Vec<(String, String)>>) -> bool {
    value.is_empty()
}

impl Default for CompleteRequest {
    fn default() -> Self {
        Self {
            include_public_ai: false,
            defer_slow_generators: false,
            buffer: String::new(),
            cwd: String::new(),
            cursor: None,
            fuzzy: false,
            history_only: false,
            include_history: true,
            suggest_first_token: false,
            current_shell: None,
            current_process: None,
            environment_variables: Arc::new(Vec::new()),
            alias: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Suggestion {
    /// Provenance for the separate, constrained AI projection; not wire data.
    #[serde(skip)]
    pub public_ai_candidate: Option<crate::public_ai::PublicAiCandidate>,
    /// The row came from the active parser argument, even when cwd is absent
    /// and no durable scope can be formed. It must never use global recency.
    #[serde(skip)]
    pub argument_value: bool,
    /// Versioned digest of cwd plus the parser's actual argument slot. This
    /// travels with the row to acceptance; the UI never guesses it later.
    #[serde(skip)]
    pub acceptance_scope: Option<String>,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub args_hint: String,
    /// Explicit shell text.  This is intentionally separate from `name`: Fig
    /// suggestions often display a friendly label but insert a command, an
    /// option alias, or a history line instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The first spelling from the source suggestion's `name` array.  The
    /// WebView uses this primary name when deciding whether an exact alias
    /// may receive an auto-execute row, even when a different alias was
    /// selected for display/insertion.
    #[serde(skip)]
    pub primary_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separator_to_add: Option<String>,
    #[serde(default)]
    pub should_add_space: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub priority: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_type: Option<String>,
    /// The query term after applying a static string `getQueryTerm` rule.
    /// This is per suggestion because a single result can contain rows from
    /// different generators, each with a different delimiter.  The raw
    /// [`CompleteResult::search_term`] remains available for rows without a
    /// query-term override and for shell deletion bookkeeping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_term: Option<String>,
    #[serde(default)]
    pub is_dangerous: bool,
    /// Internal parser fact used to decide whether an exact row may become an
    /// auto-execute action. `args_hint` cannot carry this reliably because Fig
    /// permits mandatory arguments without a display name.
    #[serde(default, skip_serializing_if = "is_false")]
    pub requires_arg: bool,
    /// Every spelling from the source `name` array. History dedup matches
    /// against this full set, the same way the WebView used `makeArray(name)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alias_names: Vec<String>,
}

impl Suggestion {
    pub fn new(name: impl Into<String>, description: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            public_ai_candidate: None,
            argument_value: false,
            acceptance_scope: None,
            name: name.into(),
            description: description.into(),
            kind: kind.into(),
            args_hint: String::new(),
            insert_value: None,
            display_name: None,
            primary_name: None,
            separator_to_add: None,
            should_add_space: false,
            hidden: false,
            // Fig's priority normalizer treats an omitted/zero priority as 50.
            priority: 50,
            icon: None,
            original_type: None,
            query_term: None,
            is_dangerous: false,
            requires_arg: false,
            alias_names: Vec::new(),
        }
    }

    pub fn with_alias_names(mut self, names: Vec<String>) -> Self {
        self.alias_names = names;
        self
    }

    pub fn with_args_hint(mut self, hint: impl Into<String>) -> Self {
        self.args_hint = hint.into();
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_meta(
        mut self,
        insert_value: Option<String>,
        display_name: Option<String>,
        separator_to_add: Option<String>,
        should_add_space: bool,
        hidden: bool,
        priority: Option<i64>,
        icon: Option<String>,
    ) -> Self {
        self.insert_value = insert_value;
        self.display_name = display_name;
        self.separator_to_add = separator_to_add;
        self.should_add_space = should_add_space;
        self.hidden = hidden;
        self.priority = priority.map_or(50, normalize_priority);
        self.icon = icon;
        self
    }

    pub fn with_dangerous(mut self, dangerous: bool) -> Self {
        self.is_dangerous = dangerous;
        self
    }

    pub fn with_insert_value(mut self, value: impl Into<String>) -> Self {
        self.insert_value = Some(value.into());
        self
    }

    pub fn with_primary_name(mut self, value: Option<String>) -> Self {
        self.primary_name = value;
        self
    }

    pub fn with_original_type(mut self, original_type: Option<String>) -> Self {
        self.original_type = original_type;
        self
    }

    pub fn with_query_term(mut self, query_term: Option<String>) -> Self {
        self.query_term = query_term;
        self
    }

    pub fn with_priority(mut self, priority: i64) -> Self {
        self.priority = normalize_priority(priority);
        self
    }
}

fn normalize_priority(priority: i64) -> i64 {
    if priority == 0 { 50 } else { priority.clamp(0, 100) }
}

/// Apply the serializable subset of Fig's `getQueryTerm` contract.
///
/// The WebView helper uses `searchTerm.slice(lastIndexOf(separator) + 1)`
/// rather than the separator's full length.  Keep that exact behavior for
/// compatibility; bundled separators are normally one character (`/`, `:`,
/// `,`, or `=`).  A missing separator leaves the whole search term intact.
pub fn query_term_for(search_term: &str, separator: Option<&str>) -> String {
    let Some(separator) = separator else {
        return search_term.to_string();
    };
    if separator.is_empty() {
        return String::new();
    }
    let Some(index) = search_term.rfind(separator) else {
        return search_term.to_string();
    };
    let Some(first) = search_term[index..].chars().next() else {
        return String::new();
    };
    search_term[index + first.len_utf8()..].to_string()
}

/// String `getQueryTerm` first; function form next. A throwing function keeps
/// the whole search term, matching `getQueryTermForSuggestion`.
pub fn query_term_with_hook(search_term: &str, separator: Option<&str>, js_hook: Option<&str>) -> String {
    if let Some(hook_id) = js_hook.filter(|id| !id.is_empty())
        && let Some(term) = crate::hook_backend::dispatch_get_query_term(hook_id, search_term)
    {
        return term;
    }
    query_term_for(search_term, separator)
}

/// Compute the matching term for one static suggestion. Explicit string
/// getQueryTerm has priority; shortcut rows use the legacy `?` prefix rule
/// only when no explicit query-term override is present.
pub(crate) fn suggestion_query_term_with_hook(
    kind: &str,
    explicit_separator: Option<&str>,
    js_hook: Option<&str>,
    query: &str,
    search_term: &str,
) -> (String, Option<String>) {
    if explicit_separator.is_some() || js_hook.is_some() {
        let term = query_term_with_hook(search_term, explicit_separator, js_hook);
        return (term.clone(), Some(term));
    }
    if kind == "shortcut" && search_term.starts_with('?') {
        let term = search_term[1..].to_string();
        return (term.clone(), Some(term));
    }
    (query.to_string(), None)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CurrentArg {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompleteResult {
    #[serde(skip)]
    pub public_ai_context: Option<crate::public_ai::PublicAiContext>,
    pub suggestions: Vec<Suggestion>,
    /// Effective fuzzy/prefix mode after applying the current spec/argument
    /// filterStrategy. This is distinct from CompleteRequest::fuzzy, which is
    /// only the user's setting.
    #[serde(default)]
    pub fuzzy: bool,
    #[serde(default)]
    pub search_term: String,
    /// Normalized token used for matching/ranking. `search_term` remains the
    /// raw shell text so the overlay can delete exactly what is under the
    /// caret (including quotes and escaped spaces).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub match_term: String,
    /// The argument currently being completed.  This is intentionally small:
    /// the overlay uses it as a fallback description when no suggestion is
    /// selected (for example a required special argument with no results).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_arg: Option<CurrentArg>,
    /// Generators were delayed by `debounce` and should be requested again
    /// after [`Self::debounce_ms`]. Static rows in this result are current.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pending_generators: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce_ms: Option<i64>,
}

/// Return the root-command key used by native ranking for this edit buffer.
/// Acceptance recording must use the same normalized token as completion
/// ranking, otherwise quoted commands and a cursor before the buffer end
/// would write into a different recency bucket.
pub fn ranking_root_command(buffer: &str, cursor: Option<u32>) -> String {
    lookup::tokenize(lookup::completion_buffer(buffer, cursor))
        .0
        .into_iter()
        .next()
        .unwrap_or_default()
}

pub struct Engine {
    specs_dir: PathBuf,
    registry: Registry,
    native: Option<Arc<NativeHooks>>,
    hook_cache: Arc<crate::hook_cache::HookCache>,
    frecency: Frecency,
    acceptance: Arc<Mutex<rank::AcceptanceIndex>>,
    frecency_loaded: bool,
    history_source: Option<rank::HistorySourceConfig>,
    /// Spec-aware `template: "history"` index over the frecency lines. Rebuilt
    /// only when the loaded history changes; requests share it by `Arc`.
    history: Arc<crate::history::HistoryStore>,
    /// Generator cache that has to survive the per-request attempt thread.
    generator_session: crate::generate::GeneratorSession,
    /// Hook results and the native catalog belong to the current normal input. History-only and
    /// cancelled attempts cannot acquire ownership or renew an idle deadline.
    hook_cache_active: bool,
    hook_cache_idle_since: Option<std::time::Instant>,
    /// Shares the hook result owner, but has a deadline even with empty caches.
    native_idle_since: Option<std::time::Instant>,
    history_index_idle_since: Option<std::time::Instant>,
}

impl Engine {
    pub fn new(specs_dir: PathBuf) -> anyhow::Result<Self> {
        Self::new_with_acceptance(specs_dir, Arc::new(Mutex::new(rank::AcceptanceIndex::load())))
    }

    pub(crate) fn new_with_acceptance(
        specs_dir: PathBuf,
        acceptance: Arc<Mutex<rank::AcceptanceIndex>>,
    ) -> anyhow::Result<Self> {
        let registry = Self::load_registry(&specs_dir)?;
        Ok(Self::from_registry(&specs_dir, registry, acceptance))
    }

    /// Index the specs directory without parsing any spec: `Registry` resolves
    /// files lazily, so this is only `index.json` plus a directory walk.
    pub(crate) fn load_registry(specs_dir: &Path) -> anyhow::Result<Registry> {
        // A generated IR directory is published by renaming the old tree out
        // of the way and then renaming the validated tree into place. There is
        // necessarily a very small interval in which `specs_dir` does not
        // exist. Treating that interval as a valid empty registry is unsafe:
        // the worker would cache it as its pristine template and keep serving
        // no completions after the new tree appears. Existing directories are
        // still allowed to be handwritten/legacy fixtures; that compatibility
        // path is important for headless tests and local overlays.
        let metadata = std::fs::metadata(specs_dir)
            .map_err(|error| anyhow::anyhow!("specs IR directory is unavailable: {}: {error}", specs_dir.display()))?;
        if !metadata.is_dir() {
            anyhow::bail!("specs IR path is not a directory: {}", specs_dir.display());
        }

        let mut registry = Registry::load(specs_dir)?;
        overlay_local_spec_dirs(&mut registry);
        Ok(registry)
    }

    /// Build around an existing index. The supervisor uses this to recover from
    /// a timed-out attempt without touching the specs directory again.
    pub(crate) fn from_registry(
        specs_dir: &Path,
        registry: Registry,
        acceptance: Arc<Mutex<rank::AcceptanceIndex>>,
    ) -> Self {
        let native = Arc::new(NativeHooks::new(specs_dir, registry.snapshot()));
        Self {
            specs_dir: specs_dir.to_path_buf(),
            registry,
            native: Some(native),
            hook_cache: crate::hook_cache::HookCache::new(),
            frecency: Frecency::default(),
            acceptance,
            frecency_loaded: false,
            history_source: None,
            history: Arc::default(),
            generator_session: crate::generate::GeneratorSession::default(),
            hook_cache_active: false,
            hook_cache_idle_since: None,
            // Construction can race cancellation before complete's first
            // check. Until a normal result commits, this owner is idle.
            native_idle_since: Some(std::time::Instant::now()),
            history_index_idle_since: None,
        }
    }

    pub fn new_with_frecency(specs_dir: PathBuf, frecency: Frecency) -> anyhow::Result<Self> {
        Self::new_with_frecency_and_acceptance(
            specs_dir,
            frecency,
            // This constructor is used by tests and headless embeddings that
            // provide their own ranking input. Keep it deterministic and do
            // not read or write the user's acceptance database.
            Arc::new(Mutex::new(rank::AcceptanceIndex::default())),
        )
    }

    pub(crate) fn new_with_frecency_and_acceptance(
        specs_dir: PathBuf,
        frecency: Frecency,
        acceptance: Arc<Mutex<rank::AcceptanceIndex>>,
    ) -> anyhow::Result<Self> {
        let registry = Self::load_registry(&specs_dir)?;
        let mut engine = Self::from_registry(&specs_dir, registry, acceptance);
        engine.frecency = frecency;
        engine.frecency_loaded = true;
        // `new_with_frecency` is the test/embedding constructor that
        // intentionally supplies its own ranking data. Treat it as the
        // default source until a request asks for a different shell or
        // history setting.
        engine.history_source = Some(rank::HistorySourceConfig {
            custom_command: None,
            all_shells: false,
            current_shell: rank::HistoryShell::Unknown,
        });
        Ok(engine)
    }

    fn rebind_hosts(&mut self, registry: &Registry) {
        self.native = Some(Arc::new(NativeHooks::new(&self.specs_dir, registry.snapshot())));
        // Refresh can be followed by an immediate cancellation before the
        // attempt's settlement path. Preserve any existing grace clock.
        self.note_idle_native(std::time::Instant::now());
        // A new spec generation clears data, not this Engine's lifetime counters.
        self.hook_cache.clear();
    }

    /// The current input explicitly ended. Preserve running grace clocks;
    /// ending twice must not keep an otherwise idle tree alive.
    pub(crate) fn end_input(&mut self) {
        let now = std::time::Instant::now();
        self.generator_session = crate::generate::GeneratorSession::default();
        self.hook_cache_active = false;
        self.note_idle_hook_cache(false, now);
        self.note_idle_native(now);
        self.note_idle_history_indexes(false, now);
        self.registry.begin_idle_completion();
        self.registry.note_idle_after_complete(now);
    }

    /// Read numeric resources without loading, refreshing, or touching caches.
    pub fn diagnostics(&self) -> crate::diagnostics::EngineDiagnostics {
        self.diagnostics_with_grace(crate::ir::SPEC_IDLE_GRACE)
    }

    pub(crate) fn diagnostics_with_grace(&self, grace: std::time::Duration) -> crate::diagnostics::EngineDiagnostics {
        let mut history = self.history.diagnostics();
        let (nodes, entries, bytes) = self.frecency.next_words_diagnostics();
        history.next_word_nodes = nodes;
        history.next_word_entries = entries;
        history.next_word_bytes = bytes;
        history.allocated_bytes += bytes;
        crate::diagnostics::EngineDiagnostics {
            registry: self.registry.diagnostics(std::time::Instant::now(), grace),
            hooks: self.hook_cache.diagnostics(),
            hook_catalog: self
                .native
                .as_ref()
                .map_or_else(Default::default, |native| native.diagnostics()),
            history,
            generator_session: self.generator_session.diagnostics(),
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The WebView's `clear-cache` event (`ftab hook clear-autocomplete-cache`):
    /// `resetCaches()` dropped every loaded and generated spec, and
    /// `generatorCache.clear()` every generator result. Re-index the specs
    /// directory so a spec edited under `devCompletionsFolder` or
    /// `~/.fig/autocomplete/build` is read again, and forget every hook
    /// result, the debounce session and the history argument index built on
    /// the old specs.
    pub fn clear_caches(&mut self) {
        let _ = self.clear_caches_and_report();
    }

    /// Release expired specs, hook results and the catalog once the worker regains Engine.
    pub(crate) fn release_idle_specs(&mut self, now: std::time::Instant, grace: std::time::Duration) {
        self.registry.release_idle(now, grace);
        if !self.hook_cache_active
            && self
                .history_index_idle_since
                .is_some_and(|since| now.saturating_duration_since(since) >= grace)
        {
            self.history.release_derived_indexes();
            self.history_index_idle_since = None;
        }
        if !self.hook_cache_active
            && self
                .hook_cache_idle_since
                .is_some_and(|since| now.saturating_duration_since(since) >= grace)
        {
            self.hook_cache.release_idle();
            self.hook_cache_idle_since = None;
        }
        if !self.hook_cache_active
            && self
                .native_idle_since
                .is_some_and(|since| now.saturating_duration_since(since) >= grace)
        {
            self.native = None;
            self.native_idle_since = None;
        }
    }

    /// Earliest resource deadline, including hooks when no spec remains cached.
    pub(crate) fn next_idle_deadline(&self, grace: std::time::Duration) -> Option<std::time::Instant> {
        let hook_deadline = (!self.hook_cache_active && self.hook_cache.has_idle_results())
            .then_some(self.hook_cache_idle_since)
            .flatten()
            .and_then(|since| since.checked_add(grace));
        let native_deadline = (!self.hook_cache_active && self.native.is_some())
            .then_some(self.native_idle_since)
            .flatten()
            .and_then(|since| since.checked_add(grace));
        let history_deadline = (!self.hook_cache_active && self.history.has_derived_indexes())
            .then_some(self.history_index_idle_since)
            .flatten()
            .and_then(|since| since.checked_add(grace));
        self.registry
            .next_idle_deadline(grace)
            .into_iter()
            .chain(hook_deadline)
            .chain(native_deadline)
            .chain(history_deadline)
            .min()
    }

    fn note_idle_hook_cache(&mut self, was_empty: bool, now: std::time::Instant) {
        if !self.hook_cache_active
            && self.hook_cache.has_idle_results()
            && (was_empty || self.hook_cache_idle_since.is_none())
        {
            self.hook_cache_idle_since = Some(now);
        }
    }

    fn note_idle_native(&mut self, now: std::time::Instant) {
        if !self.hook_cache_active && self.native.is_some() && self.native_idle_since.is_none() {
            self.native_idle_since = Some(now);
        }
    }

    fn note_idle_history_indexes(&mut self, was_empty: bool, now: std::time::Instant) {
        if !self.hook_cache_active
            && self.history.has_derived_indexes()
            && (was_empty || self.history_index_idle_since.is_none())
        {
            self.history_index_idle_since = Some(now);
        }
    }

    /// Bind a lazy owner after generation refresh. Only hook dispatch parses
    /// the catalog; missing/rejected is retained as an initialized empty owner.
    fn ensure_native(&mut self) -> Arc<NativeHooks> {
        Arc::clone(
            self.native
                .get_or_insert_with(|| Arc::new(NativeHooks::new(&self.specs_dir, self.registry.snapshot()))),
        )
    }

    #[cfg(test)]
    // Preserve absent, active, and pending marks for worker lifecycle assertions.
    #[allow(clippy::option_option)]
    pub(crate) fn idle_file_state(&self, relative: &str) -> (bool, Option<Option<std::time::Instant>>) {
        (
            self.registry.cached_load_spec(relative).is_some(),
            self.registry.idle_mark(relative),
        )
    }

    /// Same reset as [`Self::clear_caches`], with an indication that the
    /// canonical directory was successfully reopened. The worker uses this
    /// to avoid discarding its last good registry during an install rename
    /// window.
    pub(crate) fn clear_caches_and_report(&mut self) -> bool {
        self.hook_cache.clear();
        self.hook_cache_idle_since = None;
        self.generator_session = crate::generate::GeneratorSession::default();
        self.history = Arc::default();
        self.history_index_idle_since = None;
        match Self::load_registry(&self.specs_dir) {
            Ok(registry) => {
                // A successful reload is a new generation. Rebind both hosts
                // together with the Registry so hooks and lazy specs cannot
                // observe different trees.
                self.rebind_hosts(&registry);
                self.registry = registry;
                self.hook_cache_active = false;
                self.native_idle_since = None;
                self.note_idle_native(std::time::Instant::now());
                true
            },
            Err(err) => {
                tracing::warn!(%err, "clear-cache: specs directory could not be re-indexed");
                false
            },
        }
    }

    fn refresh_specs_generation(&mut self) {
        if !self.registry.needs_refresh() {
            return;
        }
        match Self::load_registry(&self.specs_dir) {
            Ok(registry) => {
                self.rebind_hosts(&registry);
                self.registry = registry;
                // GeneratorSession lives outside the Registry/hosts and can
                // otherwise replay a custom result from the prior generation.
                self.generator_session = crate::generate::GeneratorSession::default();
                self.history = Arc::default();
                self.history_index_idle_since = None;
            },
            Err(error) => {
                // The publisher may currently have the canonical path
                // absent. Keep the last generation for this request; the next
                // request retries the complete Registry+host rebuild.
                tracing::debug!(%error, "spec generation refresh deferred");
            },
        }
    }

    /// Record a successful completion acceptance. This updates the engine's
    /// in-memory ranking immediately and best-effort persists it to the shared
    /// SQLite-backed state store.
    pub fn record_acceptance(&mut self, root_command: &str, accepted_name: &str) {
        let timestamp = rank::AcceptanceIndex::now_millis();
        self.record_acceptance_at(root_command, accepted_name, timestamp);
    }

    pub(crate) fn record_acceptance_at(&mut self, root_command: &str, accepted_name: &str, timestamp: u64) {
        // Persist from a snapshot taken outside the lock: ranking clones this
        // index on every completion, and a slow SQLite write while holding the
        // mutex would stall those attempts.
        let snapshot = {
            let mut acceptance = self.acceptance.lock().unwrap_or_else(|err| err.into_inner());
            acceptance
                .record_at(root_command, accepted_name, timestamp)
                .then(|| acceptance.clone())
        };
        if let Some(snapshot) = snapshot {
            snapshot.persist();
        }
    }

    pub(crate) fn record_scoped_acceptance_at(&mut self, scope: &str, accepted_name: &str, timestamp: u64) {
        let snapshot = {
            let mut acceptance = self.acceptance.lock().unwrap_or_else(|err| err.into_inner());
            acceptance
                .record_scoped_at(scope, accepted_name, timestamp)
                .then(|| acceptance.scoped_snapshot())
        };
        if let Some(snapshot) = snapshot {
            snapshot.persist();
        }
    }

    fn ensure_frecency(&mut self, request: &CompleteRequest) {
        let custom_command = fastab_settings::settings::get_string_or("beta.history.customCommand", String::new());
        let custom_command = (!custom_command.is_empty()).then_some(custom_command);
        let all_shells = fastab_settings::settings::get_bool_or("beta.history.allShells", false);
        let source = rank::history_source_config(
            custom_command,
            all_shells,
            request.current_shell.as_deref(),
            request.current_process.as_deref(),
        );
        if self.frecency_loaded && self.history_source.as_ref() == Some(&source) {
            return;
        }
        let commands = rank::load_commands_for(&source);
        if crate::cancellation::is_cancelled() {
            return;
        }
        let Ok(frecency) = Frecency::try_from_commands(commands) else {
            return;
        };
        let _ = crate::cancellation::commit_if_active(|| {
            self.frecency = frecency;
            self.frecency_loaded = true;
            self.history_source = Some(source);
        });
    }

    pub fn complete(&mut self, request: CompleteRequest) -> anyhow::Result<CompleteResult> {
        // Even cancellation before the first check must release bytes retained
        // at construction. Refresh may bind a different snapshot, so guard it
        // separately before the second cancellation check as well.
        let _previous_capture = self
            .registry
            .snapshot()
            .map(|snapshot| snapshot.release_captured_after_request());
        crate::cancellation::check()?;
        self.refresh_specs_generation();
        let _capture = self
            .registry
            .snapshot()
            .map(|snapshot| snapshot.release_captured_after_request());
        crate::cancellation::check()?;
        let idle = self.registry.checkpoint_idle();
        let hook_cache_was_empty = !self.hook_cache.has_idle_results();
        let history_indexes_were_empty = !self.history.has_derived_indexes();
        let history_only = request.history_only;
        crate::generate::take_pending_generators();
        crate::generate::install_session(std::mem::take(&mut self.generator_session));
        crate::generate::set_defer_slow_generators(request.defer_slow_generators);
        let result = self.complete_with_thread_session(request);
        crate::generate::set_defer_slow_generators(false);
        let session = crate::generate::take_session();
        match crate::cancellation::finish_if_active(|| {
            self.generator_session = session;
            if result.is_ok() && !history_only {
                self.hook_cache_active = true;
                self.hook_cache_idle_since = None;
                self.native_idle_since = None;
                self.history_index_idle_since = None;
                self.registry.note_idle_after_complete(std::time::Instant::now());
            } else {
                self.note_idle_hook_cache(hook_cache_was_empty, std::time::Instant::now());
                self.note_idle_native(std::time::Instant::now());
                self.note_idle_history_indexes(history_indexes_were_empty, std::time::Instant::now());
                // History-only still loads specs to resolve fuzzy matching.
                // It cannot take ownership of earlier active files, but new
                // files must not be left without an idle deadline.
                self.registry.restore_idle_checkpoint(&idle, std::time::Instant::now());
            }
            result
        }) {
            Ok(result) => result,
            Err(cancelled) => {
                crate::generate::take_pending_generators();
                self.note_idle_hook_cache(hook_cache_was_empty, std::time::Instant::now());
                self.note_idle_native(std::time::Instant::now());
                self.note_idle_history_indexes(history_indexes_were_empty, std::time::Instant::now());
                self.registry.restore_idle_checkpoint(&idle, std::time::Instant::now());
                Err(cancelled.into())
            },
        }
    }

    fn complete_with_thread_session(&mut self, mut request: CompleteRequest) -> anyhow::Result<CompleteResult> {
        self.registry.begin_idle_completion();
        let buffer = lookup::completion_buffer(&request.buffer, request.cursor);
        let (tokens, ends_with_space) = lookup::tokenize(buffer);

        let history_disabled = fastab_settings::settings::get_bool_or("autocomplete.history.disableLoading", false);
        if history_loading_enabled(history_disabled) {
            self.ensure_frecency(&request);
        }
        crate::cancellation::check()?;
        let lines = self.frecency.command_lines();
        if !Arc::ptr_eq(self.history.lines(), &lines) {
            self.history = Arc::new(crate::history::HistoryStore::new(lines));
            self.history_index_idle_since = None;
        }
        crate::generate::set_history(Arc::clone(&self.history));
        if request.history_only {
            let history_search_term = if ends_with_space {
                String::new()
            } else {
                lookup::current_token_raw(buffer)
            };
            let history_match_term = if ends_with_space {
                String::new()
            } else {
                tokens.last().cloned().unwrap_or_default()
            };
            let effective_fuzzy = lookup::effective_fuzzy_for_tokens(
                &mut self.registry,
                request.fuzzy,
                &tokens,
                ends_with_space,
                &history_match_term,
                &history_search_term,
            );
            if history_disabled {
                return Ok(CompleteResult {
                    suggestions: Vec::new(),
                    fuzzy: effective_fuzzy,
                    search_term: history_search_term,
                    match_term: String::new(),
                    ..CompleteResult::default()
                });
            }
            let prefix = rank::history_prefix_from_buffer(buffer, ends_with_space, &tokens);
            let query = history_match_term.as_str();
            let suggestions = prefix.map_or_else(
                || self.frecency.history_suggestions(query, effective_fuzzy, true),
                |prefix| {
                    self.frecency
                        .history_suffix_suggestions(&prefix, query, effective_fuzzy)
                },
            );
            return Ok(CompleteResult {
                suggestions,
                fuzzy: effective_fuzzy,
                search_term: history_search_term,
                match_term: history_match_term,
                ..CompleteResult::default()
            });
        }
        let mut result = {
            let _native = hook_backend::bind_native(self.ensure_native());
            let shell = ShellContext {
                current_process: request.current_process.clone().unwrap_or_default(),
                environment_variables: std::mem::take(&mut request.environment_variables).into(),
            };
            let registry = &mut self.registry;
            let _cache = self.hook_cache.bind();
            hook_backend::enter_context(&request.cwd, &shell, || lookup::complete(registry, &request))
        };
        crate::cancellation::check()?;
        if should_merge_history(request.include_history, history_disabled) {
            let effective_fuzzy = result.fuzzy;
            let prefix = rank::history_prefix_from_buffer(buffer, ends_with_space, &tokens);
            rank::merge_history_with_prefix(&mut result, &tokens, prefix, &self.frecency, effective_fuzzy);
        }
        crate::public_ai::validate_provenance(&mut result);
        let alphabetical =
            fastab_settings::settings::get_string_or("autocomplete.sortMethod", "default".into()) == "alphabetical";
        let root_command = ranking_root_command(&request.buffer, request.cursor);
        {
            let acceptance = self.acceptance.lock().unwrap_or_else(|err| err.into_inner());
            if history_disabled {
                // A setting can change while the engine stays alive. Do not let
                // frecency loaded by an earlier request influence ranking after
                // history loading has been disabled.
                rank::apply_with_acceptance(
                    &mut result,
                    &tokens,
                    &Frecency::default(),
                    &acceptance,
                    &root_command,
                    alphabetical,
                );
            } else {
                rank::apply_with_acceptance(
                    &mut result,
                    &tokens,
                    &self.frecency,
                    &acceptance,
                    &root_command,
                    alphabetical,
                );
            }
        }
        crate::public_ai::finalize_ranked_candidates(&mut result);
        Ok(result)
    }
}

/// Fig `importSpecFromLocation`: `devCompletionsFolder` is consulted first,
/// and only while `isInDevMode()` (`autocomplete.developerMode` or
/// `autocomplete.developerModeNPM`); `~/.fig/autocomplete/build` is reached
/// only when `publicSpecExists(name)` is false, so it never shadows a bundled
/// spec.
fn overlay_local_spec_dirs(registry: &mut Registry) {
    let dev_mode = fastab_settings::settings::get_bool_or("autocomplete.developerMode", false)
        || fastab_settings::settings::get_bool_or("autocomplete.developerModeNPM", false);
    if dev_mode
        && let Ok(Some(dev)) = fastab_settings::settings::get_string("autocomplete.devCompletionsFolder")
        && !dev.is_empty()
    {
        registry.overlay_specs_dir(std::path::Path::new(&dev), crate::ir::OverlayMode::Replace);
    }
    if let Some(home) = std::env::var_os("HOME") {
        registry.overlay_specs_dir(
            &std::path::PathBuf::from(home).join(".fig/autocomplete/build"),
            crate::ir::OverlayMode::FillMissing,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::fs;
    use std::time::Duration;

    #[derive(Debug, Deserialize)]
    struct Phase1Golden {
        cases: Vec<Phase1GoldenCase>,
    }

    #[derive(Debug, Deserialize)]
    struct Phase1GoldenCase {
        name: String,
        request: CompleteRequest,
        result: serde_json::Value,
    }

    fn write_spec(dir: &std::path::Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{name}.json")), body).unwrap();
    }

    #[test]
    fn missing_specs_directory_fails_closed_instead_of_becoming_empty_registry() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("specs-ir");

        let error = Engine::load_registry(&missing).expect_err("missing IR must not load as empty");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("specs IR directory is unavailable"), "{rendered}");
        assert!(rendered.contains("specs-ir"), "{rendered}");
    }

    #[test]
    fn existing_empty_directory_remains_a_legacy_fixture() {
        let root = tempfile::tempdir().unwrap();

        let registry = Engine::load_registry(root.path()).expect("an existing fixture directory is valid");
        assert!(registry.is_empty());
    }

    fn populate_hook_results(engine: &Engine) {
        let _bound = engine.hook_cache.bind();
        let arg = crate::ir::ArgSpec {
            cache_strategy: Some("max-age".into()),
            ..crate::ir::ArgSpec::default()
        };
        crate::hook_cache::cached_script_output(&arg, "", "script", || "output".into());
        crate::hook_cache::cached_suggestions(&arg, "", "custom", "rows", || {
            vec![Suggestion::new("result", "", "arg")]
        });
        crate::hook_cache::cached_spec("spec", || Some(crate::ir::Spec::default()));
    }

    #[test]
    fn hook_idle_grace_survives_history_and_repeated_end_without_a_spec_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).unwrap();
        engine.complete(CompleteRequest::default()).unwrap();
        populate_hook_results(&engine);
        let grace = Duration::from_secs(10);
        assert!(engine.registry.next_idle_deadline(grace).is_none());
        assert!(
            engine.next_idle_deadline(grace).is_none(),
            "normal input still owns the hook results"
        );
        engine.end_input();
        let deadline = engine.next_idle_deadline(grace).unwrap();
        engine
            .complete(CompleteRequest {
                history_only: true,
                ..CompleteRequest::default()
            })
            .unwrap();
        engine.end_input();
        assert_eq!(engine.next_idle_deadline(grace), Some(deadline));
        engine.release_idle_specs(deadline - Duration::from_millis(1), grace);
        assert!(engine.hook_cache.has_idle_results());
        engine.release_idle_specs(deadline, grace);
        assert!(!engine.hook_cache.has_idle_results());
        assert!(engine.next_idle_deadline(grace).is_none());
        for stats in [
            engine.diagnostics().hooks.suggestions,
            engine.diagnostics().hooks.script_output,
        ] {
            assert_eq!((stats.entries, stats.allocated_bytes, stats.misses), (0, 0, 1));
        }
        // Fig kept generated specs for the session. The small one stays,
        // and holding it schedules no wake-up (asserted just above).
        let specs = engine.diagnostics().hooks.specs;
        assert_eq!((specs.entries, specs.misses), (1, 1));
        assert!(specs.allocated_bytes > 0);
    }

    #[test]
    fn first_unowned_hook_results_get_a_fresh_grace_and_clear_retires_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).unwrap();
        let now = std::time::Instant::now();
        engine.release_idle_specs(now, Duration::ZERO);
        let grace = Duration::from_secs(10);
        engine.hook_cache_idle_since = Some(now - Duration::from_secs(100));
        let was_empty = !engine.hook_cache.has_idle_results();
        populate_hook_results(&engine);
        // This is the same settlement used by history-only, failed and
        // cancelled attempts: an earlier empty cache has no old owner/deadline.
        engine.note_idle_hook_cache(was_empty, now);
        assert_eq!(engine.next_idle_deadline(grace), Some(now + grace));
        engine.note_idle_hook_cache(false, now + Duration::from_secs(8));
        assert_eq!(engine.next_idle_deadline(grace), Some(now + grace));
        engine.complete(CompleteRequest::default()).unwrap();
        assert!(engine.next_idle_deadline(grace).is_none());
        assert!(engine.clear_caches_and_report());
        assert!(
            !engine.hook_cache_active,
            "clear-cache also retires the worker's active owner"
        );
        assert!(engine.hook_cache_idle_since.is_none());
    }

    fn write_typed_custom(dir: &std::path::Path, hook_id: &str, name: &str) {
        let (id, entry) = crate::hook_backend::test_typed_entry(
            hook_id,
            "custom",
            serde_json::json!({
                "op": "array",
                "items": [{
                    "op": "object",
                    "fields": [{"key": "name", "value": {"op": "string", "value": name}}]
                }]
            }),
        );
        let catalog = serde_json::json!({
            "version": 1,
            "kind": "typed-hook-expressions",
            "contracts": crate::hook_backend::test_sidecar_contracts(),
            "hooks": { id: entry }
        });
        fs::write(dir.join("typed-hooks.json"), format!("{catalog}\n")).unwrap();
    }

    #[test]
    fn static_completions_release_captured_bytes_without_loading_hooks() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "demo",
            r#"{"names":["demo"],"subcommands":[{"names":["static"]}]}"#,
        );
        write_typed_custom(dir.path(), "demo#custom#0", "result");
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).unwrap();
        assert!(!engine.diagnostics().hook_catalog.load_attempted);
        assert!(engine.diagnostics().hook_catalog.captured_bytes > 0);
        // First-token suggestions default off; exercise that early return as
        // well as both unfiltered and filtered static spec completions.
        for (buffer, expected_row) in [("d", None), ("demo ", Some("static")), ("demo st", Some("static"))] {
            let result = engine
                .complete(CompleteRequest {
                    buffer: buffer.into(),
                    cwd: dir.path().display().to_string(),
                    include_history: false,
                    ..CompleteRequest::default()
                })
                .unwrap();
            if let Some(expected_row) = expected_row {
                assert!(
                    result.suggestions.iter().any(|row| row.name == expected_row),
                    "{buffer:?}: {:?}",
                    result.suggestions
                );
            } else {
                assert!(result.suggestions.is_empty(), "{buffer:?}: {:?}", result.suggestions);
            }
            assert_eq!(engine.diagnostics().hook_catalog, Default::default());
        }
        let _bound = crate::hook_backend::bind_native(Arc::clone(engine.native.as_ref().unwrap()));
        let rows =
            crate::hook_backend::dispatch_custom("demo#custom#0", &[], "/tmp", "", Duration::from_secs(1), false)
                .unwrap();
        assert_eq!(
            rows[0].name, "result",
            "first real hook verifies and reads the released sidecar"
        );
        assert!(engine.diagnostics().hook_catalog.loaded);
        assert_eq!(engine.diagnostics().hook_catalog.captured_bytes, 0);
    }

    #[test]
    fn cold_history_and_cancellation_release_sidecar_without_loading_hooks() {
        let dir = tempfile::tempdir().unwrap();
        write_typed_custom(dir.path(), "demo#custom#0", "result");
        for cancelled in [false, true] {
            let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).unwrap();
            assert!(engine.diagnostics().hook_catalog.captured_bytes > 0);
            let token = crate::cancellation::CancellationToken::new();
            if cancelled {
                token.cancel();
            }
            let _scope = crate::cancellation::enter(token);
            let result = engine.complete(CompleteRequest {
                history_only: true,
                ..CompleteRequest::default()
            });
            assert_eq!(result.is_err(), cancelled);
            assert_eq!(engine.diagnostics().hook_catalog, Default::default());
        }
    }

    #[test]
    fn unused_sidecar_is_released_from_shared_snapshot_on_idle_owner_drop() {
        let dir = tempfile::tempdir().unwrap();
        write_typed_custom(dir.path(), "demo#custom#0", "result");
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).unwrap();
        let snapshot = engine.registry.snapshot().unwrap();
        assert!(snapshot.captured_bytes() > 0);
        engine.release_idle_specs(std::time::Instant::now(), Duration::ZERO);
        assert_eq!(
            snapshot.captured_bytes(),
            0,
            "a pristine registry clone cannot pin unused sidecar bytes"
        );
        assert_eq!(engine.diagnostics().hook_catalog, Default::default());
    }

    #[test]
    fn missing_or_rejected_catalog_is_not_retried_until_owner_rebinds() {
        for malformed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            if malformed {
                fs::write(dir.path().join("typed-hooks.json"), b"invalid JSON").unwrap();
            }
            let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).unwrap();
            engine.complete(CompleteRequest::default()).unwrap();
            assert_eq!(engine.diagnostics().hook_catalog, Default::default());
            let dispatch = |engine: &mut Engine| {
                let _bound = crate::hook_backend::bind_native(engine.ensure_native());
                crate::hook_backend::dispatch_custom("demo#custom#0", &[], "/tmp", "", Duration::from_secs(1), false)
            };
            assert!(dispatch(&mut engine).is_none());
            assert!(engine.diagnostics().hook_catalog.load_attempted);
            assert!(!engine.diagnostics().hook_catalog.loaded);
            write_typed_custom(dir.path(), "demo#custom#0", "fixed");
            assert!(
                dispatch(&mut engine).is_none(),
                "an attempted empty owner must not retry"
            );
            assert!(engine.clear_caches_and_report());
            assert!(!engine.diagnostics().hook_catalog.load_attempted);
            assert_eq!(dispatch(&mut engine).unwrap()[0].name, "fixed");
        }
    }

    #[test]
    fn first_hook_after_static_completion_rejects_changed_sidecar_then_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "demo",
            r#"{"names":["demo"],"args":[{"jsCustom":"demo#custom#0"}]}"#,
        );
        write_typed_custom(dir.path(), "demo#custom#0", "from-A");
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).unwrap();
        engine
            .complete(CompleteRequest {
                buffer: "d".into(),
                ..CompleteRequest::default()
            })
            .unwrap();
        assert_eq!(engine.diagnostics().hook_catalog, Default::default());
        write_typed_custom(dir.path(), "demo#custom#0", "from-B");
        let request = CompleteRequest {
            buffer: "demo ".into(),
            cwd: dir.path().display().to_string(),
            include_history: false,
            ..CompleteRequest::default()
        };
        let rejected = engine.complete(request.clone()).unwrap();
        assert!(
            !rejected
                .suggestions
                .iter()
                .any(|row| row.name == "from-A" || row.name == "from-B")
        );
        assert!(engine.diagnostics().hook_catalog.load_attempted);
        assert!(!engine.diagnostics().hook_catalog.loaded);
        assert!(engine.registry.needs_refresh());
        let refreshed = engine.complete(request).unwrap();
        assert!(refreshed.suggestions.iter().any(|row| row.name == "from-B"));
    }

    #[test]
    fn catalog_idle_grace_survives_history_repeated_end_and_cancel_without_cached_results() {
        let dir = tempfile::tempdir().unwrap();
        write_typed_custom(dir.path(), "demo#custom#0", "result");
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).unwrap();
        engine.complete(CompleteRequest::default()).unwrap();
        {
            let _bound = crate::hook_backend::bind_native(Arc::clone(engine.native.as_ref().unwrap()));
            assert!(
                crate::hook_backend::dispatch_custom("demo#custom#0", &[], "/tmp", "", Duration::from_secs(1), false,)
                    .is_some()
            );
        }
        let grace = Duration::from_secs(10);
        assert_eq!(engine.diagnostics().registry.cached_file_count, 0);
        assert!(!engine.hook_cache.has_idle_results());
        assert!(engine.diagnostics().hook_catalog.loaded);
        assert!(engine.next_idle_deadline(grace).is_none());
        engine.end_input();
        let deadline = engine.next_idle_deadline(grace).expect("catalog alone has a deadline");
        engine
            .complete(CompleteRequest {
                history_only: true,
                ..CompleteRequest::default()
            })
            .unwrap();
        engine.end_input();
        {
            let token = crate::cancellation::CancellationToken::new();
            token.cancel();
            let _scope = crate::cancellation::enter(token);
            assert!(engine.complete(CompleteRequest::default()).is_err());
        }
        assert_eq!(engine.next_idle_deadline(grace), Some(deadline));
        engine.release_idle_specs(deadline - Duration::from_millis(1), grace);
        assert!(engine.diagnostics().hook_catalog.loaded);
        engine.release_idle_specs(deadline, grace);
        assert_eq!(engine.diagnostics().hook_catalog, Default::default());
        assert!(engine.next_idle_deadline(grace).is_none());
    }

    #[test]
    fn idle_catalog_owner_is_dropped_and_typed_completion_reloads_the_same_result() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "demo",
            r#"{"names":["demo"],"args":[{"jsCustom":"demo#custom#0"}]}"#,
        );
        write_typed_custom(dir.path(), "demo#custom#0", "result");
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).unwrap();
        let request = CompleteRequest {
            buffer: "demo ".into(),
            cwd: dir.path().display().to_string(),
            include_history: false,
            ..CompleteRequest::default()
        };
        let before = engine.complete(request.clone()).unwrap();
        assert!(before.suggestions.iter().any(|row| row.name == "result"));
        assert_eq!(engine.diagnostics().hook_catalog.parsed_descriptor_count, 1);
        assert_eq!(engine.diagnostics().hook_catalog.captured_bytes, 0);
        let old_owner = Arc::downgrade(engine.native.as_ref().unwrap());
        engine.end_input();
        let grace = Duration::from_secs(10);
        engine.release_idle_specs(engine.next_idle_deadline(grace).unwrap(), grace);
        assert!(
            old_owner.upgrade().is_none(),
            "no descriptor owner survives the idle release"
        );
        assert!(!engine.diagnostics().hook_catalog.load_attempted);
        engine
            .complete(CompleteRequest {
                history_only: true,
                ..request.clone()
            })
            .unwrap();
        assert!(
            !engine.diagnostics().hook_catalog.load_attempted,
            "history does not reload hooks"
        );
        let after = engine.complete(request).unwrap();
        assert_eq!(after.suggestions, before.suggestions);
        assert_eq!(engine.diagnostics().hook_catalog.parsed_descriptor_count, 1);
        assert!(engine.next_idle_deadline(grace).is_none());
    }

    #[test]
    fn idle_catalog_reload_verifies_the_snapshot_before_accepting_changed_sidecar_bytes() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "demo",
            r#"{"names":["demo"],"args":[{"jsCustom":"demo#custom#0"}]}"#,
        );
        write_typed_custom(dir.path(), "demo#custom#0", "from-A");
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).unwrap();
        let request = CompleteRequest {
            buffer: "demo ".into(),
            cwd: dir.path().display().to_string(),
            include_history: false,
            ..CompleteRequest::default()
        };
        assert!(
            engine
                .complete(request.clone())
                .unwrap()
                .suggestions
                .iter()
                .any(|row| row.name == "from-A")
        );
        engine.end_input();
        let grace = Duration::from_secs(10);
        engine.release_idle_specs(engine.next_idle_deadline(grace).unwrap(), grace);
        write_typed_custom(dir.path(), "demo#custom#0", "from-B");
        let rejected = engine.complete(request.clone()).unwrap();
        assert!(
            !rejected
                .suggestions
                .iter()
                .any(|row| row.name == "from-A" || row.name == "from-B")
        );
        assert!(engine.diagnostics().hook_catalog.load_attempted);
        assert!(!engine.diagnostics().hook_catalog.loaded);
        assert!(
            engine.registry.needs_refresh(),
            "sidecar digest mismatch marks the generation stale"
        );
        let refreshed = engine.complete(request).unwrap();
        assert!(refreshed.suggestions.iter().any(|row| row.name == "from-B"));
        assert!(engine.diagnostics().hook_catalog.loaded);
    }

    #[cfg(unix)]
    #[test]
    fn an_ownerless_generation_rebind_also_schedules_catalog_release() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        fs::create_dir_all(&live).unwrap();
        write_typed_custom(&live, "demo#custom#0", "from-A");
        let mut engine = Engine::new_with_frecency(live.clone(), Frecency::default()).unwrap();
        engine.release_idle_specs(std::time::Instant::now(), Duration::ZERO);
        assert!(!engine.diagnostics().hook_catalog.load_attempted);
        fs::rename(&live, root.path().join("old")).unwrap();
        fs::create_dir_all(&live).unwrap();
        write_typed_custom(&live, "demo#custom#0", "from-B");
        engine.refresh_specs_generation();
        assert!(!engine.diagnostics().hook_catalog.load_attempted);
        assert!(engine.diagnostics().hook_catalog.captured_bytes > 0);
        let grace = Duration::from_secs(10);
        let deadline = engine
            .next_idle_deadline(grace)
            .expect("ownerless new generation is idle");
        {
            let token = crate::cancellation::CancellationToken::new();
            token.cancel();
            let _scope = crate::cancellation::enter(token);
            assert!(engine.complete(CompleteRequest::default()).is_err());
        }
        assert_eq!(engine.diagnostics().hook_catalog.captured_bytes, 0);
        assert_eq!(engine.next_idle_deadline(grace), Some(deadline));
        engine.release_idle_specs(deadline, grace);
        assert!(!engine.diagnostics().hook_catalog.load_attempted);
    }

    #[cfg(unix)]
    #[test]
    fn hook_diagnostics_survive_clear_failure_and_generation_rebind() {
        use crate::ir::Spec;

        fn populate(cache: &Arc<crate::hook_cache::HookCache>) {
            let _bound = cache.bind();
            let spec = crate::hook_cache::cached_spec("key", || {
                Some(Spec {
                    names: vec!["cached".into()],
                    ..Spec::default()
                })
            })
            .unwrap();
            assert_eq!(spec.names, ["cached"]);
            assert!(crate::hook_cache::cached_spec("key", || panic!("must hit")).is_some());
        }

        fn assert_preserved(engine: &Engine, cache: &Arc<crate::hook_cache::HookCache>, lookups: u64) {
            assert!(Arc::ptr_eq(&engine.hook_cache, cache));
            let stats = cache.diagnostics().specs;
            assert_eq!((stats.entries, stats.allocated_bytes), (0, 0));
            assert_eq!((stats.hits, stats.misses, stats.capacity_clears), (lookups, lookups, 0));
        }

        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        write_spec(&live, "tool", r#"{"names":["tool"]}"#);
        let mut engine = Engine::new(live.clone()).unwrap();
        let cache = Arc::clone(&engine.hook_cache);
        populate(&cache);
        assert!(engine.clear_caches_and_report());
        assert_preserved(&engine, &cache, 1);

        populate(&cache);
        fs::rename(&live, root.path().join("old")).unwrap();
        assert!(!engine.clear_caches_and_report());
        assert_preserved(&engine, &cache, 2);

        populate(&cache);
        write_spec(&live, "tool", r#"{"names":["tool"],"description":"new generation"}"#);
        engine.refresh_specs_generation();
        assert_eq!(engine.registry.get_arc("tool").unwrap().description, "new generation");
        assert_preserved(&engine, &cache, 3);
    }

    #[cfg(unix)]
    #[test]
    fn engine_rejects_cross_generation_lazy_reads_and_rebinds_after_publish() {
        fn write_generation(dir: &std::path::Path, child_description: &str, hook_name: &str) {
            write_spec(
                dir,
                "demo",
                &serde_json::json!({
                    "names": ["demo"],
                    "description": child_description,
                    "subcommands": [{"names": ["child"], "loadSpec": "nested"}],
                    "args": [{"jsCustom": "demo#custom#0"}]
                })
                .to_string(),
            );
            write_spec(
                dir,
                "nested",
                &serde_json::json!({
                    "names": ["nested"],
                    "description": child_description
                })
                .to_string(),
            );
            fs::write(dir.join("index.json"), r#"{"files":{"demo":"demo.json"}}"#).unwrap();
            write_typed_custom(dir, "demo#custom#0", hook_name);
        }

        let root = tempfile::tempdir().unwrap();
        let generation_a = root.path().join("generation-a");
        let generation_b = root.path().join("generation-b");
        write_generation(&generation_a, "from generation A", "from-A");
        write_generation(&generation_b, "from generation B", "from-B");

        let canonical = root.path().join("specs-ir");
        let backup = root.path().join("specs-ir.backup");
        fs::rename(&generation_a, &canonical).unwrap();
        let mut engine = Engine::new(canonical.clone()).expect("generation A engine");

        fs::rename(&canonical, &backup).unwrap();
        fs::rename(&generation_b, &canonical).unwrap();

        assert!(engine.registry.get_arc("demo").is_none());
        let stale = {
            let _bound = crate::hook_backend::bind_native(std::sync::Arc::clone(engine.native.as_ref().unwrap()));
            crate::hook_backend::dispatch_custom(
                "demo#custom#0",
                &[],
                "/tmp",
                "",
                std::time::Duration::from_secs(1),
                false,
            )
        };
        assert_eq!(
            stale.expect("stale native catalog stays generation A")[0].name,
            "from-A"
        );

        let result = engine
            .complete(CompleteRequest {
                buffer: "demo ".into(),
                cwd: "/tmp".into(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("stable generation B");
        assert!(
            result.suggestions.iter().any(|suggestion| suggestion.name == "from-B"),
            "{result:?}"
        );

        let generation_c = root.path().join("generation-c");
        write_generation(&generation_c, "from generation C", "from-C");
        let backup_b = root.path().join("specs-ir.backup-b");
        fs::rename(&canonical, &backup_b).unwrap();
        let during_gap = engine
            .complete(CompleteRequest {
                buffer: "demo ".into(),
                cwd: "/tmp".into(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("gap should not poison the old engine");
        assert!(
            during_gap
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "from-B"),
            "{during_gap:?}"
        );
        fs::rename(&generation_c, &canonical).unwrap();
        fs::remove_dir_all(&backup_b).unwrap();
        let recovered = engine
            .complete(CompleteRequest {
                buffer: "demo ".into(),
                cwd: "/tmp".into(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("generation C should be retried");
        assert!(
            recovered
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "from-C"),
            "{recovered:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn identical_file_content_can_be_read_across_a_generation_replacement() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        for dir in [&a, &b] {
            write_spec(
                dir,
                "demo",
                r#"{"names":["demo"],"args":[{"jsCustom":"demo#custom#0"}]}"#,
            );
            fs::write(dir.join("index.json"), r#"{"files":{"demo":"demo.json"}}"#).unwrap();
            write_typed_custom(dir, "demo#custom#0", "same");
        }
        let canonical = root.path().join("specs-ir");
        let backup = root.path().join("backup");
        fs::rename(&a, &canonical).unwrap();
        let mut engine = Engine::new(canonical.clone()).unwrap();
        fs::rename(&canonical, &backup).unwrap();
        fs::rename(&b, &canonical).unwrap();

        let demo = engine.registry.get_arc("demo").expect("same spec bytes");
        assert_eq!(demo.names, vec!["demo"]);
        let hook = {
            let _bound = crate::hook_backend::bind_native(std::sync::Arc::clone(engine.native.as_ref().unwrap()));
            crate::hook_backend::dispatch_custom(
                "demo#custom#0",
                &[],
                "/tmp",
                "",
                std::time::Duration::from_secs(1),
                false,
            )
        };
        assert_eq!(hook.expect("same catalog bytes")[0].name, "same");
        fs::remove_dir_all(backup).unwrap();
        let _ = engine.complete(CompleteRequest {
            buffer: "demo ".into(),
            cwd: "/tmp".into(),
            include_history: false,
            ..CompleteRequest::default()
        });
    }

    #[test]
    fn generate_spec_merges_dynamic_subcommands() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        let (id, entry) = crate::hook_backend::test_typed_entry(
            "php#generateSpec#0",
            "generateSpec",
            serde_json::json!({
                "op": "spec-object",
                "fields": [
                    {"key": "name", "value": {"op": "string", "value": "php"}},
                    {
                        "key": "subcommands",
                        "value": {
                            "op": "array",
                            "items": [{
                                "op": "spec-object",
                                "fields": [
                                    {"key": "name", "value": {"op": "string", "value": "artisan"}},
                                    {"key": "description", "value": {"op": "string", "value": "Laravel"}}
                                ]
                            }]
                        }
                    }
                ]
            }),
        );
        let catalog = serde_json::json!({
            "version": 1,
            "kind": "typed-hook-expressions",
            "contracts": crate::hook_backend::test_sidecar_contracts(),
            "hooks": { id: entry }
        });
        fs::write(dir.path().join("typed-hooks.json"), format!("{catalog}\n")).unwrap();
        write_spec(
            dir.path(),
            "php",
            r#"{"names":["php"],"jsGenerateSpec":"php#generateSpec#0"}"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "php ".into(),
                cwd: dir.path().display().to_string(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            result.suggestions.iter().any(|row| row.name == "artisan"),
            "{:?}",
            result.suggestions
        );
    }

    fn engine_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|err| err.into_inner())
    }

    #[test]
    fn public_ai_acceptance_promotes_a_candidate_beyond_the_collection_budget() {
        let _lock = engine_lock();
        let _settings =
            fastab_settings::settings::install_override(fastab_settings::settings::Settings::from_slice(&[
                ("autocomplete.history.disableLoading", serde_json::json!(true)),
                ("autocomplete.sortMethod", serde_json::json!("default")),
                ("autocomplete.hideAutoExecuteSuggestion", serde_json::json!(true)),
            ]));
        let dir = tempfile::tempdir().unwrap();
        let mut subcommands: Vec<_> = (0..25)
            .map(|index| serde_json::json!({"names": [format!("a{index:02}")]}))
            .collect();
        subcommands.push(serde_json::json!({"names": ["z-last"]}));
        write_spec(
            dir.path(),
            "git",
            &serde_json::json!({"names": ["git"], "subcommands": subcommands}).to_string(),
        );
        let acceptance = Arc::new(Mutex::new(rank::AcceptanceIndex::default()));
        let mut engine = Engine::new_with_acceptance(dir.path().to_path_buf(), Arc::clone(&acceptance)).unwrap();
        engine.registry.trust_public_ai_fixture_for_test();
        let request = CompleteRequest {
            buffer: "git ".into(),
            cwd: dir.path().display().to_string(),
            include_history: false,
            include_public_ai: true,
            ..CompleteRequest::default()
        };

        let before = engine.complete(request.clone()).unwrap();
        assert_eq!(before.suggestions.len(), 26);
        assert_eq!(
            before
                .suggestions
                .iter()
                .filter(|row| row.public_ai_candidate.is_some())
                .count(),
            20
        );
        assert!(
            before
                .suggestions
                .iter()
                .find(|row| row.name == "z-last")
                .unwrap()
                .public_ai_candidate
                .is_none()
        );

        acceptance.lock().unwrap().record_at("git", "z-last", 2_000_000_000_000);
        let after = engine.complete(request.clone()).unwrap();
        assert_eq!(after.suggestions.len(), 26);
        assert_eq!(after.suggestions[0].name, "z-last");
        assert!(after.suggestions[0].public_ai_candidate.is_some());
        assert_eq!(
            after
                .suggestions
                .iter()
                .filter(|row| row.public_ai_candidate.is_some())
                .count(),
            20
        );
        assert!(
            after
                .suggestions
                .iter()
                .find(|row| row.name == "a19")
                .unwrap()
                .public_ai_candidate
                .is_none()
        );

        let lone = engine
            .complete(CompleteRequest {
                buffer: "git a24".into(),
                ..request
            })
            .unwrap();
        assert!(lone.public_ai_context.is_none());
        assert!(lone.suggestions.iter().all(|row| row.public_ai_candidate.is_none()));
    }

    #[test]
    fn public_ai_rejects_same_name_dynamic_origin_before_ranking_deduplicates() {
        let _lock = engine_lock();
        let _settings =
            fastab_settings::settings::install_override(fastab_settings::settings::Settings::from_slice(&[
                ("autocomplete.history.disableLoading", serde_json::json!(true)),
                ("autocomplete.hideAutoExecuteSuggestion", serde_json::json!(true)),
            ]));
        let dir = tempfile::tempdir().unwrap();
        write_typed_custom(dir.path(), "git#custom#0", "collision");
        write_spec(
            dir.path(),
            "git",
            &serde_json::json!({
                "names": ["git"],
                "args": [{"name": "target", "jsCustom": "git#custom#0"}],
                "subcommands": [
                    {"names": ["collision"], "insertValue": "collision"},
                    {"names": ["safe-one"]},
                    {"names": ["safe-two"]}
                ]
            })
            .to_string(),
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).unwrap();
        engine.registry.trust_public_ai_fixture_for_test();
        let result = engine
            .complete(CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                include_history: false,
                include_public_ai: true,
                ..CompleteRequest::default()
            })
            .unwrap();

        assert!(result.public_ai_context.is_some(), "{result:?}");
        assert!(result.suggestions.iter().any(|row| row.name == "collision"));
        assert_eq!(
            result.suggestions.iter().filter(|row| row.name == "collision").count(),
            1,
            "the conflicting rows should deduplicate after provenance validation"
        );
        assert!(
            result
                .suggestions
                .iter()
                .filter(|row| row.name == "collision")
                .all(|row| row.public_ai_candidate.is_none())
        );
        for name in ["safe-one", "safe-two"] {
            assert!(
                result
                    .suggestions
                    .iter()
                    .find(|row| row.name == name)
                    .unwrap()
                    .public_ai_candidate
                    .is_some(),
                "{name} should remain eligible: {result:?}"
            );
        }
    }

    #[test]
    fn argument_acceptance_uses_the_parser_slot_and_cwd_without_global_fallback() {
        let _lock = engine_lock();
        let settings = fastab_settings::settings::Settings::from_slice(&[
            ("autocomplete.history.disableLoading", serde_json::json!(true)),
            ("autocomplete.sortMethod", serde_json::json!("default")),
            ("autocomplete.hideAutoExecuteSuggestion", serde_json::json!(true)),
        ]);
        let _settings = fastab_settings::settings::install_override(settings.clone());
        let dir = tempfile::tempdir().unwrap();
        let values = serde_json::json!([{"names": ["alpha"]}, {"names": ["zeta"]}]);
        write_spec(
            dir.path(),
            "git",
            &serde_json::json!({
                "names": ["git"],
                "options": [{"names": ["--target"], "args": [{"name": "target", "suggestions": values.clone()}]}],
                "subcommands": [
                    {"names": ["checkout"], "args": [
                        {"name": "first", "suggestions": values.clone()},
                        {"name": "second", "suggestions": values.clone()}
                    ]},
                    {"names": ["switch"], "args": [{"name": "branch", "suggestions": values.clone()}]}
                ]
            })
            .to_string(),
        );
        write_spec(
            dir.path(),
            "npm",
            &serde_json::json!({
                "names": ["npm"],
                "subcommands": [{"names": ["run"], "args": [{"name": "script", "suggestions": values.clone()}]}]
            })
            .to_string(),
        );
        write_spec(
            dir.path(),
            "docker",
            &serde_json::json!({
                "names": ["docker"],
                "subcommands": [{
                    "names": ["run"],
                    "options": [{"names": ["--network"], "args": [{"name": "network", "suggestions": values.clone()}]}]
                }]
            })
            .to_string(),
        );
        let acceptance = Arc::new(Mutex::new(rank::AcceptanceIndex::default()));
        let mut engine = Engine::new_with_acceptance(dir.path().to_path_buf(), Arc::clone(&acceptance)).unwrap();
        let cwd_a = dir.path().display().to_string();
        let cwd_b = dir.path().join("other-project").display().to_string();
        let request = |buffer: &str, cwd: &str| CompleteRequest {
            buffer: buffer.into(),
            cwd: cwd.into(),
            include_history: false,
            ..CompleteRequest::default()
        };
        let scenarios = [
            ("git checkout ", "git branch"),
            ("npm run ", "npm script"),
            ("docker run --network ", "container parameter"),
        ];
        let target_index = |result: &CompleteResult| {
            result
                .suggestions
                .iter()
                .position(|row| row.name == "zeta")
                .expect("target candidate")
        };
        let mut scopes = Vec::new();
        for (buffer, label) in scenarios {
            let result = engine.complete(request(buffer, &cwd_a)).unwrap();
            assert_eq!(target_index(&result), 1, "{label}: no acceptance");
            assert!(
                result.suggestions.iter().all(|row| row.argument_value),
                "{label}: {result:?}"
            );
            scopes.push(
                result.suggestions[0]
                    .acceptance_scope
                    .clone()
                    .expect("parser slot with absolute cwd"),
            );
        }
        for root in ["git", "npm", "docker"] {
            acceptance.lock().unwrap().record_at(root, "zeta", 3_000_000_000_000);
        }
        for (buffer, label) in scenarios {
            let result = engine.complete(request(buffer, &cwd_a)).unwrap();
            assert_eq!(target_index(&result), 1, "{label}: old global acceptance");
        }

        let before = engine.complete(request("git checkout ", &cwd_a)).unwrap();
        assert_eq!(
            before.suggestions[0].name, "alpha",
            "old global acceptance must not rank arguments"
        );
        assert!(before.suggestions.iter().all(|row| row.argument_value));
        let scope = &scopes[0];
        assert!(
            before
                .suggestions
                .iter()
                .all(|row| row.acceptance_scope.as_deref() == Some(scope.as_str()))
        );
        assert_eq!(
            engine.complete(request("git checkout ", &cwd_a)).unwrap().suggestions[0]
                .acceptance_scope
                .as_deref(),
            Some(scope.as_str())
        );
        for (buffer, cwd) in [
            ("git checkout ", cwd_b.as_str()),
            ("git switch ", cwd_a.as_str()),
            ("git checkout chosen ", cwd_a.as_str()),
            ("git --target ", cwd_a.as_str()),
        ] {
            let other = engine.complete(request(buffer, cwd)).unwrap();
            assert_eq!(other.suggestions[0].name, "alpha", "{buffer} in {cwd}");
            assert!(other.suggestions[0].argument_value);
            assert_ne!(other.suggestions[0].acceptance_scope.as_deref(), Some(scope.as_str()));
        }
        let no_cwd = engine.complete(request("git checkout ", "")).unwrap();
        assert_eq!(no_cwd.suggestions[0].name, "alpha");
        assert!(
            no_cwd
                .suggestions
                .iter()
                .all(|row| row.argument_value && row.acceptance_scope.is_none())
        );

        for scope in &scopes {
            acceptance
                .lock()
                .unwrap()
                .record_scoped_at(scope, "zeta", 2_000_000_000_000);
        }
        for (buffer, label) in scenarios {
            let result = engine.complete(request(buffer, &cwd_a)).unwrap();
            assert_eq!(target_index(&result), 0, "{label}: scoped acceptance");
            let other_project = engine.complete(request(buffer, &cwd_b)).unwrap();
            assert_eq!(target_index(&other_project), 1, "{label}: other project");
        }
        assert_eq!(
            engine.complete(request("git checkout ", &cwd_b)).unwrap().suggestions[0].name,
            "alpha"
        );
        assert_eq!(
            engine.complete(request("git switch ", &cwd_a)).unwrap().suggestions[0].name,
            "alpha"
        );
        settings.set_value("autocomplete.sortMethod", "alphabetical").unwrap();
        assert_eq!(
            target_index(&engine.complete(request("git checkout ", &cwd_a)).unwrap()),
            1
        );
    }

    #[test]
    fn phase1_static_ir_complete_result_golden() {
        let _lock = engine_lock();
        let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/phase1");
        let golden: Phase1Golden = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/phase1/expected.json"
        )))
        .expect("phase1 golden JSON");
        assert!(!golden.cases.is_empty(), "phase1 golden must contain cases");
        let settings = fastab_settings::settings::Settings::from_slice(&[
            ("autocomplete.disableForCommands", serde_json::json!([])),
            ("autocomplete.hideAutoExecuteSuggestion", serde_json::json!(true)),
        ]);
        let mut registry = Registry::load(&fixture_dir).expect("phase1 static IR");

        for case in golden.cases {
            let mut result = lookup::complete_with_settings(&mut registry, &case.request, &settings);
            let (tokens, _) = lookup::tokenize(lookup::completion_buffer(&case.request.buffer, case.request.cursor));
            rank::apply_with_acceptance(
                &mut result,
                &tokens,
                &Frecency::default(),
                &rank::AcceptanceIndex::default(),
                &ranking_root_command(&case.request.buffer, case.request.cursor),
                false,
            );
            let actual = serde_json::to_value(&result).expect("serialize phase1 result");
            assert_eq!(actual, case.result, "phase1 golden case {:?}", case.name);
        }
    }

    #[test]
    fn completes_git_subcommands_from_ir() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{
              "names": ["git"],
              "description": "the stupid content tracker",
              "subcommands": [
                {"names": ["checkout"], "description": "Switch branches or restore working tree files"},
                {"names": ["commit"], "description": "Record changes to the repository"},
                {"names": ["cherry-pick"], "description": "Apply the changes introduced by some existing commits"},
                {"names": ["clone"], "description": "Clone a repository into a new directory"},
                {"names": ["status"], "description": "Show the working tree status"}
              ],
              "options": [{"names": ["--help"], "description": "Show help"}]
            }"#,
        );

        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "git ch".into(),
                cwd: dir.path().display().to_string(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");

        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "got {names:?}");
        assert!(names.contains(&"cherry-pick"), "got {names:?}");
        assert!(!names.contains(&"status"), "got {names:?}");
        assert_eq!(result.search_term, "ch");
    }

    #[test]
    fn history_merge_uses_active_argument_filter_strategy() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "argtool",
            r#"{
              "names":["argtool"],
              "filterStrategy":"fuzzy",
              "args":[{"name":"value"}]
            }"#,
        );
        let frecency = Frecency::from_commands([("argtool target".into(), 100)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");

        let prefix = engine
            .complete(CompleteRequest {
                buffer: "argtool tr".into(),
                fuzzy: false,
                include_history: true,
                ..CompleteRequest::default()
            })
            .expect("prefix completion");
        assert!(!prefix.fuzzy);
        assert!(!prefix.suggestions.iter().any(|item| item.kind == "history"));

        let history_only = engine
            .complete(CompleteRequest {
                buffer: "argtool tr".into(),
                fuzzy: false,
                history_only: true,
                ..CompleteRequest::default()
            })
            .expect("history-only completion");
        assert!(!history_only.fuzzy);
        assert!(!history_only.suggestions.iter().any(|item| item.name == "target"));

        let fuzzy = engine
            .complete(CompleteRequest {
                buffer: "argtool tr".into(),
                fuzzy: true,
                include_history: true,
                ..CompleteRequest::default()
            })
            .expect("fuzzy completion");
        assert!(fuzzy.fuzzy);
        assert!(fuzzy.suggestions.iter().any(|item| item.kind == "history"));
    }

    #[test]
    fn completes_options_for_ls() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "ls",
            r#"{
              "names": ["ls"],
              "description": "List directory contents",
              "options": [
                {"names": ["-a"], "description": "Include directory entries whose names begin with a dot"},
                {"names": ["-l"], "description": "List in long format"},
                {"names": ["--color"], "description": "Colorize output"}
              ]
            }"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "ls -".into(),
                cwd: "/".into(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.iter().any(|n| n.starts_with('-')), "got {names:?}");
    }

    #[test]
    fn preserves_insert_metadata_and_auto_executes_exact_subcommands() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{
              "names": ["git"],
              "subcommands": [
                {"names": ["status"], "description": "Show status", "priority": 77,
                 "icon": "📋"},
                {"names": ["commit"], "insertValue": "git commit", "args": [{"name": "message"}]}
              ],
              "options": [{"names": ["--message"], "requiresEquals": true,
                            "args": [{"name": "message"}]}]
            }"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");

        let exact = engine
            .complete(CompleteRequest {
                buffer: "git status".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("complete");
        let status = exact
            .suggestions
            .iter()
            .find(|suggestion| suggestion.kind == "subcommand")
            .expect("status suggestion");
        assert_eq!(status.insert_value, None);
        assert_eq!(status.priority, 77);
        assert_eq!(status.icon.as_deref(), Some("📋"));

        let partial = engine
            .complete(CompleteRequest {
                buffer: "git co".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("complete");
        let commit = partial
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "commit")
            .expect("commit suggestion");
        assert_eq!(commit.insert_value.as_deref(), Some("git commit"));

        let options = engine
            .complete(CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("complete");
        let message = options
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "--message")
            .expect("--message option");
        assert_eq!(message.separator_to_add.as_deref(), Some("="));
        assert!(message.should_add_space);
    }

    #[test]
    fn completes_bundled_mkdir_ir() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "mkdir",
            r#"{
              "names": ["mkdir"],
              "description": "Make directories",
              "args": [{"name": "directory name", "templates": ["folders"]}],
              "options": [
                {"names": ["-p", "--parents"], "description": "No error if existing"},
                {"names": ["--help"], "description": "Display this help and exit"}
              ]
            }"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "mkdir -".into(),
                cwd: "/".into(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.iter().any(|n| *n == "-p" || *n == "--parents"), "got {names:?}");
    }

    #[test]
    fn completes_cd_folders() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("readme.md"), "x").unwrap();
        write_spec(
            dir.path(),
            "cd",
            r#"{"names":["cd"],"args":[{"templates":["folders"]}]}"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "cd s".into(),
                cwd: dir.path().display().to_string(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            result.suggestions.iter().any(|s| s.name == "src/"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn completes_npm_run_scripts() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"vitest","lint":"eslint"}}"#,
        )
        .unwrap();
        write_spec(
            dir.path(),
            "npm",
            r#"{
              "names": ["npm"],
              "subcommands": [
                {"names": ["run", "run-script"], "args": [{"builtin": "npm-scripts"}]}
              ]
            }"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "npm run t".into(),
                cwd: dir.path().display().to_string(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["test"]);
    }

    #[test]
    fn unknown_command_does_not_run_cobra_complete() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fakecobra");
        std::fs::write(&bin, "#!/bin/sh\nprintf 'alpha\\tfirst\\nalpaca\\n:4\\n'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: format!("{} al", bin.display()),
                cwd: dir.path().display().to_string(),
                cursor: None,
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(result.suggestions.is_empty(), "{:?}", result.suggestions);
    }

    #[test]
    fn ranks_frequent_history_subcommand_first() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{
              "names": ["git"],
              "subcommands": [
                {"names": ["checkout"], "description": "Switch branches"},
                {"names": ["cherry-pick"], "description": "Apply commits"}
              ]
            }"#,
        );
        let frecency = Frecency::from_commands([
            ("git checkout".into(), 10),
            ("git checkout".into(), 20),
            ("git checkout".into(), 30),
            ("git cherry-pick".into(), 1),
        ]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "git ch".into(),
                cwd: "/".into(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert_eq!(result.suggestions[0].name, "checkout");
    }

    #[test]
    fn chained_buffer_ranks_and_merges_history_for_the_current_command() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{
              "names": ["git"],
              "subcommands": [
                {"names": ["checkout"], "description": "Switch branches"},
                {"names": ["cherry-pick"], "description": "Apply commits"}
              ]
            }"#,
        );
        let frecency = Frecency::from_commands([
            ("git checkout".into(), 10),
            ("git checkout".into(), 20),
            ("git checkout".into(), 30),
            ("git cherry-pick".into(), 1),
            ("git checkout -b feature".into(), 40),
        ]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "echo hello && git ch".into(),
                cwd: "/".into(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert_eq!(result.suggestions[0].name, "checkout");
        assert_eq!(ranking_root_command("echo hello && git ch", None), "git");
        let history = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.kind == "history")
            .expect("history suffix for the current git command");
        assert_eq!(history.name, "checkout -b feature");
        assert!(!history.name.contains("echo"));
    }

    #[test]
    fn first_token_history_keeps_search_term_for_insert() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["checkout"]}]}"#,
        );
        let frecency = Frecency::from_commands([("git checkout -b feature".into(), 40)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "git".into(),
                cwd: "/".into(),
                cursor: None,
                suggest_first_token: true,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert_eq!(result.search_term, "git");
        let history = result
            .suggestions
            .iter()
            .find(|s| s.kind == "history")
            .expect("history suggestion");
        assert!(history.name.starts_with("git checkout"));
        assert!(history.name.starts_with(&result.search_term));
    }

    #[test]
    fn history_only_uses_the_typed_command_prefix_as_a_suffix() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["checkout"]}]}"#,
        );
        let frecency = Frecency::from_commands([("git commit -m feature".into(), 40)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "git co".into(),
                history_only: true,
                include_history: true,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let history = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.kind == "history")
            .expect("history suggestion");
        assert_eq!(history.name, "commit -m feature");
        assert_eq!(history.insert_value.as_deref(), Some("commit -m feature"));
        assert!(!history.name.starts_with("git "));
    }

    #[test]
    fn history_only_on_a_chained_buffer_uses_the_current_command_prefix() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["checkout"]}]}"#,
        );
        let frecency = Frecency::from_commands([("git commit -m feature".into(), 40)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "echo x && git co".into(),
                history_only: true,
                include_history: true,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let history = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.kind == "history")
            .expect("history suggestion");
        assert_eq!(history.name, "commit -m feature");
        assert_eq!(result.search_term, "co");
    }

    #[test]
    fn history_only_keeps_raw_search_text_for_insertion() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        let frecency = Frecency::from_commands([("echo my file".into(), 40)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: r"echo my\ f".into(),
                history_only: true,
                include_history: true,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert_eq!(result.search_term, r"my\ f");
        assert_eq!(result.match_term, "my f");
        assert!(result.suggestions.iter().any(|item| item.name == "my file"));
    }

    #[test]
    fn first_token_completion_defaults_off_but_explicit_true_still_works() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "git", r#"{"names":["git"]}"#);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");

        let default_result = engine
            .complete(CompleteRequest {
                buffer: "gi".into(),
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            default_result.suggestions.is_empty(),
            "{:?}",
            default_result.suggestions
        );

        let explicit_result = engine
            .complete(CompleteRequest {
                buffer: "gi".into(),
                suggest_first_token: true,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            explicit_result
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "git")
        );
    }

    #[test]
    fn repeated_script_pieces_stay_compact_through_real_engine_refilter_and_end_input() {
        let _lock = engine_lock();
        let _settings =
            fastab_settings::settings::install_override(fastab_settings::settings::Settings::from_slice(&[
                ("autocomplete.history.disableLoading", serde_json::json!(true)),
                ("autocomplete.sortMethod", serde_json::json!("default")),
                ("autocomplete.hideAutoExecuteSuggestion", serde_json::json!(true)),
            ]));
        let dir = tempfile::tempdir().unwrap();
        let specs = dir.path().join("specs");
        let output = dir.path().join("output");
        let mut stdout = "alpha\n".repeat(20_000);
        stdout.push_str("\nbeta\n");
        for index in 0..200 {
            stdout.push_str(&format!("unique-{index:03}\n"));
        }
        fs::write(&output, stdout).unwrap();
        write_spec(
            &specs,
            "repeat",
            &serde_json::json!({
                "names": ["repeat"],
                "args": [{"name": "value", "script": ["cat", output.display().to_string()], "splitOn": "\n"}]
            })
            .to_string(),
        );
        let mut engine = Engine::new_with_frecency(specs, Frecency::default()).unwrap();
        let request = |buffer: &str| CompleteRequest {
            buffer: buffer.into(),
            cwd: dir.path().display().to_string(),
            include_history: false,
            ..CompleteRequest::default()
        };
        let first = engine.complete(request("repeat ")).unwrap();
        assert_eq!(first.suggestions.len(), 203);
        assert_eq!(first.suggestions[0].name, "alpha");
        assert_eq!(first.suggestions[1].name, "");
        assert_eq!(first.suggestions[2].name, "beta");
        assert_eq!(first.suggestions.last().unwrap().name, "unique-199");
        let resources = engine.diagnostics().generator_session;
        assert_eq!(resources.generators, 1);
        assert_eq!(resources.rows, 203, "unique candidates are not truncated");
        assert!(resources.allocated_bytes < 256 * 1024, "{resources:?}");
        fs::remove_file(&output).unwrap();
        let second = engine.complete(request("repeat al")).unwrap();
        assert_eq!(second.suggestions.len(), 1);
        assert_eq!(second.suggestions[0].name, "alpha");
        let after_refilter = engine.diagnostics().generator_session;
        assert_eq!(after_refilter.rows, resources.rows, "refilter retains the same rows");
        assert!(
            after_refilter.allocated_bytes <= resources.allocated_bytes + 64,
            "{after_refilter:?}"
        );
        engine.end_input();
        assert_eq!(engine.diagnostics().generator_session, Default::default());
    }

    #[test]
    fn ended_input_retires_only_derived_history_indexes_and_rebuilds_identical_rows() {
        let _lock = engine_lock();
        let _settings =
            fastab_settings::settings::install_override(fastab_settings::settings::Settings::from_slice(&[
                ("autocomplete.history.disableLoading", serde_json::json!(false)),
                ("beta.history.customCommand", serde_json::json!("")),
                ("beta.history.allShells", serde_json::json!(false)),
                ("autocomplete.sortMethod", serde_json::json!("default")),
            ]));
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "tool",
            r#"{"names":["tool"],"args":[{"name":"value","templates":["history"]}]}"#,
        );
        let frecency = Frecency::from_commands([
            ("tool alpha".into(), 1),
            ("tool beta".into(), 2),
            ("tool alpha".into(), 3),
        ]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).unwrap();
        let request = CompleteRequest {
            buffer: "tool ".into(),
            cwd: dir.path().display().to_string(),
            include_history: false,
            ..CompleteRequest::default()
        };
        let first = engine.complete(request.clone()).unwrap();
        assert_eq!(
            first
                .suggestions
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        let active = engine.diagnostics().history;
        assert_eq!(active.index_count, 1);
        assert!(active.index_bytes > 0 && active.next_word_bytes > 0);
        let lines = engine.frecency.command_lines();
        let grace = Duration::from_secs(10);
        engine.end_input();
        let deadline = engine.next_idle_deadline(grace).unwrap();
        engine.native = None;
        engine.registry.release_idle(std::time::Instant::now(), Duration::ZERO);
        assert_eq!(
            engine.next_idle_deadline(grace),
            Some(deadline),
            "indexes alone schedule retirement"
        );
        engine
            .complete(CompleteRequest {
                history_only: true,
                ..request.clone()
            })
            .unwrap();
        engine.end_input();
        assert_eq!(
            engine.next_idle_deadline(grace),
            Some(deadline),
            "history and repeated end do not extend grace"
        );
        let token = crate::cancellation::CancellationToken::new();
        token.cancel();
        {
            let _scope = crate::cancellation::enter(token);
            assert!(engine.complete(request.clone()).is_err());
        }
        assert_eq!(engine.next_idle_deadline(grace), Some(deadline));
        engine.release_idle_specs(deadline - Duration::from_millis(1), grace);
        assert_eq!(engine.diagnostics().history.index_count, 1);
        engine.release_idle_specs(deadline, grace);
        let retired = engine.diagnostics().history;
        assert_eq!(retired.index_count, 0);
        assert_eq!(retired.index_bytes, 0);
        assert_eq!(retired.next_word_bytes, active.next_word_bytes);
        assert!(Arc::ptr_eq(&lines, &engine.frecency.command_lines()));
        let rebuilt = engine.complete(request).unwrap();
        assert_eq!(
            rebuilt.suggestions, first.suggestions,
            "all row metadata and ordering survive retirement"
        );
        assert_eq!(rebuilt.public_ai_context, first.public_ai_context);
        assert_eq!(rebuilt.fuzzy, first.fuzzy);
        assert_eq!(rebuilt.search_term, first.search_term);
        assert_eq!(rebuilt.match_term, first.match_term);
        assert_eq!(
            rebuilt.current_arg.as_ref().map(|arg| (&arg.name, &arg.description)),
            first.current_arg.as_ref().map(|arg| (&arg.name, &arg.description))
        );
        assert_eq!(rebuilt.pending_generators, first.pending_generators);
        assert_eq!(rebuilt.debounce_ms, first.debounce_ms);
        assert_eq!(engine.diagnostics().history.index_count, 1);
    }

    #[test]
    fn dangerous_argument_is_inherited_by_static_and_generated_rows() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "danger",
            r#"{
              "names":["danger"],
              "args":[
                {"name":"target","isDangerous":true,
                 "suggestions":[{"names":["wipe"]}],
                 "script":["printf","generated\\n"],"splitOn":"\n"}
              ]
            }"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");

        let static_result = engine
            .complete(CompleteRequest {
                buffer: "danger wipe".into(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let static_row = static_result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "wipe")
            .expect("static dangerous row");
        assert!(static_row.is_dangerous);
        assert!(
            static_result
                .suggestions
                .iter()
                .all(|suggestion| suggestion.kind != "auto-execute")
        );

        let generated_result = engine
            .complete(CompleteRequest {
                buffer: "danger gen".into(),
                cwd: dir.path().display().to_string(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let generated_row = generated_result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "generated")
            .expect("generated dangerous row");
        assert!(generated_row.is_dangerous);
    }

    #[test]
    fn nested_path_names_keep_directory_prefix() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("main.rs"), "fn").unwrap();
        write_spec(
            dir.path(),
            "cat",
            r#"{"names":["cat"],"args":[{"templates":["filepaths"]}]}"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "cat src/m".into(),
                cwd: dir.path().display().to_string(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            result.suggestions.iter().any(|s| s.name == "src/main.rs"),
            "{:?}",
            result.suggestions
        );
        assert_eq!(result.search_term, "src/m");
        assert!(
            result
                .suggestions
                .iter()
                .all(|s| s.name.starts_with(&result.search_term))
        );
    }

    #[test]
    fn first_token_does_not_list_subcommands() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["checkout"]}]}"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "git".into(),
                cwd: "/".into(),
                suggest_first_token: true,
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(result.suggestions.iter().all(|s| s.name != "checkout"));
        // An exact match may also get an auto-execute wrapper; that follows
        // `autocomplete.hideAutoExecuteSuggestion` on disk, so look at the
        // command row itself.
        let git = result
            .suggestions
            .iter()
            .find(|s| s.name == "git" && s.kind == "arg")
            .expect("first-token command row");
        assert_eq!(git.insert_value.as_deref(), Some("git"));
        assert!(!git.should_add_space);
        assert_eq!(result.search_term, "git");
    }

    #[test]
    fn history_off_does_not_merge_history_lines() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["checkout"]}]}"#,
        );
        let frecency = Frecency::from_commands([("git checkout -b feature".into(), 40)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "git".into(),
                cwd: "/".into(),
                cursor: None,
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            result.suggestions.iter().all(|s| s.kind != "history"),
            "{:?}",
            result.suggestions
        );
    }

    fn idle_tool_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "tool",
            r#"{
              "names":["tool"],
              "args":[{"name":"query","templates":["history"]}],
              "subcommands":[{"names":["child"],"loadSpec":"child"}]
            }"#,
        );
        write_spec(
            dir.path(),
            "child",
            r#"{"names":["child"],"args":[{"name":"value","templates":["history"]}]}"#,
        );
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        );
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"tool":"tool.json","git":"git.json"}}"#,
        )
        .unwrap();
        dir
    }

    fn complete_buffer(engine: &mut Engine, buffer: &str) {
        engine
            .complete(CompleteRequest {
                buffer: buffer.into(),
                cwd: "/".into(),
                ..CompleteRequest::default()
            })
            .expect("complete");
    }

    #[test]
    fn history_index_starts_the_child_grace_without_dropping_it() {
        let _lock = engine_lock();
        let dir = idle_tool_dir();
        let frecency = Frecency::from_commands([("tool child value".into(), 40)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        complete_buffer(&mut engine, "tool ");

        let child = engine.registry.cached_load_spec("child").expect("child cached");
        let marked = engine
            .registry
            .idle_mark("child")
            .expect("child is tracked")
            .expect("child grace has a start");
        assert_eq!(engine.registry.idle_mark("tool"), Some(None));

        complete_buffer(&mut engine, "tool ");
        let still = engine.registry.cached_load_spec("child").expect("child stays cached");
        assert!(Arc::ptr_eq(&child, &still));
        assert_eq!(engine.registry.idle_mark("child"), Some(Some(marked)));
        let released_child = Arc::downgrade(&child);
        drop(child);
        drop(still);

        engine
            .registry
            .release_idle(marked + crate::ir::SPEC_IDLE_GRACE, crate::ir::SPEC_IDLE_GRACE);
        assert!(engine.registry.cached_load_spec("child").is_none());
        assert!(released_child.upgrade().is_none());
        let history = Arc::clone(&engine.history);
        let values = history.arg_values(
            &mut engine.registry,
            None,
            None,
            &[crate::history::ArgSlot {
                root: "tool".into(),
                path: vec!["child".into()],
                option: None,
                index: 0,
            }],
        );
        assert_eq!(values, vec!["value".to_string()]);
    }

    #[test]
    fn buffer_walk_into_a_child_clears_its_idle_mark() {
        let _lock = engine_lock();
        let dir = idle_tool_dir();
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        complete_buffer(&mut engine, "tool child ");
        assert!(engine.registry.cached_load_spec("child").is_some());
        assert_eq!(engine.registry.idle_mark("child"), Some(None));
    }

    #[test]
    fn leaving_a_child_starts_its_grace_and_keeps_the_arc() {
        let _lock = engine_lock();
        let dir = idle_tool_dir();
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        complete_buffer(&mut engine, "tool child ");
        let child = engine.registry.cached_load_spec("child").expect("child");
        complete_buffer(&mut engine, "git status");
        let still = engine
            .registry
            .cached_load_spec("child")
            .expect("child stays for the grace");
        assert!(Arc::ptr_eq(&child, &still));
        let marked = engine
            .registry
            .idle_mark("child")
            .expect("leaving the child starts the grace")
            .expect("the grace has a start");
        engine.registry.release_idle(
            marked + crate::ir::SPEC_IDLE_GRACE - Duration::from_millis(1),
            crate::ir::SPEC_IDLE_GRACE,
        );
        assert!(Arc::ptr_eq(
            &child,
            &engine
                .registry
                .cached_load_spec("child")
                .expect("the grace keeps the child")
        ));
    }

    #[test]
    fn history_prefix_starts_grace_instead_of_keeping_the_file_in_use() {
        let _lock = engine_lock();
        let dir = idle_tool_dir();
        write_spec(dir.path(), "sudo", r#"{"names":["sudo"]}"#);
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"tool":"tool.json","git":"git.json","sudo":"sudo.json"}}"#,
        )
        .unwrap();
        let frecency = Frecency::from_commands([("tool child value".into(), 40), ("sudo tool extra".into(), 30)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        complete_buffer(&mut engine, "tool ");

        assert!(engine.registry.is_cached("sudo"));
        let marked = engine
            .registry
            .idle_mark("sudo")
            .expect("a history prefix is tracked")
            .expect("a history prefix starts the grace");
        assert_eq!(engine.registry.idle_mark("tool"), Some(None));

        complete_buffer(&mut engine, "tool ");
        assert_eq!(engine.registry.idle_mark("sudo"), Some(Some(marked)));
        assert!(engine.registry.is_cached("sudo"));
    }

    #[test]
    fn cold_history_only_loads_release_without_an_ordinary_completion() {
        let _lock = engine_lock();
        let dir = idle_tool_dir();
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        engine
            .complete(CompleteRequest {
                buffer: "tool child ".into(),
                cwd: "/".into(),
                history_only: true,
                ..CompleteRequest::default()
            })
            .expect("history only walks into the child");

        let marked = engine
            .registry
            .idle_mark("child")
            .expect("new child is tracked")
            .expect("history-only cannot leave a new child active");
        assert_eq!(engine.registry.idle_mark("tool"), Some(Some(marked)));
        let child = engine.registry.cached_load_spec("child").expect("child is cached");
        let weak = Arc::downgrade(&child);
        drop(child);

        engine
            .registry
            .release_idle(marked + crate::ir::SPEC_IDLE_GRACE, crate::ir::SPEC_IDLE_GRACE);
        assert!(!engine.registry.is_cached("tool"));
        assert!(engine.registry.cached_load_spec("child").is_none());
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn history_only_does_not_move_an_idle_mark() {
        let _lock = engine_lock();
        let dir = idle_tool_dir();
        let frecency = Frecency::from_commands([("tool child value".into(), 40)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        complete_buffer(&mut engine, "tool ");
        let marked = engine
            .registry
            .idle_mark("child")
            .expect("child is tracked")
            .expect("child grace has a start");
        engine
            .complete(CompleteRequest {
                buffer: "tool child ".into(),
                cwd: "/".into(),
                history_only: true,
                ..CompleteRequest::default()
            })
            .expect("history only walks into the child");
        assert_eq!(engine.registry.idle_mark("child"), Some(Some(marked)));
        assert!(engine.registry.cached_load_spec("child").is_some());
    }

    #[test]
    fn history_only_preserves_active_specs_without_touching_the_next_completion() {
        let _lock = engine_lock();
        let dir = idle_tool_dir();
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        complete_buffer(&mut engine, "tool child ");
        assert_eq!(engine.registry.idle_mark("child"), Some(None));

        let mut git_mark = None;
        for buffer in ["git status", "tool child "] {
            engine
                .complete(CompleteRequest {
                    buffer: buffer.into(),
                    cwd: "/".into(),
                    history_only: true,
                    ..CompleteRequest::default()
                })
                .expect("history only");
            assert_eq!(engine.registry.idle_mark("child"), Some(None));
            assert_eq!(engine.registry.idle_mark("tool"), Some(None));
            let marked = engine
                .registry
                .idle_mark("git")
                .expect("new history-only root is tracked")
                .expect("new history-only root has a grace clock");
            assert_eq!(*git_mark.get_or_insert(marked), marked);
            assert_eq!(
                engine.next_idle_deadline(crate::ir::SPEC_IDLE_GRACE),
                Some(marked + crate::ir::SPEC_IDLE_GRACE)
            );
        }

        // The history-only child walk must not count as a touch in this
        // ordinary completion, which leaves the child and starts its grace.
        complete_buffer(&mut engine, "git status");
        let marked = engine
            .registry
            .idle_mark("child")
            .expect("child is tracked")
            .expect("ordinary completion starts the child grace");
        assert_eq!(engine.registry.idle_mark("git"), Some(None));
        assert_eq!(
            engine.next_idle_deadline(crate::ir::SPEC_IDLE_GRACE),
            Some(marked + crate::ir::SPEC_IDLE_GRACE)
        );
        assert!(engine.registry.cached_load_spec("child").is_some());
    }

    fn quiet_settings() -> impl Drop {
        fastab_settings::settings::install_override(fastab_settings::settings::Settings::from_slice(&[
            ("autocomplete.history.disableLoading", serde_json::json!(true)),
            ("autocomplete.sortMethod", serde_json::json!("default")),
            ("autocomplete.hideAutoExecuteSuggestion", serde_json::json!(true)),
        ]))
    }

    fn complete_listed(engine: &mut Engine, buffer: &str) -> CompleteResult {
        engine
            .complete(CompleteRequest {
                buffer: buffer.into(),
                cwd: "/".into(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("complete")
    }

    #[test]
    fn reload_after_idle_keeps_suggestion_rows_and_uses_a_new_arc() {
        let _lock = engine_lock();
        let _settings = quiet_settings();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "tool",
            r#"{
              "names":["tool"],
              "subcommands":[
                {"names":["child"],"description":"Stub child","loadSpec":"child"},
                {"names":["stay"],"description":"Stays on the parent","priority":30}
              ]
            }"#,
        );
        write_spec(
            dir.path(),
            "child",
            r#"{
              "names":["child"],
              "description":"Loaded child",
              "subcommands":[
                {
                  "names":["alpha"],
                  "description":"First item",
                  "priority":40,
                  "insertValue":"alpha-now",
                  "shouldAddSpace":true,
                  "args":[{"name":"file","isOptional":true}]
                },
                {
                  "names":["secret"],
                  "description":"Hidden item",
                  "hidden":true,
                  "priority":10,
                  "insertValue":"secret-now"
                },
                {
                  "names":["zeta"],
                  "description":"Last item",
                  "priority":80,
                  "shouldAddSpace":false,
                  "args":[{"name":"target"}]
                }
              ],
              "options":[
                {
                  "names":["--verbose"],
                  "description":"Talk more",
                  "priority":60,
                  "args":[{"name":"level","isOptional":true}]
                }
              ]
            }"#,
        );
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        );
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"tool":"tool.json","git":"git.json"}}"#,
        )
        .unwrap();
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");

        let parent_before = complete_listed(&mut engine, "tool ").suggestions;
        assert!(engine.registry.cached_load_spec("child").is_none());
        assert!(
            parent_before
                .iter()
                .any(|row| row.name == "child" && row.description == "Stub child"),
            "{parent_before:?}"
        );
        assert!(
            parent_before
                .iter()
                .all(|row| row.name != "alpha" && row.name != "zeta" && row.name != "secret"),
            "{parent_before:?}"
        );

        let entered = complete_listed(&mut engine, "tool child ");
        let entered_rows = entered.suggestions.clone();
        assert!(
            entered_rows.iter().any(|row| {
                row.name == "alpha"
                    && row.priority == 40
                    && row.args_hint == "[file]"
                    && row.should_add_space
                    && row.insert_value.as_deref() == Some("alpha-now")
            }),
            "{entered_rows:?}"
        );
        assert!(
            entered_rows.iter().any(|row| row.name == "zeta"
                && row.priority == 80
                && row.args_hint == "<target>"
                && !row.should_add_space
                && row.insert_value.is_none()),
            "{entered_rows:?}"
        );
        assert!(
            entered_rows
                .iter()
                .any(|row| row.name == "--verbose" && row.priority == 60 && row.args_hint == "[level]"),
            "{entered_rows:?}"
        );
        assert!(entered_rows.iter().all(|row| !row.hidden), "{entered_rows:?}");
        let child = engine.registry.cached_load_spec("child").expect("child loaded");

        let hidden = complete_listed(&mut engine, "tool child secret");
        let hidden_rows = hidden.suggestions.clone();
        assert!(
            hidden_rows.iter().any(|row| row.name == "secret"
                && row.hidden
                && row.priority == 10
                && row.insert_value.as_deref() == Some("secret-now")),
            "{hidden_rows:?}"
        );

        let git = complete_listed(&mut engine, "git status");
        assert!(git.suggestions.iter().any(|row| row.name == "status"), "{git:?}");
        let git_arc = engine.registry.get_arc("git").expect("git");
        let marked = engine
            .registry
            .idle_mark("child")
            .expect("leaving the child starts the grace")
            .expect("the grace has a start");
        engine
            .registry
            .release_idle(marked + crate::ir::SPEC_IDLE_GRACE, crate::ir::SPEC_IDLE_GRACE);

        assert!(engine.registry.cached_load_spec("child").is_none());
        assert!(!engine.registry.is_cached("tool"));
        assert!(Arc::ptr_eq(
            &git_arc,
            &engine.registry.get_arc("git").expect("git stays")
        ));

        let parent_after = complete_listed(&mut engine, "tool ").suggestions;
        assert_eq!(parent_after, parent_before);
        assert!(
            engine.registry.cached_load_spec("child").is_none(),
            "the parent menu must keep using the stub"
        );

        let reloaded = complete_listed(&mut engine, "tool child ");
        let reloaded_arc = engine.registry.cached_load_spec("child").expect("child reloaded");
        assert!(!Arc::ptr_eq(&child, &reloaded_arc));
        assert_eq!(reloaded.suggestions, entered_rows);

        let hidden_again = complete_listed(&mut engine, "tool child secret");
        assert_eq!(hidden_again.suggestions, hidden_rows);
        assert!(Arc::ptr_eq(
            &reloaded_arc,
            &engine
                .registry
                .cached_load_spec("child")
                .expect("hidden query hits the reloaded arc")
        ));
    }

    #[test]
    fn idle_release_leaves_public_ai_stubs_excluded() {
        let _lock = engine_lock();
        let _settings = quiet_settings();
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("gcloud")).unwrap();
        fs::write(
            dir.path().join("gcloud.json"),
            r#"{
              "names":["gcloud"],
              "subcommands":[
                {"names":["compute"],"description":"Compute","loadSpec":"gcloud/compute"},
                {"names":["sql"],"description":"SQL","loadSpec":"gcloud/sql"},
                {"names":["config"],"description":"Config","subcommands":[
                  {"names":["list"],"description":"List"},
                  {"names":["get"],"description":"Get"}
                ]},
                {"names":["auth"],"description":"Auth","subcommands":[
                  {"names":["login"],"description":"Login"},
                  {"names":["logout"],"description":"Logout"}
                ]}
              ]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("gcloud/compute.json"),
            r#"{
              "names":["compute"],
              "subcommands":[
                {"names":["instances"],"description":"Instances"},
                {"names":["disks"],"description":"Disks"}
              ]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("gcloud/sql.json"),
            r#"{"names":["sql"],"subcommands":[{"names":["backups"]}]}"#,
        )
        .unwrap();
        fs::write(dir.path().join("index.json"), r#"{"files":{"gcloud":"gcloud.json"}}"#).unwrap();
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        engine.registry.trust_public_ai_fixture_for_test();

        let ai = |engine: &mut Engine, buffer: &str| {
            engine
                .complete(CompleteRequest {
                    buffer: buffer.into(),
                    cwd: "/".into(),
                    include_history: false,
                    include_public_ai: true,
                    ..CompleteRequest::default()
                })
                .expect("complete")
        };

        let menu = ai(&mut engine, "gcloud ");
        assert!(menu.public_ai_context.is_some(), "{menu:?}");
        for name in ["config", "auth"] {
            assert!(
                menu.suggestions
                    .iter()
                    .find(|row| row.name == name)
                    .is_some_and(|row| row.public_ai_candidate.is_some()),
                "{name}: {menu:?}"
            );
        }
        for name in ["compute", "sql"] {
            assert!(
                menu.suggestions
                    .iter()
                    .find(|row| row.name == name)
                    .is_some_and(|row| row.public_ai_candidate.is_none()),
                "{name}: {menu:?}"
            );
        }
        assert!(engine.registry.cached_load_spec("gcloud/compute").is_none());
        assert!(engine.registry.cached_load_spec("gcloud/sql").is_none());

        let typing = ai(&mut engine, "gcloud comp");
        assert!(
            typing
                .suggestions
                .iter()
                .find(|row| row.name == "compute")
                .is_some_and(|row| row.public_ai_candidate.is_none())
        );
        assert!(engine.registry.cached_load_spec("gcloud/compute").is_none());

        let config = ai(&mut engine, "gcloud config ");
        assert!(config.public_ai_context.is_some(), "{config:?}");
        for name in ["list", "get"] {
            assert!(
                config
                    .suggestions
                    .iter()
                    .find(|row| row.name == name)
                    .is_some_and(|row| row.public_ai_candidate.is_some()),
                "{name}: {config:?}"
            );
        }
        assert!(engine.registry.cached_load_spec("gcloud/compute").is_none());

        let entered = ai(&mut engine, "gcloud compute ");
        assert!(entered.public_ai_context.is_none(), "{entered:?}");
        let entered_rows = entered.suggestions.clone();
        for name in ["instances", "disks"] {
            assert!(
                entered
                    .suggestions
                    .iter()
                    .find(|row| row.name == name)
                    .is_some_and(|row| row.public_ai_candidate.is_none()),
                "{name}: {entered:?}"
            );
        }
        let compute = engine
            .registry
            .cached_load_spec("gcloud/compute")
            .expect("compute file");
        assert!(!compute.meta.ai_resolved_reference);
        assert!(engine.registry.cached_load_spec("gcloud/sql").is_none());
        let parent = engine.registry.get_arc("gcloud").expect("gcloud");

        let _back = ai(&mut engine, "gcloud ");
        let marked = engine
            .registry
            .idle_mark("gcloud/compute")
            .expect("leaving compute starts the grace")
            .expect("the grace has a start");
        assert_eq!(engine.registry.idle_mark("gcloud"), Some(None));
        engine
            .registry
            .release_idle(marked + crate::ir::SPEC_IDLE_GRACE, crate::ir::SPEC_IDLE_GRACE);

        assert!(engine.registry.cached_load_spec("gcloud/compute").is_none());
        assert!(Arc::ptr_eq(
            &parent,
            &engine.registry.get_arc("gcloud").expect("parent stays")
        ));
        let stub = engine
            .registry
            .get_arc("gcloud")
            .expect("gcloud")
            .find_subcommand("compute")
            .expect("stub")
            .clone();
        assert!(!stub.meta.ai_resolved_reference);
        assert!(matches!(stub.load_spec, Some(crate::ir::LoadSpec::Path(ref path)) if path == "gcloud/compute"));

        let menu_after = ai(&mut engine, "gcloud ");
        assert_eq!(menu_after.suggestions, menu.suggestions);
        for name in ["config", "auth"] {
            assert!(
                menu_after
                    .suggestions
                    .iter()
                    .find(|row| row.name == name)
                    .is_some_and(|row| row.public_ai_candidate.is_some()),
                "{name}: {menu_after:?}"
            );
        }
        for name in ["compute", "sql"] {
            assert!(
                menu_after
                    .suggestions
                    .iter()
                    .find(|row| row.name == name)
                    .is_some_and(|row| row.public_ai_candidate.is_none()),
                "{name}: {menu_after:?}"
            );
        }
        assert!(
            engine.registry.cached_load_spec("gcloud/compute").is_none(),
            "the parent menu must not read the released child back"
        );

        let entered_again = ai(&mut engine, "gcloud compute ");
        assert!(entered_again.public_ai_context.is_none(), "{entered_again:?}");
        assert_eq!(entered_again.suggestions, entered_rows);
        for name in ["instances", "disks"] {
            assert!(
                entered_again
                    .suggestions
                    .iter()
                    .find(|row| row.name == name)
                    .is_some_and(|row| row.public_ai_candidate.is_none()),
                "{name}: {entered_again:?}"
            );
        }
        let reloaded = engine
            .registry
            .cached_load_spec("gcloud/compute")
            .expect("compute reloaded");
        assert!(!Arc::ptr_eq(&compute, &reloaded));
        assert!(!reloaded.meta.ai_resolved_reference);
        assert!(Arc::ptr_eq(
            &parent,
            &engine.registry.get_arc("gcloud").expect("parent arc")
        ));
        let stub = engine
            .registry
            .get_arc("gcloud")
            .expect("gcloud")
            .find_subcommand("compute")
            .expect("stub")
            .clone();
        assert!(!stub.meta.ai_resolved_reference);
        assert!(matches!(stub.load_spec, Some(crate::ir::LoadSpec::Path(ref path)) if path == "gcloud/compute"));

        let root = engine.registry.get_arc("gcloud").expect("gcloud");
        let mut tokens = vec!["gcloud".to_string(), "compute".to_string()];
        let walked = lookup::resolve_context(root, &mut tokens, true, "", "", Some(&mut engine.registry));
        assert!(walked.spec.meta.ai_resolved_reference);
        assert!(!Arc::ptr_eq(&reloaded, &walked.spec));
        let stub = engine
            .registry
            .get_arc("gcloud")
            .expect("gcloud")
            .find_subcommand("compute")
            .expect("stub")
            .clone();
        assert!(!stub.meta.ai_resolved_reference);
    }

    /// Bytes from `footprint --noCategories -f bytes -p <pid>`, the same field
    /// `scripts/memory-usage.sh` records. This test binary has no mimalloc.
    fn phys_footprint_bytes() -> u64 {
        let output = std::process::Command::new("footprint")
            .args(["--noCategories", "-f", "bytes", "-p", &std::process::id().to_string()])
            .output()
            .expect("footprint");
        assert!(
            output.status.success(),
            "footprint exited {:?}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines()
            .find_map(|line| {
                let rest = line.split("Footprint:").nth(1)?;
                rest.split_whitespace().next()?.parse::<u64>().ok()
            })
            .unwrap_or_else(|| panic!("footprint output had no Footprint byte count:\n{text}"))
    }

    fn assert_gcloud_compute_stub(engine: &mut Engine) {
        assert!(engine.registry.is_cached("gcloud"));
        assert!(!engine.registry.is_cached("compute"));
        assert!(engine.registry.cached_load_spec("gcloud/compute").is_none());
        assert!(engine.registry.idle_mark("gcloud/compute").is_none());
        let parent = engine.registry.get_arc("gcloud").expect("gcloud");
        let stub = parent.find_subcommand("compute").expect("compute stub").clone();
        drop(parent);
        assert!(!stub.meta.ai_resolved_reference);
        assert!(matches!(
            &stub.load_spec,
            Some(crate::ir::LoadSpec::Path(path)) if path == "gcloud/compute"
        ));
    }

    fn leave_gcloud_compute_and_release(engine: &mut Engine) {
        drop(complete_listed(engine, "gcloud "));
        let marked = engine
            .registry
            .idle_mark("gcloud/compute")
            .expect("leaving compute starts the grace")
            .expect("the grace has a start");
        assert_eq!(engine.registry.idle_mark("gcloud"), Some(None));
        engine
            .registry
            .release_idle(marked + crate::ir::SPEC_IDLE_GRACE, crate::ir::SPEC_IDLE_GRACE);
        assert_gcloud_compute_stub(engine);
    }

    /// Default `cargo test` skips this so the suite does not parse bundled
    /// `gcloud/compute.json`.
    #[test]
    #[ignore = "parses bundle/specs-ir/gcloud/compute.json"]
    fn compute_json_idle_release_footprint() {
        let _lock = engine_lock();
        let _settings = quiet_settings();
        let specs = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bundle/specs-ir");
        assert!(specs.join("gcloud/compute.json").is_file(), "{}", specs.display());

        let mut engine = Engine::new_with_frecency(specs, Frecency::default()).expect("engine");
        let menu = complete_listed(&mut engine, "gcloud ");
        assert!(menu.suggestions.iter().any(|row| row.name == "compute"));
        assert!(engine.registry.cached_load_spec("gcloud/compute").is_none());
        drop(menu);
        let footprint_menu = phys_footprint_bytes();

        let entered = complete_listed(&mut engine, "gcloud compute ");
        let entered_rows = entered.suggestions.clone();
        assert!(
            entered_rows.iter().any(|row| row.name == "instances"),
            "compute menu had {} rows",
            entered_rows.len()
        );
        drop(entered);
        let allocated_bytes = {
            let resident = engine
                .registry
                .cached_load_spec("gcloud/compute")
                .expect("compute loaded");
            assert!(!resident.meta.ai_resolved_reference);
            assert!(!engine.registry.is_cached("compute"));
            resident.allocated_bytes()
        };
        let footprint_held = phys_footprint_bytes();
        leave_gcloud_compute_and_release(&mut engine);
        let footprint_released = phys_footprint_bytes();
        let returned = footprint_held as i64 - footprint_released as i64;
        println!(
            "COMPUTE_IDLE rows={} allocated_bytes={allocated_bytes} footprint_menu={footprint_menu} footprint_held={footprint_held} footprint_released={footprint_released} returned={returned}",
            entered_rows.len()
        );

        let reloaded = complete_listed(&mut engine, "gcloud compute ");
        assert_eq!(reloaded.suggestions, entered_rows);
        let previous = engine
            .registry
            .cached_load_spec("gcloud/compute")
            .expect("compute reloaded");
        assert!(!previous.meta.ai_resolved_reference);
        drop(reloaded);
        leave_gcloud_compute_and_release(&mut engine);
        let again = complete_listed(&mut engine, "gcloud compute ");
        let renewed = engine
            .registry
            .cached_load_spec("gcloud/compute")
            .expect("compute reloaded again");
        assert!(!Arc::ptr_eq(&previous, &renewed));
        assert_eq!(again.suggestions, entered_rows);
    }
}

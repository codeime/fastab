//! Native generators: files, argv scripts, git refs, npm scripts/deps.

use std::cell::{Cell, LazyCell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::filegen;
use crate::ir::{ArgSpec, Builtin, GeneratorSpec, GeneratorTrigger, Spec, SuggestionMeta, Template};
use crate::process;
use crate::query::matches_query;
use crate::runtime::{Suggestion, query_term_with_hook, suggestion_query_term_with_hook};

fn hidden_generated_row_is_visible(suggestion: &Suggestion, query: &str) -> bool {
    !suggestion.hidden || suggestion.name.eq_ignore_ascii_case(query)
}

fn hidden_static_seed_is_visible(seed: &crate::ir::SuggestionSeed, query: &str, kind: &str) -> bool {
    if !seed.meta.hidden {
        return true;
    }
    let exact_query = if kind == "folder" && !query.ends_with('/') {
        format!("{query}/")
    } else {
        query.to_string()
    };
    seed.names
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(&exact_query))
}

/// Fig's `getScriptSuggestions` supplies 5000ms when no user setting is
/// present.  Generator-specific `scriptTimeout` and an ExecuteCommand
/// object's `timeout` are then compared with that value using Math.max.
/// Keep the unit explicit here: settings and spec IR store milliseconds.
pub const DEFAULT_SCRIPT_TIMEOUT_MS: i64 = 5_000;
const MAX_RESULTS: usize = 50;
const MAX_CACHED: usize = 32;
/// A generator whose last run took at least this long waits for a follow-up
/// request when the caller opts in, so the static rows do not wait for it.
const SLOW_GENERATOR: Duration = Duration::from_millis(80);
const MAX_SLOW_GENERATORS: usize = 256;

pub(crate) fn configured_script_timeout_ms() -> i64 {
    configured_script_timeout_ms_from(&fastab_settings::settings::Settings::new())
}

pub(crate) fn configured_script_timeout_ms_from(settings: &fastab_settings::settings::Settings) -> i64 {
    settings
        .get_int("autocomplete.scriptTimeout")
        .ok()
        .flatten()
        .unwrap_or(DEFAULT_SCRIPT_TIMEOUT_MS)
}

/// Port the old JS timeout selection without allowing a negative value to
/// wrap when converting to Rust's unsigned Duration.  JS's timer receives
/// zero for a result <= 0, so this clamp is the only conversion clamp.
fn effective_script_timeout_ms(
    default_timeout_ms: i64,
    generator_timeout_ms: Option<i64>,
    command_timeout_ms: Option<i64>,
) -> u64 {
    let selected = default_timeout_ms
        .max(generator_timeout_ms.unwrap_or(0))
        .max(command_timeout_ms.unwrap_or(0));
    u64::try_from(selected).unwrap_or(0)
}

fn script_timeout_for(arg: &ArgSpec) -> Duration {
    Duration::from_millis(effective_script_timeout_ms(
        configured_script_timeout_ms(),
        arg.script_timeout_ms,
        None,
    ))
}

#[derive(Clone, Default)]
struct CachedGenerator {
    results: Vec<Suggestion>,
    needs_run: bool,
    ran: bool,
}

#[derive(Clone, Default)]
pub(crate) struct GeneratorSession {
    arg_id: String,
    search_term: String,
    entries: Vec<CachedGenerator>,
}

impl GeneratorSession {
    pub(crate) fn diagnostics(&self) -> crate::diagnostics::GeneratorSessionDiagnostics {
        crate::diagnostics::GeneratorSessionDiagnostics {
            generators: self.entries.len(),
            rows: self.entries.iter().map(|entry| entry.results.len()).sum(),
            allocated_bytes: self.arg_id.capacity()
                + self.search_term.capacity()
                + self.entries.capacity() * std::mem::size_of::<CachedGenerator>()
                + self
                    .entries
                    .iter()
                    .map(|entry| {
                        entry.results.capacity() * std::mem::size_of::<Suggestion>()
                            + entry
                                .results
                                .iter()
                                .map(crate::hook_cache::suggestion_heap)
                                .sum::<usize>()
                    })
                    .sum::<usize>(),
        }
    }
}

thread_local! {
    static GENERATOR_SESSION: RefCell<GeneratorSession> = RefCell::new(GeneratorSession::default());
    /// Follow-up delay in ms, once a generator was deferred this request.
    static PENDING_GENERATORS: Cell<Option<i64>> = const { Cell::new(None) };
    static DEFER_SLOW_GENERATORS: Cell<bool> = const { Cell::new(false) };
    static HISTORY: RefCell<Arc<crate::history::HistoryStore>> = RefCell::new(Arc::default());
}

/// Install a session saved on [`crate::runtime::Engine`]. Each desktop
/// completion runs on a fresh `ec-engine-attempt` thread, so the cache has
/// to travel with the engine rather than live only in this thread-local.
pub(crate) fn install_session(session: GeneratorSession) {
    GENERATOR_SESSION.with(|cell| *cell.borrow_mut() = session);
}

pub(crate) fn take_session() -> GeneratorSession {
    GENERATOR_SESSION.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

/// Hand the `history` template generator the shell history the engine
/// loaded. The engine calls this once per request with a clone of the
/// `Arc` it owns, so this is a pointer copy, not a list copy, and the
/// per-command argument index built inside the store survives the request.
pub(crate) fn set_history(store: Arc<crate::history::HistoryStore>) {
    HISTORY.with(|cell| *cell.borrow_mut() = store);
}

pub(crate) fn history_store() -> Arc<crate::history::HistoryStore> {
    HISTORY.with(|cell| Arc::clone(&cell.borrow()))
}

pub fn take_pending_generators() -> (bool, Option<i64>) {
    PENDING_GENERATORS.with(|cell| match cell.take() {
        Some(delay_ms) => (true, Some(delay_ms)),
        None => (false, None),
    })
}

/// Generators deferred by one request share the earliest follow-up.
fn set_pending_generators(delay_ms: i64) {
    PENDING_GENERATORS.with(|cell| cell.set(Some(cell.get().map_or(delay_ms, |pending| pending.min(delay_ms)))));
}

/// Only the desktop overlay answers `pending_generators` with a follow-up
/// request, so only it may leave a known-slow generator for that request.
/// The CLI and golden cases take one request per result and run everything.
pub(crate) fn set_defer_slow_generators(enabled: bool) {
    DEFER_SLOW_GENERATORS.with(|cell| cell.set(enabled));
}

/// Fig ran every generator as its own promise and drew the static rows while
/// they loaded. Deferring all of them would make a fast `git branch` flicker in
/// a frame after the options, so only generators that ran slow last time wait.
/// Path and history templates never do: they retrigger on most keystrokes, and
/// with a `getQueryTerm` each follow-up would blank the list while it ran.
fn generator_may_defer(arg: &ArgSpec, generator: &GeneratorSpec) -> bool {
    let runs_command = !generator.script.is_empty()
        || generator.js_script.is_some()
        || generator.js_custom.is_some()
        || generator.builtin.is_some()
        || (arg.generators.is_empty() && (arg.builtin.is_some() || !arg.builtins.is_empty()));
    runs_command && !generator_lists_paths(generator, arg) && !generator.templates.contains(&Template::History)
}

/// One generator across directories and argument slots, unlike
/// [`generator_arg_id`], which also keys the cwd and the typed tokens.
fn generator_speed_key(arg: &ArgSpec, generator: &GeneratorSpec) -> String {
    format!(
        "{}::{:?}::{:?}::{}::{}::{}",
        arg.name,
        generator.builtin.or(arg.builtin),
        arg.builtins,
        generator.js_custom.as_deref().unwrap_or(""),
        generator.js_script.as_deref().unwrap_or(""),
        generator.script.join("\x1f")
    )
}

fn generator_is_known_slow(key: &str) -> bool {
    SLOW_GENERATORS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(key)
}

/// Remember how the last run went. A run cancelled early says nothing about
/// speed, but one cancelled after the budget had already passed was slow.
fn note_generator_speed(key: String, elapsed: Duration, cancelled: bool) {
    let mut slow = SLOW_GENERATORS.lock().unwrap_or_else(|error| error.into_inner());
    if elapsed >= SLOW_GENERATOR {
        if slow.len() >= MAX_SLOW_GENERATORS && !slow.contains(&key) {
            slow.clear();
        }
        slow.insert(key);
    } else if !cancelled {
        slow.remove(&key);
    }
}

fn generator_arg_id(tokens: &[String], arg: &ArgSpec, cwd: &str, search_term: &str) -> String {
    // `tokenize` does not emit an empty trailing token, so `git add ` is
    // `["git", "add"]` with an empty search term. Dropping the last token
    // there would collapse that to `"git"` and miss the cache when the user
    // then types `git add s`. Keep every token after a trailing space; drop
    // only the token currently being typed.
    let path = if search_term.is_empty() || tokens.len() <= 1 {
        tokens.join("\x1f")
    } else {
        tokens[..tokens.len() - 1].join("\x1f")
    };
    let gens = arg
        .generators
        .iter()
        .map(|generator| {
            format!(
                "{:?}:{}:{}:{}",
                generator.templates,
                generator.js_custom.as_deref().unwrap_or(""),
                generator.js_script.as_deref().unwrap_or(""),
                generator.script.join(",")
            )
        })
        .collect::<Vec<_>>()
        .join("|");
    format!(
        "{cwd}::{path}::{}::{:?}::{gens}::{}::{}",
        arg.name,
        arg.builtin,
        arg.js_custom.as_deref().unwrap_or(""),
        arg.script.join("\x1f")
    )
}

struct MtimeLru<T> {
    entries: HashMap<String, (Option<SystemTime>, T)>,
    order: VecDeque<String>,
}

impl<T> Default for MtimeLru<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl<T: Clone> MtimeLru<T> {
    fn get(&mut self, key: &str, mtime: Option<SystemTime>) -> Option<T> {
        if let Some((stored, value)) = self.entries.get(key) {
            if *stored == mtime {
                let hit = value.clone();
                if let Some(pos) = self.order.iter().position(|existing| existing == key) {
                    if pos + 1 != self.order.len() {
                        let key = self.order.remove(pos).expect("index from position()");
                        self.order.push_back(key);
                    }
                }
                return Some(hit);
            }
        } else {
            return None;
        }
        self.entries.remove(key);
        if let Some(pos) = self.order.iter().position(|existing| existing == key) {
            self.order.remove(pos);
        }
        None
    }

    fn insert(&mut self, key: String, mtime: Option<SystemTime>, value: T) {
        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), (mtime, value));
            if let Some(pos) = self.order.iter().position(|existing| existing == &key) {
                if pos + 1 != self.order.len() {
                    let key = self.order.remove(pos).expect("index from position()");
                    self.order.push_back(key);
                }
            }
            return;
        }
        if self.entries.len() >= MAX_CACHED {
            while let Some(old) = self.order.pop_front() {
                if self.entries.remove(&old).is_some() {
                    break;
                }
            }
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, (mtime, value));
    }
}

static GIT_REFS: LazyLock<Mutex<MtimeLru<Arc<[String]>>>> = LazyLock::new(|| Mutex::new(MtimeLru::default()));
static PACKAGE_JSON: LazyLock<Mutex<MtimeLru<Arc<serde_json::Value>>>> =
    LazyLock::new(|| Mutex::new(MtimeLru::default()));
/// [`generator_speed_key`]s whose last run reached [`SLOW_GENERATOR`]. Process
/// wide: a cancelled request drops its session, and the mark must survive it.
static SLOW_GENERATORS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Compatibility wrapper used by generator-focused tests and callers that do
/// not already have a resolved lookup context.  The completion path calls
/// [`generate_for_arg`] with exactly one active argument.
#[allow(dead_code)]
pub fn generate(spec: &Spec, tokens: &[String], query: &str, cwd: &str, fuzzy: bool) -> Vec<Suggestion> {
    if query.starts_with('-') {
        return Vec::new();
    }
    let ends_with_space = query.is_empty();
    let mut walk_tokens = tokens.to_vec();
    let active = crate::lookup::resolve_context(
        Arc::new(spec.clone()),
        &mut walk_tokens,
        ends_with_space,
        query,
        query,
        None,
    )
    .active_arg
    .or_else(|| {
        spec.args.first().map(|arg| crate::lookup::ActiveArg {
            arg: arg.clone(),
            slot: None,
            query: query.to_string(),
            search_term: query.to_string(),
            only_suggest_args: false,
            source: crate::lookup::ActiveArgSource::PositionalValue,
        })
    });
    if let Some(active) = active {
        return generate_for_arg_with_search_term(&active.arg, tokens, &active.query, &active.search_term, cwd, fuzzy);
    }
    Vec::new()
}

#[allow(dead_code)]
pub(crate) fn generate_for_arg(
    arg: &ArgSpec,
    tokens: &[String],
    query: &str,
    cwd: &str,
    fuzzy: bool,
) -> Vec<Suggestion> {
    generate_for_arg_with_search_term(arg, tokens, query, query, cwd, fuzzy)
}

/// Generate rows for an active argument while retaining the raw shell search
/// term.  String getQueryTerm rules are per suggestion, so filtering and the
/// query term carried by each row must be computed independently from the
/// global result match term.
pub(crate) fn generate_for_arg_with_search_term(
    arg: &ArgSpec,
    tokens: &[String],
    query: &str,
    search_term: &str,
    cwd: &str,
    fuzzy: bool,
) -> Vec<Suggestion> {
    generate_for_arg_with_history(arg, tokens, query, search_term, cwd, fuzzy, Vec::new)
}

/// Build history values only when a History generator actually runs. One
/// request shares the result across generators; cache refiltering and pending
/// debounce runs never invoke the provider.
pub(crate) fn generate_for_arg_with_history(
    arg: &ArgSpec,
    tokens: &[String],
    query: &str,
    search_term: &str,
    cwd: &str,
    fuzzy: bool,
    history_values: impl FnOnce() -> Vec<String>,
) -> Vec<Suggestion> {
    // `search_term` is deliberately raw for insertion, while every generator
    // sees the parser token's inner text.  In particular, passing `'src/m`
    // through to filegen would make it look for a literal `'src` directory.
    let history_values = LazyCell::new(history_values);
    let normalized_search_term = crate::lookup::parser_inner_text(search_term);
    let arg_query =
        if arg.meta.get_query_term.is_some() || arg.js_get_query_term.is_some() || arg.meta.js_get_query_term.is_some()
        {
            query_term_with_hook(
                &normalized_search_term,
                arg.meta.get_query_term.as_deref(),
                arg.js_get_query_term
                    .as_deref()
                    .or(arg.meta.js_get_query_term.as_deref()),
            )
        } else {
            query.to_string()
        };
    let timeout = script_timeout_for(arg);
    let mut out = generate_static_seeds(arg, &arg_query, &normalized_search_term, fuzzy);
    let generators = effective_generators(arg);
    let arg_id = generator_arg_id(tokens, arg, cwd, search_term);
    let debounce = arg.debounce_ms.is_some();
    // Fig waited 200ms for `debounce: true` and for a non-positive number.
    let debounce_ms = arg.debounce_ms.filter(|ms| *ms > 0).unwrap_or(200);
    let defer_slow = !debounce && DEFER_SLOW_GENERATORS.with(Cell::get);
    let mut pending_ms: Option<i64> = None;
    GENERATOR_SESSION.with(|session| {
        let mut session = session.borrow_mut();
        let arg_changed = session.arg_id != arg_id;
        if arg_changed {
            session.arg_id.clone_from(&arg_id);
            session.entries.clear();
            session.search_term.clear();
        }
        session.entries.resize_with(generators.len(), CachedGenerator::default);
        let previous = session.search_term.clone();
        for (index, generator) in generators.iter().enumerate() {
            if crate::cancellation::is_cancelled() {
                return;
            }
            let trigger = should_trigger(
                generator.trigger.as_ref(),
                debounce,
                arg_changed || !session.entries[index].ran,
                &normalized_search_term,
                &previous,
                generator_lists_paths(generator, arg),
            );
            // getQueryTerm tail for filtering returned names. Custom hooks
            // receive the parser token's inner text, not the raw shell token.
            let gen_query = query_term_with_hook(
                &normalized_search_term,
                generator
                    .get_query_term
                    .as_deref()
                    .or(arg.meta.get_query_term.as_deref()),
                generator
                    .js_get_query_term
                    .as_deref()
                    .or(arg.js_get_query_term.as_deref())
                    .or(arg.meta.js_get_query_term.as_deref()),
            );
            let speed_key =
                (defer_slow && generator_may_defer(arg, generator)).then(|| generator_speed_key(arg, generator));
            let deferred = debounce || speed_key.as_deref().is_some_and(generator_is_known_slow);
            // Only a deferral sets `needs_run`, so this is the request it asked for.
            let follow_up =
                !arg_changed && session.entries[index].needs_run && session.search_term == normalized_search_term;
            let has_query_rule = generator.get_query_term.is_some()
                || generator.js_get_query_term.is_some()
                || arg.meta.get_query_term.is_some()
                || arg.js_get_query_term.is_some()
                || arg.meta.js_get_query_term.is_some();
            let mut rows = if deferred && !follow_up && (trigger || arg_changed) {
                session.entries[index].needs_run = true;
                let delay_ms = if debounce { debounce_ms } else { 0 };
                pending_ms = Some(pending_ms.map_or(delay_ms, |pending| pending.min(delay_ms)));
                // Fig kept a loading generator's previous rows on screen only
                // when it had no `getQueryTerm`: with one, the old rows may
                // not filter correctly against the new term, so they are
                // withheld until the deferred run lands.
                if has_query_rule {
                    Vec::new()
                } else {
                    refilter_generated(&session.entries[index].results, &gen_query, fuzzy)
                }
            } else if !trigger && !follow_up {
                refilter_generated(&session.entries[index].results, &gen_query, fuzzy)
            } else {
                let started = Instant::now();
                let mut rows = generate_from_generator(
                    arg,
                    generator,
                    tokens,
                    &history_values,
                    &gen_query,
                    &normalized_search_term,
                    cwd,
                    fuzzy,
                    timeout,
                );
                let cancelled = crate::cancellation::is_cancelled();
                if let Some(key) = speed_key {
                    note_generator_speed(key, started.elapsed(), cancelled);
                }
                if cancelled {
                    return;
                }
                dedup_session_rows(&mut rows);
                session.entries[index].results = rows;
                session.entries[index].needs_run = false;
                session.entries[index].ran = true;
                session.entries[index].results.clone()
            };
            stamp_query_term(&mut rows, &gen_query, search_term, has_query_rule);
            out.extend(rows);
        }
        // Trigger state follows the parser token, not its raw shell spelling;
        // insertion still receives the raw token through the result below.
        session.search_term = normalized_search_term.clone();
    });
    if crate::cancellation::is_cancelled() {
        return Vec::new();
    }
    if let Some(delay_ms) = pending_ms {
        set_pending_generators(delay_ms);
    }
    if arg.meta.is_dangerous {
        for suggestion in &mut out {
            suggestion.is_dangerous = true;
        }
    }
    let has_arg_query_rule =
        arg.meta.get_query_term.is_some() || arg.js_get_query_term.is_some() || arg.meta.js_get_query_term.is_some();
    stamp_query_term(&mut out, &arg_query, search_term, has_arg_query_rule);
    dedup_suggestions(&mut out);
    out
}

fn stamp_query_term(rows: &mut [Suggestion], query_term: &str, search_term: &str, has_rule: bool) {
    if !has_rule {
        return;
    }
    for suggestion in rows {
        if suggestion.query_term.is_some() {
            continue;
        }
        // Native file/folder names already include the typed directory
        // (`src/main.rs` from `src/m`). Stamping the getQueryTerm tail would
        // make insertion delete only that tail and double the directory.
        if path_row_already_includes_directory_prefix(&suggestion.name, &suggestion.kind, search_term, query_term) {
            continue;
        }
        suggestion.query_term = Some(query_term.to_string());
    }
}

fn path_row_already_includes_directory_prefix(name: &str, kind: &str, search_term: &str, query_term: &str) -> bool {
    if !matches!(kind, "file" | "folder") {
        return false;
    }
    let normalized_search_term = crate::lookup::parser_inner_text(search_term);
    if !normalized_search_term.ends_with(query_term) {
        return false;
    }
    // `getQueryTerm: "/"` on `src/` yields an empty tail. The directory is
    // then the whole search term; skip stamping so insertion still uses `src/`.
    let directory = &normalized_search_term[..normalized_search_term.len() - query_term.len()];
    if directory.is_empty() {
        return false;
    }
    name.starts_with(directory)
}

fn generate_static_seeds(arg: &ArgSpec, query: &str, search_term: &str, fuzzy: bool) -> Vec<Suggestion> {
    let mut out = Vec::new();
    for seed in &arg.suggestions {
        let suggestion_kind = seed.meta.suggestion_type.as_deref().unwrap_or("arg");
        let (seed_query, seed_query_term) = suggestion_query_term_with_hook(
            suggestion_kind,
            seed.meta.get_query_term.as_deref(),
            seed.meta.js_get_query_term.as_deref(),
            query,
            search_term,
        );
        let name = seed
            .names
            .iter()
            .find(|candidate| matches_query(candidate, &seed_query, fuzzy))
            .or_else(|| {
                seed.meta
                    .display_name
                    .as_deref()
                    .filter(|display| matches_query(display, &seed_query, fuzzy))
                    .and_then(|_| seed.names.first())
            });
        if let Some(name) = name {
            // Hidden static rows remain available when the user typed one of
            // their aliases exactly, including a case-insensitive spelling.
            // Partial and empty searches must continue to hide them.
            if !hidden_static_seed_is_visible(seed, &seed_query, suggestion_kind) {
                continue;
            }
            let display_name = seed
                .meta
                .display_name
                .clone()
                .or_else(|| (seed.names.len() > 1).then(|| seed.names.join(", ")));
            out.push(
                Suggestion::new(name.clone(), seed.description.clone(), suggestion_kind)
                    .with_args_hint(seed.args_hint.clone())
                    .with_meta(
                        seed.meta.insert_value.clone(),
                        display_name,
                        seed.meta.separator_to_add.clone(),
                        seed.meta.should_add_space.unwrap_or(false),
                        seed.meta.hidden,
                        seed.meta.priority,
                        seed.meta.icon.clone(),
                    )
                    .with_primary_name(seed.names.first().cloned())
                    .with_alias_names(seed.names.clone())
                    .with_dangerous(seed.meta.is_dangerous || arg.meta.is_dangerous)
                    .with_original_type(seed.meta.original_type.clone())
                    .with_query_term(seed_query_term),
            );
        }
    }
    out
}

fn effective_generators(arg: &ArgSpec) -> Vec<GeneratorSpec> {
    if !arg.generators.is_empty() {
        let mut generators = arg.generators.clone();
        let covered: HashSet<Template> = generators
            .iter()
            .flat_map(|generator| generator.templates.iter().copied())
            .collect();
        let missing: Vec<Template> = arg
            .templates
            .iter()
            .copied()
            .filter(|template| !covered.contains(template))
            .collect();
        if !missing.is_empty() {
            generators.push(GeneratorSpec {
                templates: missing,
                ..GeneratorSpec::default()
            });
        }
        return generators;
    }
    vec![GeneratorSpec {
        templates: arg.templates.clone(),
        script: arg.script.clone(),
        split_on: arg.split_on.clone(),
        js_post_process: arg.js_post_process.clone(),
        js_custom: arg.js_custom.clone(),
        js_script: arg.js_script.clone(),
        cache_key: arg.cache_key.clone(),
        cache_by_directory: arg.cache_by_directory,
        cache_ttl_ms: arg.cache_ttl_ms,
        cache_strategy: arg.cache_strategy.clone(),
        script_timeout_ms: arg.script_timeout_ms,
        builtin: arg.builtin,
        get_query_term: arg.meta.get_query_term.clone(),
        js_get_query_term: arg.js_get_query_term.clone().or(arg.meta.js_get_query_term.clone()),
        js_filter_template_suggestions: None,
        trigger: None,
        ..GeneratorSpec::default()
    }]
}

fn generator_lists_paths(generator: &GeneratorSpec, arg: &ArgSpec) -> bool {
    let templates = if !arg.generators.is_empty() {
        generator.templates.as_slice()
    } else if generator.templates.is_empty() {
        arg.templates.as_slice()
    } else {
        generator.templates.as_slice()
    };
    templates
        .iter()
        .any(|template| matches!(template, Template::Filepaths | Template::Folders))
}

fn should_trigger(
    trigger: Option<&GeneratorTrigger>,
    debounce: bool,
    arg_changed: bool,
    search_term: &str,
    previous: &str,
    lists_paths: bool,
) -> bool {
    if arg_changed {
        return true;
    }
    let Some(trigger) = trigger else {
        // Fig `filepaths()` retriggers when the typed prefix changes directory
        // or filename; a missing trigger on a real JS generator does not.
        return if lists_paths { search_term != previous } else { debounce };
    };
    match trigger.on.as_str() {
        "function" => {
            let Some(hook) = trigger.js_trigger.as_deref() else {
                return true;
            };
            crate::hook_backend::dispatch_trigger(hook, search_term, previous).unwrap_or(true)
        },
        "string" => {
            let needle = trigger_string(trigger);
            last_index(search_term, &needle) != last_index(previous, &needle)
        },
        "threshold" => {
            let length = usize::try_from(trigger.length.unwrap_or(0)).unwrap_or(0);
            utf16_len(search_term) > length && utf16_len(previous) <= length
        },
        "match" => trigger_match_index(trigger, search_term) != trigger_match_index(trigger, previous),
        _ => search_term != previous,
    }
}

fn trigger_string(trigger: &GeneratorTrigger) -> String {
    match trigger.string.as_ref() {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(items)) => items
            .first()
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn trigger_match_index(trigger: &GeneratorTrigger, search_term: &str) -> i32 {
    let strings: Vec<String> = match trigger.string.as_ref() {
        Some(serde_json::Value::String(text)) => vec![text.clone()],
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    strings
        .iter()
        .position(|candidate| candidate == search_term)
        .map_or(-1, |index| i32::try_from(index).unwrap_or(-1))
}

fn last_index(haystack: &str, needle: &str) -> i32 {
    if needle.is_empty() {
        return i32::try_from(utf16_len(haystack)).unwrap_or(-1);
    }
    haystack.rfind(needle).map_or(-1, |index| {
        i32::try_from(haystack[..index].encode_utf16().count()).unwrap_or(-1)
    })
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn refilter_generated(rows: &[Suggestion], query: &str, fuzzy: bool) -> Vec<Suggestion> {
    rows.iter()
        .filter(|suggestion| generated_row_matches_query(suggestion, query, fuzzy))
        .filter(|suggestion| hidden_generated_row_is_visible(suggestion, query))
        .cloned()
        .collect()
}

fn generated_row_matches_query(suggestion: &Suggestion, query: &str, fuzzy: bool) -> bool {
    if query.is_empty() || matches_query(&suggestion.name, query, fuzzy) {
        return true;
    }
    // Native path generators filter the basename before adding the typed
    // directory prefix to the displayed name (`src/main.rs` for query `m`).
    // Apply the same rule when a cached row is reused, otherwise a raw quote
    // change can incorrectly refilter every cached path row away.
    matches!(suggestion.kind.as_str(), "file" | "folder")
        && suggestion
            .name
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .is_some_and(|basename| matches_query(basename, query, fuzzy))
}

fn hook_session_cwd() -> Option<String> {
    crate::hook_backend::current_cwd()
}

#[allow(clippy::too_many_arguments)]
fn generate_from_generator(
    arg: &ArgSpec,
    generator: &GeneratorSpec,
    tokens: &[String],
    history_values: &LazyCell<Vec<String>, impl FnOnce() -> Vec<String>>,
    query: &str,
    normalized_search_term: &str,
    cwd: &str,
    fuzzy: bool,
    timeout: Duration,
) -> Vec<Suggestion> {
    let timeout = Duration::from_millis(effective_script_timeout_ms(
        i64::try_from(timeout.as_millis()).unwrap_or(DEFAULT_SCRIPT_TIMEOUT_MS),
        generator.script_timeout_ms.or(arg.script_timeout_ms),
        None,
    ));
    let mut out = Vec::new();
    let builtins = if arg.generators.is_empty() {
        let mut builtins = arg.builtins.clone();
        if let Some(builtin) = generator.builtin.or(arg.builtin) {
            builtins.push(builtin);
        }
        builtins
    } else {
        generator.builtin.into_iter().collect()
    };
    let mut seen_builtins = HashSet::new();
    for builtin in builtins {
        if !seen_builtins.insert(builtin) {
            continue;
        }
        out.extend(match builtin {
            Builtin::GitRefs => git_refs(query, cwd, fuzzy, timeout),
            Builtin::GitBranches => git_branches(query, cwd, fuzzy, timeout),
            Builtin::GitTags => git_tags(query, cwd, fuzzy, timeout),
            Builtin::GitCommits => git_commits(query, cwd, fuzzy, timeout),
            Builtin::GitRemotes => git_remotes(query, cwd, fuzzy, timeout),
            Builtin::GitChangedFiles => git_changed_files(query, cwd, fuzzy, timeout),
            Builtin::GitStashes => git_stashes(query, cwd, fuzzy, timeout),
            Builtin::GitAliases => git_aliases(query, cwd, fuzzy, timeout),
            Builtin::NpmScripts => npm_scripts(query, cwd, fuzzy, timeout),
            Builtin::NpmDeps => npm_deps(query, cwd, fuzzy, timeout),
            Builtin::Cobra => crate::cobra::complete(tokens, cwd, fuzzy),
        });
    }
    let snapshot = arg_snapshot_for_generator(arg, generator);
    // Fig's script and custom generators both bail on `haveContextForGenerator`
    // — no cwd, no run — so an empty cwd yields no rows from either.
    // A live session comes from `hook_backend::enter_context` (Engine::complete).
    if let Some(session_cwd) = hook_session_cwd() {
        let cwd = if cwd.is_empty() { session_cwd } else { cwd.to_string() };
        out.extend(run_js_generators(
            &snapshot,
            tokens,
            query,
            normalized_search_term,
            &cwd,
            fuzzy,
            timeout,
        ));
    } else if !snapshot.script.is_empty() && !cwd.is_empty() {
        out.extend(run_script(
            &snapshot.script,
            query,
            cwd,
            fuzzy,
            timeout,
            snapshot.split_on.as_deref(),
        ));
    }
    if crate::cancellation::is_cancelled() {
        return Vec::new();
    }
    let templates = if arg.generators.is_empty() && generator.templates.is_empty() {
        arg.templates.as_slice()
    } else {
        generator.templates.as_slice()
    };
    let folders_only = (templates.contains(&Template::Folders) && !templates.contains(&Template::Filepaths))
        || generator.show_folders.as_deref() == Some("only");
    let lists_paths = templates
        .iter()
        .any(|template| matches!(template, Template::Filepaths | Template::Folders));
    let mut template_rows = Vec::new();
    if lists_paths {
        let environment = crate::hook_backend::current_shell().environment_variables;
        let filter = filegen::PathFilter {
            folders_only,
            files_only: generator.show_folders.as_deref() == Some("never"),
            extensions: generator.extensions.as_slice(),
            equals: generator.equals.as_slice(),
            filter_folders: generator.filter_folders.unwrap_or(false),
            file_priority: generator.file_priority,
            folder_priority: generator.folder_priority,
            root_directory: generator.root_directory.as_deref(),
            environment: environment.as_slice(),
            matches: generator.matches.as_deref(),
            matches_flags: generator.matches_flags.as_deref(),
        };
        // Filegen must receive parser inner text.  The raw token remains in
        // the completion result and is used later to delete the exact shell
        // spelling during insertion.
        template_rows.extend(filegen::complete_path_filtered(
            normalized_search_term,
            cwd,
            fuzzy,
            &filter,
        ));
    }
    if templates.contains(&Template::History) {
        if crate::cancellation::is_cancelled() {
            return Vec::new();
        }
        let values = LazyCell::force(history_values);
        if crate::cancellation::is_cancelled() {
            return Vec::new();
        }
        template_rows.extend(history_template_suggestions(values, query, fuzzy));
    }
    if let Some(hook) = generator.js_filter_template_suggestions.as_deref()
        && let Some(filtered) = crate::hook_backend::dispatch_filter_template_suggestions(hook, &template_rows, tokens)
    {
        template_rows = filtered;
    }
    out.extend(template_rows);
    out
}

fn arg_snapshot_for_generator(arg: &ArgSpec, generator: &GeneratorSpec) -> ArgSpec {
    if arg.generators.is_empty() {
        return arg.clone();
    }
    ArgSpec {
        name: arg.name.clone(),
        description: arg.description.clone(),
        templates: generator.templates.clone(),
        script: generator.script.clone(),
        split_on: generator.split_on.clone(),
        js_post_process: generator.js_post_process.clone(),
        js_custom: generator.js_custom.clone(),
        js_script: generator.js_script.clone(),
        cache_key: generator.cache_key.clone(),
        cache_by_directory: generator.cache_by_directory,
        cache_ttl_ms: generator.cache_ttl_ms,
        cache_strategy: generator.cache_strategy.clone(),
        script_timeout_ms: generator.script_timeout_ms.or(arg.script_timeout_ms),
        builtin: generator.builtin,
        meta: SuggestionMeta {
            get_query_term: generator.get_query_term.clone().or(arg.meta.get_query_term.clone()),
            js_get_query_term: generator
                .js_get_query_term
                .clone()
                .or(arg.js_get_query_term.clone())
                .or(arg.meta.js_get_query_term.clone()),
            is_dangerous: arg.meta.is_dangerous,
            ..SuggestionMeta::default()
        },
        js_get_query_term: generator.js_get_query_term.clone().or(arg.js_get_query_term.clone()),
        debounce_ms: arg.debounce_ms,
        ..ArgSpec::default()
    }
}

/// Fig `getHistoryArgSuggestions` rows: `{ name: value, type: "arg" }` for
/// each value, already ordered most recent first by the history index.
fn history_template_suggestions(values: &[String], query: &str, fuzzy: bool) -> Vec<Suggestion> {
    values
        .iter()
        .filter(|value| query.is_empty() || matches_query(value, query, fuzzy))
        .map(|value| Suggestion::new(value.clone(), "", "arg").with_insert_value(value.clone()))
        .collect()
}

fn dedup_suggestions(suggestions: &mut Vec<Suggestion>) {
    let mut seen = HashSet::new();
    suggestions.retain(|suggestion| {
        seen.insert((
            suggestion.name.clone(),
            suggestion.kind.clone(),
            suggestion.insert_value.clone(),
        ))
    });
}

/// Final output keeps the first (name, kind, insert_value) row. Equal keys
/// have the same refilter match, but hidden rows can disappear for a later
/// query, exposing a visible duplicate with different metadata. Keep those
/// two visibility classes separate in the session; within each class the
/// first row wins for every future query, including its original query_term.
fn dedup_session_rows(rows: &mut Vec<Suggestion>) {
    if rows.len() <= 1 {
        return;
    }
    let keep = {
        let mut seen = HashSet::new();
        rows.iter()
            .map(|row| {
                seen.insert((
                    row.name.as_str(),
                    row.kind.as_str(),
                    row.insert_value.as_deref(),
                    row.hidden,
                ))
            })
            .collect::<Vec<_>>()
    };
    let original_len = rows.len();
    let mut keep = keep.into_iter();
    rows.retain(|_| keep.next().expect("one keep flag per generator row"));
    if rows.len() != original_len && rows.capacity() > rows.len().saturating_mul(2).max(64) {
        rows.shrink_to_fit();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_js_generators(
    arg: &ArgSpec,
    tokens: &[String],
    query: &str,
    search_term: &str,
    cwd: &str,
    fuzzy: bool,
    timeout: Duration,
) -> Vec<Suggestion> {
    let has_script = !arg.script.is_empty() || arg.js_script.is_some();
    if cwd.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if has_script {
        let raw = run_script_or_post_process(arg, tokens, cwd, timeout);
        out.extend(
            raw.into_iter()
                .filter(|suggestion| matches_query(&suggestion.name, query, fuzzy))
                .filter(|suggestion| hidden_generated_row_is_visible(suggestion, query)),
        );
    }
    if let Some(hook_id) = arg.js_custom.as_deref() {
        let fallback = crate::hook_cache::custom_cache_fallback(tokens);
        let custom = with_suggestion_cache(arg, cwd, "custom", &fallback, || {
            crate::hook_backend::dispatch_custom(hook_id, tokens, cwd, search_term, timeout, arg.meta.is_dangerous)
                .unwrap_or_default()
        });
        out.extend(
            custom
                .into_iter()
                .filter(|suggestion| matches_query(&suggestion.name, query, fuzzy))
                .filter(|suggestion| hidden_generated_row_is_visible(suggestion, query)),
        );
    }
    out
}

/// Fig `getScriptSuggestions`. The command is resolved first (a function
/// `script` runs on every turn, uncached), then its stdout is cached on
/// that command and cwd, and only then shaped: `splitOn` wins, else
/// `postProcess` runs against the current tokens, else there are no rows.
/// Caching stdout rather than rows is what lets a `postProcess` that reads
/// `tokens` see the current buffer on a cache hit, as it does in Fig.
fn run_script_or_post_process(arg: &ArgSpec, tokens: &[String], cwd: &str, timeout: Duration) -> Vec<Suggestion> {
    let (command, args, timeout) = if let Some(hook_id) = arg.js_script.as_deref() {
        let Some(script) = crate::hook_backend::dispatch_script_command(hook_id, tokens) else {
            return Vec::new();
        };
        let timeout = Duration::from_millis(effective_script_timeout_ms(
            i64::try_from(timeout.as_millis()).unwrap_or(DEFAULT_SCRIPT_TIMEOUT_MS),
            arg.script_timeout_ms,
            script.timeout_ms,
        ));
        (script.command, script.args, timeout)
    } else if let Some((command, args)) = arg.script.split_first() {
        (command.clone(), args.to_vec(), timeout)
    } else {
        return Vec::new();
    };
    if command.is_empty() {
        return Vec::new();
    }
    let fallback = crate::hook_cache::script_cache_fallback(&command, &args, cwd);
    let stdout = with_script_cache(arg, cwd, &fallback, || process::execute(&command, &args, cwd, timeout));
    let mut script = Vec::with_capacity(args.len() + 1);
    script.push(command);
    script.extend(args);
    shape_script_output(arg, tokens, &script, &stdout)
}

/// Fig's `getScriptSuggestions` branches on `splitOn` first and only falls
/// back to `postProcess`. Specs that declare both rely on that order, so
/// running the hook here would feed it output it never expects. With
/// neither, the result stays `[]`.
fn shape_script_output(arg: &ArgSpec, tokens: &[String], script: &[String], stdout: &str) -> Vec<Suggestion> {
    // `executeCommandTimeout` hands both branches `cleanOutput(stdout)`.
    let stdout = crate::hook_backend::clean_output(stdout);
    if let Some(separator) = arg.split_on.as_deref().filter(|value| !value.is_empty()) {
        return all_split(&stdout, separator);
    }
    if let Some(hook_id) = arg.js_post_process.as_deref() {
        return crate::hook_backend::dispatch_post_process(hook_id, &stdout, tokens, script).unwrap_or_default();
    }
    Vec::new()
}

fn with_suggestion_cache(
    arg: &ArgSpec,
    cwd: &str,
    kind: &str,
    fallback: &str,
    run: impl FnOnce() -> Vec<Suggestion>,
) -> Vec<Suggestion> {
    crate::hook_cache::cached_suggestions(arg, cwd, kind, fallback, run)
}

fn with_script_cache(arg: &ArgSpec, cwd: &str, fallback: &str, run: impl FnOnce() -> String) -> String {
    crate::hook_cache::cached_script_output(arg, cwd, fallback, run)
}

/// Host-less fallback for callers outside a completion attempt (the
/// `generate` compatibility wrapper). Same shape rules as
/// [`shape_script_output`] minus `postProcess`, which needs the JS host.
fn run_script(
    script: &[String],
    query: &str,
    cwd: &str,
    fuzzy: bool,
    timeout: Duration,
    split_on: Option<&str>,
) -> Vec<Suggestion> {
    let Some((command, args)) = script.split_first() else {
        return Vec::new();
    };
    let stdout = process::execute(command, args, cwd, timeout);
    match split_on.filter(|value| !value.is_empty()) {
        Some(separator) => filter_split(&stdout, query, fuzzy, separator),
        None => Vec::new(),
    }
}

fn filter_lines(stdout: &str, query: &str, fuzzy: bool) -> Vec<Suggestion> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| matches_query(line, query, fuzzy))
        .take(MAX_RESULTS)
        .map(|line| Suggestion::new(line, "", "arg").with_insert_value(line))
        .collect()
}

/// Fig's `getScriptSuggestions` trims the whole stdout, then
/// `split(splitOn)`. It does not trim or drop each piece: spaces and empty
/// segments can be meaningful for the returned name and insert value.
fn filter_split(stdout: &str, query: &str, fuzzy: bool, split_on: &str) -> Vec<Suggestion> {
    if split_on.is_empty() {
        return filter_lines(stdout, query, fuzzy);
    }
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split(split_on)
        .filter(|part| matches_query(part, query, fuzzy))
        .take(MAX_RESULTS)
        .map(|part| Suggestion::new(part, "", "arg").with_insert_value(part))
        .collect()
}

fn all_lines(stdout: &str) -> Vec<Suggestion> {
    unique_script_rows(stdout.lines().map(str::trim).filter(|line| !line.is_empty()))
}

fn all_split(stdout: &str, split_on: &str) -> Vec<Suggestion> {
    if split_on.is_empty() {
        return all_lines(stdout);
    }
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    unique_script_rows(trimmed.split(split_on))
}

// Script pieces have identical metadata. Discard repeated borrowed pieces
// before allocating rows; final output already keeps only their first row.
// Empty pieces are significant for splitOn and remain as one empty row.
fn unique_script_rows<'a>(parts: impl Iterator<Item = &'a str>) -> Vec<Suggestion> {
    let mut seen = HashSet::new();
    parts
        .filter(|part| seen.insert(*part))
        .map(|part| Suggestion::new(part, "", "arg").with_insert_value(part))
        .collect()
}

fn git_refs(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let (key, mtime) = git_identity(cwd);
    if let Ok(mut cache) = GIT_REFS.lock() {
        if let Some(lines) = cache.get(&key, mtime) {
            return filter_cached_lines(&lines, query, fuzzy);
        }
    }
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "for-each-ref".into(),
            "--format=%(refname:short)".into(),
            "refs/heads".into(),
            "refs/tags".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };
    let lines: Arc<[String]> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>()
        .into();
    if let Ok(mut cache) = GIT_REFS.lock() {
        cache.insert(key, mtime, lines.clone());
    }
    filter_cached_lines(&lines, query, fuzzy)
}

fn git_branches(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "for-each-ref".into(),
            "--format=%(refname:short)%09%(symref)".into(),
            "refs/heads".into(),
            "refs/remotes".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };
    branch_lines_to_suggestions(&stdout, query, fuzzy)
}

fn branch_lines_to_suggestions(stdout: &str, query: &str, fuzzy: bool) -> Vec<Suggestion> {
    let mut names: Vec<_> = stdout
        .lines()
        .filter_map(|line| {
            let (name, symref) = line.split_once('\t').unwrap_or((line, ""));
            let name = name.trim();
            // `refs/remotes/origin/HEAD` is a symbolic convenience ref. Git
            // shortens it to just `origin`, which otherwise looks like a real
            // branch beside `origin/main`.
            (symref.trim().is_empty() && !name.is_empty() && matches_query(name, query, fuzzy))
                .then(|| name.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .take(MAX_RESULTS)
        .map(|name| Suggestion::new(name, "", "branch"))
        .collect()
}

fn git_tags(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "for-each-ref".into(),
            "--format=%(refname:short)".into(),
            "refs/tags".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };
    lines_to_suggestions(&stdout, query, fuzzy, "tag")
}

fn git_commits(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "--no-optional-locks".into(),
            "log".into(),
            "--all".into(),
            "--pretty=format:%h%x09%s".into(),
            "-n".into(),
            "100".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };
    stdout
        .lines()
        .filter_map(|line| {
            let (name, description) = line.split_once('\t').unwrap_or((line, ""));
            let name = name.trim();
            if name.is_empty() || !matches_query(name, query, fuzzy) {
                return None;
            }
            Some(Suggestion::new(name, description.trim(), "commit"))
        })
        .take(MAX_RESULTS)
        .collect()
}

fn git_remotes(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute("git", &["remote".into()], cwd, timeout) else {
        return Vec::new();
    };
    lines_to_suggestions(&stdout, query, fuzzy, "remote")
}

fn git_changed_files(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "--no-optional-locks".into(),
            "status".into(),
            "--short".into(),
            "--untracked-files=all".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for line in stdout.lines() {
        let Some(path) = line.get(3..).map(str::trim) else {
            continue;
        };
        let path = path.rsplit_once(" -> ").map_or(path, |(_, new)| new).trim_matches('"');
        if !path.is_empty() && matches_query(path, query, fuzzy) {
            names.push(path.to_string());
        }
    }
    names.sort();
    names.dedup();
    names
        .into_iter()
        .take(MAX_RESULTS)
        .map(|name| Suggestion::new(name, "Changed file", "file"))
        .collect()
}

fn git_stashes(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "--no-optional-locks".into(),
            "stash".into(),
            "list".into(),
            "--pretty=format:%gd%x09%s".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };
    stdout
        .lines()
        .filter_map(|line| {
            let (name, description) = line.split_once('\t').unwrap_or((line, ""));
            let name = name.trim();
            if name.is_empty() || !matches_query(name, query, fuzzy) {
                return None;
            }
            Some(Suggestion::new(name, description.trim(), "stash"))
        })
        .take(MAX_RESULTS)
        .collect()
}

fn lines_to_suggestions(stdout: &str, query: &str, fuzzy: bool, kind: &str) -> Vec<Suggestion> {
    let mut names: Vec<_> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && matches_query(line, query, fuzzy))
        .map(ToOwned::to_owned)
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .take(MAX_RESULTS)
        .map(|name| Suggestion::new(name, "", kind))
        .collect()
}

fn git_aliases(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "--no-optional-locks".into(),
            "config".into(),
            "--get-regexp".into(),
            "^alias.".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };

    stdout
        .lines()
        .filter_map(parse_git_alias)
        .filter(|suggestion| matches_query(&suggestion.name, query, fuzzy))
        .take(MAX_RESULTS)
        .collect()
}

fn parse_git_alias(line: &str) -> Option<Suggestion> {
    let line = line.trim();
    let value = line.strip_prefix("alias.")?;
    let mut fields = value.splitn(2, char::is_whitespace);
    let name = fields.next()?.trim();
    if name.is_empty() {
        return None;
    }
    let expansion = fields.next().unwrap_or("").trim();
    Some(Suggestion::new(name, format!("Alias for '{expansion}'"), "arg"))
}

fn filter_cached_lines(lines: &[String], query: &str, fuzzy: bool) -> Vec<Suggestion> {
    lines
        .iter()
        .filter(|line| matches_query(line, query, fuzzy))
        .take(MAX_RESULTS)
        .map(|line| Suggestion::new(line.clone(), "", "arg"))
        .collect()
}

fn git_identity(cwd: &str) -> (String, Option<SystemTime>) {
    let Some(git) = find_git_dir(cwd) else {
        return (format!("nogit:{cwd}"), None);
    };
    (git.display().to_string(), git_refs_mtime(&git))
}

fn git_refs_mtime(git: &std::path::Path) -> Option<SystemTime> {
    let mut best: Option<SystemTime> = None;
    for rel in ["HEAD", "packed-refs", "refs/heads", "refs/tags"] {
        if let Ok(mtime) = fs::metadata(git.join(rel)).and_then(|meta| meta.modified()) {
            best = Some(best.map_or(mtime, |prev| prev.max(mtime)));
        }
    }
    best.or_else(|| fs::metadata(git).and_then(|meta| meta.modified()).ok())
}

fn find_git_dir(cwd: &str) -> Option<PathBuf> {
    let mut dir = PathBuf::from(if cwd.is_empty() { "." } else { cwd });
    for _ in 0..16 {
        let git = dir.join(".git");
        if git.is_dir() {
            return Some(git);
        }
        if git.is_file() {
            return resolve_gitdir_file(&git).or(Some(git));
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

fn resolve_gitdir_file(file: &PathBuf) -> Option<PathBuf> {
    let text = fs::read_to_string(file).ok()?;
    for line in text.lines() {
        let rest = line.trim().strip_prefix("gitdir:")?.trim();
        if rest.is_empty() {
            continue;
        }
        let path = PathBuf::from(rest);
        let resolved = if path.is_absolute() {
            path
        } else {
            file.parent()?.join(path)
        };
        if resolved.is_dir() {
            return Some(resolved);
        }
    }
    None
}

fn npm_scripts(query: &str, cwd: &str, fuzzy: bool, _timeout: Duration) -> Vec<Suggestion> {
    let Some(pkg) = read_package_json(cwd) else {
        return Vec::new();
    };
    let Some(scripts) = pkg.get("scripts").and_then(|value| value.as_object()) else {
        return Vec::new();
    };
    let mut suggestions: Vec<Suggestion> = scripts
        .iter()
        .filter(|(name, _)| matches_query(name, query, fuzzy))
        .map(|(name, cmd)| Suggestion::new(name.clone(), cmd.as_str().unwrap_or(""), "arg"))
        .collect();
    suggestions.sort_by(|a, b| a.name.cmp(&b.name));
    suggestions.truncate(MAX_RESULTS);
    suggestions
}

fn npm_deps(query: &str, cwd: &str, fuzzy: bool, _timeout: Duration) -> Vec<Suggestion> {
    let Some(pkg) = read_package_json(cwd) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for key in ["dependencies", "devDependencies", "optionalDependencies"] {
        if let Some(deps) = pkg.get(key).and_then(|value| value.as_object()) {
            names.extend(deps.keys().cloned());
        }
    }
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|name| matches_query(name, query, fuzzy))
        .take(MAX_RESULTS)
        .map(|name| Suggestion::new(name, "dependency", "arg"))
        .collect()
}

fn read_package_json(cwd: &str) -> Option<Arc<serde_json::Value>> {
    let mut dir = PathBuf::from(if cwd.is_empty() { "." } else { cwd });
    for _ in 0..8 {
        let candidate = dir.join("package.json");
        if candidate.is_file() {
            let key = candidate.display().to_string();
            let mtime = fs::metadata(&candidate).and_then(|meta| meta.modified()).ok();
            if let Ok(mut cache) = PACKAGE_JSON.lock() {
                if let Some(parsed) = cache.get(&key, mtime) {
                    return Some(parsed);
                }
            }
            let bytes = fs::read(&candidate).ok()?;
            let parsed: Arc<serde_json::Value> = Arc::new(serde_json::from_slice(&bytes).ok()?);
            if let Ok(mut cache) = PACKAGE_JSON.lock() {
                cache.insert(key, mtime, parsed.clone());
            }
            return Some(parsed);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook_backend::{self, NativeHooks};
    use crate::hook_types::ShellContext;
    use crate::ir::{ArgSpec, OptionSpec, Spec, SuggestionMeta, SuggestionSeed, Template};
    use serde_json::json;
    use std::fs;
    use std::sync::Arc;

    fn with_session<R>(cwd: &str, f: impl FnOnce() -> R) -> R {
        let cache = crate::hook_cache::HookCache::new();
        let _bound = cache.bind();
        hook_backend::enter_context(cwd, &ShellContext::default(), f)
    }

    fn with_native<R>(native: NativeHooks, cwd: &str, f: impl FnOnce() -> R) -> R {
        let cache = crate::hook_cache::HookCache::new();
        let _hooks = hook_backend::bind_native(Arc::new(native));
        let _bound = cache.bind();
        hook_backend::enter_context(cwd, &ShellContext::default(), f)
    }

    fn suggestion_name(name: &str) -> serde_json::Value {
        json!({
            "op": "object",
            "fields": [{"key": "name", "value": {"op": "string", "value": name}}]
        })
    }

    fn constant_suggestions(names: &[&str]) -> serde_json::Value {
        json!({
            "op": "array",
            "items": names.iter().copied().map(suggestion_name).collect::<Vec<_>>()
        })
    }

    fn split_lines_prefix(prefix: &str) -> serde_json::Value {
        json!({
            "op": "array-map",
            "value": {
                "op": "array-filter",
                "value": {
                    "op": "string-split",
                    "value": {"op": "arg", "index": 0},
                    "separator": {"op": "string", "value": "\n"}
                },
                "fn": {
                    "op": "lambda",
                    "params": ["s"],
                    "body": {"op": "truthy", "value": {"op": "var", "name": "s"}}
                }
            },
            "fn": {
                "op": "lambda",
                "params": ["s"],
                "body": {
                    "op": "object",
                    "fields": [{
                        "key": "name",
                        "value": {
                            "op": "string-concat",
                            "parts": [
                                {"op": "string", "value": prefix},
                                {"op": "var", "name": "s"}
                            ]
                        }
                    }]
                }
            }
        })
    }

    #[test]
    fn script_generator_filters_prefix() {
        let spec = Spec {
            names: vec!["demo".into()],
            args: vec![ArgSpec {
                script: vec!["printf".into(), "alpha\nbeta\nalpaca\n".into()],
                split_on: Some("\n".into()),
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(&spec, &["demo".into(), "al".into()], "al", "/", false);
        let names: Vec<_> = suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "{names:?}");
        assert!(names.contains(&"alpaca"), "{names:?}");
        assert!(!names.contains(&"beta"), "{names:?}");
    }

    #[test]
    fn script_split_on_uses_the_compiler_separator() {
        let spec = Spec {
            names: vec!["demo".into()],
            args: vec![ArgSpec {
                script: vec!["printf".into(), "alpha,beta,alpaca".into()],
                split_on: Some(",".into()),
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(&spec, &["demo".into(), "al".into()], "al", "/", false);
        let names: Vec<_> = suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "{names:?}");
        assert!(names.contains(&"alpaca"), "{names:?}");
        assert!(!names.contains(&"beta"), "{names:?}");
        assert!(!names.iter().any(|name| name.contains(',')), "{names:?}");
    }

    #[test]
    fn empty_split_helper_is_defensive_and_raw_crlf_pieces_are_preserved() {
        // The public generation paths already discard falsy splitOn: "".
        // This helper-only branch remains defensive and line-oriented.
        assert_eq!(
            filter_split("alpha\r\nbeta\r\nalpaca\r\n", "al", false, "")
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "alpaca"]
        );
        // JS trims the entire output, not each interior split piece. When
        // handed raw CRLF, an interior CR is therefore preserved.
        assert_eq!(
            filter_split("alpha\r\nbeta\r\nalpaca\r\n", "al", false, "\n")
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha\r", "alpaca"]
        );
    }

    #[test]
    fn split_on_preserves_each_piece_exactly_after_outer_trim() {
        let stdout = "  alpha,  beta  ,,gamma  ";
        let rows = all_split(stdout, ",");
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "  beta  ", "", "gamma"]);
        let inserted: Vec<_> = rows.iter().map(|row| row.insert_value.as_deref()).collect();
        assert_eq!(inserted, vec![Some("alpha"), Some("  beta  "), Some(""), Some("gamma")]);
        let filtered = filter_split(stdout, "  b", false, ",");
        assert_eq!(
            filtered.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["  beta  "]
        );
    }

    #[test]
    fn session_dedup_preserves_first_metadata_and_future_hidden_refiltering() {
        let mut hidden = Suggestion::new("alpha", "hidden first", "arg").with_insert_value("alpha");
        hidden.hidden = true;
        hidden.query_term = Some("first query".into());
        let mut visible = hidden.clone();
        visible.hidden = false;
        visible.description = "visible fallback".into();
        visible.query_term = None;
        let mut later = visible.clone();
        later.description = "later duplicate".into();
        later.query_term = Some("later query".into());
        let mut original = vec![hidden.clone(), visible.clone(), later];
        original.extend(std::iter::repeat_n(visible.clone(), 1024));
        let mut compact = original.clone();
        dedup_session_rows(&mut compact);
        assert_eq!(compact, vec![hidden, visible]);
        assert!(
            compact.capacity() <= 64,
            "removed rows must release their spare capacity"
        );

        for query in ["", "a", "alpha", "ALPHA", "z"] {
            for fuzzy in [false, true] {
                let finish = |rows: &[Suggestion]| {
                    let mut rows = refilter_generated(rows, query, fuzzy);
                    stamp_query_term(&mut rows, query, &format!("src/{query}"), true);
                    dedup_suggestions(&mut rows);
                    rows
                };
                assert_eq!(finish(&compact), finish(&original), "query={query:?}, fuzzy={fuzzy}");
            }
        }
    }

    #[test]
    fn script_timeout_matches_webview_default_and_max_priority() {
        assert_eq!(DEFAULT_SCRIPT_TIMEOUT_MS, 5_000);
        assert_eq!(effective_script_timeout_ms(5_000, None, None), 5_000);
        assert_eq!(effective_script_timeout_ms(5_000, Some(6_000), None), 6_000);
        assert_eq!(effective_script_timeout_ms(5_000, Some(3_000), Some(7_000)), 7_000);
        // A negative setting/generator timeout never wraps into a huge Rust
        // duration; JS's setTimeout path observes the resulting zero.
        assert_eq!(effective_script_timeout_ms(-1_000, Some(-100), None), 0);
    }

    #[test]
    fn npm_run_reads_package_scripts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"vitest","lint":"eslint .","build":"tsc"}}"#,
        )
        .unwrap();
        let spec = Spec {
            names: vec!["npm".into()],
            args: vec![ArgSpec {
                builtin: Some(Builtin::NpmScripts),
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let cwd = dir.path().display().to_string();
        let suggestions = generate(&spec, &["npm".into(), "run".into(), "t".into()], "t", &cwd, false);
        let names: Vec<_> = suggestions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["test"]);
        let again = generate(&spec, &["npm".into(), "run".into(), "t".into()], "t", &cwd, false);
        assert_eq!(again.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["test"]);
    }

    #[test]
    fn mtime_lru_keeps_fresh_and_evicts_stale() {
        let mut cache = MtimeLru::default();
        let t1 = SystemTime::UNIX_EPOCH;
        cache.insert("a".into(), Some(t1), "one".to_string());
        assert_eq!(cache.get("a", Some(t1)).as_deref(), Some("one"));
        assert_eq!(cache.get("a", Some(t1 + Duration::from_secs(1))), None);
        cache.insert("a".into(), Some(t1 + Duration::from_secs(1)), "two".to_string());
        assert_eq!(
            cache.get("a", Some(t1 + Duration::from_secs(1))).as_deref(),
            Some("two")
        );
        for i in 0..MAX_CACHED {
            cache.insert(format!("k{i}"), Some(t1), i.to_string());
        }
        assert_eq!(cache.get("a", Some(t1 + Duration::from_secs(1))), None);
        let last = (MAX_CACHED - 1).to_string();
        assert_eq!(
            cache.get(&format!("k{}", MAX_CACHED - 1), Some(t1)).as_deref(),
            Some(last.as_str())
        );
    }

    #[test]
    fn git_refs_mtime_changes_when_a_branch_file_is_added() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let before = git_refs_mtime(&git);
        std::thread::sleep(Duration::from_millis(20));
        fs::write(git.join("refs/heads/feature"), "abc\n").unwrap();
        let after = git_refs_mtime(&git);
        assert_ne!(before, after, "new branch must bust the git-ref cache");
    }

    #[test]
    fn find_git_dir_resolves_worktree_gitdir_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.git");
        fs::create_dir_all(real.join("refs/heads")).unwrap();
        fs::write(real.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let work = dir.path().join("work");
        fs::create_dir(&work).unwrap();
        fs::write(work.join(".git"), format!("gitdir: {}\n", real.display())).unwrap();
        let found = find_git_dir(&work.display().to_string()).expect("worktree git dir");
        assert_eq!(found, real);
        assert!(git_refs_mtime(&found).is_some());
    }

    #[test]
    fn git_ref_output_is_prefix_filtered() {
        let spec = Spec {
            names: vec!["checkout".into()],
            args: vec![ArgSpec {
                script: vec!["printf".into(), "feature-x\nmain\norigin/foo\n".into()],
                split_on: Some("\n".into()),
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(
            &spec,
            &["git".into(), "checkout".into(), "fea".into()],
            "fea",
            "/",
            false,
        );
        assert_eq!(
            suggestions.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["feature-x"]
        );
    }

    #[test]
    fn git_alias_output_is_normalized_like_the_fig_generator() {
        let alias = parse_git_alias("alias.co checkout -b main").expect("alias");
        assert_eq!(alias.name, "co");
        assert_eq!(alias.description, "Alias for 'checkout -b main'");
        assert_eq!(alias.kind, "arg");
        assert!(parse_git_alias("not-an-alias").is_none());
    }

    #[test]
    fn git_alias_builtin_reads_aliases_from_the_worktree_config() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().display().to_string();
        let initialized = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&cwd)
            .status()
            .expect("git");
        assert!(initialized.success(), "git init failed");
        let configured = std::process::Command::new("git")
            .args(["config", "alias.co", "checkout -b main"])
            .current_dir(&cwd)
            .status()
            .expect("git");
        assert!(configured.success(), "git config failed");

        let spec = Spec {
            names: vec!["git".into()],
            args: vec![ArgSpec {
                builtin: Some(Builtin::GitAliases),
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(&spec, &["git".into()], "co", &cwd, false);
        assert_eq!(
            suggestions
                .iter()
                .map(|suggestion| suggestion.name.as_str())
                .collect::<Vec<_>>(),
            vec!["co"]
        );
        assert_eq!(suggestions[0].description, "Alias for 'checkout -b main'");
    }

    #[test]
    fn filepaths_with_query_term_still_list_the_typed_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("main.rs"), "fn").unwrap();
        fs::write(dir.path().join("other.rs"), "x").unwrap();
        let arg = ArgSpec {
            templates: vec![Template::Filepaths],
            meta: SuggestionMeta {
                get_query_term: Some("/".into()),
                ..SuggestionMeta::default()
            },
            ..ArgSpec::default()
        };
        let suggestions = generate_for_arg_with_search_term(
            &arg,
            &["cat".into(), "src/m".into()],
            "m",
            "src/m",
            &dir.path().display().to_string(),
            false,
        );
        assert!(
            suggestions.iter().any(|item| item.name == "src/main.rs"),
            "{suggestions:?}"
        );
        assert!(
            suggestions.iter().all(|item| item.name != "other.rs"),
            "{suggestions:?}"
        );
        let row = suggestions
            .iter()
            .find(|item| item.name == "src/main.rs")
            .expect("prefixed path");
        assert_eq!(
            row.query_term.as_deref(),
            None,
            "prefixed native path rows keep the raw search term for insertion"
        );
    }

    #[test]
    fn quoted_search_term_still_skips_stamping_native_path_rows() {
        assert!(path_row_already_includes_directory_prefix(
            "src/foo/", "folder", "'src/fo", "fo"
        ));
        assert!(path_row_already_includes_directory_prefix(
            "src/foo/",
            "folder",
            r#""src/fo"#,
            "fo"
        ));
        assert!(path_row_already_includes_directory_prefix(
            "src/main.rs",
            "file",
            "src/m",
            "m"
        ));
        assert!(!path_row_already_includes_directory_prefix(
            "file.txt", "file", "dir/fi", "fi"
        ));
    }

    #[test]
    fn filepaths_with_query_term_after_trailing_slash_keep_raw_search() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("main.rs"), "fn").unwrap();
        let arg = ArgSpec {
            templates: vec![Template::Filepaths],
            meta: SuggestionMeta {
                get_query_term: Some("/".into()),
                ..SuggestionMeta::default()
            },
            ..ArgSpec::default()
        };
        let suggestions = generate_for_arg_with_search_term(
            &arg,
            &["cat".into(), "src/".into()],
            "",
            "src/",
            &dir.path().display().to_string(),
            false,
        );
        let row = suggestions
            .iter()
            .find(|item| item.name == "src/main.rs")
            .expect("prefixed path");
        assert_eq!(row.query_term.as_deref(), None, "{suggestions:?}");
    }

    #[test]
    fn filepaths_lookup_uses_parser_inner_text_for_quoted_and_escaped_tokens() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("main.rs"), "fn").unwrap();
        fs::create_dir(dir.path().join("my dir")).unwrap();
        let cwd = dir.path().display().to_string();
        let raw_tokens = [
            ("'src/m", "src/main.rs"),
            (r#""src/m"#, "src/main.rs"),
            ("$'src/m", "src/main.rs"),
            (r"my\ d", "my dir/"),
        ];

        for (raw, expected_name) in raw_tokens {
            // Give each invocation a distinct generator identity so this
            // table tests the filesystem path rather than a prior session
            // result for the same normalized token.
            let arg = ArgSpec {
                name: raw.into(),
                templates: vec![Template::Filepaths],
                meta: SuggestionMeta {
                    get_query_term: Some("/".into()),
                    ..SuggestionMeta::default()
                },
                ..ArgSpec::default()
            };
            let normalized = crate::lookup::parser_inner_text(raw);
            let query = query_term_with_hook(&normalized, Some("/"), None);
            let suggestions =
                generate_for_arg_with_search_term(&arg, &["cat".into(), normalized.clone()], &query, raw, &cwd, false);
            let row = suggestions
                .iter()
                .find(|suggestion| suggestion.name == expected_name)
                .unwrap_or_else(|| panic!("raw={raw:?}, normalized={normalized:?}, rows={suggestions:?}"));
            if expected_name.starts_with("src/") {
                assert_eq!(row.query_term, None, "raw={raw:?}, rows={suggestions:?}");
            }
        }
    }

    #[test]
    fn filepaths_relist_when_the_typed_prefix_changes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("main.rs"), "fn").unwrap();
        fs::write(dir.path().join("other.rs"), "x").unwrap();
        let cwd = dir.path().display().to_string();
        let arg = ArgSpec {
            templates: vec![Template::Filepaths],
            ..ArgSpec::default()
        };
        let first = generate_for_arg_with_search_term(&arg, &["cat".into(), "s".into()], "s", "s", &cwd, false);
        assert!(first.iter().any(|item| item.name == "src/"), "{first:?}");
        let second =
            generate_for_arg_with_search_term(&arg, &["cat".into(), "src/m".into()], "src/m", "src/m", &cwd, false);
        assert!(second.iter().any(|item| item.name == "src/main.rs"), "{second:?}");
        assert!(second.iter().all(|item| item.name != "other.rs"), "{second:?}");
    }

    #[test]
    fn each_generator_keeps_its_own_script_and_templates() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("main.rs"), "fn").unwrap();
        let cwd = dir.path().display().to_string();
        let arg = ArgSpec {
            templates: vec![Template::Filepaths],
            script: vec!["printf".into(), "from-flat\n".into()],
            split_on: Some("\n".into()),
            generators: vec![
                crate::ir::GeneratorSpec {
                    templates: vec![Template::Filepaths],
                    ..crate::ir::GeneratorSpec::default()
                },
                crate::ir::GeneratorSpec {
                    script: vec!["printf".into(), "from-gen\n".into()],
                    split_on: Some("\n".into()),
                    ..crate::ir::GeneratorSpec::default()
                },
            ],
            ..ArgSpec::default()
        };
        let suggestions = generate_for_arg_with_search_term(&arg, &["demo".into(), "".into()], "", "", &cwd, false);
        let names: Vec<_> = suggestions.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names.iter().filter(|name| **name == "src/").count(), 1, "{names:?}");
        assert!(names.contains(&"from-gen"), "{names:?}");
        assert!(!names.contains(&"from-flat"), "{names:?}");
    }

    #[test]
    fn arg_level_templates_still_run_alongside_generators() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        let cwd = dir.path().display().to_string();
        let arg = ArgSpec {
            templates: vec![Template::Filepaths],
            generators: vec![crate::ir::GeneratorSpec {
                script: vec!["printf".into(), "from-gen\n".into()],
                split_on: Some("\n".into()),
                ..crate::ir::GeneratorSpec::default()
            }],
            ..ArgSpec::default()
        };
        let suggestions = generate_for_arg_with_search_term(&arg, &["mount".into(), "".into()], "", "", &cwd, false);
        let names: Vec<_> = suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"from-gen"), "{names:?}");
        assert!(names.contains(&"src/"), "{names:?}");
    }

    #[test]
    fn filepaths_helper_extensions_keep_suffix_matches_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("keep.py"), "").unwrap();
        fs::write(dir.path().join("drop.txt"), "").unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        let cwd = dir.path().display().to_string();
        let arg = ArgSpec {
            meta: SuggestionMeta {
                get_query_term: Some("/".into()),
                ..SuggestionMeta::default()
            },
            generators: vec![crate::ir::GeneratorSpec {
                templates: vec![Template::Filepaths],
                get_query_term: Some("/".into()),
                extensions: vec!["py".into()],
                file_priority: Some(76),
                ..crate::ir::GeneratorSpec::default()
            }],
            ..ArgSpec::default()
        };
        let rows = generate_for_arg_with_search_term(&arg, &["python".into(), "".into()], "", "", &cwd, false);
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert!(names.contains(&"keep.py"), "{names:?}");
        assert!(names.contains(&"src/"), "{names:?}");
        assert!(!names.contains(&"drop.txt"), "{names:?}");
        let py = rows.iter().find(|row| row.name == "keep.py").unwrap();
        assert_eq!(py.priority, 76);
    }

    #[test]
    fn filepaths_helper_matches_keeps_env_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".env"), "").unwrap();
        fs::write(dir.path().join(".env.staging"), "").unwrap();
        fs::write(dir.path().join("keep.py"), "").unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        let cwd = dir.path().display().to_string();
        let arg = ArgSpec {
            meta: SuggestionMeta {
                get_query_term: Some("/".into()),
                ..SuggestionMeta::default()
            },
            generators: vec![crate::ir::GeneratorSpec {
                templates: vec![Template::Filepaths],
                get_query_term: Some("/".into()),
                matches: Some(r"^\.env.*$".into()),
                ..crate::ir::GeneratorSpec::default()
            }],
            ..ArgSpec::default()
        };
        let rows = generate_for_arg_with_search_term(&arg, &["dotenv-vault".into(), "".into()], "", "", &cwd, false);
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert!(names.contains(&".env"), "{names:?}");
        assert!(names.contains(&".env.staging"), "{names:?}");
        assert!(names.contains(&"src/"), "{names:?}");
        assert!(!names.contains(&"keep.py"), "{names:?}");
    }

    #[test]
    fn generated_rows_carry_function_query_term() {
        let arg = ArgSpec {
            js_get_query_term: Some("unused".into()),
            meta: SuggestionMeta {
                get_query_term: Some("/".into()),
                ..SuggestionMeta::default()
            },
            script: vec!["printf".into(), "main\n".into()],
            split_on: Some("\n".into()),
            ..ArgSpec::default()
        };
        let suggestions =
            generate_for_arg_with_search_term(&arg, &["cd".into(), "src/m".into()], "m", "src/m", "/", false);
        let row = suggestions.iter().find(|item| item.name == "main").expect("row");
        assert_eq!(row.query_term.as_deref(), Some("m"));
    }

    #[test]
    fn empty_generator_result_does_not_retrigger_on_the_next_keystroke() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let cwd = dir.path().display().to_string();
        let script = format!("printf x >> '{}'; true", count.display());
        let arg = ArgSpec {
            name: "empty-retrigger".into(),
            script: vec!["sh".into(), "-c".into(), script],
            ..ArgSpec::default()
        };
        let first = generate_for_arg_with_search_term(&arg, &["cmd".into(), "a".into()], "a", "a", &cwd, false);
        let second = generate_for_arg_with_search_term(&arg, &["cmd".into(), "ab".into()], "ab", "ab", &cwd, false);
        assert!(first.is_empty(), "{first:?}");
        assert!(second.is_empty(), "{second:?}");
        assert_eq!(fs::read_to_string(&count).unwrap().matches('x').count(), 1);
    }

    #[test]
    fn generator_arg_id_treats_trailing_space_as_the_same_slot() {
        let arg = ArgSpec {
            name: "pathspec".into(),
            ..ArgSpec::default()
        };
        let after_space = generator_arg_id(&["git".into(), "add".into()], &arg, "/tmp", "");
        let while_typing = generator_arg_id(&["git".into(), "add".into(), "s".into()], &arg, "/tmp", "s");
        let other_command = generator_arg_id(&["git".into(), "rm".into()], &arg, "/tmp", "");
        assert_eq!(after_space, while_typing);
        assert_ne!(after_space, other_command);
    }

    #[test]
    fn trailing_space_keeps_the_same_generator_arg_as_the_typed_token() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let cwd = dir.path().display().to_string();
        let script = format!("printf 'src\\n'; printf x >> '{}'", count.display());
        let arg = ArgSpec {
            name: "trailing-space-arg-id".into(),
            script: vec!["sh".into(), "-c".into(), script],
            split_on: Some("\n".into()),
            ..ArgSpec::default()
        };
        let first = generate_for_arg_with_search_term(&arg, &["git".into(), "add".into()], "", "", &cwd, false);
        let second =
            generate_for_arg_with_search_term(&arg, &["git".into(), "add".into(), "s".into()], "s", "s", &cwd, false);
        assert!(first.iter().any(|row| row.name == "src"), "{first:?}");
        assert!(second.iter().any(|row| row.name == "src"), "{second:?}");
        assert_eq!(fs::read_to_string(&count).unwrap().matches('x').count(), 1);
    }

    #[test]
    fn debounced_generator_with_a_query_term_withholds_stale_rows_while_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().display().to_string();
        let run = |arg: &ArgSpec, tokens: &[&str], term: &str| {
            let tokens: Vec<String> = tokens.iter().map(|token| (*token).to_string()).collect();
            let rows = generate_for_arg_with_search_term(arg, &tokens, term, term, &cwd, false);
            let (pending, _) = take_pending_generators();
            let names: Vec<String> = rows.iter().map(|row| row.name.clone()).collect();
            (names, pending)
        };
        let with_query_term = ArgSpec {
            name: "query-term-debounce".into(),
            script: vec![
                "sh".into(),
                "-c".into(),
                "printf 'alpha\\nalpha\\nbeta\\nbeta\\n'".into(),
            ],
            split_on: Some("\n".into()),
            debounce_ms: Some(200),
            meta: SuggestionMeta {
                get_query_term: Some("/".into()),
                ..SuggestionMeta::default()
            },
            ..ArgSpec::default()
        };
        install_session(GeneratorSession::default());
        // First keystroke: nothing cached yet, generator is debounced.
        let (names, pending) = run(&with_query_term, &["tool"], "");
        assert!(pending);
        assert!(names.is_empty(), "{names:?}");
        // Debounce follow-up: same term, the generator runs.
        let (names, pending) = run(&with_query_term, &["tool"], "");
        assert!(!pending);
        assert_eq!(names, vec!["alpha", "beta"]);
        // A new term re-debounces. Fig hides a loading generator's old rows
        // when it has a `getQueryTerm`, so nothing is shown while waiting.
        let (names, pending) = run(&with_query_term, &["tool", "a"], "a");
        assert!(pending);
        assert!(names.is_empty(), "{names:?}");

        let without_query_term = ArgSpec {
            name: "plain-debounce".into(),
            meta: SuggestionMeta::default(),
            ..with_query_term.clone()
        };
        install_session(GeneratorSession::default());
        let _ = run(&without_query_term, &["tool"], "");
        let (names, _) = run(&without_query_term, &["tool"], "");
        assert_eq!(names, vec!["alpha", "beta"]);
        // Without a query rule the previous rows stay, filtered by the term.
        let (names, pending) = run(&without_query_term, &["tool", "a"], "a");
        assert!(pending);
        assert_eq!(names, vec!["alpha"]);
        install_session(GeneratorSession::default());
    }

    fn slow_script_arg(name: &str) -> ArgSpec {
        ArgSpec {
            name: name.into(),
            script: vec!["sh".into(), "-c".into(), "sleep 0.1; printf 'alpha\\nbeta\\n'".into()],
            split_on: Some("\n".into()),
            ..ArgSpec::default()
        }
    }

    fn run_slot(arg: &ArgSpec, slot: &str, cwd: &str) -> (Vec<String>, (bool, Option<i64>)) {
        let tokens = vec!["tool".to_string(), slot.to_string()];
        let rows = generate_for_arg_with_search_term(arg, &tokens, "", "", cwd, false);
        (
            rows.into_iter().map(|row| row.name).collect(),
            take_pending_generators(),
        )
    }

    #[test]
    fn a_known_slow_generator_waits_for_an_immediate_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().display().to_string();
        let arg = slow_script_arg("slow-follow-up");
        set_defer_slow_generators(true);
        install_session(GeneratorSession::default());
        // Nothing is known about the generator yet, so it runs inline.
        let (names, pending) = run_slot(&arg, "one", &cwd);
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(pending, (false, None));
        // A new slot: the last run was slow, so the rows wait for a follow-up
        // with no delay instead of holding up the rest of the list.
        let (names, pending) = run_slot(&arg, "two", &cwd);
        assert!(names.is_empty(), "{names:?}");
        assert_eq!(pending, (true, Some(0)));
        let (names, pending) = run_slot(&arg, "two", &cwd);
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(pending, (false, None));
        set_defer_slow_generators(false);
        install_session(GeneratorSession::default());
    }

    #[test]
    fn slow_generators_stay_inline_for_callers_without_follow_ups() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().display().to_string();
        let arg = slow_script_arg("slow-inline");
        install_session(GeneratorSession::default());
        for slot in ["one", "two"] {
            let (names, pending) = run_slot(&arg, slot, &cwd);
            assert_eq!(names, vec!["alpha", "beta"], "{slot}");
            assert_eq!(pending, (false, None), "{slot}");
        }
        install_session(GeneratorSession::default());
    }

    #[test]
    fn generator_speed_follows_the_last_run_that_finished() {
        let key = "speed-mark-test";
        note_generator_speed(key.into(), SLOW_GENERATOR, false);
        assert!(generator_is_known_slow(key));
        // Cancelled early: no evidence that the generator got faster.
        note_generator_speed(key.into(), Duration::ZERO, true);
        assert!(generator_is_known_slow(key));
        note_generator_speed(key.into(), Duration::ZERO, false);
        assert!(!generator_is_known_slow(key));
    }

    #[test]
    fn only_generators_that_run_commands_can_be_deferred() {
        let may_defer = |arg: &ArgSpec| generator_may_defer(arg, &effective_generators(arg)[0]);
        let script = ArgSpec {
            script: vec!["true".into()],
            ..ArgSpec::default()
        };
        assert!(may_defer(&script));
        let paths = ArgSpec {
            templates: vec![Template::Filepaths],
            ..script.clone()
        };
        assert!(!may_defer(&paths));
        let history = ArgSpec {
            templates: vec![Template::History],
            ..ArgSpec::default()
        };
        assert!(!may_defer(&history));
    }

    #[test]
    fn different_subcommands_do_not_share_a_generator_session() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let cwd = dir.path().display().to_string();
        let script = format!("printf x >> '{}'; true", count.display());
        let arg = ArgSpec {
            name: "pathspec".into(),
            script: vec!["sh".into(), "-c".into(), script],
            ..ArgSpec::default()
        };
        let _ = generate_for_arg_with_search_term(&arg, &["git".into(), "add".into()], "", "", &cwd, false);
        let _ = generate_for_arg_with_search_term(&arg, &["git".into(), "rm".into()], "", "", &cwd, false);
        assert_eq!(fs::read_to_string(&count).unwrap().matches('x').count(), 2);
    }

    #[test]
    fn history_template_rows_are_arg_rows_in_index_order_filtered_by_query() {
        let arg = ArgSpec {
            templates: vec![Template::History],
            ..ArgSpec::default()
        };
        let values = vec!["feature".to_string(), "main".to_string(), "fix/1".to_string()];
        let rows = generate_for_arg_with_history(&arg, &["git".into(), "checkout".into()], "", "", "/", false, || {
            values.clone()
        });
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["feature", "main", "fix/1"]);
        assert!(rows.iter().all(|row| row.kind == "arg"), "{rows:?}");

        let rows = generate_for_arg_with_history(
            &arg,
            &["git".into(), "checkout".into(), "f".into()],
            "f",
            "f",
            "/",
            false,
            || values.clone(),
        );
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["feature", "fix/1"]);
    }

    #[test]
    fn history_provider_runs_once_for_multiple_consumers_and_never_for_a_cache_hit() {
        install_session(GeneratorSession::default());
        let arg = ArgSpec {
            name: "lazy-history-multiple".into(),
            generators: vec![
                GeneratorSpec {
                    templates: vec![Template::History],
                    ..GeneratorSpec::default()
                },
                GeneratorSpec {
                    templates: vec![Template::History],
                    ..GeneratorSpec::default()
                },
            ],
            ..ArgSpec::default()
        };
        let builds = Cell::new(0);
        let rows = generate_for_arg_with_history(&arg, &["tool".into(), "w".into()], "w", "w", "/", false, || {
            builds.set(builds.get() + 1);
            vec!["west".into(), "web".into(), "api".into()]
        });
        assert_eq!(builds.get(), 1);
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["west", "web"]
        );
        let rows = generate_for_arg_with_history(&arg, &["tool".into(), "we".into()], "we", "we", "/", false, || {
            panic!("cache refilter must not call HistoryStore::arg_values")
        });
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["west", "web"]
        );
        install_session(GeneratorSession::default());
    }

    #[test]
    fn history_provider_waits_for_debounce_and_obeys_trigger_changes() {
        install_session(GeneratorSession::default());
        let arg = ArgSpec {
            name: "lazy-history-debounce".into(),
            templates: vec![Template::History],
            debounce_ms: Some(200),
            ..ArgSpec::default()
        };
        let builds = Cell::new(0);
        let run = |arg: &ArgSpec, term: &str| {
            let rows =
                generate_for_arg_with_history(arg, &["tool".into(), term.into()], term, term, "/", false, || {
                    builds.set(builds.get() + 1);
                    vec!["west".into(), "web".into(), "api".into()]
                });
            (
                rows.into_iter().map(|row| row.name).collect::<Vec<_>>(),
                take_pending_generators().0,
            )
        };
        let (rows, pending) = run(&arg, "w");
        assert!(pending);
        assert!(rows.is_empty());
        assert_eq!(builds.get(), 0);
        let (rows, pending) = run(&arg, "w");
        assert!(!pending);
        assert_eq!(rows, ["west", "web"]);
        assert_eq!(builds.get(), 1);
        let (rows, pending) = run(&arg, "we");
        assert!(pending);
        assert_eq!(rows, ["west", "web"]);
        assert_eq!(builds.get(), 1);
        let (rows, pending) = run(&arg, "we");
        assert!(!pending);
        assert_eq!(rows, ["west", "web"]);
        assert_eq!(builds.get(), 2);

        install_session(GeneratorSession::default());
        let triggered = ArgSpec {
            name: "lazy-history-trigger".into(),
            generators: vec![GeneratorSpec {
                templates: vec![Template::History],
                trigger: Some(GeneratorTrigger {
                    on: "threshold".into(),
                    length: Some(1),
                    string: None,
                    js_trigger: None,
                }),
                ..GeneratorSpec::default()
            }],
            ..ArgSpec::default()
        };
        let _ = run(&triggered, "w");
        assert_eq!(builds.get(), 3);
        let _ = run(&triggered, "we");
        assert_eq!(builds.get(), 4);
        let _ = run(&triggered, "web");
        assert_eq!(builds.get(), 4);
        let static_arg = ArgSpec {
            name: "no-history-consumer".into(),
            ..ArgSpec::default()
        };
        let _ = generate_for_arg_with_history(&static_arg, &["tool".into()], "", "", "/", false, || {
            panic!("no History template has a consumer")
        });
        install_session(GeneratorSession::default());
    }

    #[test]
    fn cancelled_generator_does_not_publish_partial_rows_or_cache_them() {
        install_session(GeneratorSession::default());
        let arg = ArgSpec {
            name: "cancelled-history-provider".into(),
            templates: vec![Template::History],
            suggestions: vec![crate::ir::SuggestionSeed {
                names: vec!["static-before-cancellation".into()],
                ..crate::ir::SuggestionSeed::default()
            }],
            ..ArgSpec::default()
        };
        let token = crate::cancellation::CancellationToken::new();
        let scope = crate::cancellation::enter(token.clone());
        let rows = generate_for_arg_with_history(&arg, &["tool".into()], "", "", "/", false, || {
            assert!(token.cancel());
            vec!["partial-history".into()]
        });
        assert!(rows.is_empty());
        let session = take_session();
        assert!(!session.entries[0].ran);
        assert!(session.entries[0].results.is_empty());
        drop(scope);
        install_session(session);
        let rows = generate_for_arg_with_history(&arg, &["tool".into()], "", "", "/", false, || {
            vec!["fresh-history".into()]
        });
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["static-before-cancellation", "fresh-history"]
        );
        install_session(GeneratorSession::default());
    }

    #[test]
    fn folders_template_lists_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("readme.md"), "x").unwrap();
        let spec = Spec {
            names: vec!["cd".into()],
            args: vec![ArgSpec {
                templates: vec![Template::Folders],
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(
            &spec,
            &["cd".into(), "s".into()],
            "s",
            &dir.path().display().to_string(),
            false,
        );
        assert!(suggestions.iter().any(|s| s.name == "src/"), "{suggestions:?}");
        assert!(suggestions.iter().all(|s| s.kind != "file"));
    }

    #[test]
    fn option_args_are_completed_after_the_flag() {
        let spec = Spec {
            names: vec!["ls".into()],
            options: vec![Arc::new(OptionSpec {
                names: vec!["--color".into()],
                args: vec![ArgSpec {
                    suggestions: vec![
                        SuggestionSeed {
                            names: vec!["always".into()],
                            description: String::new(),
                            ..SuggestionSeed::default()
                        },
                        SuggestionSeed {
                            names: vec!["never".into()],
                            description: String::new(),
                            ..SuggestionSeed::default()
                        },
                    ],
                    ..ArgSpec::default()
                }],
                ..OptionSpec::default()
            })],
            args: vec![ArgSpec {
                templates: vec![Template::Filepaths],
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(&spec, &["ls".into(), "--color".into(), "a".into()], "a", "/", false);
        let names: Vec<_> = suggestions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["always"]);
    }

    #[test]
    fn only_the_active_positional_arg_runs_its_generator() {
        let spec = Spec {
            names: vec!["demo".into()],
            args: vec![
                ArgSpec {
                    suggestions: vec![SuggestionSeed {
                        names: vec!["alpha".into()],
                        ..SuggestionSeed::default()
                    }],
                    ..ArgSpec::default()
                },
                ArgSpec {
                    suggestions: vec![SuggestionSeed {
                        names: vec!["beta".into()],
                        ..SuggestionSeed::default()
                    }],
                    ..ArgSpec::default()
                },
            ],
            ..Spec::default()
        };
        let suggestions = generate(&spec, &["demo".into(), "alpha".into(), "b".into()], "b", "/", false);
        assert_eq!(
            suggestions.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            vec!["beta"]
        );
    }

    #[test]
    fn duplicate_static_and_builtin_rows_are_removed() {
        let spec = Spec {
            names: vec!["demo".into()],
            args: vec![ArgSpec {
                suggestions: vec![
                    SuggestionSeed {
                        names: vec!["same".into()],
                        ..SuggestionSeed::default()
                    },
                    SuggestionSeed {
                        names: vec!["same".into()],
                        ..SuggestionSeed::default()
                    },
                ],
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(&spec, &["demo".into(), "s".into()], "s", "/", false);
        assert_eq!(suggestions.iter().filter(|item| item.name == "same").count(), 1);
    }

    #[test]
    fn git_native_generators_use_their_narrow_data_sources() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().display().to_string();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&cwd)
                .status()
                .expect("git");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "ec@example.invalid"]);
        run(&["config", "user.name", "Easy Complete"]);
        fs::write(dir.path().join("tracked.txt"), "initial").unwrap();
        run(&["add", "tracked.txt"]);
        run(&["commit", "-qm", "initial commit"]);
        run(&["branch", "feature"]);
        run(&["tag", "v1"]);
        run(&["remote", "add", "origin", "https://example.invalid/repo.git"]);
        fs::write(dir.path().join("untracked.txt"), "pending").unwrap();

        let arg = |builtin| ArgSpec {
            builtin: Some(builtin),
            ..ArgSpec::default()
        };
        let branches = generate_for_arg(&arg(Builtin::GitBranches), &["git".into()], "fea", &cwd, false);
        assert!(branches.iter().any(|item| item.name == "feature"), "{branches:?}");
        let tags = generate_for_arg(&arg(Builtin::GitTags), &["git".into()], "v", &cwd, false);
        assert!(tags.iter().any(|item| item.name == "v1"), "{tags:?}");
        let commits = generate_for_arg(&arg(Builtin::GitCommits), &["git".into()], "", &cwd, false);
        assert!(
            commits.iter().any(|item| item.description.contains("initial commit")),
            "{commits:?}"
        );
        let remotes = generate_for_arg(&arg(Builtin::GitRemotes), &["git".into()], "o", &cwd, false);
        assert_eq!(
            remotes.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            vec!["origin"]
        );
        let changed = generate_for_arg(&arg(Builtin::GitChangedFiles), &["git".into()], "u", &cwd, false);
        assert!(changed.iter().any(|item| item.name == "untracked.txt"), "{changed:?}");
    }

    #[test]
    fn post_process_maps_stdout_and_skips_empty_cwd() {
        let native = hook_backend::test_native_hooks(vec![hook_backend::test_typed_entry(
            "demo#postProcess#0",
            "postProcess",
            split_lines_prefix("x-"),
        )]);
        let arg = ArgSpec {
            script: vec!["printf".into(), "alpha\nbeta\n".into()],
            js_post_process: Some("demo#postProcess#0".into()),
            ..ArgSpec::default()
        };
        let rows = with_native(native, "/", || generate_for_arg(&arg, &["demo".into()], "", "/", false));
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert!(names.contains(&"x-alpha"), "{names:?}");
        assert!(names.contains(&"x-beta"), "{names:?}");

        let native = hook_backend::test_native_hooks(vec![hook_backend::test_typed_entry(
            "demo#postProcess#0",
            "postProcess",
            split_lines_prefix("x-"),
        )]);
        let empty = with_native(native, "", || generate_for_arg(&arg, &["demo".into()], "", "", false));
        assert!(empty.is_empty(), "{empty:?}");
    }

    #[test]
    fn split_on_wins_over_post_process_like_the_webview() {
        let native = hook_backend::test_native_hooks(vec![hook_backend::test_typed_entry(
            "demo#postProcess#0",
            "postProcess",
            constant_suggestions(&["from-hook"]),
        )]);
        let arg = ArgSpec {
            script: vec!["printf".into(), "alpha,alpha,beta,,beta,".into()],
            split_on: Some(",".into()),
            js_post_process: Some("demo#postProcess#0".into()),
            ..ArgSpec::default()
        };
        let rows = with_native(native, "/", || generate_for_arg(&arg, &["demo".into()], "", "/", false));
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", ""]);
        let session = take_session();
        assert_eq!(session.entries[0].results.len(), 3);

        let native = hook_backend::test_native_hooks(vec![hook_backend::test_typed_entry(
            "demo#postProcess#0",
            "postProcess",
            constant_suggestions(&["from-hook"]),
        )]);
        let empty_split = ArgSpec {
            split_on: Some(String::new()),
            script: vec!["printf".into(), "alpha,beta".into()],
            js_post_process: Some("demo#postProcess#0".into()),
            ..ArgSpec::default()
        };
        let rows = with_native(native, "/", || {
            generate_for_arg(&empty_split, &["demo-empty".into()], "", "/", false)
        });
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["from-hook"]);
    }

    #[test]
    fn empty_split_on_without_post_process_has_no_script_rows() {
        let script = vec!["printf".into(), "alpha\nbeta\n".into()];
        let rows = run_script(&script, "", "/", false, Duration::from_secs(1), Some(""));
        assert!(rows.is_empty(), "{rows:?}");

        let arg = ArgSpec {
            script,
            split_on: Some(String::new()),
            ..ArgSpec::default()
        };
        let rows = with_session("/", || {
            generate_for_arg(&arg, &["empty-split-no-hook".into()], "", "/", false)
        });
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn post_process_errors_become_empty() {
        let native = hook_backend::test_native_hooks(vec![hook_backend::test_typed_entry(
            "demo#postProcess#0",
            "postProcess",
            json!({"op": "throw", "class": "Error", "message": {"op": "string", "value": "boom"}}),
        )]);
        let arg = ArgSpec {
            script: vec!["printf".into(), "alpha\n".into()],
            js_post_process: Some("demo#postProcess#0".into()),
            ..ArgSpec::default()
        };
        let rows = with_native(native, "/", || generate_for_arg(&arg, &["demo".into()], "", "/", false));
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn custom_context_search_term_is_the_full_parser_token() {
        let native = hook_backend::test_native_hooks(vec![hook_backend::test_typed_entry(
            "demo#custom#0",
            "custom",
            json!({
                "op": "array",
                "items": [
                    {
                        "op": "object",
                        "fields": [
                            {"key": "name", "value": {"op": "string", "value": "foo"}},
                            {"key": "description", "value": {"op": "ctx-search-term"}}
                        ]
                    },
                    {"op": "object", "fields": [{"key": "name", "value": {"op": "string", "value": "bar"}}]}
                ]
            }),
        )]);
        let cwd = tempfile::tempdir().unwrap().path().display().to_string();
        let arg = ArgSpec {
            js_custom: Some("demo#custom#0".into()),
            meta: SuggestionMeta {
                get_query_term: Some("/".into()),
                ..SuggestionMeta::default()
            },
            ..ArgSpec::default()
        };
        let rows = with_native(native, &cwd, || {
            generate_for_arg_with_search_term(&arg, &["demo".into(), "src/foo".into()], "foo", "'src/foo", &cwd, false)
        });
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["foo"]
        );
        assert_eq!(rows[0].description, "src/foo");
    }

    #[test]
    fn custom_generator_receives_parser_inner_text_without_second_unescape() {
        let native = hook_backend::test_native_hooks(vec![hook_backend::test_typed_entry(
            "demo#custom#0",
            "custom",
            json!({
                "op": "array",
                "items": [{
                    "op": "object",
                    "fields": [{"key": "name", "value": {"op": "ctx-search-term"}}]
                }]
            }),
        )]);
        let cwd = tempfile::tempdir().unwrap().path().display().to_string();
        let arg = ArgSpec {
            js_custom: Some("demo#custom#0".into()),
            ..ArgSpec::default()
        };
        let raw = r"$'foo\'bar";
        let normalized = r"foo\'bar";
        let rows = with_native(native, &cwd, || {
            generate_for_arg_with_search_term(&arg, &["demo".into(), normalized.into()], normalized, raw, &cwd, false)
        });
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec![normalized]
        );
    }

    #[test]
    fn custom_and_cache_avoid_repeat_spawns() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().display().to_string();
        let native = hook_backend::test_native_hooks(vec![hook_backend::test_typed_entry(
            "demo#custom#0",
            "custom",
            json!({
                "op": "seq",
                "items": [
                    {
                        "op": "exec",
                        "command": {"op": "string", "value": "sh"},
                        "args": {
                            "op": "array",
                            "items": [
                                {"op": "string", "value": "-c"},
                                {
                                    "op": "string-concat",
                                    "parts": [
                                        {"op": "string", "value": "echo x >> \""},
                                        {"op": "ctx-cwd"},
                                        {"op": "string", "value": "/count\""}
                                    ]
                                }
                            ]
                        },
                        "cwd": {"op": "null"},
                        "env": {"op": "null"},
                        "timeout": {"op": "null"}
                    },
                    {
                        "op": "array",
                        "items": [{
                            "op": "object",
                            "fields": [{"key": "name", "value": {"op": "string", "value": "from-custom"}}]
                        }]
                    }
                ]
            }),
        )]);
        let arg = ArgSpec {
            js_custom: Some("demo#custom#0".into()),
            cache_key: Some("custom".into()),
            cache_ttl_ms: Some(60_000),
            ..ArgSpec::default()
        };
        let first = with_native(native, &cwd, || {
            let first = generate_for_arg(&arg, &["demo".into()], "", &cwd, false);
            let second = generate_for_arg(&arg, &["demo".into()], "", &cwd, false);
            (first, second)
        });
        assert_eq!(
            first.0.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["from-custom"]
        );
        assert_eq!(
            first.1.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["from-custom"]
        );
        let written = fs::read_to_string(dir.path().join("count")).unwrap_or_default();
        assert_eq!(written.matches('x').count(), 1, "{written}");
    }

    #[test]
    fn cached_script_rows_are_refiltered_when_the_query_changes() {
        let dir = tempfile::tempdir().unwrap();
        let native = hook_backend::test_native_hooks(vec![hook_backend::test_typed_entry(
            "demo#postProcess#0",
            "postProcess",
            split_lines_prefix(""),
        )]);
        let count = dir.path().join("count");
        let script = format!("printf 'web\\napi\\nwest\\n'; echo x >> '{}'", count.display());
        let arg = ArgSpec {
            script: vec!["sh".into(), "-c".into(), script],
            js_post_process: Some("demo#postProcess#0".into()),
            cache_key: Some("rows".into()),
            cache_ttl_ms: Some(60_000),
            ..ArgSpec::default()
        };
        let cwd = dir.path().display().to_string();
        let (first, second) = with_native(native, &cwd, || {
            let first = generate_for_arg(&arg, &["demo".into(), "w".into()], "w", &cwd, false);
            let second = generate_for_arg(&arg, &["demo".into(), "we".into()], "we", &cwd, false);
            (first, second)
        });
        assert_eq!(
            first.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["web", "west"]
        );
        assert_eq!(
            second.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["web", "west"]
        );
        let written = fs::read_to_string(&count).unwrap_or_default();
        assert_eq!(written.matches('x').count(), 1, "{written}");
    }

    #[test]
    fn script_cache_without_a_cache_key_does_not_include_the_typed_token() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let script = format!("printf 'web\\napi\\n'; echo x >> '{}'", count.display());
        let arg = ArgSpec {
            script: vec!["sh".into(), "-c".into(), script],
            split_on: Some("\n".into()),
            cache_ttl_ms: Some(60_000),
            ..ArgSpec::default()
        };
        let cwd = dir.path().display().to_string();
        let (first, second) = with_session(&cwd, || {
            let first = generate_for_arg(&arg, &["demo".into(), "w".into()], "w", &cwd, false);
            let second = generate_for_arg(&arg, &["demo".into(), "we".into()], "we", &cwd, false);
            (first, second)
        });
        assert!(first.iter().any(|row| row.name == "web"), "{first:?}");
        assert!(second.iter().any(|row| row.name == "web"), "{second:?}");
        let written = fs::read_to_string(&count).unwrap_or_default();
        assert_eq!(written.matches('x').count(), 1, "{written}");
    }

    #[test]
    fn script_generators_without_a_cache_key_do_not_share_an_entry() {
        let native = hook_backend::test_native_hooks(vec![
            hook_backend::test_typed_entry(
                "kdemo#script#0",
                "script",
                json!({"op": "array", "items": [
                    {"op": "string", "value": "printf"},
                    {"op": "string", "value": "pod-alpha\npod-beta\n"}
                ]}),
            ),
            hook_backend::test_typed_entry(
                "kdemo#script#1",
                "script",
                json!({"op": "array", "items": [
                    {"op": "string", "value": "printf"},
                    {"op": "string", "value": "node-1\nnode-2\n"}
                ]}),
            ),
        ]);
        let pods = ArgSpec {
            js_script: Some("kdemo#script#0".into()),
            split_on: Some("\n".into()),
            cache_ttl_ms: Some(3_600_000),
            cache_strategy: Some("stale-while-revalidate".into()),
            ..ArgSpec::default()
        };
        let nodes = ArgSpec {
            js_script: Some("kdemo#script#1".into()),
            ..pods.clone()
        };
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().display().to_string();
        let (first, second) = with_native(native, &cwd, || {
            let first = generate_for_arg(&pods, &["kdemo".into(), "pods".into()], "", &cwd, false);
            let second = generate_for_arg(&nodes, &["kdemo".into(), "nodes".into()], "", &cwd, false);
            (first, second)
        });
        assert_eq!(
            first.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["pod-alpha", "pod-beta"]
        );
        assert_eq!(
            second.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["node-1", "node-2"]
        );
    }

    #[test]
    fn script_cache_is_keyed_on_the_directory_even_without_cache_by_directory() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let script = format!("pwd; echo x >> '{}'", count.display());
        let arg = ArgSpec {
            script: vec!["sh".into(), "-c".into(), script],
            split_on: Some("\n".into()),
            cache_ttl_ms: Some(60_000),
            ..ArgSpec::default()
        };
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let cwd_a = a.display().to_string();
        let cwd_b = b.display().to_string();
        let (first, second) = with_session(&cwd_a, || {
            let first = generate_for_arg(&arg, &["demo".into()], "", &cwd_a, false);
            let second = generate_for_arg(&arg, &["demo".into()], "", &cwd_b, false);
            (first, second)
        });
        assert!(first.iter().any(|row| row.name.ends_with("/a")), "{first:?}");
        assert!(second.iter().any(|row| row.name.ends_with("/b")), "{second:?}");
        assert_eq!(fs::read_to_string(&count).unwrap_or_default().matches('x').count(), 2);
    }

    #[test]
    fn cached_script_output_is_reshaped_by_post_process_with_the_current_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let native = hook_backend::test_native_hooks(vec![hook_backend::test_typed_entry(
            "demo#postProcess#0",
            "postProcess",
            json!({
                "op": "array-map",
                "value": {
                    "op": "array-filter",
                    "value": {
                        "op": "string-split",
                        "value": {"op": "arg", "index": 0},
                        "separator": {"op": "string", "value": "\n"}
                    },
                    "fn": {
                        "op": "lambda",
                        "params": ["s"],
                        "body": {"op": "truthy", "value": {"op": "var", "name": "s"}}
                    }
                },
                "fn": {
                    "op": "lambda",
                    "params": ["s"],
                    "body": {
                        "op": "object",
                        "fields": [{
                            "key": "name",
                            "value": {
                                "op": "string-concat",
                                "parts": [
                                    {"op": "var", "name": "s"},
                                    {"op": "string", "value": "@"},
                                    {
                                        "op": "array-index",
                                        "value": {"op": "arg", "index": 1},
                                        "index": {
                                            "op": "sub",
                                            "left": {"op": "length", "value": {"op": "arg", "index": 1}},
                                            "right": {"op": "integer", "value": 1}
                                        }
                                    }
                                ]
                            }
                        }]
                    }
                }
            }),
        )]);
        let count = dir.path().join("count");
        let script = format!("printf 'row\\n'; echo x >> '{}'", count.display());
        let arg = ArgSpec {
            script: vec!["sh".into(), "-c".into(), script],
            js_post_process: Some("demo#postProcess#0".into()),
            cache_ttl_ms: Some(60_000),
            ..ArgSpec::default()
        };
        let cwd = dir.path().display().to_string();
        let (first, second) = with_native(native, &cwd, || {
            let first = generate_for_arg(&arg, &["demo".into(), "one".into()], "", &cwd, false);
            let second = generate_for_arg(&arg, &["demo".into(), "two".into()], "", &cwd, false);
            (first, second)
        });
        assert_eq!(
            first.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["row@one"]
        );
        assert_eq!(
            second.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["row@two"]
        );
        assert_eq!(fs::read_to_string(&count).unwrap_or_default().matches('x').count(), 1);
    }

    #[test]
    fn script_without_split_on_or_post_process_yields_no_rows() {
        let arg = ArgSpec {
            script: vec!["printf".into(), "alpha\n".into()],
            ..ArgSpec::default()
        };
        let cwd = tempfile::tempdir().unwrap().path().display().to_string();
        let rows = with_session(&cwd, || generate_for_arg(&arg, &["demo".into()], "", &cwd, false));
        assert!(rows.is_empty(), "{rows:?}");
        let rows = generate_for_arg(&arg, &["demo".into()], "", &cwd, false);
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn scripts_need_a_cwd_like_every_fig_generator() {
        let arg = ArgSpec {
            script: vec!["printf".into(), "alpha\n".into()],
            split_on: Some("\n".into()),
            ..ArgSpec::default()
        };
        let rows = with_session("", || generate_for_arg(&arg, &["demo".into()], "", "", false));
        assert!(rows.is_empty(), "{rows:?}");
        let rows = generate_for_arg(&arg, &["demo".into()], "", "", false);
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn git_branch_generator_omits_remote_head_symbolic_ref() {
        let rows = "main\t\norigin\trefs/remotes/origin/main\norigin/main\t\n";
        let suggestions = branch_lines_to_suggestions(rows, "", false);
        assert_eq!(
            suggestions.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            vec!["main", "origin/main"]
        );
    }
}

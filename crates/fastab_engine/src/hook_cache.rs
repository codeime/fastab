//! Generator result caches that used to live on the QuickJS host.
//!
//! Fig `runCachedGenerator` / `getScriptSuggestions` keying is unchanged: a
//! generator without a `cache` block is uncached unless
//! `beta.autocomplete.auto-cache` is on.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::diagnostics::{CacheMapDiagnostics, HookDiagnostics};
use crate::ir::{ArgSpec, Spec};
use crate::runtime::Suggestion;

/// Ceiling for each per-engine hook cache. Directory-keyed generators mint a
/// new entry per cwd, so a long desktop session would otherwise grow these
/// maps without bound. Wholesale clearing at the cap is fine: entries are
/// cheap to regenerate and the cap is far above one session's working set.
const MAX_CACHE_ENTRIES: usize = 512;
/// Bound payload as well as entry count: script results can each retain 256 KiB.
/// Each of the three maps gets this budget; an oversized result is still served
/// to its caller, but is not retained for another request.
const MAX_CACHE_BYTES: usize = 4 * 1024 * 1024;
/// Generated specs kept through the idle grace. Fig cached each parse,
/// `generateSpec` included, for the whole session; dropping them meant
/// `git help -a` or `cargo --list` ran again on the first keystroke of every
/// command typed after a pause (24 ms and ~100 ms on CI). A map over this
/// estimate is cleared like the generator maps.
const IDLE_SPEC_CACHE_BYTES: usize = 256 * 1024;

thread_local! {
    static CURRENT: std::cell::RefCell<Option<Arc<HookCache>>> = const { std::cell::RefCell::new(None) };
}

#[derive(Clone)]
struct CacheEntry<T> {
    value: T,
    fetched_at: Instant,
}

/// Fig `CacheEntry.entry`: both strategies serve the stored value until its
/// TTL passes, then refetch and hand back the *fresh* result. (`swrCache`
/// only returns the stale value while another fetch is in flight, which a
/// synchronous engine never has.) `None` never expires.
#[derive(Clone, Copy)]
struct CachePolicy {
    ttl: Option<Duration>,
}

/// Avoid leaking cache keys: store the optional key as owned on the policy.
struct OwnedCachePolicy {
    by_directory: bool,
    key: Option<String>,
    ttl: Option<Duration>,
}

#[derive(Default)]
struct CacheCounters {
    hits: u64,
    misses: u64,
    expired_removals: u64,
    capacity_clears: u64,
}

struct CacheMap<T> {
    entries: HashMap<String, T>,
    allocated_bytes: usize,
    counters: CacheCounters,
}

impl<T> Default for CacheMap<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            allocated_bytes: 0,
            counters: CacheCounters::default(),
        }
    }
}

impl<T> CacheMap<T> {
    fn clear(&mut self) {
        self.entries = HashMap::new();
        self.allocated_bytes = 0;
    }

    fn diagnostics(&self) -> CacheMapDiagnostics {
        CacheMapDiagnostics {
            entries: self.entries.len(),
            allocated_bytes: self.allocated_bytes,
            hits: self.counters.hits,
            misses: self.counters.misses,
            expired_removals: self.counters.expired_removals,
            capacity_clears: self.counters.capacity_clears,
        }
    }
}

trait CachePayload {
    fn heap_bytes(&self) -> usize;
}

impl CachePayload for String {
    fn heap_bytes(&self) -> usize {
        self.capacity()
    }
}

impl CachePayload for Vec<Suggestion> {
    fn heap_bytes(&self) -> usize {
        self.capacity() * std::mem::size_of::<Suggestion>() + self.iter().map(suggestion_heap).sum::<usize>()
    }
}

impl CachePayload for Spec {
    fn heap_bytes(&self) -> usize {
        self.allocated_bytes() - std::mem::size_of::<Self>()
    }
}

impl<T: CachePayload> CachePayload for CacheEntry<T> {
    fn heap_bytes(&self) -> usize {
        self.value.heap_bytes()
    }
}

pub(crate) fn suggestion_heap(suggestion: &Suggestion) -> usize {
    suggestion.name.capacity()
        + suggestion.description.capacity()
        + suggestion.kind.capacity()
        + suggestion.args_hint.capacity()
        + suggestion.insert_value.as_ref().map_or(0, String::capacity)
        + suggestion.display_name.as_ref().map_or(0, String::capacity)
        + suggestion.primary_name.as_ref().map_or(0, String::capacity)
        + suggestion.separator_to_add.as_ref().map_or(0, String::capacity)
        + suggestion.icon.as_ref().map_or(0, String::capacity)
        + suggestion.original_type.as_ref().map_or(0, String::capacity)
        + suggestion.query_term.as_ref().map_or(0, String::capacity)
        + suggestion.acceptance_scope.as_ref().map_or(0, String::capacity)
        + suggestion.public_ai_candidate.as_ref().map_or(0, |candidate| {
            candidate.name.capacity() + candidate.description.capacity()
        })
        + suggestion.alias_names.capacity() * std::mem::size_of::<String>()
        + suggestion.alias_names.iter().map(String::capacity).sum::<usize>()
}

impl<T: CachePayload> CacheMap<T> {
    fn entry_bytes(key: &String, value: &T) -> usize {
        std::mem::size_of::<String>() + key.capacity() + std::mem::size_of::<T>() + value.heap_bytes()
    }

    fn remove(&mut self, key: &str) {
        if let Some((key, value)) = self.entries.remove_entry(key) {
            self.allocated_bytes -= Self::entry_bytes(&key, &value);
            if self.entries.is_empty() {
                self.clear();
            }
        }
    }

    fn insert(&mut self, key: String, value: T) {
        // Remove a replaced value before applying either budget. An oversized
        // replacement must not leave its older value available for later hits.
        self.remove(&key);
        let bytes = Self::entry_bytes(&key, &value);
        if bytes > MAX_CACHE_BYTES {
            return;
        }
        if self.entries.len() >= MAX_CACHE_ENTRIES || self.allocated_bytes + bytes > MAX_CACHE_BYTES {
            self.clear();
            self.counters.capacity_clears = self.counters.capacity_clears.saturating_add(1);
        }
        self.allocated_bytes += bytes;
        self.entries.insert(key, value);
    }
}

pub struct HookCache {
    /// `custom` generator results, keyed like Fig's `generatorCache`.
    suggestion_cache: Mutex<CacheMap<CacheEntry<Vec<Suggestion>>>>,
    /// Script generator stdout. Fig caches the `executeCommand` output and
    /// re-applies `splitOn` / `postProcess` on every hit, so the hook still
    /// sees the current tokens; caching rows here would freeze them.
    script_output_cache: Mutex<CacheMap<CacheEntry<String>>>,
    spec_cache: Mutex<CacheMap<Spec>>,
}

impl Default for HookCache {
    fn default() -> Self {
        Self {
            suggestion_cache: Mutex::new(CacheMap::default()),
            script_output_cache: Mutex::new(CacheMap::default()),
            spec_cache: Mutex::new(CacheMap::default()),
        }
    }
}

impl HookCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn bind(self: &Arc<Self>) -> BoundCache {
        CURRENT.with(|cell| {
            let previous = cell.replace(Some(Arc::clone(self)));
            BoundCache { previous }
        })
    }

    pub fn clear(&self) {
        self.suggestion_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clear();
        self.script_output_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clear();
        self.spec_cache.lock().unwrap_or_else(|err| err.into_inner()).clear();
    }

    /// The idle grace ran out: generator results go, and generated specs
    /// stay while they fit [`IDLE_SPEC_CACHE_BYTES`].
    pub(crate) fn release_idle(&self) {
        self.suggestion_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clear();
        self.script_output_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clear();
        let mut specs = self.spec_cache.lock().unwrap_or_else(|err| err.into_inner());
        if specs.allocated_bytes > IDLE_SPEC_CACHE_BYTES {
            specs.clear();
        }
    }

    /// Whether anything here waits on the idle grace. Generated specs that
    /// [`Self::release_idle`] keeps never own a deadline, so a quiet engine
    /// holding only those is not woken.
    pub(crate) fn has_idle_results(&self) -> bool {
        let suggestions_empty = self
            .suggestion_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .entries
            .is_empty();
        let scripts_empty = self
            .script_output_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .entries
            .is_empty();
        let specs_retire = self
            .spec_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .allocated_bytes
            > IDLE_SPEC_CACHE_BYTES;
        !(suggestions_empty && scripts_empty) || specs_retire
    }

    /// Take each map's snapshot under its own lock, without nesting locks or
    /// expiring entries. TTL belongs to the caller's lookup policy.
    pub(crate) fn diagnostics(&self) -> HookDiagnostics {
        let suggestions = self
            .suggestion_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .diagnostics();
        let script_output = self
            .script_output_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .diagnostics();
        let specs = self
            .spec_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .diagnostics();
        HookDiagnostics {
            suggestions,
            script_output,
            specs,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn allocated_bytes(&self) -> usize {
        let snapshot = self.diagnostics();
        snapshot.suggestions.allocated_bytes + snapshot.script_output.allocated_bytes + snapshot.specs.allocated_bytes
    }
}

pub struct BoundCache {
    previous: Option<Arc<HookCache>>,
}

impl Drop for BoundCache {
    fn drop(&mut self) {
        CURRENT.with(|cell| {
            *cell.borrow_mut() = self.previous.take();
        });
    }
}

fn current() -> Option<Arc<HookCache>> {
    CURRENT.with(|cell| cell.borrow().clone())
}

/// Fig `runCachedGenerator`: `[cacheByDirectory ? cwd : undefined, cacheKey ||
/// fallback].toString()`. `fallback` is the joined token array for `custom`
/// generators and the serialized `executeCommand` input for scripts.
pub fn cache_key(cache_by_directory: bool, cwd: &str, cache_key: Option<&str>, fallback: &str) -> String {
    let second = cache_key.filter(|key| !key.is_empty()).unwrap_or(fallback);
    if cache_by_directory {
        format!("{cwd},{second}")
    } else {
        format!(",{second}")
    }
}

/// Fig `getScriptSuggestions` caches on `JSON.stringify(executeCommandInput)`
/// — the resolved command, its args and the cwd — never on the typed tokens,
/// so two generators that compute different commands cannot share an entry
/// and the same command in another directory does not either.
pub fn script_cache_fallback(command: &str, args: &[String], cwd: &str) -> String {
    serde_json::json!({ "command": command, "args": args, "cwd": cwd }).to_string()
}

/// Fig `runCachedGenerator` falls back to `tokenArray.join(" ")` for
/// generators that do not run a script.
pub fn custom_cache_fallback(tokens: &[String]) -> String {
    tokens.join(" ")
}

/// Fig `runCachedGenerator` + `CacheEntry.entry`. A generator without a
/// `cache` block is uncached unless `beta.autocomplete.auto-cache` is on,
/// which gives it `{ strategy: "stale-while-revalidate", ttl: 1000 }`. With
/// a block, the TTL rules follow the JS arithmetic exactly: `max-age` with
/// no `ttl` compares against `NaN` and never expires, while
/// `stale-while-revalidate` (the default) defaults `maxAge` to 0 and
/// refetches on every turn — `cache: { cacheByDirectory: true }` alone is
/// therefore not a cache at all.
fn owned_cache_policy(arg: &ArgSpec) -> Option<OwnedCachePolicy> {
    let has_explicit = arg.cache_key.is_some()
        || arg.cache_by_directory.is_some()
        || arg.cache_ttl_ms.is_some()
        || arg.cache_strategy.is_some();
    if !has_explicit {
        let auto = fastab_settings::settings::get_bool_or("beta.autocomplete.auto-cache", false);
        return auto.then(|| OwnedCachePolicy {
            by_directory: false,
            key: None,
            ttl: Some(Duration::from_millis(1_000)),
        });
    }
    let max_age = arg.cache_strategy.as_deref() == Some("max-age");
    let ttl = match arg.cache_ttl_ms {
        Some(ms) => Some(Duration::from_millis(u64::try_from(ms).unwrap_or(0))),
        None if max_age => None,
        None => Some(Duration::ZERO),
    };
    Some(OwnedCachePolicy {
        by_directory: arg.cache_by_directory.unwrap_or(false),
        key: arg.cache_key.clone(),
        ttl,
    })
}

fn cache_get<T: Clone + CachePayload>(
    cache: &Mutex<CacheMap<CacheEntry<T>>>,
    key: &str,
    policy: CachePolicy,
) -> Option<T> {
    crate::cancellation::commit_if_active(|| {
        let mut cache = cache.lock().unwrap_or_else(|err| err.into_inner());
        let expired = cache
            .entries
            .get(key)
            .is_some_and(|entry| policy.ttl.is_some_and(|ttl| entry.fetched_at.elapsed() > ttl));
        if expired {
            cache.remove(key);
            cache.counters.expired_removals = cache.counters.expired_removals.saturating_add(1);
        }
        let value = cache.entries.get(key).map(|entry| entry.value.clone());
        if value.is_some() {
            cache.counters.hits = cache.counters.hits.saturating_add(1);
        } else {
            cache.counters.misses = cache.counters.misses.saturating_add(1);
        }
        value
    })
    .ok()
    .flatten()
}

fn cache_put<T: CachePayload>(cache: &Mutex<CacheMap<CacheEntry<T>>>, key: String, value: T) {
    let _ = crate::cancellation::commit_if_active(|| {
        let mut cache = cache.lock().unwrap_or_else(|err| err.into_inner());
        cache.insert(
            key,
            CacheEntry {
                value,
                fetched_at: Instant::now(),
            },
        );
    });
}

fn run_cached<T: Clone + Default + CachePayload>(
    cache: &Mutex<CacheMap<CacheEntry<T>>>,
    arg: &ArgSpec,
    cwd: &str,
    kind: &str,
    fallback_key: &str,
    run: impl FnOnce() -> T,
) -> T {
    if crate::cancellation::is_cancelled() {
        return T::default();
    }
    let Some(policy) = owned_cache_policy(arg) else {
        return run();
    };
    let key = format!(
        "{kind}:{}",
        cache_key(policy.by_directory, cwd, policy.key.as_deref(), fallback_key)
    );
    let lookup = CachePolicy { ttl: policy.ttl };
    if let Some(hit) = cache_get(cache, &key, lookup) {
        return hit;
    }
    if crate::cancellation::is_cancelled() {
        return T::default();
    }
    let value = run();
    if crate::cancellation::is_cancelled() {
        return T::default();
    }
    cache_put(cache, key, value.clone());
    value
}

pub fn cached_suggestions(
    arg: &ArgSpec,
    cwd: &str,
    kind: &str,
    fallback_key: &str,
    run: impl FnOnce() -> Vec<Suggestion>,
) -> Vec<Suggestion> {
    if crate::cancellation::is_cancelled() {
        return Vec::new();
    }
    match current() {
        Some(cache) => run_cached(&cache.suggestion_cache, arg, cwd, kind, fallback_key, run),
        None => run(),
    }
}

pub fn cached_script_output(arg: &ArgSpec, cwd: &str, fallback_key: &str, run: impl FnOnce() -> String) -> String {
    if crate::cancellation::is_cancelled() {
        return String::new();
    }
    match current() {
        Some(cache) => run_cached(&cache.script_output_cache, arg, cwd, "script", fallback_key, run),
        None => run(),
    }
}

pub fn cached_spec(cache_key: &str, run: impl FnOnce() -> Option<Spec>) -> Option<Spec> {
    crate::cancellation::check().ok()?;
    let Some(cache) = current() else {
        return run();
    };
    let hit = crate::cancellation::commit_if_active(|| {
        let mut map = cache.spec_cache.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(spec) = map.entries.get(cache_key).cloned() {
            map.counters.hits = map.counters.hits.saturating_add(1);
            return Some(spec);
        }
        map.counters.misses = map.counters.misses.saturating_add(1);
        None
    })
    .ok()?;
    if let Some(spec) = hit {
        return Some(spec);
    }
    crate::cancellation::check().ok()?;
    let value = run()?;
    let cached = value.clone();
    let key = cache_key.to_owned();
    crate::cancellation::commit_if_active(|| {
        let mut map = cache.spec_cache.lock().unwrap_or_else(|err| err.into_inner());
        map.insert(key, cached);
    })
    .ok()?;
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_during_generation_cannot_publish_cache_entries() {
        for kind in ["suggestions", "script", "spec"] {
            let cache = HookCache::new();
            let _bound = cache.bind();
            let token = crate::cancellation::CancellationToken::new();
            let _scope = crate::cancellation::enter(token.clone());
            let arg = ArgSpec {
                cache_strategy: Some("max-age".into()),
                ..ArgSpec::default()
            };
            match kind {
                "suggestions" => assert!(
                    cached_suggestions(&arg, "", "custom", "key", || {
                        token.cancel();
                        vec![Suggestion::new("late", "", "arg")]
                    })
                    .is_empty()
                ),
                "script" => assert!(
                    cached_script_output(&arg, "", "key", || {
                        token.cancel();
                        "late stdout".into()
                    })
                    .is_empty()
                ),
                _ => assert!(
                    cached_spec("key", || {
                        token.cancel();
                        Some(Spec {
                            names: vec!["late".into()],
                            ..Spec::default()
                        })
                    })
                    .is_none()
                ),
            }
            let stats = cache.diagnostics();
            assert_eq!(
                stats.suggestions.entries + stats.script_output.entries + stats.specs.entries,
                0
            );
            assert_eq!(
                stats.suggestions.misses + stats.script_output.misses + stats.specs.misses,
                1
            );
            assert!(cached_spec("another", || panic!("already cancelled")).is_none());
            assert_eq!(cache.diagnostics(), stats);
        }
    }

    #[test]
    fn empty_hook_cache_reports_no_retained_bytes() {
        assert_eq!(HookCache::default().allocated_bytes(), 0);
    }

    #[test]
    fn spec_entry_counts_the_key_and_tree_and_clear_drops_it() {
        let cache = HookCache::default();
        let spec = Spec {
            names: vec!["tool".into()],
            description: "demo".into(),
            ..Spec::default()
        };
        let expected = std::mem::size_of::<String>() + "tool-spec".len() + spec.allocated_bytes();
        cache
            .spec_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert("tool-spec".into(), spec);
        assert_eq!(cache.allocated_bytes(), expected);
        cache.clear();
        assert_eq!(cache.allocated_bytes(), 0);
    }

    #[test]
    fn script_and_suggestion_payloads_are_counted_without_map_nodes() {
        let cache = HookCache::default();
        let aliases = vec!["g".to_owned()];
        assert_eq!(aliases.capacity(), 1);
        let mut suggestion = Suggestion::new("git", "checkout", "subcommand").with_alias_names(aliases);
        suggestion.acceptance_scope = Some("scope".into());
        suggestion.public_ai_candidate = Some(crate::public_ai::PublicAiCandidate {
            name: "public-name".into(),
            description: "public-description".into(),
        });
        let rows = vec![suggestion];
        assert_eq!(rows.capacity(), 1);
        let suggestion_expected = std::mem::size_of::<String>()
            + "rows".len()
            + std::mem::size_of::<CacheEntry<Vec<Suggestion>>>()
            + std::mem::size_of::<Suggestion>()
            + "git".len()
            + "checkout".len()
            + "subcommand".len()
            + "scope".len()
            + "public-name".len()
            + "public-description".len()
            + std::mem::size_of::<String>()
            + "g".len();
        cache_put(&cache.suggestion_cache, "rows".into(), rows);
        let script_expected =
            std::mem::size_of::<String>() + "out".len() + std::mem::size_of::<CacheEntry<String>>() + "stdout".len();
        cache_put(&cache.script_output_cache, "out".into(), "stdout".into());
        assert_eq!(cache.allocated_bytes(), suggestion_expected + script_expected);
    }

    #[test]
    fn hook_cache_entry_cap_stays_at_512() {
        assert_eq!(MAX_CACHE_ENTRIES, 512);
    }

    #[test]
    fn diagnostics_preserve_the_readers_ttl_and_do_not_expire_entries() {
        let cache = HookCache::default();
        cache_put(&cache.script_output_cache, "key".into(), "value".into());
        cache
            .script_output_cache
            .lock()
            .unwrap()
            .entries
            .get_mut("key")
            .unwrap()
            .fetched_at = Instant::now() - Duration::from_secs(10);
        let initial = cache.diagnostics();
        assert_eq!(initial.script_output.entries, 1);
        assert!(initial.script_output.allocated_bytes > 0);
        assert_eq!(initial, cache.diagnostics());
        assert_eq!(
            cache_get(&cache.script_output_cache, "key", CachePolicy { ttl: None }),
            Some("value".into())
        );
        assert_eq!(
            cache_get(
                &cache.script_output_cache,
                "key",
                CachePolicy {
                    ttl: Some(Duration::from_secs(1))
                }
            ),
            None
        );
        assert_eq!(
            cache_get(&cache.script_output_cache, "key", CachePolicy { ttl: None }),
            None
        );
        let after = cache.diagnostics().script_output;
        assert_eq!((after.entries, after.allocated_bytes), (0, 0));
        assert_eq!((after.hits, after.misses, after.expired_removals), (1, 2, 1));
    }

    #[test]
    fn replacement_at_capacity_preserves_other_entries_and_counts_only_overflow() {
        let mut map = CacheMap::default();
        for index in 0..MAX_CACHE_ENTRIES {
            map.insert(index.to_string(), index.to_string());
        }
        map.insert("0".into(), "99".into());
        assert_eq!(map.entries.len(), MAX_CACHE_ENTRIES);
        assert_eq!(map.entries["1"], "1");
        assert_eq!(map.counters.capacity_clears, 0);
        map.insert("overflow".into(), "1".into());
        assert_eq!(map.entries.len(), 1);
        assert_eq!(map.counters.capacity_clears, 1);
        map.clear();
        assert!(map.entries.is_empty());
        assert_eq!(map.entries.capacity(), 0);
        assert_eq!(map.allocated_bytes, 0);
        assert_eq!(map.counters.capacity_clears, 1);
    }

    #[test]
    fn byte_budget_counts_spare_capacity_and_rejects_oversized_replacements() {
        let mut map = CacheMap::default();
        let mut first = String::with_capacity(MAX_CACHE_BYTES / 2);
        first.push('a');
        let expected = CacheMap::<String>::entry_bytes(&"first".to_owned(), &first);
        map.insert("first".into(), first);
        assert_eq!(map.allocated_bytes, expected);
        assert!(map.allocated_bytes > MAX_CACHE_BYTES / 2);
        map.insert("second".into(), String::with_capacity(MAX_CACHE_BYTES / 2));
        assert!(!map.entries.contains_key("first"));
        assert_eq!(map.entries.len(), 1);
        assert_eq!(map.counters.capacity_clears, 1);
        assert!(map.allocated_bytes <= MAX_CACHE_BYTES);
        map.insert("second".into(), String::with_capacity(MAX_CACHE_BYTES));
        assert!(
            map.entries.is_empty(),
            "an oversized replacement cannot leave a stale hit"
        );
        assert_eq!(map.allocated_bytes, 0);
        map.insert("small".into(), "value".into());
        let before = map.allocated_bytes;
        map.insert("small".into(), "v".into());
        assert_eq!(map.allocated_bytes, before - 4);
        map.remove("small");
        assert_eq!(map.allocated_bytes, 0);
    }

    #[test]
    fn all_hook_maps_serve_but_do_not_retain_oversized_payloads() {
        let cache = HookCache::new();
        let _bound = cache.bind();
        let arg = ArgSpec {
            cache_strategy: Some("max-age".into()),
            ..ArgSpec::default()
        };
        let script = cached_script_output(&arg, "", "large", || "x".repeat(MAX_CACHE_BYTES));
        assert_eq!(script.len(), MAX_CACHE_BYTES);
        let rows = cached_suggestions(&arg, "", "custom", "large", || {
            vec![Suggestion::new("row", "x".repeat(MAX_CACHE_BYTES), "arg")]
        });
        assert_eq!(rows[0].description.len(), MAX_CACHE_BYTES);
        let spec = cached_spec("large", || {
            Some(Spec {
                description: "x".repeat(MAX_CACHE_BYTES),
                ..Spec::default()
            })
        })
        .unwrap();
        assert_eq!(spec.description.len(), MAX_CACHE_BYTES);
        assert!(!cache.has_idle_results());
        assert_eq!(cache.allocated_bytes(), 0);
    }

    #[test]
    fn disabled_cache_does_not_lookup_and_callbacks_run_outside_locks() {
        let _settings = fastab_settings::settings::install_override(fastab_settings::settings::Settings::new_fake());
        let cache = HookCache::new();
        let _bound = cache.bind();
        assert_eq!(
            cached_script_output(&ArgSpec::default(), "", "uncached", || "uncached".into()),
            "uncached"
        );
        assert_eq!(cache.diagnostics(), HookDiagnostics::default());
        let arg = ArgSpec {
            cache_strategy: Some("max-age".into()),
            ..ArgSpec::default()
        };
        let rows = cached_suggestions(&arg, "", "custom", "test", || {
            assert!(cache.suggestion_cache.try_lock().is_ok());
            vec![Suggestion::new("row", "description", "arg")]
        });
        assert_eq!(
            cached_suggestions(&arg, "", "custom", "test", || panic!("cache hit")),
            rows
        );
        assert_eq!(
            cached_script_output(&arg, "", "test", || {
                assert!(cache.script_output_cache.try_lock().is_ok());
                "stdout".into()
            }),
            "stdout"
        );
        assert_eq!(cached_script_output(&arg, "", "test", || panic!("cache hit")), "stdout");
        assert!(
            cached_spec("missing", || {
                assert!(cache.spec_cache.try_lock().is_ok());
                None
            })
            .is_none()
        );
        let spec = cached_spec("spec", || {
            Some(Spec {
                names: vec!["tool".into()],
                ..Spec::default()
            })
        })
        .unwrap();
        assert_eq!(cached_spec("spec", || panic!("cache hit")).unwrap(), spec);
        let before = cache.diagnostics();
        assert_eq!((before.suggestions.hits, before.suggestions.misses), (1, 1));
        assert_eq!((before.script_output.hits, before.script_output.misses), (1, 1));
        assert_eq!((before.specs.hits, before.specs.misses), (1, 2));
        cache.clear();
        let after = cache.diagnostics();
        for (before, after) in [
            (before.suggestions, after.suggestions),
            (before.script_output, after.script_output),
            (before.specs, after.specs),
        ] {
            assert_eq!((after.entries, after.allocated_bytes), (0, 0));
            assert_eq!(
                (after.hits, after.misses, after.capacity_clears),
                (before.hits, before.misses, 0)
            );
        }
    }

    #[test]
    fn diagnostic_counters_saturate() {
        let cache = HookCache::default();
        {
            let mut map = cache.script_output_cache.lock().unwrap();
            map.counters = CacheCounters {
                hits: u64::MAX,
                misses: u64::MAX,
                expired_removals: u64::MAX,
                capacity_clears: u64::MAX,
            };
            for index in 0..MAX_CACHE_ENTRIES {
                map.insert(
                    index.to_string(),
                    CacheEntry {
                        value: "value".into(),
                        fetched_at: Instant::now() - Duration::from_secs(10),
                    },
                );
            }
        }
        assert!(cache_get(&cache.script_output_cache, "0", CachePolicy { ttl: None }).is_some());
        cache_put(&cache.script_output_cache, "overflow".into(), "value".into());
        cache
            .script_output_cache
            .lock()
            .unwrap()
            .entries
            .get_mut("overflow")
            .unwrap()
            .fetched_at = Instant::now() - Duration::from_secs(10);
        assert!(
            cache_get(
                &cache.script_output_cache,
                "overflow",
                CachePolicy {
                    ttl: Some(Duration::ZERO)
                }
            )
            .is_none()
        );
        let counts = cache.diagnostics().script_output;
        assert_eq!(
            (
                counts.hits,
                counts.misses,
                counts.expired_removals,
                counts.capacity_clears
            ),
            (u64::MAX, u64::MAX, u64::MAX, u64::MAX)
        );
    }
}

//! Static Fig-spec IR loaded from build-time JSON (no V8).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::snapshot::{DirectorySnapshot, EntryKind};
use crate::versioned::VersionedCommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Builtin {
    GitRefs,
    GitBranches,
    GitTags,
    GitCommits,
    GitRemotes,
    GitChangedFiles,
    GitStashes,
    GitAliases,
    NpmScripts,
    NpmDeps,
    Cobra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Template {
    Filepaths,
    Folders,
    History,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterStrategy {
    Prefix,
    Fuzzy,
    Default,
}

impl FilterStrategy {
    pub fn effective_fuzzy(self, user_fuzzy: bool) -> bool {
        match self {
            Self::Prefix => false,
            Self::Fuzzy => true,
            Self::Default => user_fuzzy,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SuggestionMeta {
    /// Set when a static path `loadSpec` is followed: the file root at read
    /// time, or a subcommand path once the walker enters that token. Skipped
    /// by serde so input JSON cannot grant or erase provenance.
    #[serde(skip)]
    pub ai_resolved_reference: bool,
    /// Runtime-only taint for rows that came from a generateSpec/loadSpec
    /// hook result. Pinned static siblings remain eligible after a mixed
    /// static/dynamic spec is merged.
    #[serde(skip)]
    pub ai_generated: bool,
    /// Explicit Fig suggestion type.  The surrounding collection supplies a
    /// default (`arg`, `subcommand`, or `option`), but static rows may override
    /// it with values such as `file`, `folder`, or `special`.
    #[serde(default, rename = "type", alias = "kind")]
    pub suggestion_type: Option<String>,
    /// Type of the row before a wrapper such as auto-execute changed it.
    #[serde(default, alias = "originalType")]
    pub original_type: Option<String>,
    /// String form of Fig's getQueryTerm.
    #[serde(default, alias = "getQueryTerm")]
    pub get_query_term: Option<String>,
    /// Extracted function-form `getQueryTerm`.
    #[serde(default, alias = "jsGetQueryTerm")]
    pub js_get_query_term: Option<String>,
    /// Explicit text to put in the shell buffer.  Fig calls this `insertValue`.
    #[serde(default, alias = "insertValue")]
    pub insert_value: Option<String>,
    /// Text shown in the list while keeping `insert_value` as the accepted text.
    #[serde(default, alias = "displayName")]
    pub display_name: Option<String>,
    /// Separator to append before the cursor (for example `=` for an option).
    #[serde(default, alias = "separatorToAdd")]
    pub separator_to_add: Option<String>,
    /// Explicitly override the automatic trailing-space heuristic.
    #[serde(default, alias = "shouldAddSpace")]
    pub should_add_space: Option<bool>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub priority: Option<i64>,
    /// Fig icon URI/emoji marker.  The desktop layer may resolve this to an image.
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default, alias = "isDangerous")]
    pub is_dangerous: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SuggestionSeed {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// Preformatted display hint for static suggestion arguments. Dynamic JS
    /// argument objects are intentionally not retained in the native IR.
    #[serde(default, alias = "argsHint")]
    pub args_hint: String,
    #[serde(flatten)]
    pub meta: SuggestionMeta,
}

/// A static reference to another Fig spec.  String references are resolved by
/// `Registry` from the bundled JSON files.  Inline objects are retained for
/// forward compatibility with argument/option `loadSpec` values; node-level
/// inline objects are normally flattened by the compiler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum LoadSpec {
    Path(String),
    Inline(Box<Spec>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArgSpec {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub templates: Vec<Template>,
    #[serde(default)]
    pub script: Vec<String>,
    /// Fig `splitOn` separator. When present the native runner splits script
    /// stdout on this string instead of assuming newlines. A JS `postProcess`
    /// hook, when present, runs instead of this split.
    #[serde(default, alias = "splitOn")]
    pub split_on: Option<String>,
    /// Extracted Fig `postProcess` hook id. NativeHooks looks this up in
    /// `typed-hooks.json` (typed IR or a named adapter).
    #[serde(default, alias = "jsPostProcess")]
    pub js_post_process: Option<String>,
    /// Extracted Fig `custom` generator hook id.
    #[serde(default, alias = "jsCustom")]
    pub js_custom: Option<String>,
    /// Extracted function-form `script` hook id. Returns argv / a command object.
    #[serde(default, alias = "jsScript")]
    pub js_script: Option<String>,
    /// Fig generator `cache.cacheKey`. Combined with tokens/cwd in the Rust SWR cache.
    #[serde(default, alias = "cacheKey")]
    pub cache_key: Option<String>,
    #[serde(default, alias = "cacheByDirectory")]
    pub cache_by_directory: Option<bool>,
    #[serde(default, alias = "cacheTtl")]
    pub cache_ttl_ms: Option<i64>,
    /// Generator-level `scriptTimeout`, in milliseconds.  The compiler keeps
    /// this alongside the static script/builtin it selected so the native
    /// runner can preserve Fig's max(default, generator, command) rule.
    #[serde(default, alias = "scriptTimeout")]
    pub script_timeout_ms: Option<i64>,
    #[serde(default)]
    pub builtin: Option<Builtin>,
    /// Some Fig arguments combine several generators (for example checkout
    /// offers branches, tags and paths).  Keep every native generator instead
    /// of silently replacing all but the last one during compilation.
    #[serde(default)]
    pub builtins: Vec<Builtin>,
    #[serde(default)]
    pub suggestions: Vec<SuggestionSeed>,
    #[serde(flatten)]
    pub meta: SuggestionMeta,
    #[serde(default, alias = "loadSpec")]
    pub load_spec: Option<LoadSpec>,
    /// Resolved static argument spec.  Keep the source `load_spec` in the
    /// deserialized IR (so dynamic/unsupported forms remain observable), but
    /// expose a native-ready tree for the lookup state machine without
    /// serializing a second copy back into the bundle.
    #[serde(skip)]
    pub resolved_spec: Option<Box<Spec>>,
    #[serde(default, alias = "isOptional")]
    pub is_optional: bool,
    #[serde(default, alias = "isVariadic")]
    pub is_variadic: bool,
    #[serde(default, alias = "filterStrategy")]
    pub filter_strategy: Option<FilterStrategy>,
    /// Per-argument override for the global always-suggest-current-token
    /// setting. `None` means inherit the setting; `Some(false)` is explicit.
    #[serde(default, rename = "suggestCurrentToken", alias = "suggest_current_token")]
    pub suggest_current_token: Option<bool>,
    /// When a variadic argument has already consumed a value, this controls
    /// whether a following option may start a new option context.
    #[serde(default, alias = "optionsCanBreakVariadicArg")]
    pub options_can_break_variadic_arg: Option<bool>,
    /// Completed token loads that command's spec from the registry, matching
    /// Fig `isCommand`. While the token is still being typed, Fig does not
    /// list bundled command names — only the argument's own generators.
    #[serde(default, alias = "isCommand")]
    pub is_command: bool,
    /// Like `is_command`, but path tokens resolve by basename (Fig `isScript`).
    #[serde(default, alias = "isScript")]
    pub is_script: bool,
    /// Prefix concatenated with the token to form a global spec name
    /// (`python/` + `http` → `python/http`).
    #[serde(default, alias = "isModule")]
    pub is_module: Option<String>,
    #[serde(default, alias = "jsLoadSpec")]
    pub js_load_spec: Option<String>,
    #[serde(default, alias = "jsGetQueryTerm")]
    pub js_get_query_term: Option<String>,
    #[serde(default, alias = "debounceMs")]
    pub debounce_ms: Option<i64>,
    #[serde(default, alias = "parserDirectives")]
    pub parser_directives: Option<ParserDirectives>,
    #[serde(default, alias = "cacheStrategy")]
    pub cache_strategy: Option<String>,
    /// Per-generator Fig metadata (trigger, templates, hooks). Flattened
    /// script/builtin/template fields remain the execution snapshot.
    #[serde(default)]
    pub generators: Vec<GeneratorSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GeneratorSpec {
    #[serde(default)]
    pub templates: Vec<Template>,
    #[serde(default)]
    pub script: Vec<String>,
    #[serde(default, alias = "splitOn")]
    pub split_on: Option<String>,
    #[serde(default, alias = "jsPostProcess")]
    pub js_post_process: Option<String>,
    #[serde(default, alias = "jsCustom")]
    pub js_custom: Option<String>,
    #[serde(default, alias = "jsScript")]
    pub js_script: Option<String>,
    #[serde(default, alias = "cacheKey")]
    pub cache_key: Option<String>,
    #[serde(default, alias = "cacheByDirectory")]
    pub cache_by_directory: Option<bool>,
    #[serde(default, alias = "cacheTtl")]
    pub cache_ttl_ms: Option<i64>,
    #[serde(default, alias = "cacheStrategy")]
    pub cache_strategy: Option<String>,
    #[serde(default, alias = "scriptTimeout")]
    pub script_timeout_ms: Option<i64>,
    #[serde(default)]
    pub builtin: Option<Builtin>,
    #[serde(default, alias = "getQueryTerm")]
    pub get_query_term: Option<String>,
    #[serde(default, alias = "jsGetQueryTerm")]
    pub js_get_query_term: Option<String>,
    #[serde(default, alias = "jsFilterTemplateSuggestions")]
    pub js_filter_template_suggestions: Option<String>,
    #[serde(default)]
    pub trigger: Option<GeneratorTrigger>,
    /// Fig `filepaths({ extensions })` — suffix match from the right.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Fig `filepaths({ equals })` — exact file/folder names that always pass.
    #[serde(default)]
    pub equals: Vec<String>,
    /// Fig `filepaths({ showFolders })`: `only` / `never`. Omitted is `always`.
    #[serde(default, alias = "showFolders")]
    pub show_folders: Option<String>,
    /// Fig `filepaths({ filterFolders })`: folders must also match extensions.
    #[serde(default, alias = "filterFolders")]
    pub filter_folders: Option<bool>,
    #[serde(default, alias = "filePriority")]
    pub file_priority: Option<i64>,
    #[serde(default, alias = "folderPriority")]
    pub folder_priority: Option<i64>,
    /// Fig `filepaths({ rootDirectory })`: listing base instead of cwd.
    #[serde(default, alias = "rootDirectory")]
    pub root_directory: Option<String>,
    /// Fig `filepaths({ matches })` regex source, without the `/…/` delimiters.
    #[serde(default)]
    pub matches: Option<String>,
    #[serde(default, alias = "matchesFlags")]
    pub matches_flags: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GeneratorTrigger {
    #[serde(default)]
    pub on: String,
    #[serde(default)]
    pub string: Option<serde_json::Value>,
    #[serde(default)]
    pub length: Option<i64>,
    #[serde(default, alias = "jsTrigger")]
    pub js_trigger: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParserDirectives {
    /// If true, options stop being consumable after a positional argument has
    /// been entered in the current completion object.
    #[serde(default, alias = "optionsMustPrecedeArguments")]
    pub options_must_precede_arguments: Option<bool>,
    /// Treat a single-dash token such as `-foo` as a long option rather than a
    /// POSIX short-option chain.
    #[serde(default, alias = "flagsArePosixNoncompliant")]
    pub flags_are_posix_noncompliant: Option<bool>,
    /// Separators used by attached option values (`--foo:value`). An explicit
    /// empty array intentionally disables attached separators.
    #[serde(default, alias = "optionArgSeparators")]
    pub option_arg_separators: Option<Vec<String>>,
    /// Spec-level token rewrite used by Fig `parserDirectives.alias`.
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default, alias = "jsAlias")]
    pub js_alias: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OptionSpec {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub args: Vec<ArgSpec>,
    #[serde(flatten)]
    pub meta: SuggestionMeta,
    #[serde(default, alias = "loadSpec")]
    pub load_spec: Option<LoadSpec>,
    /// A boolean `requiresSeparator` uses the command's default separator (`=`)
    /// while a string value is preserved by the compiler in `separator_to_add`.
    #[serde(default, alias = "requiresSeparator")]
    pub requires_separator: Option<serde_json::Value>,
    #[serde(default, alias = "requiresEquals")]
    pub requires_equals: bool,
    #[serde(default, alias = "requiresSubcommand")]
    pub requires_subcommand: Option<bool>,
    /// Options that become unavailable after this option is passed. Values
    /// use the same alias spellings as Fig's `exclusiveOn` field.
    #[serde(default, alias = "exclusiveOn")]
    pub exclusive_on: Vec<String>,
    /// Options whose rows should be promoted while one of these dependencies
    /// is still unmet. The WebView uses priority 75 for these rows.
    #[serde(default, alias = "dependsOn")]
    pub depends_on: Vec<String>,
    /// `false`/omitted means once, `true` means unlimited, and a number is the
    /// maximum number of times the option may be passed.
    #[serde(default, alias = "isRepeatable")]
    pub is_repeatable: Option<serde_json::Value>,
    /// Persistent options are copied into child subcommands by Fig's parser.
    /// The compiler keeps this marker on the option while `Spec` stores the
    /// current effective persistent set separately.
    #[serde(default, alias = "isPersistent")]
    pub is_persistent: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Spec {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// Each child is its own `Arc`. A descent that does not rewrite the node
    /// clones this handle. The bytes of a shared child are counted once.
    /// Identical children are not interned; only options go through that pool.
    #[serde(default)]
    pub subcommands: Vec<Arc<Spec>>,
    /// Shared within a file and across specs loaded by one [`Registry`].
    /// `persistentOptions` stays a separate list: lookup merges that list
    /// and treats `options` as the node's own flags.
    #[serde(
        default,
        deserialize_with = "deserialize_shared_options",
        serialize_with = "serialize_shared_options"
    )]
    pub options: Vec<Arc<OptionSpec>>,
    /// Effective persistent options for this node. For a lazy `loadSpec`, the
    /// lookup walker merges the parent set into this set as it descends.
    #[serde(
        default,
        alias = "persistentOptions",
        deserialize_with = "deserialize_shared_options",
        serialize_with = "serialize_shared_options"
    )]
    pub persistent_options: Vec<Arc<OptionSpec>>,
    #[serde(default)]
    pub args: Vec<ArgSpec>,
    #[serde(default, alias = "additionalSuggestions")]
    pub additional_suggestions: Vec<SuggestionSeed>,
    #[serde(flatten)]
    pub meta: SuggestionMeta,
    #[serde(default, alias = "loadSpec")]
    pub load_spec: Option<LoadSpec>,
    #[serde(default, alias = "requiresSubcommand")]
    pub requires_subcommand: Option<bool>,
    #[serde(default, alias = "filterStrategy")]
    pub filter_strategy: Option<FilterStrategy>,
    #[serde(default, alias = "parserDirectives")]
    pub parser_directives: Option<ParserDirectives>,
    /// Extracted Fig `generateSpec` hook id. Walk merges the returned tree
    /// into this node, keeping the wrapper names.
    #[serde(default, alias = "jsGenerateSpec")]
    pub js_generate_spec: Option<String>,
    #[serde(default, alias = "generateSpecCacheKey")]
    pub generate_spec_cache_key: Option<String>,
    #[serde(default, alias = "jsLoadSpec")]
    pub js_load_spec: Option<String>,
}

impl Spec {
    pub fn has_name(&self, name: &str) -> bool {
        self.names.iter().any(|candidate| candidate == name)
    }

    pub fn find_subcommand(&self, name: &str) -> Option<&Arc<Spec>> {
        self.subcommands.iter().find(|spec| spec.has_name(name))
    }

    /// Drop the spare capacity serde leaves on every `Vec` in this tree.
    ///
    /// JSON arrays have no length prefix, so deserialize doubles storage and
    /// a loaded spec keeps that slack until the LRU drops it. Element order
    /// and values stay put; only `capacity` changes.
    pub(crate) fn shrink_to_fit(&mut self) {
        self.names.shrink_to_fit();
        shrink_vec(&mut self.subcommands);
        shrink_vec(&mut self.options);
        shrink_vec(&mut self.persistent_options);
        shrink_vec(&mut self.args);
        shrink_vec(&mut self.additional_suggestions);
        if let Some(LoadSpec::Inline(spec)) = &mut self.load_spec {
            spec.shrink_to_fit();
        }
        if let Some(directives) = &mut self.parser_directives {
            directives.shrink_to_fit();
        }
    }

    /// Bytes retained by this tree: the struct itself, vector buffers, string
    /// payloads, and each distinct [`OptionSpec`] once. Shared options are not
    /// counted again. String spare capacity and allocator headers are left
    /// out so the figure does not depend on the allocator. Hook caches use
    /// this estimate for their payload budget; it is not a total heap limit.
    pub fn allocated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.heap_bytes(
                &mut std::collections::HashSet::new(),
                &mut std::collections::HashSet::new(),
            )
    }

    fn heap_bytes(
        &self,
        seen_options: &mut std::collections::HashSet<usize>,
        seen_specs: &mut std::collections::HashSet<usize>,
    ) -> usize {
        string_vec_heap(&self.names)
            + string_heap(&self.description)
            + self.subcommands.capacity() * std::mem::size_of::<Arc<Spec>>()
            + shared_spec_bodies(&self.subcommands, seen_options, seen_specs)
            + shared_option_slots(&self.options)
            + shared_option_bodies(&self.options, seen_options, seen_specs)
            + shared_option_slots(&self.persistent_options)
            + shared_option_bodies(&self.persistent_options, seen_options, seen_specs)
            + self.args.capacity() * std::mem::size_of::<ArgSpec>()
            + self
                .args
                .iter()
                .map(|arg| arg.heap_bytes(seen_options, seen_specs))
                .sum::<usize>()
            + self.additional_suggestions.capacity() * std::mem::size_of::<SuggestionSeed>()
            + self
                .additional_suggestions
                .iter()
                .map(SuggestionSeed::heap_bytes)
                .sum::<usize>()
            + self.meta.heap_bytes()
            + load_spec_heap(&self.load_spec, seen_options, seen_specs)
            + self.parser_directives.as_ref().map_or(0, ParserDirectives::heap_bytes)
            + opt_string_heap(&self.js_generate_spec)
            + opt_string_heap(&self.generate_spec_cache_key)
            + opt_string_heap(&self.js_load_spec)
    }
}

// A slice hides `capacity`, which is the spare allocation this measurement exists to see.
#[allow(clippy::ptr_arg)]
fn shared_option_slots(options: &Vec<Arc<OptionSpec>>) -> usize {
    options.capacity() * std::mem::size_of::<Arc<OptionSpec>>()
}

fn shared_option_bodies(
    options: &[Arc<OptionSpec>],
    seen_options: &mut std::collections::HashSet<usize>,
    seen_specs: &mut std::collections::HashSet<usize>,
) -> usize {
    let mut total = 0;
    for option in options {
        if !seen_options.insert(Arc::as_ptr(option) as usize) {
            continue;
        }
        total += std::mem::size_of::<OptionSpec>() + option.heap_bytes(seen_options, seen_specs);
    }
    total
}

fn shared_spec_bodies(
    specs: &[Arc<Spec>],
    seen_options: &mut std::collections::HashSet<usize>,
    seen_specs: &mut std::collections::HashSet<usize>,
) -> usize {
    let mut total = 0;
    for spec in specs {
        if !seen_specs.insert(Arc::as_ptr(spec) as usize) {
            continue;
        }
        total += std::mem::size_of::<Spec>() + spec.heap_bytes(seen_options, seen_specs);
    }
    total
}

fn load_spec_heap(
    load_spec: &Option<LoadSpec>,
    seen_options: &mut std::collections::HashSet<usize>,
    seen_specs: &mut std::collections::HashSet<usize>,
) -> usize {
    match load_spec {
        Some(LoadSpec::Path(path)) => path.len(),
        Some(LoadSpec::Inline(spec)) => std::mem::size_of::<Spec>() + spec.heap_bytes(seen_options, seen_specs),
        None => 0,
    }
}

fn shrink_vec<T: ShrinkSpecTree>(items: &mut Vec<T>) {
    items.shrink_to_fit();
    for item in items {
        item.shrink_tree();
    }
}

trait ShrinkSpecTree {
    fn shrink_tree(&mut self);
}

impl ShrinkSpecTree for Spec {
    fn shrink_tree(&mut self) {
        self.shrink_to_fit();
    }
}

impl ShrinkSpecTree for OptionSpec {
    fn shrink_tree(&mut self) {
        self.names.shrink_to_fit();
        self.exclusive_on.shrink_to_fit();
        self.depends_on.shrink_to_fit();
        shrink_vec(&mut self.args);
        if let Some(LoadSpec::Inline(spec)) = &mut self.load_spec {
            spec.shrink_to_fit();
        }
    }
}

impl ShrinkSpecTree for Arc<OptionSpec> {
    fn shrink_tree(&mut self) {
        if let Some(option) = Arc::get_mut(self) {
            option.shrink_tree();
        }
    }
}

impl ShrinkSpecTree for Arc<Spec> {
    fn shrink_tree(&mut self) {
        if let Some(spec) = Arc::get_mut(self) {
            spec.shrink_to_fit();
        }
    }
}

impl OptionSpec {
    fn heap_bytes(
        &self,
        seen_options: &mut std::collections::HashSet<usize>,
        seen_specs: &mut std::collections::HashSet<usize>,
    ) -> usize {
        string_vec_heap(&self.names)
            + string_heap(&self.description)
            + self.args.capacity() * std::mem::size_of::<ArgSpec>()
            + self
                .args
                .iter()
                .map(|arg| arg.heap_bytes(seen_options, seen_specs))
                .sum::<usize>()
            + self.meta.heap_bytes()
            + load_spec_heap(&self.load_spec, seen_options, seen_specs)
            + self.requires_separator.as_ref().map_or(0, json_heap)
            + string_vec_heap(&self.exclusive_on)
            + string_vec_heap(&self.depends_on)
            + self.is_repeatable.as_ref().map_or(0, json_heap)
    }
}

fn deserialize_shared_options<'de, D>(deserializer: D) -> Result<Vec<Arc<OptionSpec>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let options = Vec::<OptionSpec>::deserialize(deserializer)?;
    Ok(options.into_iter().map(Arc::new).collect())
}

fn serialize_shared_options<S>(options: &[Arc<OptionSpec>], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.collect_seq(options.iter().map(Arc::as_ref))
}

#[derive(Default)]
struct OptionHashWriter(std::collections::hash_map::DefaultHasher);

impl OptionHashWriter {
    fn finish(self) -> u64 {
        self.0.finish()
    }
}

impl std::io::Write for OptionHashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        // Hash the byte stream, not each chunk's length-prefixed Hash value.
        self.0.write(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OptionHashProfileMode {
    LegacyVec,
    StreamedNoMemo,
    StreamedMemo,
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct OptionHashProfile {
    mode: OptionHashProfileMode,
    calls: usize,
}

#[cfg(test)]
thread_local! {
    // Only ignored profiling/tests opt in. No switches or counters are
    // compiled into the production interning path.
    static OPTION_HASH_PROFILE: std::cell::Cell<Option<OptionHashProfile>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn legacy_option_hash(option: &OptionSpec) -> u64 {
    let bytes = serde_json::to_vec(option).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&bytes, &mut hasher);
    hasher.finish()
}

fn hash_option(option: &OptionSpec) -> u64 {
    #[cfg(test)]
    if OPTION_HASH_PROFILE.with(|slot| {
        let Some(mut profile) = slot.get() else {
            return false;
        };
        profile.calls = profile.calls.saturating_add(1);
        slot.set(Some(profile));
        profile.mode == OptionHashProfileMode::LegacyVec
    }) {
        return legacy_option_hash(option);
    }

    let mut writer = OptionHashWriter::default();
    if serde_json::to_writer(&mut writer, option).is_err() {
        // Do not fingerprint a partially serialized value. Equality still
        // protects this fallback bucket, as it did for an empty JSON Vec.
        return OptionHashWriter::default().finish();
    }
    writer.finish()
}

#[cfg(test)]
fn intern_one_option(option: Arc<OptionSpec>, pool: &mut HashMap<u64, Vec<Weak<OptionSpec>>>) -> Arc<OptionSpec> {
    let hash = hash_option(&option);
    intern_one_option_hashed(option, pool, hash)
}

fn intern_one_option_hashed(
    option: Arc<OptionSpec>,
    pool: &mut HashMap<u64, Vec<Weak<OptionSpec>>>,
    hash: u64,
) -> Arc<OptionSpec> {
    let bucket = pool.entry(hash).or_default();
    bucket.retain(|weak| weak.strong_count() > 0);
    if let Some(existing) = bucket
        .iter()
        .filter_map(Weak::upgrade)
        .find(|existing| existing.as_ref() == option.as_ref())
    {
        return existing;
    }
    bucket.push(Arc::downgrade(&option));
    option
}

// Lives only for one registry interning traversal. Weak ownership both avoids
// retaining replaced bodies and verifies identity before accepting a hit.
type OptionFingerprintMemo = HashMap<usize, (Weak<OptionSpec>, u64)>;

fn memoized_option_hash(option: &Arc<OptionSpec>, memo: &mut Option<OptionFingerprintMemo>) -> u64 {
    let Some(memo) = memo else {
        return hash_option(option);
    };
    let key = Arc::as_ptr(option) as usize;
    if let Some((weak, hash)) = memo.get(&key)
        && weak.upgrade().is_some_and(|cached| Arc::ptr_eq(&cached, option))
    {
        return *hash;
    }
    let hash = hash_option(option);
    memo.insert(key, (Arc::downgrade(option), hash));
    hash
}

fn intern_arg_options(
    arg: &mut ArgSpec,
    pool: &mut HashMap<u64, Vec<Weak<OptionSpec>>>,
    memo: &mut Option<OptionFingerprintMemo>,
) {
    if let Some(spec) = &mut arg.resolved_spec {
        intern_spec_options_with_memo(spec, pool, memo);
    }
    if let Some(LoadSpec::Inline(spec)) = &mut arg.load_spec {
        intern_spec_options_with_memo(spec, pool, memo);
    }
}

fn intern_option_children(
    option: &mut Arc<OptionSpec>,
    pool: &mut HashMap<u64, Vec<Weak<OptionSpec>>>,
    memo: &mut Option<OptionFingerprintMemo>,
) {
    let Some(option) = Arc::get_mut(option) else {
        return;
    };
    for arg in &mut option.args {
        intern_arg_options(arg, pool, memo);
    }
    if let Some(LoadSpec::Inline(spec)) = &mut option.load_spec {
        intern_spec_options_with_memo(spec, pool, memo);
    }
}

fn intern_option_list(
    options: &mut Vec<Arc<OptionSpec>>,
    pool: &mut HashMap<u64, Vec<Weak<OptionSpec>>>,
    memo: &mut Option<OptionFingerprintMemo>,
) {
    for option in options {
        intern_option_children(option, pool, memo);
        let hash = memoized_option_hash(option, memo);
        *option = intern_one_option_hashed(Arc::clone(option), pool, hash);
    }
}

/// Share byte-identical options inside `spec`. The initial parse has not yet
/// shared option bodies, so do not allocate a fingerprint memo for this pass.
fn intern_spec_options_local(spec: &mut Spec) {
    let mut pool = HashMap::new();
    intern_spec_options_with_memo(spec, &mut pool, &mut None);
}

fn intern_spec_options(spec: &mut Spec, pool: &mut HashMap<u64, Vec<Weak<OptionSpec>>>) {
    #[cfg(test)]
    let memoize = OPTION_HASH_PROFILE.with(|slot| {
        slot.get()
            .is_none_or(|profile| profile.mode == OptionHashProfileMode::StreamedMemo)
    });
    #[cfg(not(test))]
    let memoize = true;
    let mut memo = memoize.then(HashMap::new);
    intern_spec_options_with_memo(spec, pool, &mut memo);
}

fn intern_spec_options_with_memo(
    spec: &mut Spec,
    pool: &mut HashMap<u64, Vec<Weak<OptionSpec>>>,
    memo: &mut Option<OptionFingerprintMemo>,
) {
    for child in &mut spec.subcommands {
        intern_spec_options_with_memo(Arc::make_mut(child), pool, memo);
    }
    for arg in &mut spec.args {
        intern_arg_options(arg, pool, memo);
    }
    if let Some(LoadSpec::Inline(inner)) = &mut spec.load_spec {
        intern_spec_options_with_memo(inner, pool, memo);
    }
    intern_option_list(&mut spec.options, pool, memo);
    intern_option_list(&mut spec.persistent_options, pool, memo);
}

impl ShrinkSpecTree for ArgSpec {
    fn shrink_tree(&mut self) {
        self.templates.shrink_to_fit();
        self.script.shrink_to_fit();
        self.builtins.shrink_to_fit();
        shrink_vec(&mut self.suggestions);
        shrink_vec(&mut self.generators);
        if let Some(LoadSpec::Inline(spec)) = &mut self.load_spec {
            spec.shrink_to_fit();
        }
        if let Some(spec) = &mut self.resolved_spec {
            spec.shrink_to_fit();
        }
        if let Some(directives) = &mut self.parser_directives {
            directives.shrink_to_fit();
        }
    }
}

impl ArgSpec {
    fn heap_bytes(
        &self,
        seen_options: &mut std::collections::HashSet<usize>,
        seen_specs: &mut std::collections::HashSet<usize>,
    ) -> usize {
        string_heap(&self.name)
            + string_heap(&self.description)
            + self.templates.capacity() * std::mem::size_of::<Template>()
            + string_vec_heap(&self.script)
            + opt_string_heap(&self.split_on)
            + opt_string_heap(&self.js_post_process)
            + opt_string_heap(&self.js_custom)
            + opt_string_heap(&self.js_script)
            + opt_string_heap(&self.cache_key)
            + opt_string_heap(&self.cache_strategy)
            + self.builtins.capacity() * std::mem::size_of::<Builtin>()
            + self.suggestions.capacity() * std::mem::size_of::<SuggestionSeed>()
            + self.suggestions.iter().map(SuggestionSeed::heap_bytes).sum::<usize>()
            + self.meta.heap_bytes()
            + load_spec_heap(&self.load_spec, seen_options, seen_specs)
            + self.resolved_spec.as_ref().map_or(0, |spec| {
                std::mem::size_of::<Spec>() + spec.heap_bytes(seen_options, seen_specs)
            })
            + opt_string_heap(&self.is_module)
            + opt_string_heap(&self.js_load_spec)
            + opt_string_heap(&self.js_get_query_term)
            + self.parser_directives.as_ref().map_or(0, ParserDirectives::heap_bytes)
            + self.generators.capacity() * std::mem::size_of::<GeneratorSpec>()
            + self.generators.iter().map(GeneratorSpec::heap_bytes).sum::<usize>()
    }
}

impl ShrinkSpecTree for SuggestionSeed {
    fn shrink_tree(&mut self) {
        self.names.shrink_to_fit();
    }
}

impl SuggestionSeed {
    fn heap_bytes(&self) -> usize {
        string_vec_heap(&self.names)
            + string_heap(&self.description)
            + string_heap(&self.args_hint)
            + self.meta.heap_bytes()
    }
}

impl ShrinkSpecTree for GeneratorSpec {
    fn shrink_tree(&mut self) {
        self.templates.shrink_to_fit();
        self.script.shrink_to_fit();
        self.extensions.shrink_to_fit();
        self.equals.shrink_to_fit();
    }
}

impl GeneratorSpec {
    fn heap_bytes(&self) -> usize {
        self.templates.capacity() * std::mem::size_of::<Template>()
            + string_vec_heap(&self.script)
            + opt_string_heap(&self.split_on)
            + opt_string_heap(&self.js_post_process)
            + opt_string_heap(&self.js_custom)
            + opt_string_heap(&self.js_script)
            + opt_string_heap(&self.cache_key)
            + opt_string_heap(&self.cache_strategy)
            + opt_string_heap(&self.get_query_term)
            + opt_string_heap(&self.js_get_query_term)
            + opt_string_heap(&self.js_filter_template_suggestions)
            + string_vec_heap(&self.extensions)
            + string_vec_heap(&self.equals)
            + opt_string_heap(&self.show_folders)
            + opt_string_heap(&self.root_directory)
            + opt_string_heap(&self.matches)
            + opt_string_heap(&self.matches_flags)
            + self.trigger.as_ref().map_or(0, GeneratorTrigger::heap_bytes)
    }
}

impl GeneratorTrigger {
    fn heap_bytes(&self) -> usize {
        self.on.len() + self.string.as_ref().map_or(0, json_heap) + opt_string_heap(&self.js_trigger)
    }
}

impl ParserDirectives {
    fn shrink_to_fit(&mut self) {
        if let Some(separators) = &mut self.option_arg_separators {
            separators.shrink_to_fit();
        }
    }

    fn heap_bytes(&self) -> usize {
        opt_string_heap(&self.alias)
            + opt_string_heap(&self.js_alias)
            + self.option_arg_separators.as_ref().map_or(0, string_vec_heap)
    }
}

fn string_heap(value: &str) -> usize {
    value.len()
}

// A slice hides `capacity`, which is the spare allocation this measurement exists to see.
#[allow(clippy::ptr_arg)]
fn string_vec_heap(values: &Vec<String>) -> usize {
    values.capacity() * std::mem::size_of::<String>() + values.iter().map(String::len).sum::<usize>()
}

fn json_heap(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
        serde_json::Value::String(text) => text.len(),
        serde_json::Value::Array(items) => {
            items.capacity() * std::mem::size_of::<serde_json::Value>() + items.iter().map(json_heap).sum::<usize>()
        },
        serde_json::Value::Object(entries) => {
            entries.len() * (std::mem::size_of::<String>() + std::mem::size_of::<serde_json::Value>())
                + entries
                    .iter()
                    .map(|(key, value)| key.len() + json_heap(value))
                    .sum::<usize>()
        },
    }
}

impl SuggestionMeta {
    fn heap_bytes(&self) -> usize {
        opt_string_heap(&self.suggestion_type)
            + opt_string_heap(&self.original_type)
            + opt_string_heap(&self.get_query_term)
            + opt_string_heap(&self.js_get_query_term)
            + opt_string_heap(&self.insert_value)
            + opt_string_heap(&self.display_name)
            + opt_string_heap(&self.separator_to_add)
            + opt_string_heap(&self.icon)
    }
}

fn opt_string_heap(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, String::len)
}

#[derive(Debug, Default, Clone)]
pub struct Registry {
    /// Cached release-pin decision, evaluated only for an AI opt-in request.
    public_ai_baseline: Option<bool>,
    /// Programmatic insertion has no verified source even in a pinned tree.
    public_ai_inserted: bool,
    specs: HashMap<String, Arc<Spec>>,
    /// The captured generation against which both the index and lazy spec
    /// files are checked. Cloning a registry clones this handle, not an
    /// unchecked canonical path.
    snapshot: Option<DirectorySnapshot>,
    files: HashMap<Arc<str>, PathBuf>,
    /// Root directory is retained so a node's `loadSpec: "foo/bar"` can be
    /// resolved without adding every implementation path to command names.
    root: PathBuf,
    /// New compiler output supplies command/alias mappings in index.json.  In
    /// that mode nested implementation files stay private; old indexes keep
    /// the historical relative-path fallback in `index_dir`.
    has_command_file_map: bool,
    /// Case-insensitive sorted command names; `Arc` shares the `files` keys.
    names: Vec<Arc<str>>,
    /// LRU of loaded spec trees (one entry per file, not per alias).
    loaded: VecDeque<Arc<Spec>>,
    /// Specs overlaid from a user directory. They live outside `loaded` so
    /// the LRU can never evict one and fall back to the bundled file it was
    /// meant to replace — that fallback was silent, and for a command the
    /// bundle does not know it left no completion at all.
    pinned: Vec<Arc<Spec>>,
    /// `index.json` `versioned` map: command → selector + version files.
    versioned: HashMap<String, VersionedCommand>,
    /// Per-session CLI versions (`None` = detection failed). Keyed by command.
    version_cache: HashMap<String, Option<String>>,
    /// Specs loaded by relative IR path for versioned selection.
    path_specs: HashMap<PathBuf, Arc<Spec>>,
    /// `loadSpec` files entered during a walk, keyed by relative IR path.
    /// Each `Arc` also lives in `loaded`, so it takes one of the 48 LRU slots
    /// instead of a second cache. The parent spec keeps its stub.
    load_spec_cache: HashMap<PathBuf, Arc<Spec>>,
    /// Identical options loaded by this registry share one `Arc`. Entries are
    /// `Weak`, so evicting the last spec that used an option drops the body.
    option_pool: HashMap<u64, Vec<Weak<OptionSpec>>>,
    /// Relative IR path → when the edit-buffer walk last stopped using this
    /// cached file. `None` means a buffer walk still holds it. Absent means
    /// this registry has not been asked to track the file. Specs inserted
    /// without a path never appear here.
    idle_since: HashMap<PathBuf, Option<Instant>>,
    /// History indexing sets this so a side-loaded file is not recorded as
    /// the edit buffer still using it. A separate allocation: the guard
    /// restores it while `build_index` holds `&mut Registry`, and a cloned
    /// registry must not share that flag. `Engine` moves across threads.
    suppress_idle_touch: IdleTouchFlag,
    /// Paths returned to this completion's edit-buffer walk. Applied once
    /// after the walk, so a grace already running is not restarted.
    idle_touched: HashSet<PathBuf>,
}

/// How [`Registry::overlay_specs_dir`] treats a name the bundle already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayMode {
    /// Fig `devCompletionsFolder`: tried before the public spec, so it wins.
    Replace,
    /// Fig `~/.fig/autocomplete/build`: consulted only when
    /// `publicSpecExists(name)` is false, so a bundled name keeps its spec.
    FillMissing,
}

const MAX_CACHED_SPECS: usize = 48;
const MAX_NAME_MATCHES: usize = 50;

/// How long a parsed spec stays after the edit buffer stops using it.
pub(crate) const SPEC_IDLE_GRACE: Duration = Duration::from_secs(10);

/// Paths and marks only: an attempt checkpoint must never pin parsed trees.
// Preserve all three registry states: unmarked, active, and an idle timestamp.
#[allow(clippy::option_option)]
pub(crate) struct IdleCheckpoint(HashMap<PathBuf, Option<Option<Instant>>>);

struct UnlinkedSpec {
    /// `specs` or `load_spec_cache` lost this `Arc`. A path-only `loadSpec`
    /// lives only in the latter and still counts as one LRU file.
    lru_slot: bool,
    /// `path_specs` lost this `Arc`. Idle release counts that as a drop.
    /// LRU eviction still keys success off `lru_slot` only.
    path_specs: bool,
}

/// Per-registry idle-touch switch. Cloning a registry starts recording again
/// instead of sharing the flag the original's history walk may be holding.
#[derive(Debug)]
struct IdleTouchFlag(Arc<AtomicBool>);

impl Default for IdleTouchFlag {
    fn default() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
}

impl Clone for IdleTouchFlag {
    fn clone(&self) -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
}

/// Restores [`Registry::suppress_idle_touch`] when the history walk returns,
/// including by unwind. The flag lives outside the registry allocation so the
/// walk can hold `&mut Registry` for the whole build.
pub(crate) struct IdleTouchPause {
    flag: Arc<AtomicBool>,
    previous: bool,
}

impl Drop for IdleTouchPause {
    fn drop(&mut self) {
        self.flag.store(self.previous, AtomicOrdering::Relaxed);
    }
}

/// Max-heap by ignore-ASCII-case so we can keep the 50 alphabetically first fuzzy hits.
struct AlphaMax<'a>(&'a str);

impl PartialEq for AlphaMax<'_> {
    fn eq(&self, other: &Self) -> bool {
        crate::query::cmp_ignore_ascii_case(self.0, other.0) == Ordering::Equal
    }
}

impl Eq for AlphaMax<'_> {}

impl PartialOrd for AlphaMax<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AlphaMax<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        crate::query::cmp_ignore_ascii_case(self.0, other.0)
    }
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, spec: Spec) {
        self.public_ai_inserted = true;
        self.insert_loaded(spec, None);
    }

    pub(crate) fn is_public_ai_root(&mut self, name: &str, root: &Arc<Spec>) -> bool {
        if self.public_ai_inserted
            || std::env::var_os("EC_SPECS_DIR").is_some()
            || self.versioned.contains_key(name)
            || self.pinned.iter().any(|spec| Arc::ptr_eq(spec, root))
            || !self.specs.get(name).is_some_and(|spec| Arc::ptr_eq(spec, root))
        {
            return false;
        }
        let Some(snapshot) = self.snapshot.as_ref() else {
            return false;
        };
        if snapshot.is_stale() || snapshot.generation_changed() {
            return false;
        }
        *self
            .public_ai_baseline
            .get_or_insert_with(|| crate::spec_pair::matches_public_ai_baseline(snapshot))
    }

    #[cfg(test)]
    pub(crate) fn trust_public_ai_fixture_for_test(&mut self) {
        self.public_ai_baseline = Some(true);
    }

    fn insert_loaded(&mut self, mut spec: Spec, path: Option<&Path>) {
        spec.shrink_to_fit();
        intern_spec_options(&mut spec, &mut self.option_pool);
        let spec = Arc::new(spec);
        for name in &spec.names {
            if name.is_empty() || self.is_pinned_name(name) {
                continue;
            }
            if let Some(path) = path {
                if self.has_command_file_map && !self.files.contains_key(name.as_str()) {
                    continue;
                }
                if self
                    .files
                    .get(name.as_str())
                    .is_some_and(|existing| existing.as_path() != path)
                {
                    continue;
                }
            }
            self.specs.insert(name.clone(), spec.clone());
            self.remember_name(Arc::<str>::from(name.as_str()));
        }
        self.loaded.push_back(spec);
    }

    fn is_pinned_name(&self, name: &str) -> bool {
        self.specs
            .get(name)
            .is_some_and(|current| self.pinned.iter().any(|pinned| Arc::ptr_eq(pinned, current)))
    }

    fn remember_name(&mut self, name: Arc<str>) {
        if name.is_empty() {
            return;
        }
        let idx = self
            .names
            .partition_point(|existing| crate::query::cmp_ignore_ascii_case(existing, &name).is_lt());
        if self
            .names
            .get(idx)
            .is_some_and(|existing| existing.eq_ignore_ascii_case(&name))
        {
            return;
        }
        self.names.insert(idx, name);
    }

    fn remember_file(&mut self, name: String, path: PathBuf) {
        let name: Arc<str> = name.into();
        if let std::collections::hash_map::Entry::Vacant(entry) = self.files.entry(name.clone()) {
            entry.insert(path);
            self.remember_name(name);
        }
    }

    fn rebuild_names(&mut self) {
        let mut names: Vec<Arc<str>> = self.files.keys().cloned().collect();
        names.extend(
            self.pinned
                .iter()
                .flat_map(|spec| spec.names.iter())
                .filter(|name| !name.is_empty())
                .map(|name| Arc::<str>::from(name.as_str())),
        );
        names.sort_by(|a, b| crate::query::cmp_ignore_ascii_case(a, b));
        names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        self.names = names;
    }

    fn touch_loaded(&mut self, spec: &Arc<Spec>) {
        let Some(pos) = self.loaded.iter().position(|cached| Arc::ptr_eq(cached, spec)) else {
            return;
        };
        if pos + 1 == self.loaded.len() {
            return;
        }
        let cached = self.loaded.remove(pos).expect("index from position()");
        self.loaded.push_back(cached);
    }

    fn evict_oldest_spec(&mut self) {
        while let Some(old) = self.loaded.pop_front() {
            // One pop drops one cached file. A path-only `loadSpec` is not in
            // `specs`; it still occupies a slot and must count as the eviction.
            // Entries in neither map are leftovers and keep the scan going.
            let unlinked = self.unlink_cached_arc(&old);
            self.forget_unowned_idle_paths();
            if unlinked.lru_slot {
                return;
            }
        }
    }

    /// Remove `old` from every cache that might own it. `loaded` is included
    /// so a caller that still has the `Arc` (LRU pop, idle release) does not
    /// leave a second handle behind. The caller's own `Arc` stays alive.
    fn unlink_cached_arc(&mut self, old: &Arc<Spec>) -> UnlinkedSpec {
        let before_specs = self.specs.len();
        self.specs.retain(|_, cached| !Arc::ptr_eq(cached, old));
        let before_paths = self.load_spec_cache.len();
        self.load_spec_cache.retain(|_, cached| !Arc::ptr_eq(cached, old));
        let before_versioned = self.path_specs.len();
        self.path_specs.retain(|_, cached| !Arc::ptr_eq(cached, old));
        self.loaded.retain(|cached| !Arc::ptr_eq(cached, old));
        UnlinkedSpec {
            lru_slot: self.specs.len() < before_specs || self.load_spec_cache.len() < before_paths,
            path_specs: self.path_specs.len() < before_versioned,
        }
    }

    fn forget_unowned_idle_paths(&mut self) {
        // Run after unlinking: another cache may still own a different tree
        // for the same file, which must keep its original grace clock.
        let stale: Vec<PathBuf> = self
            .idle_since
            .keys()
            .filter(|path| self.releasable_arcs_for_path(path).next().is_none())
            .cloned()
            .collect();
        for path in stale {
            self.idle_since.remove(&path);
        }
    }

    /// A path can own distinct ordinary and versioned trees. Inspect every
    /// cached alias as well: a pinned alias must not hide a bundled sibling.
    /// This borrowed iterator may yield the same tree through several aliases.
    fn releasable_arcs_for_path<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = &'a Arc<Spec>> + 'a {
        self.load_spec_cache
            .get(path)
            .into_iter()
            .chain(self.path_specs.get(path))
            .chain(self.specs.iter().filter_map(move |(name, spec)| {
                self.files
                    .get(name.as_str())
                    .is_some_and(|existing| existing.as_path() == path)
                    .then_some(spec)
            }))
            .filter(move |spec| !self.pinned.iter().any(|pinned| Arc::ptr_eq(pinned, spec)))
    }

    /// Record whether `relative` is waiting to be released. `None` clears a
    /// pending release without deleting the parsed tree. Paths that do not
    /// parse are ignored. Specs inserted with no file path are not tracked.
    #[cfg(test)]
    pub(crate) fn set_idle_since(&mut self, relative: &str, since: Option<Instant>) {
        let Some(path) = idle_path(relative) else {
            return;
        };
        self.idle_since.insert(path, since);
    }

    /// Drop parsed trees whose idle mark is at least `grace` old.
    ///
    /// The temporary `Arc` is dropped before [`Self::prune_dead_options`], so
    /// option bodies owned only by the released tree leave the pool. Pinned
    /// overlays are left in place, even when their path is past `grace`.
    /// `version_cache` is not touched.
    pub(crate) fn release_idle(&mut self, now: Instant, grace: Duration) {
        let expired: Vec<PathBuf> = self
            .idle_since
            .iter()
            .filter_map(|(path, since)| {
                let since = (*since)?;
                (now.saturating_duration_since(since) >= grace).then(|| path.clone())
            })
            .collect();
        let mut released = false;
        for path in expired {
            let mut trees = Vec::new();
            for spec in self.releasable_arcs_for_path(&path) {
                if !trees.iter().any(|tree| Arc::ptr_eq(tree, spec)) {
                    trees.push(Arc::clone(spec));
                }
            }
            for spec in trees {
                let unlinked = self.unlink_cached_arc(&spec);
                released |= unlinked.lru_slot || unlinked.path_specs;
            }
            // Every non-pinned tree for this path has now been unlinked and
            // the temporary handles dropped before pruning the option pool.
            self.idle_since.remove(&path);
        }
        if released {
            self.prune_dead_options();
        }
    }

    fn cached_file_paths(&self) -> HashSet<PathBuf> {
        self.load_spec_cache
            .keys()
            .chain(self.path_specs.keys())
            .cloned()
            .chain(
                self.specs
                    .keys()
                    .filter_map(|name| self.files.get(name.as_str()).cloned()),
            )
            .filter(|path| self.releasable_arcs_for_path(path).next().is_some())
            .collect()
    }

    pub(crate) fn checkpoint_idle(&self) -> IdleCheckpoint {
        IdleCheckpoint(
            self.cached_file_paths()
                .into_iter()
                .map(|path| {
                    let mark = self.idle_since.get(&path).copied();
                    (path, mark)
                })
                .collect(),
        )
    }

    /// Preserve earlier input ownership and grace clocks, but give files loaded
    /// by a cancelled or history-only completion their own release deadline.
    pub(crate) fn restore_idle_checkpoint(&mut self, checkpoint: &IdleCheckpoint, now: Instant) {
        self.forget_unowned_idle_paths();
        for path in self.cached_file_paths() {
            match checkpoint.0.get(&path) {
                Some(Some(mark)) => {
                    self.idle_since.insert(path, *mark);
                },
                Some(None) => {
                    self.idle_since.remove(&path);
                },
                None => {
                    self.idle_since.insert(path, Some(now));
                },
            }
        }
        self.idle_touched.clear();
    }

    pub(crate) fn diagnostics(&self, now: Instant, grace: Duration) -> crate::diagnostics::RegistryDiagnostics {
        let paths = self.cached_file_paths();
        let mut seen_specs = HashSet::new();
        let mut seen_options = HashSet::new();
        let allocated_bytes = self
            .specs
            .values()
            .chain(self.loaded.iter())
            .chain(self.pinned.iter())
            .chain(self.path_specs.values())
            .chain(self.load_spec_cache.values())
            .map(|spec| {
                if seen_specs.insert(Arc::as_ptr(spec) as usize) {
                    std::mem::size_of::<Spec>() + spec.heap_bytes(&mut seen_options, &mut seen_specs)
                } else {
                    0
                }
            })
            .sum();
        crate::diagnostics::RegistryDiagnostics {
            cached_file_count: paths.len(),
            idle_file_count: paths
                .iter()
                .filter(|path| matches!(self.idle_since.get(*path), Some(Some(_))))
                .count(),
            allocated_bytes,
            next_deadline_ms: self
                .next_idle_deadline(grace)
                .map(|deadline| u64::try_from(deadline.saturating_duration_since(now).as_millis()).unwrap_or(u64::MAX)),
        }
    }

    /// Drop touches recorded by an earlier request. A history-only completion
    /// never reaches [`Self::note_idle_after_complete`], so this is what keeps
    /// its loads from counting as the next edit-buffer walk.
    pub(crate) fn begin_idle_completion(&mut self) {
        self.idle_touched.clear();
    }

    /// History indexing calls this around `annotate_history_command`.
    pub(crate) fn pause_idle_touch(&self) -> IdleTouchPause {
        IdleTouchPause {
            flag: Arc::clone(&self.suppress_idle_touch.0),
            previous: self.suppress_idle_touch.0.swap(true, AtomicOrdering::Relaxed),
        }
    }

    fn note_idle_touch_path(&mut self, path: &Path) {
        if self.suppress_idle_touch.0.load(AtomicOrdering::Relaxed) {
            return;
        }
        self.idle_touched.insert(path.to_path_buf());
    }

    /// Apply this completion's touched set.
    ///
    /// A path the edit buffer returned is marked in use (`None`). A cached
    /// path it did not return starts the grace once; a grace already running
    /// keeps its original timestamp. Pinned overlays are not given a deadline.
    pub(crate) fn note_idle_after_complete(&mut self, now: Instant) {
        let mut paths = HashSet::new();
        paths.extend(self.load_spec_cache.keys().cloned());
        paths.extend(self.path_specs.keys().cloned());
        for name in self.specs.keys() {
            if let Some(path) = self.files.get(name.as_str()) {
                paths.insert(path.clone());
            }
        }
        self.forget_unowned_idle_paths();
        for path in paths {
            if self.releasable_arcs_for_path(&path).next().is_none() {
                self.idle_since.remove(&path);
                continue;
            }
            if self.idle_touched.contains(&path) {
                self.idle_since.insert(path, None);
                continue;
            }
            if matches!(self.idle_since.get(&path), Some(Some(_))) {
                continue;
            }
            self.idle_since.insert(path, Some(now));
        }
        self.idle_touched.clear();
    }

    /// Earliest moment a pending file may be released. In-use marks and pinned
    /// overlays are not deadlines. Neither are paths with no cached tree left.
    pub(crate) fn next_idle_deadline(&self, grace: Duration) -> Option<Instant> {
        self.idle_since
            .iter()
            .filter_map(|(path, since)| {
                let since = (*since)?;
                self.releasable_arcs_for_path(path).next()?;
                since.checked_add(grace)
            })
            .min()
    }

    #[cfg(test)]
    pub(crate) fn idle_mark(&self, relative: &str) -> Option<Option<Instant>> {
        let path = idle_path(relative)?;
        self.idle_since.get(&path).copied()
    }

    fn prune_dead_options(&mut self) {
        self.option_pool.retain(|_, bucket| {
            bucket.retain(|option| option.strong_count() != 0);
            !bucket.is_empty()
        });
    }

    fn ensure_loaded(&mut self, name: &str) {
        if let Some(spec) = self.specs.get(name).cloned() {
            self.touch_loaded(&spec);
            if !self.is_pinned_name(name)
                && let Some(path) = self.files.get(name).cloned()
            {
                self.note_idle_touch_path(&path);
            }
            return;
        }
        let Some(path) = self.files.get(name).cloned() else {
            return;
        };
        let loaded = if let Some(snapshot) = self.snapshot.as_ref() {
            load_snapshot_file(snapshot, &path, &self.files, &mut Vec::new())
        } else {
            let root = self.root.clone();
            load_spec_file(&path, &root, &self.files, &mut Vec::new())
        };
        match loaded {
            Ok(mut spec) => {
                if self.loaded.len() >= MAX_CACHED_SPECS {
                    self.evict_oldest_spec();
                    // The eviction's temporary Arc must be dropped before
                    // pruning. Also collect options whose external owners
                    // were released after an earlier eviction.
                    self.prune_dead_options();
                }
                if !spec.names.iter().any(|candidate| candidate == name) {
                    spec.names.push(name.to_string());
                    spec.names.shrink_to_fit();
                }
                if !self.has_command_file_map {
                    for alias in &spec.names {
                        if !alias.is_empty() {
                            self.remember_file(alias.clone(), path.clone());
                        }
                    }
                }
                self.insert_loaded(spec, Some(&path));
                self.note_idle_touch_path(&path);
            },
            Err(error) => {
                // A missing/different generation must not look like an
                // ordinary absent command. The snapshot marks itself stale
                // for I/O or digest failures; the next Engine request will
                // attempt an atomic Registry+NativeHooks rebuild.
                tracing::warn!(command = %name, path = %path.display(), %error, "spec lazy load failed");
            },
        }
    }

    pub fn get(&mut self, name: &str) -> Option<&Spec> {
        self.ensure_loaded(name);
        self.specs.get(name).map(Arc::as_ref)
    }

    /// Same lookup as [`Self::get`], but the caller can keep the spec after
    /// the next mutable registry operation (for example an `isCommand` switch).
    pub fn get_arc(&mut self, name: &str) -> Option<Arc<Spec>> {
        self.ensure_loaded(name);
        self.specs.get(name).cloned()
    }

    pub fn versioned_command(&self, name: &str) -> Option<&VersionedCommand> {
        self.versioned.get(name)
    }

    /// Session-cached versioned root spec. Detection failures load the
    /// highest file (with that file's diffs applied), matching WebView.
    pub fn get_versioned_arc(&mut self, name: &str, cwd: &str, timeout: std::time::Duration) -> Option<Arc<Spec>> {
        let entry = self.versioned.get(name)?.clone();
        crate::cancellation::check().ok()?;
        let detected = if let Some(cached) = self.version_cache.get(name) {
            cached.clone()
        } else {
            let detected = crate::versioned::detect_cli_version(&entry, cwd, timeout);
            // Cancellation is not a cached detection failure. A later request
            // must still probe the real CLI version instead of choosing fallback.
            crate::cancellation::commit_if_active(|| {
                self.version_cache.insert(name.to_string(), detected.clone());
            })
            .ok()?;
            detected
        };
        let relative = crate::versioned::resolve_versioned_path(&entry, detected.as_deref())?;
        self.load_relative_spec(&relative, name)
    }

    /// The spec a walked subcommand's `loadSpec` path points at.
    ///
    /// Command-map paths share that command's LRU entry. Nested files such as
    /// `gcloud/compute.json` take a slot of their own. Either way the parsed
    /// tree is not written back onto the parent stub.
    pub(crate) fn load_referenced_spec(&mut self, reference: &str) -> Option<Arc<Spec>> {
        let reference = reference.trim().trim_start_matches("./");
        if reference.is_empty() || reference.contains('\\') {
            return None;
        }
        // Same resolution as `resolve_reference_path`: a command-map key wins
        // over `reference.json`. `heroku` is `heroku/8.6.0.json`, not
        // `heroku.json`.
        let (command_name, relative_path) = if let Some((name, path)) = self.files.get_key_value(reference) {
            (Some(name.to_string()), path.clone())
        } else {
            let relative = if reference.ends_with(".json") {
                reference.to_string()
            } else {
                format!("{reference}.json")
            };
            let relative_path = safe_relative_path(&relative)?;
            let command_name = self
                .files
                .iter()
                .find(|(_, path)| path.as_path() == relative_path.as_path())
                .map(|(name, _)| name.to_string());
            (command_name, relative_path)
        };
        if let Some(spec) = self.load_spec_cache.get(&relative_path).cloned() {
            self.touch_loaded(&spec);
            self.note_idle_touch_path(&relative_path);
            return Some(spec);
        }
        // Share the command LRU entry when it is the file this path names.
        // A dev-folder overlay replaces `specs[name]` but leaves `files`
        // pointing at the bundle, so a pinned name must load the referenced
        // file into the path cache instead of returning the overlay.
        if let Some(name) = command_name {
            if !self.is_pinned_name(&name) {
                if let Some(spec) = self.get_arc(&name) {
                    self.note_idle_touch_path(&relative_path);
                    return Some(spec);
                }
            }
        }

        let loaded = if let Some(snapshot) = self.snapshot.as_ref() {
            if !snapshot.is_file(&relative_path) {
                tracing::warn!(path = %relative_path.display(), "loadSpec target missing");
                return None;
            }
            load_snapshot_file(snapshot, &relative_path, &self.files, &mut Vec::new())
        } else {
            let Some(path) = relative_path
                .to_str()
                .and_then(|relative| safe_index_path(&self.root, relative))
            else {
                tracing::warn!(path = %relative_path.display(), "loadSpec target missing");
                return None;
            };
            load_spec_file(&path, &self.root, &self.files, &mut Vec::new())
        };
        match loaded {
            Ok(mut spec) => {
                if self.loaded.len() >= MAX_CACHED_SPECS {
                    self.evict_oldest_spec();
                    self.prune_dead_options();
                }
                intern_spec_options(&mut spec, &mut self.option_pool);
                let spec = Arc::new(spec);
                self.loaded.push_back(Arc::clone(&spec));
                self.note_idle_touch_path(&relative_path);
                self.load_spec_cache.insert(relative_path, Arc::clone(&spec));
                Some(spec)
            },
            Err(error) => {
                tracing::warn!(path = %relative_path.display(), %error, "loadSpec target failed");
                None
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn cached_load_spec(&self, relative: &str) -> Option<Arc<Spec>> {
        let relative = relative.trim().trim_start_matches("./");
        let relative = if relative.ends_with(".json") {
            relative.to_string()
        } else {
            format!("{relative}.json")
        };
        let path = safe_relative_path(&relative)?;
        self.load_spec_cache.get(&path).cloned()
    }

    /// Version files live in `path_specs`, outside the 48-slot LRU.
    #[cfg(test)]
    pub(crate) fn cached_versioned_spec(&self, relative: &str) -> Option<Arc<Spec>> {
        let path = safe_relative_path(relative.trim().trim_start_matches("./"))?;
        self.path_specs.get(&path).cloned()
    }

    fn load_relative_spec(&mut self, relative: &str, name: &str) -> Option<Arc<Spec>> {
        // DirectorySnapshot keys are root-relative. An absolute path fails
        // `read_file`, so `get_versioned_arc` returns None and lookup falls
        // back to `files.<command>` (the default / highest IR).
        let relative = relative.trim().trim_start_matches("./");
        let relative_path = safe_relative_path(relative)?;
        if let Some(spec) = self.path_specs.get(&relative_path).cloned() {
            self.note_idle_touch_path(&relative_path);
            return Some(spec);
        }
        let loaded = if let Some(snapshot) = self.snapshot.as_ref() {
            if !snapshot.is_file(&relative_path) {
                tracing::warn!(command = %name, path = %relative, "versioned spec missing from snapshot");
                return None;
            }
            load_snapshot_file(snapshot, &relative_path, &self.files, &mut Vec::new())
        } else {
            let path = safe_index_path(&self.root, relative)?;
            load_spec_file(&path, &self.root, &self.files, &mut Vec::new())
        };
        match loaded {
            Ok(mut spec) => {
                if !spec.names.iter().any(|candidate| candidate == name) {
                    spec.names.push(name.to_string());
                    spec.names.shrink_to_fit();
                }
                spec.shrink_to_fit();
                intern_spec_options(&mut spec, &mut self.option_pool);
                let spec = Arc::new(spec);
                self.note_idle_touch_path(&relative_path);
                self.path_specs.insert(relative_path, spec.clone());
                Some(spec)
            },
            Err(error) => {
                tracing::warn!(command = %name, path = %relative, %error, "versioned spec load failed");
                None
            },
        }
    }

    pub fn command_names_matching(&self, query: &str) -> Vec<(String, String)> {
        self.command_names_matching_with(query, false)
    }

    /// First-token completion mirrors the shell command generator, which
    /// keeps the exact current command in the result so the legacy
    /// auto-execute wrapper can offer Enter on an already-complete token.
    /// The normal command-name lookup intentionally omits exact matches for
    /// subcommand-style completion, so expose this narrow variant instead of
    /// changing that established behavior.
    pub fn command_names_matching_including_exact_with(&self, query: &str, fuzzy: bool) -> Vec<(String, String)> {
        let mut matches = self.command_names_matching_with(query, fuzzy);
        if !query.is_empty()
            && let Some(name) = self.names.iter().find(|name| name.eq_ignore_ascii_case(query))
        {
            matches.insert(0, (name.to_string(), String::new()));
        }
        matches
    }

    pub fn command_names_matching_with(&self, query: &str, fuzzy: bool) -> Vec<(String, String)> {
        if query.is_empty() {
            return self
                .names
                .iter()
                .take(MAX_NAME_MATCHES)
                .map(|name| (name.to_string(), String::new()))
                .collect();
        }
        if fuzzy {
            let mut heap = BinaryHeap::with_capacity(MAX_NAME_MATCHES + 1);
            for name in &self.names {
                if name.eq_ignore_ascii_case(query) || !crate::query::matches_query(name, query, true) {
                    continue;
                }
                heap.push(AlphaMax(name.as_ref()));
                if heap.len() > MAX_NAME_MATCHES {
                    heap.pop();
                }
            }
            let mut matched: Vec<&str> = heap.into_iter().map(|item| item.0).collect();
            matched.sort_by(|a, b| crate::query::cmp_ignore_ascii_case(a, b));
            return matched
                .into_iter()
                .map(|name| (name.to_string(), String::new()))
                .collect();
        }
        let start = self
            .names
            .partition_point(|name| crate::query::cmp_ignore_ascii_case(name, query).is_lt());
        let mut out = Vec::new();
        for name in &self.names[start..] {
            if !crate::query::starts_with_ignore_case(name, query) {
                break;
            }
            if name.eq_ignore_ascii_case(query) {
                continue;
            }
            out.push((name.to_string(), String::new()));
            if out.len() >= MAX_NAME_MATCHES {
                break;
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.files.len().max(self.specs.len())
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.specs.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn loaded_spec_count(&self) -> usize {
        self.loaded.len()
    }

    #[cfg(test)]
    pub(crate) fn is_cached(&self, name: &str) -> bool {
        self.specs.contains_key(name)
    }

    pub fn load(dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let snapshot = DirectorySnapshot::open(dir.as_ref())
            .with_context(|| format!("open specs IR directory: {}", dir.as_ref().display()))?;
        Self::load_snapshot(snapshot)
    }

    pub(crate) fn load_snapshot(snapshot: DirectorySnapshot) -> anyhow::Result<Self> {
        crate::spec_pair::verify_if_present_snapshot(&snapshot)?;
        let mut registry = Self::new();
        registry.root = snapshot.display_path().to_path_buf();
        registry.snapshot = Some(snapshot.clone());
        registry.has_command_file_map = read_index_snapshot(&snapshot, &mut registry)?;
        index_dir_snapshot(&snapshot, Path::new(""), &mut registry)?;
        registry.rebuild_names();
        Ok(registry)
    }

    pub(crate) fn snapshot(&self) -> Option<DirectorySnapshot> {
        self.snapshot.clone()
    }

    pub(crate) fn needs_refresh(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.is_stale() || snapshot.generation_changed())
    }

    /// Overlay JSON specs from `dir` for the life of the registry. Used for
    /// `autocomplete.devCompletionsFolder` ([`OverlayMode::Replace`]) and
    /// `~/.fig/autocomplete/build` ([`OverlayMode::FillMissing`]).
    pub fn overlay_specs_dir(&mut self, dir: &Path, mode: OverlayMode) {
        if !dir.is_dir() {
            return;
        }
        overlay_json_dir(self, dir, mode);
        self.rebuild_names();
    }

    pub fn overlay_spec(&mut self, mut spec: Spec, mode: OverlayMode) {
        spec.shrink_to_fit();
        intern_spec_options(&mut spec, &mut self.option_pool);
        let spec = Arc::new(spec);
        let mut claimed = false;
        let mut displaced = Vec::new();
        for name in &spec.names {
            if name.is_empty() {
                continue;
            }
            if mode == OverlayMode::FillMissing && (self.files.contains_key(name.as_str()) || self.is_pinned_name(name))
            {
                continue;
            }
            if let Some(old) = self.specs.insert(name.clone(), spec.clone())
                && mode == OverlayMode::Replace
            {
                displaced.push(old);
            }
            self.remember_name(Arc::<str>::from(name.as_str()));
            claimed = true;
        }
        if claimed {
            self.pinned.push(spec);
        }
        let mut released = false;
        for old in displaced {
            // A public API caller can overlay an already loaded registry.
            // Once the last alias is replaced, an LRU-only tree is unreachable;
            // keep trees still owned by another alias/path or a pinned overlay.
            if self
                .specs
                .values()
                .chain(self.load_spec_cache.values())
                .chain(self.path_specs.values())
                .chain(self.pinned.iter())
                .any(|cached| Arc::ptr_eq(cached, &old))
            {
                continue;
            }
            let before = self.loaded.len();
            self.loaded.retain(|cached| !Arc::ptr_eq(cached, &old));
            released |= self.loaded.len() < before;
        }
        // All displaced handles must be gone before removing dead weak entries.
        if released {
            self.prune_dead_options();
        }
    }
}

/// Load a standalone JSON spec without indexing a directory.
pub fn load_standalone_spec(path: &Path) -> Option<Spec> {
    let root = path.parent().unwrap_or(path);
    let mut spec = load_spec_file(path, root, &HashMap::new(), &mut Vec::new()).ok()?;
    // Registry loads share options through its pool; a standalone spec has
    // no pool, so share them within the file.
    intern_spec_options_local(&mut spec);
    Some(spec)
}

/// `?` shortcuts: walk from `cwd` toward `/` for `.fig/autocomplete/build/{name}.json`.
pub fn load_project_fig_spec(cwd: &str, name: &str) -> Option<Spec> {
    if name.is_empty() || cwd.is_empty() {
        return None;
    }
    let file = format!("{name}.json");
    let mut dir = PathBuf::from(cwd);
    loop {
        for rel in [".fig/autocomplete/build", "fig/autocomplete/build"] {
            let path = dir.join(rel).join(&file);
            if path.is_file() {
                return load_standalone_spec(&path);
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

pub fn load_home_fig_spec(name: &str) -> Option<Spec> {
    if name.is_empty() {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    load_standalone_spec(
        &PathBuf::from(home)
            .join(".fig/autocomplete/build")
            .join(format!("{name}.json")),
    )
}

/// Script argv0 (`./foo`, `/usr/bin/git`): `{scriptDir}/.fig/autocomplete/build/{basename}.json`.
pub fn load_script_local_spec(cwd: &str, command: &str) -> Option<Spec> {
    let basename = command.rsplit(['/', '\\']).next().filter(|name| !name.is_empty())?;
    let dir = command.rsplit_once(['/', '\\']).map_or("", |(dir, _)| dir);
    let base = if dir.starts_with('/') || dir.starts_with('~') {
        PathBuf::from(expand_home_dir(dir))
    } else if dir == "." || dir.starts_with("./") {
        PathBuf::from(cwd).join(dir.trim_start_matches("./"))
    } else {
        PathBuf::from(cwd).join(dir)
    };
    for rel in [".fig/autocomplete/build", "fig/autocomplete/build"] {
        let path = base.join(rel).join(format!("{basename}.json"));
        if path.is_file() {
            return load_standalone_spec(&path);
        }
    }
    None
}

fn expand_home_dir(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}

fn overlay_json_dir(registry: &mut Registry, dir: &Path, mode: OverlayMode) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            overlay_json_dir(registry, &path, mode);
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == "index.json" || !name.ends_with(".json") {
            continue;
        }
        if let Some(spec) = load_standalone_spec(&path) {
            registry.overlay_spec(spec, mode);
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct IrIndex {
    files: Option<HashMap<String, String>>,
    versioned: Option<HashMap<String, VersionedCommand>>,
}

fn safe_index_path(root: &Path, relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    let path = root.join(path);
    path.is_file().then_some(path)
}

fn read_index_snapshot(snapshot: &DirectorySnapshot, registry: &mut Registry) -> anyhow::Result<bool> {
    let relative = Path::new("index.json");
    let Some(bytes) = snapshot
        .read_optional_file(relative)
        .with_context(|| format!("read {}", relative.display()))?
    else {
        return Ok(false);
    };
    let index: IrIndex = serde_json::from_slice(&bytes).with_context(|| format!("parse {}", relative.display()))?;
    let Some(files) = index.files else {
        return Ok(false);
    };
    if let Some(versioned) = index.versioned {
        registry.versioned = versioned
            .into_iter()
            .filter(|(command, _)| !command.is_empty())
            .collect();
    }
    for (command, relative) in files {
        if command.is_empty() {
            continue;
        }
        let relative = relative.trim().trim_start_matches("./");
        let Some(path) = safe_relative_path(relative) else {
            continue;
        };
        if snapshot.is_file(&path) {
            registry.remember_file(command, path);
        }
    }
    // The presence of `files`, even when every entry is invalid, is
    // authoritative.  `Registry::load` must not fall back to recursively
    // exposing nested implementation files in that case.
    Ok(true)
}

fn index_dir_snapshot(snapshot: &DirectorySnapshot, dir: &Path, registry: &mut Registry) -> anyhow::Result<()> {
    for name in snapshot
        .read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
    {
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let path = dir.join(&name);
        match snapshot
            .kind(&path)
            .with_context(|| format!("inspect {}", path.display()))?
        {
            EntryKind::Directory => {
                index_dir_snapshot(snapshot, &path, registry)?;
                continue;
            },
            EntryKind::File => {},
        }
        if name_str == "index.json" || !name_str.ends_with(".json") {
            continue;
        }
        let key = path.with_extension("").to_string_lossy().replace('\\', "/");
        if !key.is_empty() && key != "index" {
            // With the new command map, every command entry comes from
            // index.json.  All JSON paths are implementation details and are
            // resolved only when a node's loadSpec references them.  When the
            // field is absent, retain the historical relative-path fallback.
            if registry.has_command_file_map {
                continue;
            }
            let key: Arc<str> = key.into();
            registry.files.entry(key).or_insert(path);
        }
    }
    Ok(())
}

fn safe_relative_path(relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(path.to_path_buf())
}

#[cfg(test)]
fn idle_path(relative: &str) -> Option<PathBuf> {
    let relative = relative.trim().trim_start_matches("./");
    if relative.is_empty() || relative.contains('\\') {
        return None;
    }
    let relative = if relative.ends_with(".json") {
        relative.to_string()
    } else {
        format!("{relative}.json")
    };
    safe_relative_path(&relative)
}

fn resolve_reference_path(root: &Path, files: &HashMap<Arc<str>, PathBuf>, reference: &str) -> Option<PathBuf> {
    let reference = reference.trim().trim_start_matches("./");
    if reference.is_empty() || reference.contains('\\') {
        return None;
    }
    if let Some(path) = files.get(reference) {
        return Some(path.clone());
    }
    let relative = if reference.ends_with(".json") {
        reference.to_string()
    } else {
        format!("{reference}.json")
    };
    safe_index_path(root, &relative)
}

pub(crate) fn replace_spec_with_loaded(base: &mut Spec, mut loaded: Spec) {
    // `loadSpec` in the JS parser replaces the current completion object.
    // Keep the wrapper names so a parent such as `chezmoi git` or `pass grep`
    // still resolves the node by the spelling present in the command line.
    if !base.names.is_empty() {
        loaded.names = base.names.clone();
    }
    *base = loaded;
}

/// Follow this node's own `loadSpec`. Subcommand paths stay on the stub until
/// the walker enters that token. Inline objects are already in the file, so
/// they expand here. `follow_path` is set only for the file's root object,
/// matching the one root `loadSpec` q-cli resolves at the start of a walk.
fn follow_load_spec(
    spec: &mut Spec,
    root: &Path,
    files: &HashMap<Arc<str>, PathBuf>,
    stack: &mut Vec<PathBuf>,
    follow_path: bool,
) {
    let should_follow = match &spec.load_spec {
        Some(LoadSpec::Inline(_)) => true,
        Some(LoadSpec::Path(_)) => follow_path,
        None => false,
    };
    if !should_follow {
        return;
    }
    let Some(load_spec) = spec.load_spec.take() else {
        return;
    };
    let target = match load_spec {
        LoadSpec::Path(reference) => resolve_reference_path(root, files, &reference),
        LoadSpec::Inline(target) => {
            replace_spec_with_loaded(spec, *target);
            None
        },
    };
    if let Some(target_path) = target {
        let already_loading = stack.iter().any(|path| path == &target_path);
        if !already_loading {
            if let Ok(loaded) = load_spec_file_inner(&target_path, root, files, stack) {
                replace_spec_with_loaded(spec, loaded);
            }
        }
    }
    spec.meta.ai_resolved_reference = true;
}

fn resolve_unentered_descendants(
    spec: &mut Spec,
    root: &Path,
    files: &HashMap<Arc<str>, PathBuf>,
    stack: &mut Vec<PathBuf>,
) {
    for child in &mut spec.subcommands {
        let child = Arc::make_mut(child);
        follow_load_spec(child, root, files, stack, false);
        resolve_unentered_descendants(child, root, files, stack);
    }
    for arg in &mut spec.args {
        resolve_arg_spec(arg, root, files, stack);
    }
    for option in &mut spec.options {
        let Some(option) = Arc::get_mut(option) else {
            continue;
        };
        for arg in &mut option.args {
            resolve_arg_spec(arg, root, files, stack);
        }
    }
}

fn resolve_spec_references(spec: &mut Spec, root: &Path, files: &HashMap<Arc<str>, PathBuf>, stack: &mut Vec<PathBuf>) {
    follow_load_spec(spec, root, files, stack, true);
    resolve_unentered_descendants(spec, root, files, stack);
}

fn resolve_arg_spec(arg: &mut ArgSpec, root: &Path, files: &HashMap<Arc<str>, PathBuf>, stack: &mut Vec<PathBuf>) {
    let Some(load_spec) = arg.load_spec.as_ref() else {
        return;
    };

    // Path targets stay on the argument until that token is consumed.
    // Inline objects are already in this file.
    if let LoadSpec::Inline(target) = load_spec {
        let mut loaded = (**target).clone();
        resolve_spec_references(&mut loaded, root, files, stack);
        arg.resolved_spec = Some(Box::new(loaded));
    }
}

fn load_spec_file_inner(
    path: &Path,
    root: &Path,
    files: &HashMap<Arc<str>, PathBuf>,
    stack: &mut Vec<PathBuf>,
) -> anyhow::Result<Spec> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut spec: Spec = serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    stack.push(path.to_path_buf());
    resolve_spec_references(&mut spec, root, files, stack);
    stack.pop();
    // No option interning here: every registry caller interns the tree into
    // its shared pool next, which also shares identical options inside the
    // file. A local pass first serialized every option a second time.
    spec.shrink_to_fit();
    Ok(spec)
}

fn load_spec_file(
    path: &Path,
    root: &Path,
    files: &HashMap<Arc<str>, PathBuf>,
    stack: &mut Vec<PathBuf>,
) -> anyhow::Result<Spec> {
    load_spec_file_inner(path, root, files, stack)
}

fn resolve_snapshot_reference_path(
    snapshot: &DirectorySnapshot,
    files: &HashMap<Arc<str>, PathBuf>,
    reference: &str,
) -> Option<PathBuf> {
    let reference = reference.trim().trim_start_matches("./");
    if reference.is_empty() || reference.contains('\\') {
        return None;
    }
    if let Some(path) = files.get(reference) {
        return Some(path.clone());
    }
    let relative = if reference.ends_with(".json") {
        reference.to_string()
    } else {
        format!("{reference}.json")
    };
    let path = safe_relative_path(&relative)?;
    snapshot.is_file(&path).then_some(path)
}

fn follow_snapshot_load_spec(
    spec: &mut Spec,
    snapshot: &DirectorySnapshot,
    files: &HashMap<Arc<str>, PathBuf>,
    stack: &mut Vec<PathBuf>,
    follow_path: bool,
) -> anyhow::Result<()> {
    let should_follow = match &spec.load_spec {
        Some(LoadSpec::Inline(_)) => true,
        Some(LoadSpec::Path(_)) => follow_path,
        None => false,
    };
    if !should_follow {
        return Ok(());
    }
    let Some(load_spec) = spec.load_spec.take() else {
        return Ok(());
    };
    let target = match load_spec {
        LoadSpec::Path(reference) => resolve_snapshot_reference_path(snapshot, files, &reference),
        LoadSpec::Inline(target) => {
            replace_spec_with_loaded(spec, *target);
            None
        },
    };
    if let Some(target_path) = target {
        let already_loading = stack.iter().any(|path| path == &target_path);
        if !already_loading {
            let loaded = load_snapshot_file_inner(snapshot, &target_path, files, stack)?;
            replace_spec_with_loaded(spec, loaded);
        }
    }
    spec.meta.ai_resolved_reference = true;
    Ok(())
}

fn resolve_snapshot_unentered_descendants(
    spec: &mut Spec,
    snapshot: &DirectorySnapshot,
    files: &HashMap<Arc<str>, PathBuf>,
    stack: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    for child in &mut spec.subcommands {
        let child = Arc::make_mut(child);
        follow_snapshot_load_spec(child, snapshot, files, stack, false)?;
        resolve_snapshot_unentered_descendants(child, snapshot, files, stack)?;
    }
    for arg in &mut spec.args {
        resolve_snapshot_arg_spec(arg, snapshot, files, stack)?;
    }
    for option in &mut spec.options {
        let Some(option) = Arc::get_mut(option) else {
            continue;
        };
        for arg in &mut option.args {
            resolve_snapshot_arg_spec(arg, snapshot, files, stack)?;
        }
    }
    Ok(())
}

fn resolve_snapshot_spec_references(
    spec: &mut Spec,
    snapshot: &DirectorySnapshot,
    files: &HashMap<Arc<str>, PathBuf>,
    stack: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    follow_snapshot_load_spec(spec, snapshot, files, stack, true)?;
    resolve_snapshot_unentered_descendants(spec, snapshot, files, stack)
}

fn resolve_snapshot_arg_spec(
    arg: &mut ArgSpec,
    snapshot: &DirectorySnapshot,
    files: &HashMap<Arc<str>, PathBuf>,
    stack: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    let Some(load_spec) = arg.load_spec.as_ref() else {
        return Ok(());
    };
    // Path targets stay on the argument until that token is consumed.
    // A missing or swapped child then fails that one completion, not the
    // parent insert. Inline objects are already in this file.
    if let LoadSpec::Inline(target) = load_spec {
        let mut loaded = (**target).clone();
        resolve_snapshot_spec_references(&mut loaded, snapshot, files, stack)?;
        arg.resolved_spec = Some(Box::new(loaded));
    }
    Ok(())
}

/// A `LoadSpec::Path` still on this tree was not digest-checked while reading
/// `path`. Subcommand paths and argument paths both stay unread until the
/// walker enters that token. Inline objects are already expanded.
fn contains_deferred_load_spec_path(spec: &Spec) -> bool {
    spec.subcommands
        .iter()
        .any(|child| contains_deferred_load_spec_path(child))
        || matches!(spec.load_spec, Some(LoadSpec::Path(_)))
        || spec.args.iter().any(arg_has_deferred_load_spec_path)
        || spec.options.iter().any(|option| {
            option.args.iter().any(arg_has_deferred_load_spec_path) || option_has_deferred_load_spec(option)
        })
        || spec.persistent_options.iter().any(|option| {
            option.args.iter().any(arg_has_deferred_load_spec_path) || option_has_deferred_load_spec(option)
        })
}

fn option_has_deferred_load_spec(option: &OptionSpec) -> bool {
    match &option.load_spec {
        Some(LoadSpec::Path(_)) => true,
        Some(LoadSpec::Inline(inner)) => contains_deferred_load_spec_path(inner),
        None => false,
    }
}

fn arg_has_deferred_load_spec_path(arg: &ArgSpec) -> bool {
    if let Some(resolved) = arg.resolved_spec.as_deref() {
        if contains_deferred_load_spec_path(resolved) {
            return true;
        }
    } else if matches!(arg.load_spec, Some(LoadSpec::Path(_))) {
        return true;
    }
    if let Some(LoadSpec::Inline(inner)) = &arg.load_spec {
        return contains_deferred_load_spec_path(inner);
    }
    false
}

fn load_snapshot_file_inner(
    snapshot: &DirectorySnapshot,
    path: &Path,
    files: &HashMap<Arc<str>, PathBuf>,
    stack: &mut Vec<PathBuf>,
) -> anyhow::Result<Spec> {
    let bytes = snapshot
        .read_file(path)
        .with_context(|| format!("read {}", path.display()))?;
    let mut spec: Spec = serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    stack.push(path.to_path_buf());
    let resolved = resolve_snapshot_spec_references(&mut spec, snapshot, files, stack);
    stack.pop();
    resolved?;
    // The publisher swaps the directory. Parent bytes can match while a
    // deferred child does not. Caching that parent would keep the old
    // generation until LRU eviction. Refuse the insert; Engine::complete
    // reloads before the next request. A stable generation does not stat
    // or read those children.
    if contains_deferred_load_spec_path(&spec) && snapshot.generation_changed() {
        anyhow::bail!(
            "snapshot generation changed with a deferred loadSpec still unread: {}",
            path.display()
        );
    }
    // Interned by the caller into the registry pool, as in `load_spec_file_inner`.
    spec.shrink_to_fit();
    Ok(spec)
}

fn load_snapshot_file(
    snapshot: &DirectorySnapshot,
    path: &Path,
    files: &HashMap<Arc<str>, PathBuf>,
    stack: &mut Vec<PathBuf>,
) -> anyhow::Result<Spec> {
    load_snapshot_file_inner(snapshot, path, files, stack)
}

#[cfg(test)]
mod tests {
    use std::sync::Weak;
    use std::time::{Duration, Instant};

    use super::*;

    struct OptionHashProfileGuard(Option<OptionHashProfile>);

    impl OptionHashProfileGuard {
        fn enter(mode: OptionHashProfileMode) -> Self {
            Self(OPTION_HASH_PROFILE.with(|slot| slot.replace(Some(OptionHashProfile { mode, calls: 0 }))))
        }

        // Keep observation scoped to a live profiling guard.
        #[allow(clippy::unused_self)]
        fn calls(&self) -> usize {
            OPTION_HASH_PROFILE.with(|slot| slot.get().unwrap().calls)
        }
    }

    impl Drop for OptionHashProfileGuard {
        fn drop(&mut self) {
            OPTION_HASH_PROFILE.with(|slot| slot.set(self.0));
        }
    }

    fn write_spec(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{name}.json")), body).unwrap();
    }

    #[test]
    fn versioned_spec_loads_from_snapshot_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("tool")).unwrap();
        fs::write(
            dir.path().join("tool/1.0.0.json"),
            r#"{"names":["tool"],"subcommands":[{"names":["old"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("tool/1.0.0+1.1.0.json"),
            r#"{"names":["tool"],"subcommands":[{"names":["old"]},{"names":["extra"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("tool/2.0.0.json"),
            r#"{"names":["tool"],"subcommands":[{"names":["new"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("index.json"),
            r#"{
              "completions":["tool"],
              "files":{"tool":"tool/2.0.0.json"},
              "versioned":{
                "tool":{
                  "command":["tool","--version"],
                  "parse":"after-first-space",
                  "fallback":"2.0.0",
                  "files":{"1.0.0":"tool/1.0.0.json","2.0.0":"tool/2.0.0.json"},
                  "applied":{"1.0.0":{"1.1.0":"tool/1.0.0+1.1.0.json"}}
                }
              }
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let _guard = crate::process::mock::install(vec![crate::process::mock::ExecRule {
            command: Some("tool".into()),
            args: Some(vec!["--version".into()]),
            stdout: "tool 1.0.5".into(),
            ..crate::process::mock::ExecRule::default()
        }]);
        let spec = registry
            .get_versioned_arc("tool", "/", std::time::Duration::from_secs(5))
            .expect("versioned snapshot load");
        assert!(spec.find_subcommand("extra").is_some());
        assert!(spec.find_subcommand("old").is_some());
        assert!(spec.find_subcommand("new").is_none());
    }

    #[test]
    fn loads_git_fixture_subcommands() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{
              "names": ["git"],
              "description": "the stupid content tracker",
              "subcommands": [
                {"names": ["checkout"], "description": "Switch branches"},
                {"names": ["commit"], "description": "Record changes"},
                {"names": ["cherry-pick"], "description": "Apply commits"}
              ],
              "options": [{"names": ["--help"], "description": "Show help"}]
            }"#,
        );
        let mut registry = Registry::load(dir.path()).expect("load");
        let git = registry.get("git").expect("git spec");
        assert!(git.find_subcommand("checkout").is_some());
        assert!(git.find_subcommand("cherry-pick").is_some());
        assert!(git.find_subcommand("status").is_none());
        assert_eq!(git.options[0].names, vec!["--help"]);
    }

    #[test]
    fn loads_mkdir_fixture_options_and_folder_template() {
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
                {"names": ["-v", "--verbose"], "description": "Print a message"}
              ]
            }"#,
        );
        let mut registry = Registry::load(dir.path()).expect("load");
        let mkdir = registry.get("mkdir").expect("mkdir spec");
        assert_eq!(mkdir.args[0].templates, vec![Template::Folders]);
        assert!(mkdir.options.iter().any(|opt| opt.names.iter().any(|n| n == "-p")));
    }

    #[test]
    fn shrink_to_fit_drops_spare_capacity_without_changing_elements() {
        let mut spec = Spec {
            names: Vec::with_capacity(32),
            options: Vec::with_capacity(16),
            persistent_options: Vec::with_capacity(8),
            subcommands: Vec::with_capacity(8),
            ..Spec::default()
        };
        spec.names.push("tool".to_owned());
        spec.options.push(Arc::new(OptionSpec {
            names: Vec::with_capacity(20),
            exclusive_on: Vec::with_capacity(6),
            ..OptionSpec::default()
        }));
        Arc::get_mut(&mut spec.options[0])
            .expect("option is not shared yet")
            .names
            .push("--flag".to_owned());
        spec.persistent_options.push(Arc::new(OptionSpec {
            names: vec!["--global".to_owned()],
            ..OptionSpec::default()
        }));
        let mut child = Spec::default();
        child.names = Vec::with_capacity(10);
        child.names.push("sub".to_owned());
        child.args.push(ArgSpec {
            suggestions: Vec::with_capacity(12),
            generators: vec![GeneratorSpec {
                script: Vec::with_capacity(9),
                extensions: vec!["rs".to_owned()],
                ..GeneratorSpec::default()
            }],
            ..ArgSpec::default()
        });
        child.args[0].suggestions.push(SuggestionSeed {
            names: Vec::with_capacity(7),
            ..SuggestionSeed::default()
        });
        child.args[0].suggestions[0].names.push("one".to_owned());
        child.parser_directives = Some(ParserDirectives {
            option_arg_separators: Some(Vec::with_capacity(5)),
            ..ParserDirectives::default()
        });
        spec.subcommands.push(Arc::new(child));

        let names = spec.names.clone();
        let option_names = spec.options[0].names.clone();
        let sub_names = spec.subcommands[0].names.clone();
        let suggestion_names = spec.subcommands[0].args[0].suggestions[0].names.clone();
        assert!(spec.names.capacity() > spec.names.len());
        assert!(spec.options.capacity() > spec.options.len());

        spec.shrink_to_fit();

        assert_eq!(spec.names, names);
        assert_eq!(spec.options[0].names, option_names);
        assert_eq!(spec.subcommands[0].names, sub_names);
        assert_eq!(spec.subcommands[0].args[0].suggestions[0].names, suggestion_names);
        assert_eq!(spec.options[0].exclusive_on, Vec::<String>::new());
        assert_eq!(spec.names.capacity(), spec.names.len());
        assert_eq!(spec.options.capacity(), spec.options.len());
        assert_eq!(spec.options[0].names.capacity(), spec.options[0].names.len());
        assert_eq!(spec.options[0].exclusive_on.capacity(), 0);
        assert_eq!(spec.persistent_options.capacity(), spec.persistent_options.len());
        assert_eq!(spec.subcommands.capacity(), spec.subcommands.len());
        assert_eq!(spec.subcommands[0].names.capacity(), spec.subcommands[0].names.len());
        assert_eq!(spec.subcommands[0].args.capacity(), spec.subcommands[0].args.len());
        assert_eq!(
            spec.subcommands[0].args[0].suggestions.capacity(),
            spec.subcommands[0].args[0].suggestions.len()
        );
        assert_eq!(
            spec.subcommands[0].args[0].suggestions[0].names.capacity(),
            spec.subcommands[0].args[0].suggestions[0].names.len()
        );
        assert_eq!(
            spec.subcommands[0].args[0].generators[0].script.capacity(),
            spec.subcommands[0].args[0].generators[0].script.len()
        );
        let separators = spec.subcommands[0]
            .parser_directives
            .as_ref()
            .and_then(|directives| directives.option_arg_separators.as_ref())
            .expect("separators");
        assert_eq!(separators.capacity(), separators.len());
    }

    #[test]
    fn loaded_spec_vecs_match_their_length() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "tool",
            r#"{
              "names": ["tool"],
              "options": [
                {"names": ["--a"], "exclusiveOn": ["--b"]},
                {"names": ["--b"]},
                {"names": ["--c"]},
                {"names": ["--d"]},
                {"names": ["--e"]}
              ],
              "persistentOptions": [{"names": ["--global"]}],
              "subcommands": [{
                "names": ["sub"],
                "args": [{
                  "name": "x",
                  "suggestions": [{"names": ["one", "two", "three"]}],
                  "generators": [{"script": ["echo", "hi"], "extensions": ["rs", "toml"]}]
                }]
              }]
            }"#,
        );
        let mut registry = Registry::load(dir.path()).expect("load");
        let spec = registry.get("tool").expect("tool spec");
        assert_eq!(spec.options.len(), 5);
        assert_eq!(spec.options.capacity(), spec.options.len());
        assert_eq!(spec.options[0].names.capacity(), spec.options[0].names.len());
        assert_eq!(
            spec.options[0].exclusive_on.capacity(),
            spec.options[0].exclusive_on.len()
        );
        assert_eq!(spec.persistent_options.capacity(), spec.persistent_options.len());
        assert_eq!(spec.subcommands.capacity(), spec.subcommands.len());
        assert_eq!(spec.subcommands[0].args.capacity(), spec.subcommands[0].args.len());
        assert_eq!(
            spec.subcommands[0].args[0].suggestions.capacity(),
            spec.subcommands[0].args[0].suggestions.len()
        );
        assert_eq!(
            spec.subcommands[0].args[0].suggestions[0].names.capacity(),
            spec.subcommands[0].args[0].suggestions[0].names.len()
        );
        assert_eq!(
            spec.subcommands[0].args[0].generators[0].script.capacity(),
            spec.subcommands[0].args[0].generators[0].script.len()
        );
        assert_eq!(
            spec.subcommands[0].args[0].generators[0].extensions.capacity(),
            spec.subcommands[0].args[0].generators[0].extensions.len()
        );
        assert_eq!(spec.names, vec!["tool".to_owned()]);
        assert_eq!(spec.options[0].names, vec!["--a".to_owned()]);
        assert!(spec.find_subcommand("sub").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_version_detection_is_not_cached_as_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "v1", r#"{"names":["tool"],"description":"selected-old"}"#);
        write_spec(dir.path(), "v2", r#"{"names":["tool"],"description":"fallback-new"}"#);
        let started = dir.path().join("started");
        let allow = dir.path().join("allow");
        let index = serde_json::json!({
            "files": {"tool":"v2.json"},
            "versioned": {"tool": {
                "command":["/bin/sh", "-c",
                    "if [ ! -e \"$2\" ]; then : > \"$1\"; while :; do sleep 1; done; fi; printf '1.0.0'",
                    "version-fixture", started, allow],
                "parse":"stdout", "fallback":"2.0.0", "files":{"1.0.0":"v1.json","2.0.0":"v2.json"}
            }}
        });
        fs::write(dir.path().join("index.json"), index.to_string()).unwrap();
        let token = crate::cancellation::CancellationToken::new();
        let scope = crate::cancellation::enter(token.clone());
        let marker = started.clone();
        let canceller = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !marker.exists() {
                if Instant::now() >= deadline {
                    token.cancel();
                    panic!("version command did not start");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            token.cancel();
        });
        let mut registry = Registry::load(dir.path()).unwrap();
        assert!(
            registry
                .get_versioned_arc("tool", "/", Duration::from_secs(5))
                .is_none()
        );
        canceller.join().unwrap();
        assert!(!registry.version_cache.contains_key("tool"));
        drop(scope);
        fs::write(allow, "").unwrap();
        assert_eq!(
            registry
                .get_versioned_arc("tool", "/", Duration::from_secs(5))
                .unwrap()
                .description,
            "selected-old"
        );
    }

    #[test]
    fn cancelled_idle_checkpoint_preserves_old_marks_and_releases_new_trees() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["old", "active", "new"] {
            write_spec(dir.path(), name, &format!(r#"{{"names":["{name}"]}}"#));
        }
        let mut registry = Registry::load(dir.path()).unwrap();
        registry.get_arc("old").unwrap();
        registry.get_arc("active").unwrap();
        let original = Instant::now();
        registry.set_idle_since("old", Some(original));
        registry.set_idle_since("active", None);
        let checkpoint = registry.checkpoint_idle();
        registry.get_arc("old").unwrap();
        let new = registry.get_arc("new").unwrap();
        let weak = Arc::downgrade(&new);
        drop(new);
        let cancelled = original + Duration::from_secs(2);
        registry.restore_idle_checkpoint(&checkpoint, cancelled);
        assert_eq!(registry.idle_mark("old"), Some(Some(original)));
        assert_eq!(registry.idle_mark("active"), Some(None));
        assert_eq!(registry.idle_mark("new"), Some(Some(cancelled)));
        registry.release_idle(cancelled + SPEC_IDLE_GRACE, SPEC_IDLE_GRACE);
        assert!(weak.upgrade().is_none());
        assert!(registry.specs.contains_key("active"));
    }

    #[test]
    fn option_fingerprint_matches_the_unbuffered_json_byte_stream() {
        let rich = OptionSpec {
            names: vec!["--文件".into(), "-f".into()],
            description: "quotes: \" \\ and newline\n".into(),
            args: vec![ArgSpec {
                name: "target".into(),
                templates: vec![Template::Filepaths],
                load_spec: Some(LoadSpec::Inline(Box::new(Spec {
                    names: vec!["nested".into()],
                    options: vec![Arc::new(OptionSpec {
                        names: vec!["--inner".into()],
                        ..OptionSpec::default()
                    })],
                    ..Spec::default()
                }))),
                ..ArgSpec::default()
            }],
            is_repeatable: Some(serde_json::json!(3)),
            requires_separator: Some(serde_json::json!(":")),
            ..OptionSpec::default()
        };
        for option in [OptionSpec::default(), rich] {
            let bytes = serde_json::to_vec(&option).unwrap();
            let mut reference = std::collections::hash_map::DefaultHasher::new();
            reference.write(&bytes);
            assert_eq!(hash_option(&option), reference.finish());
            assert_eq!(hash_option(&option.clone()), reference.finish());
        }
    }

    #[test]
    fn option_fingerprint_writer_keeps_order_across_flushes_and_large_fragments() {
        use std::io::Write as _;

        let bytes: Vec<u8> = (0..1025).map(|index| u8::try_from(index % 251).unwrap()).collect();
        let mut reference = std::collections::hash_map::DefaultHasher::new();
        reference.write(&bytes);
        for chunk_size in [1, 7, 255, 256, 257, 1025] {
            let mut writer = OptionHashWriter::default();
            for (index, chunk) in bytes.chunks(chunk_size).enumerate() {
                writer.write_all(chunk).unwrap();
                if index % 3 == 0 {
                    writer.flush().unwrap();
                }
            }
            writer.write_all(&[]).unwrap();
            assert_eq!(writer.finish(), reference.finish());
        }
    }

    #[test]
    fn option_fingerprint_collisions_still_require_structural_equality() {
        let plain = Arc::new(OptionSpec {
            names: vec!["--same".into()],
            ..OptionSpec::default()
        });
        let mut tainted = plain.as_ref().clone();
        // Provenance is excluded from JSON, so these unequal options have a
        // real fingerprint collision without weakening the production hash.
        tainted.meta.ai_generated = true;
        assert_ne!(plain.as_ref(), &tainted);
        assert_eq!(hash_option(&plain), hash_option(&tainted));
        let mut pool = HashMap::new();
        let original = intern_one_option(Arc::clone(&plain), &mut pool);
        let collision = intern_one_option(Arc::new(tainted), &mut pool);
        assert!(!Arc::ptr_eq(&original, &collision));
        assert!(collision.meta.ai_generated);
        let equal = intern_one_option(Arc::new(plain.as_ref().clone()), &mut pool);
        assert!(Arc::ptr_eq(&original, &equal));
    }

    #[test]
    fn option_fingerprint_memo_is_scoped_to_one_registry_pass() {
        for (mode, expected_calls) in [
            (OptionHashProfileMode::StreamedNoMemo, 3),
            (OptionHashProfileMode::StreamedMemo, 1),
        ] {
            let option = Arc::new(OptionSpec {
                names: vec!["--shared".into()],
                ..OptionSpec::default()
            });
            let mut spec = Spec {
                options: vec![Arc::clone(&option), Arc::clone(&option)],
                persistent_options: vec![Arc::clone(&option)],
                ..Spec::default()
            };
            let mut pool = HashMap::new();
            let profile = OptionHashProfileGuard::enter(mode);
            intern_spec_options(&mut spec, &mut pool);
            assert_eq!(profile.calls(), expected_calls);
            assert!(spec.options.iter().all(|shared| Arc::ptr_eq(shared, &option)));
            // Only the pool's Weak survives return; the pass memo is gone.
            assert_eq!(Arc::weak_count(&option), 1);
            intern_spec_options(&mut spec, &mut pool);
            assert_eq!(profile.calls(), expected_calls * 2);
            let weak = Arc::downgrade(&option);
            drop(spec);
            drop(option);
            assert!(weak.upgrade().is_none(), "neither memo nor pool may own a body");
        }
    }

    #[test]
    fn option_fingerprint_memo_validates_weak_identity() {
        let option = Arc::new(OptionSpec {
            names: vec!["--candidate".into()],
            ..OptionSpec::default()
        });
        let other = Arc::new(OptionSpec::default());
        let expected = hash_option(&option);
        let key = Arc::as_ptr(&option) as usize;
        let mut memo = Some(HashMap::from([(key, (Arc::downgrade(&other), expected ^ 1))]));
        assert_eq!(memoized_option_hash(&option, &mut memo), expected);
        memo.as_mut()
            .unwrap()
            .insert(key, (Arc::downgrade(&other), expected ^ 1));
        drop(other);
        assert_eq!(memoized_option_hash(&option, &mut memo), expected);
        let weak = Arc::downgrade(&option);
        drop(option);
        assert!(
            weak.upgrade().is_none(),
            "the live memo must only retain Weak ownership"
        );
    }

    #[test]
    #[ignore = "profiles fingerprints in bundle/specs-ir/gcloud/compute.json"]
    fn compute_option_fingerprint_profile() {
        fn collect_args<'a>(args: &'a [ArgSpec], out: &mut Vec<&'a Arc<OptionSpec>>) {
            for arg in args {
                if let Some(spec) = &arg.resolved_spec {
                    collect_options(spec, out);
                }
                if let Some(LoadSpec::Inline(spec)) = &arg.load_spec {
                    collect_options(spec, out);
                }
            }
        }

        fn collect_options<'a>(spec: &'a Spec, out: &mut Vec<&'a Arc<OptionSpec>>) {
            for child in &spec.subcommands {
                collect_options(child, out);
            }
            collect_args(&spec.args, out);
            if let Some(LoadSpec::Inline(spec)) = &spec.load_spec {
                collect_options(spec, out);
            }
            for option in spec.options.iter().chain(&spec.persistent_options) {
                collect_args(&option.args, out);
                if let Some(LoadSpec::Inline(spec)) = &option.load_spec {
                    collect_options(spec, out);
                }
                out.push(option);
            }
        }

        fn measure(options: &[&Arc<OptionSpec>], legacy: bool, memoize: bool) -> (u128, usize) {
            let started = Instant::now();
            let mut memo: HashMap<usize, (Weak<OptionSpec>, u64)> = HashMap::new();
            let mut calls = 0;
            let mut checksum = 0;
            for option in options {
                let key = Arc::as_ptr(option) as usize;
                let cached = memo.get(&key).and_then(|(weak, hash)| {
                    weak.upgrade()
                        .filter(|cached| Arc::ptr_eq(cached, option))
                        .map(|_| *hash)
                });
                let hash = cached.unwrap_or_else(|| {
                    calls += 1;
                    let hash = if legacy {
                        legacy_option_hash(option)
                    } else {
                        hash_option(option)
                    };
                    if memoize {
                        memo.insert(key, (Arc::downgrade(option), hash));
                    }
                    hash
                });
                checksum ^= hash;
            }
            std::hint::black_box(checksum);
            // Include destruction of the temporary Weak memo in its cost.
            drop(memo);
            (started.elapsed().as_micros(), calls)
        }

        let specs = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bundle/specs-ir");
        let bytes = fs::read(specs.join("gcloud/compute.json")).unwrap();
        let started = Instant::now();
        let mut spec: Spec = serde_json::from_slice(&bytes).unwrap();
        let parse_us = started.elapsed().as_micros();
        let mut raw = Vec::new();
        collect_options(&spec, &mut raw);
        let raw_slots = raw.len();
        assert!(raw_slots > 0);
        drop(raw);
        let started = Instant::now();
        intern_spec_options_local(&mut spec);
        let intern_us = started.elapsed().as_micros();
        let mut options = Vec::new();
        collect_options(&spec, &mut options);
        assert_eq!(options.len(), raw_slots);
        let unique = options
            .iter()
            .map(|option| Arc::as_ptr(option) as usize)
            .collect::<HashSet<_>>();
        let serialized_bytes: usize = options
            .iter()
            .map(|option| serde_json::to_vec(option).unwrap().len())
            .sum();
        println!(
            "OPTION_HASH slots={raw_slots} unique={} serialized_bytes={serialized_bytes} parse_us={parse_us} intern_us={intern_us}",
            unique.len()
        );
        // Rotate order to expose warm-cache bias. These are isolated hash
        // passes over real shared options, not end-to-end completion timings.
        for round in 0..6 {
            for offset in 0..3 {
                let mode = (round + offset) % 3;
                let (micros, calls) = measure(&options, mode == 0, mode == 2);
                let name = ["legacy_vec", "writer", "writer_weak_memo"][mode];
                println!("OPTION_HASH round={round} mode={name} elapsed_us={micros} hash_calls={calls}");
            }
        }
        drop(options);
        drop(unique);
        drop(spec);
        drop(bytes);

        // Exercise the real snapshot/read/parse/local-intern/registry-intern
        // path as well. The thread-local test-only switch compares all three
        // implementations in one binary; rotating order exposes cache bias.
        // These are load timings, not complete() or peak-memory measurements.
        for round in 0..3 {
            for offset in 0..3 {
                let mode = [
                    OptionHashProfileMode::LegacyVec,
                    OptionHashProfileMode::StreamedNoMemo,
                    OptionHashProfileMode::StreamedMemo,
                ][(round + offset) % 3];
                let profile = OptionHashProfileGuard::enter(mode);
                let started = Instant::now();
                let mut registry = Registry::load(&specs).unwrap();
                let registry_load_us = started.elapsed().as_micros();
                let registry_hash_calls = profile.calls();
                let started = Instant::now();
                let compute = registry.load_referenced_spec("gcloud/compute").unwrap();
                let compute_load_us = started.elapsed().as_micros();
                let compute_hash_calls = profile.calls() - registry_hash_calls;
                assert!(compute.find_subcommand("instances").is_some());
                println!(
                    "OPTION_LOAD round={round} mode={mode:?} registry_load_us={registry_load_us} registry_hash_calls={registry_hash_calls} compute_load_us={compute_load_us} compute_hash_calls={compute_hash_calls} allocated_bytes={}",
                    compute.allocated_bytes()
                );
            }
        }
    }

    #[test]
    fn identical_options_share_one_allocation_and_the_pool_does_not_keep_them() {
        let dir = tempfile::tempdir().unwrap();
        let option = r#"{"names":["--same"],"description":"one"}"#;
        write_spec(
            dir.path(),
            "tool",
            &format!(
                r#"{{
                  "names":["tool"],
                  "options":[{option},{option}],
                  "persistentOptions":[{option}],
                  "subcommands":[{{"names":["sub"],"options":[{option},{{"names":["--other"],"description":"two"}}]}}]
                }}"#
            ),
        );
        write_spec(
            dir.path(),
            "other",
            &format!(r#"{{"names":["other"],"options":[{option}]}}"#),
        );
        let mut registry = Registry::load(dir.path()).expect("load");
        let tool = registry.get_arc("tool").expect("tool");
        let other = registry.get_arc("other").expect("other");
        assert!(Arc::ptr_eq(&tool.options[0], &tool.options[1]));
        assert!(Arc::ptr_eq(&tool.options[0], &tool.persistent_options[0]));
        assert!(Arc::ptr_eq(&tool.options[0], &tool.subcommands[0].options[0]));
        assert!(Arc::ptr_eq(&tool.options[0], &other.options[0]));
        assert!(!Arc::ptr_eq(&tool.options[0], &tool.subcommands[0].options[1]));
        assert_eq!(tool.options[0].description, "one");
        assert_eq!(tool.subcommands[0].options[1].names, vec!["--other".to_owned()]);

        let weak = Arc::downgrade(&tool.options[0]);
        drop(tool);
        drop(other);
        drop(registry);
        assert!(weak.upgrade().is_none(), "the intern pool must not keep options alive");
    }

    #[test]
    fn option_intern_replaces_a_dropped_body_instead_of_resurrecting_it() {
        let mut pool = HashMap::new();
        let option = Arc::new(OptionSpec {
            names: vec!["--a".to_owned()],
            ..OptionSpec::default()
        });
        let shared = intern_one_option(Arc::clone(&option), &mut pool);
        assert!(Arc::ptr_eq(&option, &shared));
        drop(shared);
        drop(option);
        let again = intern_one_option(
            Arc::new(OptionSpec {
                names: vec!["--a".to_owned()],
                ..OptionSpec::default()
            }),
            &mut pool,
        );
        assert_eq!(Arc::strong_count(&again), 1);
    }

    #[test]
    fn shared_option_body_is_counted_once() {
        let option = Arc::new(OptionSpec {
            names: vec!["--flag".into()],
            description: "hello".into(),
            ..OptionSpec::default()
        });
        let empty = Spec::default();
        let once = Spec {
            options: vec![Arc::clone(&option)],
            ..Spec::default()
        };
        let shared = Spec {
            options: vec![Arc::clone(&option), Arc::clone(&option)],
            ..Spec::default()
        };
        let distinct = Spec {
            options: vec![
                Arc::clone(&option),
                Arc::new(OptionSpec {
                    names: vec!["--flag".into()],
                    description: "hello".into(),
                    ..OptionSpec::default()
                }),
            ],
            ..Spec::default()
        };
        let listed_twice = Spec {
            options: vec![Arc::clone(&option)],
            persistent_options: vec![Arc::clone(&option)],
            ..Spec::default()
        };
        assert_eq!(once.options.capacity(), 1);
        assert_eq!(shared.options.capacity(), 2);
        assert_eq!(distinct.options.capacity(), 2);
        assert_eq!(listed_twice.options.capacity(), 1);
        assert_eq!(listed_twice.persistent_options.capacity(), 1);
        let slot = std::mem::size_of::<Arc<OptionSpec>>();
        let body = once.allocated_bytes() - empty.allocated_bytes() - slot;
        assert!(body > slot);
        assert_eq!(shared.allocated_bytes(), once.allocated_bytes() + slot);
        assert_eq!(distinct.allocated_bytes(), once.allocated_bytes() + slot + body);
        assert_eq!(
            listed_twice.allocated_bytes(),
            empty.allocated_bytes() + (slot * 2) + body
        );
    }

    #[test]
    fn shared_subcommand_body_is_counted_once() {
        let child = Arc::new(Spec {
            names: vec!["child".into()],
            description: "nested".into(),
            ..Spec::default()
        });
        let empty = Spec::default();
        let once = Spec {
            subcommands: vec![Arc::clone(&child)],
            ..Spec::default()
        };
        let shared = Spec {
            subcommands: vec![Arc::clone(&child), Arc::clone(&child)],
            ..Spec::default()
        };
        let distinct = Spec {
            subcommands: vec![
                Arc::clone(&child),
                Arc::new(Spec {
                    names: vec!["child".into()],
                    description: "nested".into(),
                    ..Spec::default()
                }),
            ],
            ..Spec::default()
        };
        let slot = std::mem::size_of::<Arc<Spec>>();
        let body = once.allocated_bytes() - empty.allocated_bytes() - slot;
        assert!(body > slot);
        assert_eq!(shared.allocated_bytes(), once.allocated_bytes() + slot);
        assert_eq!(distinct.allocated_bytes(), once.allocated_bytes() + slot + body);
    }

    #[test]
    fn command_names_matching_skips_exact_and_matches_prefix() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "git", r#"{"names":["git"],"description":"git"}"#);
        write_spec(dir.path(), "gzip", r#"{"names":["gzip"],"description":"gzip"}"#);
        let registry = Registry::load(dir.path()).expect("load");
        assert_eq!(registry.loaded_spec_count(), 0);
        let names: Vec<_> = registry
            .command_names_matching("gi")
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["git"]);
        assert!(registry.command_names_matching("git").is_empty());
        assert_eq!(registry.loaded_spec_count(), 0);
        let gz: Vec<_> = registry
            .command_names_matching("gz")
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(gz, vec!["gzip"]);
        assert_eq!(registry.loaded_spec_count(), 0);
    }

    #[test]
    fn spec_cache_evicts_oldest_not_all() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            write_spec(dir.path(), &format!("cmd{i}"), &format!(r#"{{"names":["cmd{i}"]}}"#));
        }
        let mut registry = Registry::load(dir.path()).expect("load");
        for i in 0..48 {
            assert!(registry.get(&format!("cmd{i}")).is_some());
        }
        assert_eq!(registry.loaded_spec_count(), 48);
        assert!(registry.is_cached("cmd0"));
        assert!(registry.get("cmd48").is_some());
        assert_eq!(registry.loaded_spec_count(), 48);
        assert!(!registry.is_cached("cmd0"));
        assert!(registry.is_cached("cmd1"));
        assert!(registry.is_cached("cmd48"));
        assert!(registry.get("cmd0").is_some());
        assert!(registry.is_cached("cmd0"));
        assert!(!registry.is_cached("cmd1"));
    }

    #[test]
    fn overlaid_specs_survive_lru_eviction_and_fill_missing_never_shadows_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..60 {
            write_spec(
                dir.path(),
                &format!("cmd{i}"),
                &format!(r#"{{"names":["cmd{i}"],"description":"bundled"}}"#),
            );
        }
        let overlay = dir.path().join("overlay");
        fs::create_dir(&overlay).unwrap();
        fs::write(
            overlay.join("cmd0.json"),
            r#"{"names":["cmd0"],"description":"from dev folder"}"#,
        )
        .unwrap();
        fs::write(
            overlay.join("mine.json"),
            r#"{"names":["mine"],"description":"not in the bundle"}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).expect("load");
        registry.overlay_specs_dir(&overlay, OverlayMode::Replace);
        assert_eq!(
            registry.get("cmd0").map(|spec| spec.description.as_str()),
            Some("from dev folder")
        );
        assert!(
            registry
                .command_names_matching("mi")
                .iter()
                .any(|(name, _)| name == "mine")
        );

        // Churn the LRU well past its capacity.
        for i in 1..60 {
            assert!(registry.get(&format!("cmd{i}")).is_some());
        }
        assert_eq!(
            registry.get("cmd0").map(|spec| spec.description.as_str()),
            Some("from dev folder")
        );
        assert_eq!(
            registry.get("mine").map(|spec| spec.description.as_str()),
            Some("not in the bundle")
        );

        // `~/.fig/autocomplete/build` only fills names the bundle lacks.
        let home_build = dir.path().join("home-build");
        fs::create_dir(&home_build).unwrap();
        fs::write(
            home_build.join("cmd1.json"),
            r#"{"names":["cmd1"],"description":"home build"}"#,
        )
        .unwrap();
        fs::write(
            home_build.join("extra.json"),
            r#"{"names":["extra"],"description":"home build"}"#,
        )
        .unwrap();
        registry.overlay_specs_dir(&home_build, OverlayMode::FillMissing);
        assert_eq!(
            registry.get("cmd1").map(|spec| spec.description.as_str()),
            Some("bundled")
        );
        assert_eq!(
            registry.get("extra").map(|spec| spec.description.as_str()),
            Some("home build")
        );
    }

    #[test]
    fn spec_cache_lru_keeps_recently_used() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            write_spec(dir.path(), &format!("cmd{i}"), &format!(r#"{{"names":["cmd{i}"]}}"#));
        }
        let mut registry = Registry::load(dir.path()).expect("load");
        for i in 0..48 {
            assert!(registry.get(&format!("cmd{i}")).is_some());
        }
        assert!(registry.get("cmd0").is_some());
        assert!(registry.get("cmd48").is_some());
        assert!(registry.is_cached("cmd0"));
        assert!(!registry.is_cached("cmd1"));
        assert!(registry.is_cached("cmd48"));
    }

    #[test]
    fn alias_does_not_overwrite_an_existing_spec_file() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "g",
            r#"{"names":["g"],"description":"the g tool","subcommands":[{"names":["only-in-g"]}]}"#,
        );
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git","g"],"description":"git","subcommands":[{"names":["checkout"]}]}"#,
        );
        let mut registry = Registry::load(dir.path()).expect("load");
        let git = registry.get("git").expect("git");
        assert!(git.find_subcommand("checkout").is_some());
        let g = registry.get("g").expect("g");
        assert!(g.find_subcommand("only-in-g").is_some(), "{g:?}");
        assert!(g.find_subcommand("checkout").is_none());
    }

    #[test]
    fn fuzzy_name_match_keeps_alphabetically_first_fifty() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..60 {
            write_spec(
                dir.path(),
                &format!("test{i:02}"),
                &format!(r#"{{"names":["test{i:02}"]}}"#),
            );
        }
        let registry = Registry::load(dir.path()).expect("load");
        let names: Vec<_> = registry
            .command_names_matching_with("te", true)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names.len(), 50);
        assert_eq!(names[0], "test00");
        assert_eq!(names[49], "test49");
        assert!(!names.iter().any(|n| n == "test59"));
    }

    #[test]
    fn indexes_alias_names() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "npm",
            r#"{
              "names": ["npm"],
              "subcommands": [
                {"names": ["install", "i", "add"], "description": "Install a package"}
              ]
            }"#,
        );
        let mut registry = Registry::load(dir.path()).expect("load");
        let npm = registry.get("npm").unwrap();
        assert!(npm.find_subcommand("i").is_some());
        assert!(npm.find_subcommand("add").is_some());
    }

    #[test]
    fn root_load_spec_still_resolves_while_child_paths_stay_stubs() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "wrapper",
            r#"{"names":["wrapper"],"description":"wrapper description","loadSpec":"real"}"#,
        );
        write_spec(
            dir.path(),
            "real",
            r#"{
              "names":["real-target"],
              "description":"from real",
              "subcommands":[
                {"names":["child"]},
                {"names":["later"],"loadSpec":"later"}
              ]
            }"#,
        );
        write_spec(
            dir.path(),
            "later",
            r#"{"names":["later"],"subcommands":[{"names":["nope"]}]}"#,
        );
        fs::write(dir.path().join("index.json"), r#"{"files":{"wrapper":"wrapper.json"}}"#).unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        let wrapper = registry.get("wrapper").expect("wrapper");
        assert_eq!(wrapper.names, vec!["wrapper"]);
        assert_eq!(wrapper.description, "from real");
        assert!(wrapper.find_subcommand("child").is_some());
        let later = wrapper.find_subcommand("later").expect("later stub");
        assert!(matches!(later.load_spec, Some(LoadSpec::Path(ref path)) if path == "later"));
        assert!(later.find_subcommand("nope").is_none());
        assert!(wrapper.meta.ai_resolved_reference);
        assert!(!later.meta.ai_resolved_reference);
        assert!(registry.cached_load_spec("later").is_none());
    }

    #[test]
    fn inline_subcommand_expands_at_load_and_path_sibling_does_not() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "tool",
            r#"{
              "names":["tool"],
              "subcommands":[
                {"names":["now"],"loadSpec":{"names":["now"],"description":"expanded","subcommands":[{"names":["inside"]}]}},
                {"names":["later"],"description":"stub","loadSpec":"later"}
              ]
            }"#,
        );
        write_spec(
            dir.path(),
            "later",
            r#"{"names":["later"],"subcommands":[{"names":["hidden"]}]}"#,
        );
        fs::write(dir.path().join("index.json"), r#"{"files":{"tool":"tool.json"}}"#).unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        let tool = registry.get("tool").expect("tool");
        let now = tool.find_subcommand("now").expect("now");
        assert_eq!(now.description, "expanded");
        assert!(now.find_subcommand("inside").is_some());
        assert!(now.meta.ai_resolved_reference);
        let later = tool.find_subcommand("later").expect("later");
        assert_eq!(later.description, "stub");
        assert!(matches!(later.load_spec, Some(LoadSpec::Path(ref path)) if path == "later"));
        assert!(!later.meta.ai_resolved_reference);
        assert!(later.find_subcommand("hidden").is_none());
    }

    #[test]
    fn referenced_load_spec_occupies_one_lru_slot() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..48 {
            write_spec(dir.path(), &format!("cmd{i}"), &format!(r#"{{"names":["cmd{i}"]}}"#));
        }
        fs::create_dir_all(dir.path().join("gcloud")).unwrap();
        fs::write(
            dir.path().join("gcloud/compute.json"),
            r#"{"names":["compute"],"subcommands":[{"names":["instances"]}]}"#,
        )
        .unwrap();
        let mut files = String::from("{\"files\":{");
        for i in 0..48 {
            if i > 0 {
                files.push(',');
            }
            files.push_str(&format!("\"cmd{i}\":\"cmd{i}.json\""));
        }
        files.push_str("}}");
        fs::write(dir.path().join("index.json"), files).unwrap();
        let mut registry = Registry::load(dir.path()).expect("load");
        for i in 0..48 {
            assert!(registry.get(&format!("cmd{i}")).is_some());
        }
        assert_eq!(registry.loaded_spec_count(), 48);
        let compute = registry.load_referenced_spec("gcloud/compute").expect("compute");
        assert!(compute.find_subcommand("instances").is_some());
        assert_eq!(registry.loaded_spec_count(), 48);
        assert!(!registry.is_cached("cmd0"));
        assert!(registry.is_cached("cmd1"));
        assert!(registry.cached_load_spec("gcloud/compute").is_some());
        assert!(!registry.is_cached("compute"));
        assert!(
            registry
                .command_names_matching("")
                .iter()
                .all(|(name, _)| name != "compute")
        );
    }

    #[test]
    fn referenced_command_file_shares_its_lru_entry_unless_overlaid() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        );
        write_spec(
            dir.path(),
            "host",
            r#"{"names":["host"],"subcommands":[{"names":["git"],"loadSpec":"git"}]}"#,
        );
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"git":"git.json","host":"host.json"}}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).expect("load");
        let host = registry.get_arc("host").expect("host");
        let mut tokens = vec!["host".to_string(), "git".to_string()];
        let walked = crate::lookup::resolve_context(host, &mut tokens, true, "", "", Some(&mut registry));
        assert!(walked.spec.find_subcommand("status").is_some());
        assert_eq!(walked.spec.names, vec!["git"]);
        assert_eq!(registry.loaded_spec_count(), 2);
        assert!(registry.cached_load_spec("git").is_none());

        let overlay = dir.path().join("overlay");
        fs::create_dir(&overlay).unwrap();
        fs::write(
            overlay.join("git.json"),
            r#"{"names":["git"],"description":"overlay","subcommands":[{"names":["from-overlay"]}]}"#,
        )
        .unwrap();
        registry.overlay_specs_dir(&overlay, OverlayMode::Replace);
        let host = registry.get_arc("host").expect("host");
        let mut tokens = vec!["host".to_string(), "git".to_string()];
        let walked = crate::lookup::resolve_context(host, &mut tokens, true, "", "", Some(&mut registry));
        assert!(walked.spec.find_subcommand("status").is_some());
        assert!(walked.spec.find_subcommand("from-overlay").is_none());
        assert_eq!(registry.get("git").expect("overlay").description, "overlay");
    }

    #[test]
    fn command_name_load_spec_follows_the_mapped_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("nested")).unwrap();
        fs::write(
            dir.path().join("nested/alias.json"),
            r#"{"names":["alias-target"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("alias.json"),
            r#"{"names":["decoy"],"subcommands":[{"names":["decoy"]}]}"#,
        )
        .unwrap();
        write_spec(
            dir.path(),
            "host",
            r#"{"names":["host"],"subcommands":[{"names":["alias"],"loadSpec":"alias"}]}"#,
        );
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"alias":"nested/alias.json","host":"host.json"}}"#,
        )
        .unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        let host = registry.get_arc("host").expect("host");
        let mut tokens = vec!["host".to_string(), "alias".to_string()];
        let walked = crate::lookup::resolve_context(host, &mut tokens, true, "", "", Some(&mut registry));
        assert!(walked.spec.find_subcommand("status").is_some());
        assert!(walked.spec.find_subcommand("decoy").is_none());
        assert_eq!(walked.spec.names, vec!["alias"]);
        assert_eq!(registry.loaded_spec_count(), 2);
        assert!(registry.cached_load_spec("alias").is_none());
        assert!(registry.cached_load_spec("nested/alias").is_none());
        assert!(registry.is_cached("alias"));

        let overlay = dir.path().join("overlay");
        fs::create_dir(&overlay).unwrap();
        fs::write(
            overlay.join("alias.json"),
            r#"{"names":["alias"],"description":"overlay","subcommands":[{"names":["from-overlay"]}]}"#,
        )
        .unwrap();
        registry.overlay_specs_dir(&overlay, OverlayMode::Replace);
        let host = registry.get_arc("host").expect("host");
        let mut tokens = vec!["host".to_string(), "alias".to_string()];
        let walked = crate::lookup::resolve_context(host, &mut tokens, true, "", "", Some(&mut registry));
        assert!(walked.spec.find_subcommand("status").is_some());
        assert!(walked.spec.find_subcommand("from-overlay").is_none());
        assert_eq!(registry.get("alias").expect("overlay").description, "overlay");
        assert!(registry.cached_load_spec("nested/alias").is_some());
    }

    #[test]
    fn command_map_loads_versioned_alias_and_hides_nested_implementation_files() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "docker",
            r#"{
              "names":["docker"],
              "subcommands":[
                {"names":["compose"],"loadSpec":"docker-compose"},
                {"names":["domains"],"loadSpec":"gcloud/domains"}
              ]
            }"#,
        );
        write_spec(
            dir.path(),
            "docker-compose",
            r#"{"names":["docker-compose"],"subcommands":[{"names":["up"]}]}"#,
        );
        write_spec(
            dir.path(),
            "gcloud",
            r#"{"names":["gcloud"],"subcommands":[{"names":["domains"],"loadSpec":"gcloud/domains"}]}"#,
        );
        fs::create_dir_all(dir.path().join("gcloud")).unwrap();
        fs::write(
            dir.path().join("gcloud/domains.json"),
            r#"{"names":["domains"],"subcommands":[{"names":["list"]}]}"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("heroku")).unwrap();
        fs::write(
            dir.path().join("heroku/8.0.0.json"),
            r#"{"names":["heroku"],"subcommands":[{"names":["old"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("heroku/8.6.0.json"),
            r#"{"names":["heroku"],"subcommands":[{"names":["new"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("index.json"),
            r#"{"completions":["docker","gcloud","heroku"],"files":{"docker":"docker.json","gcloud":"gcloud.json","heroku":"heroku/8.6.0.json"}}"#,
        )
        .unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        let names: Vec<_> = registry
            .command_names_matching("")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(names.iter().any(|name| name == "heroku"));
        assert!(!names.iter().any(|name| name == "domains"));

        {
            let docker = registry.get("docker").expect("docker");
            let compose = docker.find_subcommand("compose").expect("compose");
            assert!(
                matches!(compose.load_spec, Some(LoadSpec::Path(ref path)) if path == "docker-compose"),
                "compose stays a stub until the walker enters it"
            );
            assert!(compose.find_subcommand("up").is_none());
            let domains = docker.find_subcommand("domains").expect("domains");
            assert!(matches!(domains.load_spec, Some(LoadSpec::Path(ref path)) if path == "gcloud/domains"));
            assert!(domains.find_subcommand("list").is_none());
        }
        let heroku = registry.get("heroku").expect("heroku");
        assert!(heroku.find_subcommand("new").is_some());
        assert!(heroku.find_subcommand("old").is_none());
        assert!(!registry.names.iter().any(|name| name.as_ref() == "domains"));
    }

    #[test]
    fn command_map_keeps_canonical_paths_and_filters_unmapped_spec_aliases() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "appwrite", r#"{"names":["index","internal"]}"#);
        write_spec(
            dir.path(),
            "autojump",
            r#"{"names":["autojump"],"description":"canonical"}"#,
        );
        write_spec(dir.path(), "j", r#"{"names":["autojump"],"description":"alias file"}"#);
        fs::write(
            dir.path().join("index.json"),
            r#"{
              "files": {
                "appwrite": "appwrite.json",
                "index": "appwrite.json",
                "autojump": "autojump.json",
                "j": "j.json"
              }
            }"#,
        )
        .unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        {
            let appwrite = registry.get("appwrite").expect("appwrite");
            assert!(appwrite.has_name("appwrite"));
        }
        assert!(registry.get("internal").is_none());

        {
            let autojump = registry.get("autojump").expect("autojump");
            assert_eq!(autojump.description, "canonical");
        }
        let j = registry.get("j").expect("j");
        assert_eq!(j.description, "alias file");
    }

    #[test]
    fn invalid_command_map_does_not_fallback_to_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("gcloud")).unwrap();
        fs::write(dir.path().join("gcloud/domains.json"), r#"{"names":["domains"]}"#).unwrap();
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"gcloud":"gcloud/missing.json"}}"#,
        )
        .unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        assert!(registry.command_names_matching("").is_empty());
        assert!(registry.get("gcloud").is_none());
        assert!(registry.get("domains").is_none());
    }

    #[test]
    fn node_load_spec_replaces_wrapper_fields_for_pass_and_chezmoi() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "pass",
            r#"{
              "names":["pass"],
              "subcommands":[{
                "names":["grep"],
                "description":"wrapper description",
                "args":[{"name":"pass-name"}],
                "loadSpec":"grep"
              }]
            }"#,
        );
        write_spec(
            dir.path(),
            "grep",
            r#"{
              "names":["grep-target"],
              "description":"loaded description",
              "args":[{"name":"pattern"},{"name":"file"}]
            }"#,
        );
        write_spec(
            dir.path(),
            "chezmoi",
            r#"{
              "names":["chezmoi"],
              "subcommands":[{
                "names":["git"],
                "description":"wrapper description",
                "args":[{"name":"source-dir"}],
                "loadSpec":"git"
              }]
            }"#,
        );
        write_spec(
            dir.path(),
            "git",
            r#"{
              "names":["git-target"],
              "description":"loaded description",
              "args":[{"name":"command"}]
            }"#,
        );
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"pass":"pass.json","chezmoi":"chezmoi.json"}}"#,
        )
        .unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        {
            let pass = registry.get("pass").expect("pass");
            let grep = pass.find_subcommand("grep").expect("grep");
            assert_eq!(grep.names, vec!["grep"]);
            assert_eq!(grep.description, "wrapper description");
            assert!(matches!(grep.load_spec, Some(LoadSpec::Path(ref path)) if path == "grep"));
            assert_eq!(
                grep.args.iter().map(|arg| arg.name.as_str()).collect::<Vec<_>>(),
                vec!["pass-name"]
            );
        }

        let chezmoi = registry.get_arc("chezmoi").expect("chezmoi");
        let git = chezmoi.find_subcommand("git").expect("git");
        assert_eq!(git.description, "wrapper description");
        assert!(matches!(git.load_spec, Some(LoadSpec::Path(ref path)) if path == "git"));

        let mut tokens = vec!["chezmoi".to_string(), "git".to_string()];
        let walked = crate::lookup::resolve_context(chezmoi, &mut tokens, true, "", "", Some(&mut registry));
        assert_eq!(walked.spec.names, vec!["git"]);
        assert_eq!(walked.spec.description, "loaded description");
        assert_eq!(
            walked.spec.args.iter().map(|arg| arg.name.as_str()).collect::<Vec<_>>(),
            vec!["command"]
        );
        let chezmoi = registry.get("chezmoi").expect("chezmoi");
        let git = chezmoi.find_subcommand("git").expect("git");
        assert_eq!(git.description, "wrapper description");
        assert!(git.args.iter().any(|arg| arg.name == "source-dir"));
    }

    #[test]
    fn load_spec_cycle_and_missing_reference_are_safe() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "a",
            r#"{"names":["a"],"subcommands":[{"names":["b"],"loadSpec":"b"},{"names":["missing"],"loadSpec":"not-present"}],"args":[{"name":"missing arg","loadSpec":"not-present"},{"name":"cycle arg","loadSpec":"b"}]}"#,
        );
        write_spec(
            dir.path(),
            "b",
            r#"{"names":["b"],"subcommands":[{"names":["a"],"loadSpec":"a"}],"args":[{"name":"cycle back","loadSpec":"a"}]}"#,
        );
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"a":"a.json","b":"b.json"}}"#,
        )
        .unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        let a = registry.get("a").expect("a");
        let b = a.find_subcommand("b").expect("b");
        assert!(matches!(b.load_spec, Some(LoadSpec::Path(ref path)) if path == "b"));
        assert!(b.find_subcommand("a").is_none());
        assert!(a.find_subcommand("missing").is_some());
        assert!(a.args[0].resolved_spec.is_none());
        assert!(a.args[1].resolved_spec.is_none());
        assert!(matches!(a.args[1].load_spec, Some(LoadSpec::Path(ref path)) if path == "b"));

        let a_arc = registry.get_arc("a").expect("a");
        let mut tokens = vec!["a".to_string(), "missing-value".to_string(), "cycle-value".to_string()];
        let walked = crate::lookup::resolve_context(a_arc, &mut tokens, true, "", "", Some(&mut registry));
        assert_eq!(walked.spec.names, vec!["b"]);
        assert!(walked.spec.find_subcommand("a").is_some());
        assert!(walked.spec.args[0].resolved_spec.is_none());
        assert!(registry.is_cached("b"));
        let a_after = registry.get("a").expect("a");
        assert!(a_after.args[1].resolved_spec.is_none());

        let a_arc = registry.get_arc("a").expect("a");
        let mut tokens = vec![
            "a".to_string(),
            "missing-value".to_string(),
            "cycle-value".to_string(),
            "back".to_string(),
        ];
        let walked = crate::lookup::resolve_context(a_arc, &mut tokens, true, "", "", Some(&mut registry));
        assert_eq!(walked.spec.names, vec!["a"]);
        assert!(walked.spec.find_subcommand("b").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn changed_nested_snapshot_file_does_not_cache_a_partial_parent() {
        fn write_generation(root: &Path, nested_description: &str) {
            fs::create_dir_all(root).unwrap();
            fs::write(
                root.join("demo.json"),
                r#"{"names":["demo"],"subcommands":[{"names":["child"],"loadSpec":"nested"}]}"#,
            )
            .unwrap();
            fs::write(
                root.join("nested.json"),
                serde_json::json!({
                    "names": ["nested"],
                    "description": nested_description,
                })
                .to_string(),
            )
            .unwrap();
            fs::write(root.join("plain.json"), r#"{"names":["plain"],"description":"same"}"#).unwrap();
            fs::write(
                root.join("index.json"),
                r#"{"files":{"demo":"demo.json","plain":"plain.json"}}"#,
            )
            .unwrap();
        }

        let root = tempfile::tempdir().unwrap();
        let generation_a = root.path().join("generation-a");
        let generation_b = root.path().join("generation-b");
        write_generation(&generation_a, "generation A");
        write_generation(&generation_b, "generation B");
        let canonical = root.path().join("specs-ir");
        let backup = root.path().join("backup");
        fs::rename(&generation_a, &canonical).unwrap();
        let mut registry = Registry::load(&canonical).expect("generation A registry");

        // The parent bytes are identical, but its nested loadSpec differs.
        // Loading the parent no longer parses that child. Caching the stub
        // anyway would keep this generation until LRU eviction, so `get`
        // fails closed. A file with no deferred path still loads when its
        // own bytes match.
        fs::rename(&canonical, &backup).unwrap();
        fs::rename(&generation_b, &canonical).unwrap();
        assert!(registry.get("demo").is_none());
        assert!(!registry.is_cached("demo"));
        assert!(registry.cached_load_spec("nested").is_none());
        let plain = registry.get("plain").expect("identical file with no deferred loadSpec");
        assert_eq!(plain.description, "same");
        assert!(registry.needs_refresh());
    }

    #[cfg(unix)]
    #[test]
    fn changed_argument_load_spec_does_not_cache_parent() {
        fn write_generation(root: &Path, nested_description: &str) {
            fs::create_dir_all(root).unwrap();
            fs::write(
                root.join("demo.json"),
                r#"{"names":["demo"],"args":[{"name":"input","loadSpec":"nested"}]}"#,
            )
            .unwrap();
            fs::write(
                root.join("nested.json"),
                serde_json::json!({
                    "names": ["nested"],
                    "description": nested_description,
                })
                .to_string(),
            )
            .unwrap();
            fs::write(root.join("index.json"), r#"{"files":{"demo":"demo.json"}}"#).unwrap();
        }

        let root = tempfile::tempdir().unwrap();
        let generation_a = root.path().join("generation-a");
        let generation_b = root.path().join("generation-b");
        write_generation(&generation_a, "generation A");
        write_generation(&generation_b, "generation B");
        let canonical = root.path().join("specs-ir");
        let backup = root.path().join("backup");
        fs::rename(&generation_a, &canonical).unwrap();
        let mut registry = Registry::load(&canonical).expect("generation A registry");
        fs::rename(&canonical, &backup).unwrap();
        fs::rename(&generation_b, &canonical).unwrap();
        assert!(registry.get("demo").is_none());
        assert!(!registry.is_cached("demo"));
        assert!(registry.cached_load_spec("nested").is_none());
        assert!(registry.needs_refresh());
    }

    #[test]
    fn resolves_static_argument_load_specs_for_args_and_options() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "tool",
            r#"{
              "names":["tool"],
              "args":[{"name":"datasource","loadSpec":"arg-target"}],
              "options":[{"names":["--config"],"args":[{"name":"file","loadSpec":"option-target"}]}]
            }"#,
        );
        write_spec(
            dir.path(),
            "arg-target",
            r#"{"names":["arg-target"],"subcommands":[{"names":["list"]}]}"#,
        );
        write_spec(
            dir.path(),
            "option-target",
            r#"{"names":["option-target"],"subcommands":[{"names":["show"]}]}"#,
        );

        let mut registry = Registry::load(dir.path()).expect("load");
        {
            let tool = registry.get("tool").expect("tool");
            assert!(tool.args[0].resolved_spec.is_none());
            assert!(tool.options[0].args[0].resolved_spec.is_none());
            assert!(matches!(tool.args[0].load_spec, Some(LoadSpec::Path(ref path)) if path == "arg-target"));
            assert!(
                matches!(tool.options[0].args[0].load_spec, Some(LoadSpec::Path(ref path)) if path == "option-target")
            );
        }
        assert!(!registry.is_cached("arg-target"));
        assert!(!registry.is_cached("option-target"));

        let tool = registry.get_arc("tool").expect("tool");
        let mut typing = vec!["tool".to_string(), "ds".to_string()];
        let _typing = crate::lookup::resolve_context(tool, &mut typing, false, "ds", "ds", Some(&mut registry));
        assert!(!registry.is_cached("arg-target"));

        let tool = registry.get_arc("tool").expect("tool");
        let mut entered = vec!["tool".to_string(), "ds".to_string()];
        let walked = crate::lookup::resolve_context(tool, &mut entered, true, "", "", Some(&mut registry));
        assert!(walked.spec.find_subcommand("list").is_some());
        assert!(registry.is_cached("arg-target"));
        assert!(!registry.is_cached("option-target"));
        assert!(registry.get("tool").expect("tool").args[0].resolved_spec.is_none());
    }

    #[test]
    fn resolves_inline_argument_load_spec_without_executing_code() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "dscl",
            r#"{
              "names":["dscl"],
              "args":[{"name":"datasource","loadSpec":{"names":["dscl"],"subcommands":[{"names":["list"]}]}}]
            }"#,
        );
        let mut registry = Registry::load(dir.path()).expect("load");
        let dscl = registry.get("dscl").expect("dscl");
        let Some(LoadSpec::Inline(loaded)) = dscl.args[0].load_spec.as_ref() else {
            panic!("expected inline loadSpec");
        };
        assert!(loaded.find_subcommand("list").is_some());
        let resolved = dscl.args[0].resolved_spec.as_deref().expect("resolved inline loadSpec");
        assert!(resolved.find_subcommand("list").is_some());
    }

    #[derive(Clone)]
    struct WarnCounter {
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        needle: &'static str,
    }

    struct WarnVisitor<'a> {
        counter: &'a WarnCounter,
    }

    impl tracing::field::Visit for WarnVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" && format!("{value:?}").contains(self.counter.needle) {
                self.counter.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" && value.contains(self.counter.needle) {
                self.counter.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    impl tracing::Subscriber for WarnCounter {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::WARN
        }

        fn register_callsite(&self, _: &'static tracing::Metadata<'static>) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::sometimes()
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            event.record(&mut WarnVisitor { counter: self });
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    fn count_load_spec_warns(needle: &'static str, body: impl FnOnce()) -> usize {
        let counter = WarnCounter {
            hits: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            needle,
        };
        let probe = counter.clone();
        tracing::subscriber::with_default(counter, || {
            tracing::callsite::rebuild_interest_cache();
            body();
        });
        probe.hits.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn missing_subcommand_load_spec_keeps_the_stub_and_warns_once() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "tool",
            r#"{
              "names":["tool"],
              "subcommands":[
                {"names":["gone"],"description":"kept","loadSpec":"not-present","subcommands":[{"names":["still-here"]}]},
                {"names":["stay"]}
              ]
            }"#,
        );
        fs::write(dir.path().join("index.json"), r#"{"files":{"tool":"tool.json"}}"#).unwrap();
        let mut registry = Registry::load(dir.path()).expect("load");
        let tool = registry.get_arc("tool").expect("tool");
        let warns = count_load_spec_warns("loadSpec target missing", || {
            let mut tokens = vec!["tool".to_string(), "gone".to_string()];
            let walked = crate::lookup::resolve_context(tool, &mut tokens, true, "", "", Some(&mut registry));
            assert_eq!(walked.spec.names, vec!["gone"]);
            assert_eq!(walked.spec.description, "kept");
            assert!(walked.spec.find_subcommand("still-here").is_some());
            assert!(walked.spec.find_subcommand("stay").is_none());
            assert!(!walked.spec.meta.ai_resolved_reference);
        });
        assert_eq!(warns, 1);
        assert!(registry.is_cached("tool"));
        assert!(registry.cached_load_spec("not-present").is_none());
        let tool = registry.get("tool").expect("tool");
        let gone = tool.find_subcommand("gone").expect("stub");
        assert_eq!(gone.description, "kept");
        assert!(gone.find_subcommand("still-here").is_some());
        assert!(matches!(gone.load_spec, Some(LoadSpec::Path(ref path)) if path == "not-present"));
    }

    #[test]
    fn corrupt_subcommand_load_spec_keeps_the_stub_and_warns_once() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "tool",
            r#"{
              "names":["tool"],
              "subcommands":[
                {"names":["broken"],"description":"kept","loadSpec":"broken","subcommands":[{"names":["still-here"]}]}
              ]
            }"#,
        );
        fs::write(dir.path().join("broken.json"), "{").unwrap();
        fs::write(dir.path().join("index.json"), r#"{"files":{"tool":"tool.json"}}"#).unwrap();
        let mut registry = Registry::load(dir.path()).expect("load");
        let tool = registry.get_arc("tool").expect("tool");
        let warns = count_load_spec_warns("loadSpec target failed", || {
            let mut tokens = vec!["tool".to_string(), "broken".to_string()];
            let walked = crate::lookup::resolve_context(tool, &mut tokens, true, "", "", Some(&mut registry));
            assert_eq!(walked.spec.names, vec!["broken"]);
            assert_eq!(walked.spec.description, "kept");
            assert!(walked.spec.find_subcommand("still-here").is_some());
            assert!(!walked.spec.meta.ai_resolved_reference);
        });
        assert_eq!(warns, 1);
        assert!(registry.is_cached("tool"));
        assert!(registry.cached_load_spec("broken").is_none());
        let tool = registry.get("tool").expect("tool");
        let broken = tool.find_subcommand("broken").expect("stub");
        assert!(matches!(broken.load_spec, Some(LoadSpec::Path(ref path)) if path == "broken"));
        assert!(broken.find_subcommand("still-here").is_some());
    }

    #[test]
    fn root_load_spec_cycle_stops_on_the_load_stack() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "wrapper",
            r#"{"names":["wrapper"],"description":"wrapper description","loadSpec":"a"}"#,
        );
        write_spec(
            dir.path(),
            "a",
            r#"{"names":["a"],"description":"from-a","loadSpec":"b"}"#,
        );
        write_spec(
            dir.path(),
            "b",
            r#"{"names":["b"],"description":"from-b","loadSpec":"a","subcommands":[{"names":["leaf"]}]}"#,
        );
        fs::write(dir.path().join("index.json"), r#"{"files":{"wrapper":"wrapper.json"}}"#).unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        let wrapper = registry.get("wrapper").expect("wrapper");
        assert_eq!(wrapper.names, vec!["wrapper"]);
        assert_eq!(wrapper.description, "from-b");
        assert!(wrapper.find_subcommand("leaf").is_some());
        assert!(wrapper.meta.ai_resolved_reference);
        assert!(wrapper.load_spec.is_none());
        assert!(registry.get("a").is_none());
        assert!(registry.get("b").is_none());
        assert!(registry.cached_load_spec("a").is_none());
        assert!(registry.cached_load_spec("b").is_none());
    }

    #[test]
    fn subcommand_load_spec_cycle_reenters_one_file_per_token() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "host",
            r#"{"names":["host"],"subcommands":[{"names":["a"],"loadSpec":"a"}]}"#,
        );
        write_spec(
            dir.path(),
            "a",
            r#"{"names":["a-file"],"description":"from-a","subcommands":[{"names":["b"],"loadSpec":"b"}]}"#,
        );
        write_spec(
            dir.path(),
            "b",
            r#"{"names":["b-file"],"description":"from-b","subcommands":[{"names":["a"],"loadSpec":"a"}]}"#,
        );
        fs::write(dir.path().join("index.json"), r#"{"files":{"host":"host.json"}}"#).unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        let host = registry.get_arc("host").expect("host");
        let mut tokens = vec!["host".to_string(), "a".to_string()];
        let first = crate::lookup::resolve_context(host, &mut tokens, true, "", "", Some(&mut registry));
        assert_eq!(first.spec.names, vec!["a"]);
        assert_eq!(first.spec.description, "from-a");
        assert!(first.spec.find_subcommand("b").is_some());
        let cached_a = registry.cached_load_spec("a").expect("a file");
        assert_eq!(registry.loaded_spec_count(), 2);

        let host = registry.get_arc("host").expect("host");
        let mut tokens = vec!["host".to_string(), "a".to_string(), "b".to_string(), "a".to_string()];
        let again = crate::lookup::resolve_context(host, &mut tokens, true, "", "", Some(&mut registry));
        assert_eq!(again.spec.names, vec!["a"]);
        assert_eq!(again.spec.description, "from-a");
        assert!(again.spec.find_subcommand("b").is_some());
        assert!(std::sync::Arc::ptr_eq(
            &cached_a,
            &registry.cached_load_spec("a").expect("same a file")
        ));
        assert!(registry.cached_load_spec("b").is_some());
        assert_eq!(registry.loaded_spec_count(), 3);
        let host = registry.get("host").expect("host");
        let stub = host.find_subcommand("a").expect("stub");
        assert!(matches!(stub.load_spec, Some(LoadSpec::Path(ref path)) if path == "a"));
        assert!(stub.find_subcommand("b").is_none());
    }

    #[test]
    fn versioned_spec_loads_only_the_resolved_file_and_defers_its_child() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("heroku")).unwrap();
        fs::write(
            dir.path().join("heroku/8.0.0.json"),
            r#"{
              "names":["heroku"],
              "subcommands":[
                {"names":["old"],"loadSpec":"heroku/old"},
                {"names":["apps"]}
              ]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("heroku/old.json"),
            r#"{"names":["old"],"subcommands":[{"names":["list"]}]}"#,
        )
        .unwrap();
        fs::write(dir.path().join("heroku/8.6.0.json"), "{").unwrap();
        fs::write(
            dir.path().join("index.json"),
            r#"{
              "completions":["heroku"],
              "files":{"heroku":"heroku/8.6.0.json"},
              "versioned":{
                "heroku":{
                  "command":["heroku","--version"],
                  "parse":"after-first-space",
                  "fallback":"8.6.0",
                  "files":{"8.0.0":"heroku/8.0.0.json","8.6.0":"heroku/8.6.0.json"}
                }
              }
            }"#,
        )
        .unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        let _guard = crate::process::mock::install(vec![crate::process::mock::ExecRule {
            command: Some("heroku".into()),
            args: Some(vec!["--version".into()]),
            stdout: "heroku 8.0.1".into(),
            ..crate::process::mock::ExecRule::default()
        }]);
        let spec = registry
            .get_versioned_arc("heroku", "/", std::time::Duration::from_secs(5))
            .expect("resolved 8.0.0 file");
        assert!(spec.find_subcommand("apps").is_some());
        assert!(spec.find_subcommand("new").is_none());
        let old = spec.find_subcommand("old").expect("old stub");
        assert!(matches!(old.load_spec, Some(LoadSpec::Path(ref path)) if path == "heroku/old"));
        assert!(old.find_subcommand("list").is_none());
        assert!(registry.cached_versioned_spec("heroku/8.0.0.json").is_some());
        assert!(registry.cached_versioned_spec("heroku/8.6.0.json").is_none());
        assert!(registry.cached_load_spec("heroku/old").is_none());
        assert_eq!(registry.loaded_spec_count(), 0);

        let result = crate::lookup::complete(
            &mut registry,
            &crate::runtime::CompleteRequest {
                buffer: "heroku old ".into(),
                include_history: false,
                ..crate::runtime::CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"list"), "{names:?}");
        assert!(!names.contains(&"apps"), "{names:?}");
        assert!(registry.cached_load_spec("heroku/old").is_some());
        assert!(registry.cached_versioned_spec("heroku/8.6.0.json").is_none());
        assert_eq!(registry.loaded_spec_count(), 1);
        let spec = registry
            .cached_versioned_spec("heroku/8.0.0.json")
            .expect("version file stays put");
        let old = spec.find_subcommand("old").expect("stub");
        assert!(matches!(old.load_spec, Some(LoadSpec::Path(ref path)) if path == "heroku/old"));
        assert!(old.find_subcommand("list").is_none());
    }

    fn past_grace(now: Instant) -> Instant {
        now.checked_sub(SPEC_IDLE_GRACE).expect("grace fits in the clock")
    }

    #[test]
    fn idle_release_drops_only_the_expired_file() {
        assert_eq!(SPEC_IDLE_GRACE, Duration::from_secs(10));
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "alpha",
            r#"{"names":["alpha"],"subcommands":[{"names":["one"]}]}"#,
        );
        write_spec(
            dir.path(),
            "beta",
            r#"{"names":["beta"],"subcommands":[{"names":["two"]}]}"#,
        );
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"alpha":"alpha.json","beta":"beta.json"}}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let alpha = registry.get_arc("alpha").unwrap();
        let beta = registry.get_arc("beta").unwrap();
        let now = Instant::now();
        registry.set_idle_since("beta", Some(past_grace(now)));
        registry.release_idle(now, SPEC_IDLE_GRACE);

        assert!(registry.is_cached("alpha"));
        assert!(Arc::ptr_eq(&alpha, &registry.get_arc("alpha").unwrap()));
        assert!(!registry.is_cached("beta"));
        assert!(registry.cached_load_spec("beta").is_none());
        assert_eq!(registry.loaded_spec_count(), 1);
        assert!(registry.loaded.iter().all(|spec| !Arc::ptr_eq(spec, &beta)));
        drop(beta);
    }

    #[test]
    fn idle_release_keeps_the_parent_stub_and_reloads_a_new_child_arc() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "tool",
            r#"{"names":["tool"],"subcommands":[{"names":["child"],"loadSpec":"child"}]}"#,
        );
        write_spec(
            dir.path(),
            "child",
            r#"{"names":["child"],"subcommands":[{"names":["leaf"]}]}"#,
        );
        fs::write(dir.path().join("index.json"), r#"{"files":{"tool":"tool.json"}}"#).unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let parent = registry.get_arc("tool").unwrap();
        let child = registry.load_referenced_spec("child").unwrap();
        assert!(registry.cached_load_spec("child").is_some());
        assert!(parent.find_subcommand("child").unwrap().load_spec.is_some());

        let now = Instant::now();
        registry.set_idle_since("child.json", Some(past_grace(now)));
        registry.release_idle(now, SPEC_IDLE_GRACE);

        assert!(Arc::ptr_eq(&parent, &registry.get_arc("tool").unwrap()));
        let stub = registry
            .get_arc("tool")
            .unwrap()
            .find_subcommand("child")
            .unwrap()
            .clone();
        assert!(matches!(stub.load_spec, Some(LoadSpec::Path(ref path)) if path == "child"));
        assert!(registry.cached_load_spec("child").is_none());
        assert!(registry.loaded.iter().all(|spec| !Arc::ptr_eq(spec, &child)));

        let reloaded = registry.load_referenced_spec("child").unwrap();
        assert!(!Arc::ptr_eq(&child, &reloaded));
        assert!(reloaded.find_subcommand("leaf").is_some());
    }

    #[test]
    fn command_path_idle_release_drops_the_shared_lru_entry() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "tool",
            r#"{"names":["tool"],"subcommands":[{"names":["run"]}]}"#,
        );
        fs::write(dir.path().join("index.json"), r#"{"files":{"tool":"tool.json"}}"#).unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let shared = registry.load_referenced_spec("tool").unwrap();
        assert!(registry.cached_load_spec("tool").is_none());
        assert!(registry.is_cached("tool"));

        let now = Instant::now();
        registry.set_idle_since("tool", Some(past_grace(now)));
        registry.release_idle(now, SPEC_IDLE_GRACE);

        assert!(!registry.is_cached("tool"));
        assert!(registry.cached_load_spec("tool").is_none());
        assert!(registry.loaded.iter().all(|spec| !Arc::ptr_eq(spec, &shared)));
        let reloaded = registry.get_arc("tool").unwrap();
        assert!(!Arc::ptr_eq(&shared, &reloaded));
        assert!(reloaded.find_subcommand("run").is_some());
    }

    fn idle_identity_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "tool", r#"{"names":["tool"],"description":"bundled"}"#);
        fs::write(dir.path().join("index.json"), r#"{"files":{"tool":"tool.json"}}"#).unwrap();
        dir
    }

    #[test]
    fn replace_drops_only_the_displaced_lru_only_tree_and_its_options() {
        let dir = idle_identity_fixture();
        write_spec(
            dir.path(),
            "tool",
            r#"{"names":["tool"],"options":[{"names":["--old"]}]}"#,
        );
        let mut registry = Registry::load(dir.path()).unwrap();
        let old = registry.get_arc("tool").unwrap();
        let old_weak = Arc::downgrade(&old);
        let option_weak = Arc::downgrade(&old.options[0]);
        drop(old);
        registry.insert(Spec {
            names: vec!["unrelated".into()],
            ..Spec::default()
        });
        let unrelated = registry.get_arc("unrelated").unwrap();

        registry.overlay_spec(
            Spec {
                names: vec!["tool".into()],
                ..Spec::default()
            },
            OverlayMode::Replace,
        );

        assert!(old_weak.upgrade().is_none());
        assert!(option_weak.upgrade().is_none());
        assert!(registry.option_pool.is_empty());
        assert_eq!(registry.loaded_spec_count(), 1);
        assert!(Arc::ptr_eq(&unrelated, &registry.get_arc("unrelated").unwrap()));
        let pinned_weak = Arc::downgrade(&registry.get_arc("tool").unwrap());
        registry.overlay_spec(
            Spec {
                names: vec!["tool".into()],
                description: "second overlay".into(),
                ..Spec::default()
            },
            OverlayMode::Replace,
        );
        // Pinned overlay lifetime remains the lifetime of the registry.
        assert!(pinned_weak.upgrade().is_some());
        assert_eq!(registry.get("tool").unwrap().description, "second overlay");
    }

    #[test]
    fn replace_keeps_the_same_paths_versioned_tree_and_deadline() {
        let dir = idle_identity_fixture();
        let mut registry = Registry::load(dir.path()).unwrap();
        let ordinary = registry.get_arc("tool").unwrap();
        let ordinary_weak = Arc::downgrade(&ordinary);
        let versioned = registry.load_relative_spec("tool.json", "tool").unwrap();
        let versioned_weak = Arc::downgrade(&versioned);
        drop((ordinary, versioned));
        registry.begin_idle_completion();
        let now = Instant::now();
        registry.note_idle_after_complete(now);

        registry.overlay_spec(
            Spec {
                names: vec!["tool".into()],
                ..Spec::default()
            },
            OverlayMode::Replace,
        );

        assert!(ordinary_weak.upgrade().is_none());
        assert!(versioned_weak.upgrade().is_some());
        assert_eq!(registry.idle_mark("tool"), Some(Some(now)));
        assert_eq!(
            registry.next_idle_deadline(SPEC_IDLE_GRACE),
            Some(now + SPEC_IDLE_GRACE)
        );
        registry.release_idle(now + SPEC_IDLE_GRACE, SPEC_IDLE_GRACE);
        assert!(versioned_weak.upgrade().is_none());
        assert!(registry.is_cached("tool"));
    }

    #[test]
    fn idle_release_drops_ordinary_and_versioned_trees_for_the_same_path() {
        let dir = idle_identity_fixture();
        let mut registry = Registry::load(dir.path()).unwrap();
        let ordinary = registry.get_arc("tool").unwrap();
        let versioned = registry.load_relative_spec("tool.json", "tool").unwrap();
        assert!(!Arc::ptr_eq(&ordinary, &versioned));
        let ordinary_weak = Arc::downgrade(&ordinary);
        let versioned_weak = Arc::downgrade(&versioned);
        drop((ordinary, versioned));
        registry.begin_idle_completion();
        let now = Instant::now();
        registry.note_idle_after_complete(now);

        registry.release_idle(now + SPEC_IDLE_GRACE, SPEC_IDLE_GRACE);

        assert!(!registry.is_cached("tool"));
        assert!(registry.cached_versioned_spec("tool.json").is_none());
        assert_eq!(registry.loaded_spec_count(), 0);
        assert!(registry.idle_mark("tool").is_none());
        assert!(registry.next_idle_deadline(SPEC_IDLE_GRACE).is_none());
        assert!(ordinary_weak.upgrade().is_none());
        assert!(versioned_weak.upgrade().is_none());
    }

    #[test]
    fn pinned_command_does_not_renew_its_bundled_load_spec() {
        let dir = idle_identity_fixture();
        let mut registry = Registry::load(dir.path()).unwrap();
        registry.overlay_spec(
            Spec {
                names: vec!["tool".into()],
                description: "overlay".into(),
                ..Spec::default()
            },
            OverlayMode::Replace,
        );
        let bundled = registry.load_referenced_spec("tool").unwrap();
        assert_eq!(bundled.description, "bundled");
        let bundled_weak = Arc::downgrade(&bundled);
        drop(bundled);
        let now = Instant::now();
        registry.note_idle_after_complete(now);
        assert_eq!(registry.idle_mark("tool"), Some(None));

        registry.begin_idle_completion();
        let pinned = registry.get_arc("tool").unwrap();
        assert_eq!(pinned.description, "overlay");
        registry.note_idle_after_complete(now);
        assert_eq!(registry.idle_mark("tool"), Some(Some(now)));
        registry.begin_idle_completion();
        registry.get_arc("tool").unwrap();
        registry.note_idle_after_complete(now + Duration::from_secs(5));
        assert_eq!(registry.idle_mark("tool"), Some(Some(now)));

        registry.release_idle(now + SPEC_IDLE_GRACE, SPEC_IDLE_GRACE);
        assert!(registry.cached_load_spec("tool").is_none());
        assert!(bundled_weak.upgrade().is_none());
        assert!(Arc::ptr_eq(&pinned, &registry.get_arc("tool").unwrap()));
    }

    #[test]
    fn lru_eviction_keeps_the_sibling_versioned_trees_original_deadline() {
        let dir = idle_identity_fixture();
        let mut registry = Registry::load(dir.path()).unwrap();
        registry.overlay_spec(
            Spec {
                names: vec!["tool".into()],
                ..Spec::default()
            },
            OverlayMode::Replace,
        );
        // A pinned command makes the ordinary file use load_spec_cache,
        // which used to hide its versioned sibling when forgetting a mark.
        registry.load_referenced_spec("tool").unwrap();
        let versioned = registry.load_relative_spec("tool.json", "tool").unwrap();
        let versioned_weak = Arc::downgrade(&versioned);
        drop(versioned);
        registry.begin_idle_completion();
        let now = Instant::now();
        registry.note_idle_after_complete(now);

        registry.evict_oldest_spec();

        assert!(registry.is_cached("tool"));
        assert!(registry.cached_load_spec("tool").is_none());
        assert!(registry.cached_versioned_spec("tool.json").is_some());
        assert_eq!(registry.idle_mark("tool"), Some(Some(now)));
        assert_eq!(
            registry.next_idle_deadline(SPEC_IDLE_GRACE),
            Some(now + SPEC_IDLE_GRACE)
        );
        registry.release_idle(now + SPEC_IDLE_GRACE, SPEC_IDLE_GRACE);
        assert!(registry.cached_versioned_spec("tool.json").is_none());
        assert!(versioned_weak.upgrade().is_none());
        assert!(registry.idle_mark("tool").is_none());
    }

    #[test]
    fn idle_release_checks_all_cached_aliases_despite_a_pinned_alias() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "tool", r#"{"names":["tool","alias","other"]}"#);
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"tool":"tool.json","alias":"tool.json","other":"tool.json"}}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let bundled = registry.get_arc("tool").unwrap();
        assert!(Arc::ptr_eq(&bundled, &registry.get_arc("alias").unwrap()));
        assert!(Arc::ptr_eq(&bundled, &registry.get_arc("other").unwrap()));
        let bundled_weak = Arc::downgrade(&bundled);
        drop(bundled);
        registry.begin_idle_completion();
        let now = Instant::now();
        registry.note_idle_after_complete(now);
        registry.overlay_spec(
            Spec {
                names: vec!["tool".into()],
                ..Spec::default()
            },
            OverlayMode::Replace,
        );
        assert!(bundled_weak.upgrade().is_some());
        assert_eq!(registry.loaded_spec_count(), 1);
        assert_eq!(registry.idle_mark("tool"), Some(Some(now)));
        registry.begin_idle_completion();
        let pinned = registry.get_arc("tool").unwrap();
        registry.note_idle_after_complete(now + Duration::from_millis(1));
        assert_eq!(registry.idle_mark("tool"), Some(Some(now)));
        assert_eq!(
            registry.next_idle_deadline(SPEC_IDLE_GRACE),
            Some(now + SPEC_IDLE_GRACE)
        );

        registry.release_idle(now + SPEC_IDLE_GRACE, SPEC_IDLE_GRACE);

        assert!(!registry.is_cached("alias"));
        assert!(!registry.is_cached("other"));
        assert!(bundled_weak.upgrade().is_none());
        assert_eq!(registry.loaded_spec_count(), 0);
        assert!(registry.idle_mark("tool").is_none());
        assert!(Arc::ptr_eq(&pinned, &registry.get_arc("tool").unwrap()));
    }

    #[test]
    fn idle_release_leaves_pinned_overlays() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "tool", r#"{"names":["tool"],"description":"bundled"}"#);
        fs::write(dir.path().join("index.json"), r#"{"files":{"tool":"tool.json"}}"#).unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        registry.get("tool").unwrap();
        registry.overlay_spec(
            Spec {
                names: vec!["tool".into()],
                description: "overlay".into(),
                ..Spec::default()
            },
            OverlayMode::Replace,
        );
        let pinned = registry.get_arc("tool").unwrap();
        assert_eq!(pinned.description, "overlay");

        let now = Instant::now();
        registry.set_idle_since("tool.json", Some(past_grace(now)));
        registry.release_idle(now, SPEC_IDLE_GRACE);
        let after = registry.get_arc("tool").unwrap();
        assert!(Arc::ptr_eq(&pinned, &after));
        assert_eq!(after.description, "overlay");
    }

    #[test]
    fn idle_release_drops_option_bodies_with_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "tool",
            r#"{"names":["tool"],"options":[{"names":["--flag"],"description":"flag"}]}"#,
        );
        fs::write(dir.path().join("index.json"), r#"{"files":{"tool":"tool.json"}}"#).unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        registry.get("tool").unwrap();
        let weaks: Vec<Weak<OptionSpec>> = registry
            .option_pool
            .values()
            .flat_map(|bucket| bucket.iter().cloned())
            .collect();
        assert!(!weaks.is_empty());
        assert!(weaks.iter().all(|weak| weak.strong_count() > 0));

        let now = Instant::now();
        registry.set_idle_since("tool", Some(past_grace(now)));
        registry.release_idle(now, SPEC_IDLE_GRACE);

        assert!(weaks.iter().all(|weak| weak.strong_count() == 0));
        assert!(registry.option_pool.is_empty());
    }

    #[test]
    fn idle_release_drops_versioned_files_and_keeps_the_detected_version() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("tool")).unwrap();
        fs::write(
            dir.path().join("tool/1.0.0.json"),
            r#"{"names":["tool"],"subcommands":[{"names":["old"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("index.json"),
            r#"{
              "files":{"tool":"tool/1.0.0.json"},
              "versioned":{
                "tool":{
                  "command":["tool","--version"],
                  "parse":"after-first-space",
                  "fallback":"1.0.0",
                  "files":{"1.0.0":"tool/1.0.0.json"}
                }
              }
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let _guard = crate::process::mock::install(vec![crate::process::mock::ExecRule {
            command: Some("tool".into()),
            args: Some(vec!["--version".into()]),
            stdout: "tool 1.0.0".into(),
            ..crate::process::mock::ExecRule::default()
        }]);
        registry.get_versioned_arc("tool", "/", Duration::from_secs(5)).unwrap();
        let version = registry.version_cache.get("tool").cloned();
        assert!(version.is_some());
        assert!(registry.cached_versioned_spec("tool/1.0.0.json").is_some());

        let now = Instant::now();
        registry.set_idle_since("tool/1.0.0.json", Some(past_grace(now)));
        registry.release_idle(now, SPEC_IDLE_GRACE);

        assert!(registry.cached_versioned_spec("tool/1.0.0.json").is_none());
        assert_eq!(registry.version_cache.get("tool").cloned(), version);
    }

    #[test]
    fn idle_release_waits_out_the_grace_and_ignores_an_active_mark() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "tool", r#"{"names":["tool"]}"#);
        fs::write(dir.path().join("index.json"), r#"{"files":{"tool":"tool.json"}}"#).unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        registry.get("tool").unwrap();
        let now = Instant::now();

        registry.set_idle_since("tool", Some(now));
        registry.release_idle(now, SPEC_IDLE_GRACE);
        assert!(registry.is_cached("tool"));

        let within = now
            .checked_sub(SPEC_IDLE_GRACE - Duration::from_millis(1))
            .expect("the grace fits in the clock");
        registry.set_idle_since("tool", Some(within));
        registry.release_idle(now, SPEC_IDLE_GRACE);
        assert!(registry.is_cached("tool"));

        registry.set_idle_since("tool", None);
        registry.release_idle(now + Duration::from_secs(3_600), SPEC_IDLE_GRACE);
        assert!(registry.is_cached("tool"));
    }

    #[test]
    fn inserted_specs_without_a_path_are_not_idle_released() {
        let mut registry = Registry::new();
        registry.insert(Spec {
            names: vec!["solo".into()],
            ..Spec::default()
        });
        let now = Instant::now();
        registry.set_idle_since("solo.json", Some(past_grace(now)));
        registry.release_idle(now, SPEC_IDLE_GRACE);
        assert!(registry.is_cached("solo"));
    }

    #[test]
    fn lru_eviction_forgets_an_idle_mark_before_the_file_is_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..49 {
            write_spec(dir.path(), &format!("cmd{i}"), &format!(r#"{{"names":["cmd{i}"]}}"#));
        }
        let mut files = String::from("{\"files\":{");
        for i in 0..49 {
            if i > 0 {
                files.push(',');
            }
            files.push_str(&format!("\"cmd{i}\":\"cmd{i}.json\""));
        }
        files.push_str("}}");
        fs::write(dir.path().join("index.json"), files).unwrap();

        let mut registry = Registry::load(dir.path()).unwrap();
        registry.get("cmd0").unwrap();
        let now = Instant::now();
        registry.set_idle_since("cmd0", Some(past_grace(now)));
        for i in 1..49 {
            assert!(registry.get(&format!("cmd{i}")).is_some());
        }
        assert!(!registry.is_cached("cmd0"));
        assert!(!registry.idle_since.contains_key(Path::new("cmd0.json")));

        let reloaded = registry.get_arc("cmd0").unwrap();
        registry.release_idle(now, SPEC_IDLE_GRACE);
        assert!(Arc::ptr_eq(&reloaded, &registry.get_arc("cmd0").unwrap()));
    }

    #[test]
    fn lru_eviction_of_a_path_only_load_spec_forgets_its_idle_mark() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..48 {
            write_spec(dir.path(), &format!("cmd{i}"), &format!(r#"{{"names":["cmd{i}"]}}"#));
        }
        fs::create_dir_all(dir.path().join("gcloud")).unwrap();
        fs::write(
            dir.path().join("gcloud/compute.json"),
            r#"{"names":["compute"],"subcommands":[{"names":["instances"]}]}"#,
        )
        .unwrap();
        let mut files = String::from("{\"files\":{");
        for i in 0..48 {
            if i > 0 {
                files.push(',');
            }
            files.push_str(&format!("\"cmd{i}\":\"cmd{i}.json\""));
        }
        files.push_str("}}");
        fs::write(dir.path().join("index.json"), files).unwrap();

        let mut registry = Registry::load(dir.path()).unwrap();
        for i in 0..48 {
            assert!(registry.get(&format!("cmd{i}")).is_some());
        }
        registry.load_referenced_spec("gcloud/compute").unwrap();
        let now = Instant::now();
        registry.set_idle_since("gcloud/compute.json", Some(past_grace(now)));
        for i in 1..48 {
            assert!(registry.get(&format!("cmd{i}")).is_some());
        }
        assert!(registry.get("cmd0").is_some());
        assert!(registry.cached_load_spec("gcloud/compute").is_none());
        assert!(!registry.idle_since.contains_key(Path::new("gcloud/compute.json")));

        let reloaded = registry.load_referenced_spec("gcloud/compute").unwrap();
        registry.release_idle(now, SPEC_IDLE_GRACE);
        let still = registry.cached_load_spec("gcloud/compute").unwrap();
        assert!(Arc::ptr_eq(&reloaded, &still));
        assert!(still.find_subcommand("instances").is_some());
    }

    #[test]
    fn idle_release_keeps_an_option_body_still_held_by_another_tree() {
        let dir = tempfile::tempdir().unwrap();
        let option = r#"{"names":["--same"],"description":"one"}"#;
        write_spec(
            dir.path(),
            "alpha",
            &format!(r#"{{"names":["alpha"],"options":[{option}]}}"#),
        );
        write_spec(
            dir.path(),
            "beta",
            &format!(r#"{{"names":["beta"],"options":[{option}]}}"#),
        );
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"alpha":"alpha.json","beta":"beta.json"}}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let alpha = registry.get_arc("alpha").unwrap();
        let beta = registry.get_arc("beta").unwrap();
        assert!(Arc::ptr_eq(&alpha.options[0], &beta.options[0]));
        drop(beta);

        let now = Instant::now();
        registry.set_idle_since("beta", Some(past_grace(now)));
        registry.release_idle(now, SPEC_IDLE_GRACE);

        assert!(!registry.is_cached("beta"));
        assert!(Arc::ptr_eq(&alpha, &registry.get_arc("alpha").unwrap()));
        assert!(Arc::ptr_eq(
            &alpha.options[0],
            &registry.get_arc("alpha").unwrap().options[0]
        ));
        let still_shared = Arc::downgrade(&alpha.options[0]);
        assert!(registry.option_pool.values().any(|bucket| {
            bucket
                .iter()
                .any(|weak| weak.ptr_eq(&still_shared) && weak.strong_count() > 0)
        }));
    }

    #[test]
    fn idle_note_keeps_a_running_grace_and_a_pause_does_not_count_as_use() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "alpha", r#"{"names":["alpha"]}"#);
        write_spec(dir.path(), "beta", r#"{"names":["beta"],"description":"bundled"}"#);
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"alpha":"alpha.json","beta":"beta.json"}}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        registry.get("alpha").unwrap();
        registry.get("beta").unwrap();
        registry.begin_idle_completion();
        registry.get("alpha").unwrap();
        {
            let _outer = registry.pause_idle_touch();
            {
                let _inner = registry.pause_idle_touch();
                registry.get("beta").unwrap();
            }
            registry.get("beta").unwrap();
        }
        let mut cloned = registry.clone();
        cloned.begin_idle_completion();
        cloned.get("beta").unwrap();
        cloned.note_idle_after_complete(Instant::now());
        assert_eq!(cloned.idle_mark("beta"), Some(None));
        let now = Instant::now();
        registry.note_idle_after_complete(now);
        assert_eq!(registry.idle_mark("alpha"), Some(None));
        assert_eq!(registry.idle_mark("beta"), Some(Some(now)));
        assert!(registry.is_cached("beta"));

        let later = now + Duration::from_secs(5);
        registry.begin_idle_completion();
        registry.get("alpha").unwrap();
        registry.note_idle_after_complete(later);
        assert_eq!(registry.idle_mark("alpha"), Some(None));
        assert_eq!(registry.idle_mark("beta"), Some(Some(now)));

        registry.overlay_spec(
            Spec {
                names: vec!["beta".into()],
                description: "overlay".into(),
                ..Spec::default()
            },
            OverlayMode::Replace,
        );
        registry.begin_idle_completion();
        registry.note_idle_after_complete(later + Duration::from_secs(5));
        assert_eq!(registry.idle_mark("beta"), None);
        let pinned = registry.get_arc("beta").unwrap();
        registry.release_idle(later + SPEC_IDLE_GRACE, SPEC_IDLE_GRACE);
        assert!(Arc::ptr_eq(&pinned, &registry.get_arc("beta").unwrap()));
        assert_eq!(registry.get("beta").unwrap().description, "overlay");
    }

    #[test]
    fn next_idle_deadline_uses_the_earliest_pending_mark_and_skips_pins() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "alpha", r#"{"names":["alpha"]}"#);
        write_spec(dir.path(), "beta", r#"{"names":["beta"]}"#);
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"alpha":"alpha.json","beta":"beta.json"}}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        registry.get("alpha").unwrap();
        registry.get("beta").unwrap();
        let now = Instant::now();
        registry.set_idle_since("alpha", None);
        registry.set_idle_since("beta", Some(now));
        // An earlier deadline without a cached tree must not wake the worker.
        registry.set_idle_since("ghost", Some(past_grace(now)));
        assert_eq!(
            registry.next_idle_deadline(SPEC_IDLE_GRACE),
            Some(now + SPEC_IDLE_GRACE)
        );

        registry.set_idle_since("alpha", Some(now + Duration::from_secs(10)));
        assert_eq!(
            registry.next_idle_deadline(SPEC_IDLE_GRACE),
            Some(now + SPEC_IDLE_GRACE)
        );

        registry.overlay_spec(
            Spec {
                names: vec!["beta".into()],
                description: "overlay".into(),
                ..Spec::default()
            },
            OverlayMode::Replace,
        );
        assert_eq!(
            registry.next_idle_deadline(SPEC_IDLE_GRACE),
            Some(now + Duration::from_secs(10) + SPEC_IDLE_GRACE)
        );
        registry.set_idle_since("alpha", None);
        assert_eq!(registry.idle_mark("ghost"), Some(Some(past_grace(now))));
        assert_eq!(registry.next_idle_deadline(SPEC_IDLE_GRACE), None);
    }
}

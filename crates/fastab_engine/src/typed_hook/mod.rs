#![cfg_attr(not(test), allow(dead_code))]
#![allow(clippy::map_err_ignore)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::unnested_or_patterns)]
//! The closed, typed expression language used by native hook adapters.
//!
//! The JavaScript compiler emits this descriptor at build time.  This module
//! deliberately does not know a command name or a hook id: it accepts the
//! versioned field contracts and evaluates the expression supplied by that
//! contract.  Production sidecars emit every field that compiles; T3.1 wires
//! this evaluator into the Native hook backend.  Keeping the JSON boundary
//! strict is important here.  A
//! new operation or a field with a different meaning must be rejected until
//! both the compiler and this evaluator have been updated.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

use crate::hook_types::ScriptCommand;
use crate::runtime::Suggestion;

const IR_VERSION: u64 = 1;
const IR_KIND: &str = "typed-hook-expression";
const SOURCE_FIELD: &str = "trigger";
const GET_QUERY_TERM_SOURCE_FIELD: &str = "getQueryTerm";
const POST_PROCESS_SOURCE_FIELD: &str = "postProcess";
const SCRIPT_SOURCE_FIELD: &str = "script";
const FILTER_TEMPLATE_SUGGESTIONS_SOURCE_FIELD: &str = "filterTemplateSuggestions";
const CUSTOM_SOURCE_FIELD: &str = "custom";
const ALIAS_SOURCE_FIELD: &str = "alias";
const LOAD_SPEC_SOURCE_FIELD: &str = "loadSpec";
const GENERATE_SPEC_SOURCE_FIELD: &str = "generateSpec";
const SIDECAR_SOURCE_FIELDS: [&str; 9] = [
    SOURCE_FIELD,
    GET_QUERY_TERM_SOURCE_FIELD,
    POST_PROCESS_SOURCE_FIELD,
    SCRIPT_SOURCE_FIELD,
    FILTER_TEMPLATE_SUGGESTIONS_SOURCE_FIELD,
    CUSTOM_SOURCE_FIELD,
    ALIAS_SOURCE_FIELD,
    LOAD_SPEC_SOURCE_FIELD,
    GENERATE_SPEC_SOURCE_FIELD,
];
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
const MAX_STRING_LITERAL_UNITS: usize = 32_768;
const MAX_SERIALIZED_DESCRIPTOR_BYTES: usize = 256 * 1024;
const MAX_NODES: usize = 512;
const MAX_DEPTH: usize = 24;
const TRIGGER_PARAM_COUNT: usize = 2;
const GET_QUERY_TERM_PARAM_COUNT: usize = 1;
const CATALOG_KIND: &str = "typed-hook-expressions";
const MAX_CATALOG_BYTES: usize = 64 * 1024 * 1024;
const MAX_CATALOG_HOOKS: usize = 65_536;
const MAX_HOOK_ID_BYTES: usize = 4_096;
const MAX_HOOK_PATH_BYTES: usize = 4_096;
const MAX_MODULE_BASENAME_BYTES: usize = 255;
const REFERENCE_BASELINE_VERSION: u64 = 1;
const REFERENCE_BASELINE_KIND: &str = "typed-trigger-reference";
const GET_QUERY_TERM_REFERENCE_BASELINE_KIND: &str = "typed-get-query-term-reference";
const FIELD_REFERENCE_BASELINE_KIND: &str = "typed-field-reference";
const MAX_REFERENCE_BASELINE_BYTES: usize = 64 * 1024 * 1024;
const MAX_REFERENCE_CASES: usize = 256;
const MAX_REFERENCE_CORPUS_BYTES: usize = 1_000_000;
const REFERENCE_EXEC_MARKER: &str = "$referenceExec";
const ASDF_GET_QUERY_TERM_CANDIDATE_IDS: [&str; 2] = ["asdf#getQueryTerm#7", "asdf#getQueryTerm#8"];

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum TypedValueType {
    String,
    Bool,
    Integer,
    StringArray,
    Json,
    JsonArray,
    Suggestion,
    SuggestionArray,
    StringRecord,
    StringSet,
    ValueArray,
    Regex,
    Null,
    Exec,
    Context,
    ExecResult,
    Spec,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TypedHookParam {
    index: u64,
    #[serde(rename = "type")]
    value_type: TypedValueType,
}

/// The expression enum is internally tagged so every operation has one
/// unambiguous JSON shape. `deny_unknown_fields` is intentionally attached to
/// the enum: adding an operation field without updating this evaluator must
/// fail closed at deserialization time.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "op", deny_unknown_fields)]
enum TypedExpr {
    #[serde(rename = "arg")]
    Arg { index: u64 },
    #[serde(rename = "string")]
    String { value: String },
    #[serde(rename = "bool")]
    Bool { value: bool },
    #[serde(rename = "integer")]
    Integer { value: i64 },
    #[serde(rename = "null")]
    Null,
    #[serde(rename = "array")]
    Array { items: Vec<TypedExpr> },
    #[serde(rename = "length")]
    Length { value: Box<TypedExpr> },
    #[serde(rename = "string-includes")]
    StringIncludes {
        value: Box<TypedExpr>,
        needle: Box<TypedExpr>,
    },
    #[serde(rename = "string-index-of")]
    StringIndexOf {
        value: Box<TypedExpr>,
        needle: Box<TypedExpr>,
    },
    #[serde(rename = "string-last-index-of")]
    StringLastIndexOf {
        value: Box<TypedExpr>,
        needle: Box<TypedExpr>,
    },
    #[serde(rename = "string-slice")]
    StringSlice {
        value: Box<TypedExpr>,
        start: Box<TypedExpr>,
    },
    #[serde(rename = "string-slice-after-first")]
    StringSliceAfterFirst {
        value: Box<TypedExpr>,
        needle: Box<TypedExpr>,
    },
    #[serde(rename = "string-substring")]
    StringSubstring {
        value: Box<TypedExpr>,
        start: Box<TypedExpr>,
        end: Box<TypedExpr>,
    },
    #[serde(rename = "string-split")]
    StringSplit {
        value: Box<TypedExpr>,
        separator: Box<TypedExpr>,
    },
    #[serde(rename = "string-trim")]
    StringTrim { value: Box<TypedExpr> },
    #[serde(rename = "string-trim-start")]
    StringTrimStart { value: Box<TypedExpr> },
    #[serde(rename = "string-trim-end")]
    StringTrimEnd { value: Box<TypedExpr> },
    #[serde(rename = "string-replace")]
    StringReplace {
        value: Box<TypedExpr>,
        needle: Box<TypedExpr>,
        replacement: Box<TypedExpr>,
    },
    #[serde(rename = "string-replace-all")]
    StringReplaceAll {
        value: Box<TypedExpr>,
        needle: Box<TypedExpr>,
        replacement: Box<TypedExpr>,
    },
    #[serde(rename = "string-starts-with")]
    StringStartsWith {
        value: Box<TypedExpr>,
        needle: Box<TypedExpr>,
    },
    #[serde(rename = "string-ends-with")]
    StringEndsWith {
        value: Box<TypedExpr>,
        needle: Box<TypedExpr>,
    },
    #[serde(rename = "string-to-lower")]
    StringToLower { value: Box<TypedExpr> },
    #[serde(rename = "string-to-upper")]
    StringToUpper { value: Box<TypedExpr> },
    #[serde(rename = "string-pad-start")]
    StringPadStart {
        value: Box<TypedExpr>,
        target: Box<TypedExpr>,
        pad: Box<TypedExpr>,
    },
    #[serde(rename = "string-pad-end")]
    StringPadEnd {
        value: Box<TypedExpr>,
        target: Box<TypedExpr>,
        pad: Box<TypedExpr>,
    },
    #[serde(rename = "string-repeat")]
    StringRepeat {
        value: Box<TypedExpr>,
        count: Box<TypedExpr>,
    },
    #[serde(rename = "string-concat")]
    StringConcat { parts: Vec<TypedExpr> },
    #[serde(rename = "string-char-at")]
    StringCharAt {
        value: Box<TypedExpr>,
        index: Box<TypedExpr>,
    },
    #[serde(rename = "string-at")]
    StringAt {
        value: Box<TypedExpr>,
        index: Box<TypedExpr>,
    },
    #[serde(rename = "array-includes")]
    ArrayIncludes {
        value: Box<TypedExpr>,
        needle: Box<TypedExpr>,
    },
    #[serde(rename = "strict-eq")]
    StrictEq {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "strict-ne")]
    StrictNe {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "add")]
    Add {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "sub")]
    Sub {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "mul")]
    Mul {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "lt")]
    LessThan {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "le")]
    LessThanOrEqual {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "gt")]
    GreaterThan {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "ge")]
    GreaterThanOrEqual {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "not")]
    Not { value: Box<TypedExpr> },
    #[serde(rename = "nullish")]
    Nullish {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "and")]
    And {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "or")]
    Or {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "if")]
    If {
        condition: Box<TypedExpr>,
        #[serde(rename = "then")]
        then_branch: Box<TypedExpr>,
        #[serde(rename = "else")]
        else_branch: Box<TypedExpr>,
    },
    #[serde(rename = "lambda")]
    Lambda { params: Vec<String>, body: Box<TypedExpr> },
    #[serde(rename = "var")]
    Var { name: String },
    #[serde(rename = "let")]
    Let {
        name: String,
        value: Box<TypedExpr>,
        body: Box<TypedExpr>,
    },
    #[serde(rename = "block")]
    Block { items: Vec<TypedExpr> },
    #[serde(rename = "return")]
    Return { value: Box<TypedExpr> },
    #[serde(rename = "catch-return")]
    CatchReturn { body: Box<TypedExpr> },
    #[serde(rename = "break")]
    Break,
    #[serde(rename = "continue")]
    Continue,
    #[serde(rename = "try")]
    Try {
        body: Box<TypedExpr>,
        catch: Box<TypedExpr>,
    },
    #[serde(rename = "for-of")]
    ForOf {
        names: Vec<String>,
        value: Box<TypedExpr>,
        body: Box<TypedExpr>,
    },
    #[serde(rename = "assign-var")]
    AssignVar { name: String, value: Box<TypedExpr> },
    #[serde(rename = "assign-prop")]
    AssignProp {
        object: Box<TypedExpr>,
        key: Box<TypedExpr>,
        value: Box<TypedExpr>,
    },
    #[serde(rename = "seq")]
    Seq { items: Vec<TypedExpr> },
    #[serde(rename = "truthy")]
    Truthy { value: Box<TypedExpr> },
    #[serde(rename = "loose-eq")]
    LooseEq {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "loose-ne")]
    LooseNe {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "typeof")]
    Typeof { value: Box<TypedExpr> },
    #[serde(rename = "object")]
    Object { fields: Vec<TypedObjectField> },
    #[serde(rename = "spread")]
    Spread {
        value: Box<TypedExpr>,
        fields: Vec<TypedObjectField>,
    },
    #[serde(rename = "object-assign")]
    ObjectAssign { parts: Vec<TypedExpr> },
    #[serde(rename = "get")]
    Get { value: Box<TypedExpr>, key: Box<TypedExpr> },
    #[serde(rename = "object-keys")]
    ObjectKeys { value: Box<TypedExpr> },
    #[serde(rename = "object-entries")]
    ObjectEntries { value: Box<TypedExpr> },
    #[serde(rename = "object-values")]
    ObjectValues { value: Box<TypedExpr> },
    #[serde(rename = "array-map")]
    ArrayMap {
        value: Box<TypedExpr>,
        #[serde(rename = "fn")]
        callback: Box<TypedExpr>,
    },
    #[serde(rename = "array-filter")]
    ArrayFilter {
        value: Box<TypedExpr>,
        #[serde(rename = "fn")]
        callback: Box<TypedExpr>,
    },
    #[serde(rename = "array-flat-map")]
    ArrayFlatMap {
        value: Box<TypedExpr>,
        #[serde(rename = "fn")]
        callback: Box<TypedExpr>,
    },
    #[serde(rename = "array-slice")]
    ArraySlice {
        value: Box<TypedExpr>,
        start: Box<TypedExpr>,
        end: Box<TypedExpr>,
    },
    #[serde(rename = "array-join")]
    ArrayJoin {
        value: Box<TypedExpr>,
        separator: Box<TypedExpr>,
    },
    #[serde(rename = "array-some")]
    ArraySome {
        value: Box<TypedExpr>,
        #[serde(rename = "fn")]
        callback: Box<TypedExpr>,
    },
    #[serde(rename = "array-every")]
    ArrayEvery {
        value: Box<TypedExpr>,
        #[serde(rename = "fn")]
        callback: Box<TypedExpr>,
    },
    #[serde(rename = "array-find")]
    ArrayFind {
        value: Box<TypedExpr>,
        #[serde(rename = "fn")]
        callback: Box<TypedExpr>,
    },
    #[serde(rename = "array-find-index")]
    ArrayFindIndex {
        value: Box<TypedExpr>,
        #[serde(rename = "fn")]
        callback: Box<TypedExpr>,
    },
    #[serde(rename = "array-index-of")]
    ArrayIndexOf {
        value: Box<TypedExpr>,
        needle: Box<TypedExpr>,
    },
    #[serde(rename = "array-concat")]
    ArrayConcat { parts: Vec<TypedExpr> },
    #[serde(rename = "array-reverse")]
    ArrayReverse { value: Box<TypedExpr> },
    #[serde(rename = "array-sort")]
    ArraySort {
        value: Box<TypedExpr>,
        #[serde(rename = "fn")]
        callback: Box<TypedExpr>,
    },
    #[serde(rename = "array-index")]
    ArrayIndex {
        value: Box<TypedExpr>,
        index: Box<TypedExpr>,
    },
    #[serde(rename = "array-push")]
    ArrayPush { name: String, item: Box<TypedExpr> },
    #[serde(rename = "array-pop")]
    ArrayPop { value: Box<TypedExpr> },
    #[serde(rename = "array-entries")]
    ArrayEntries { value: Box<TypedExpr> },
    #[serde(rename = "array-shift")]
    ArrayShift { name: String },
    #[serde(rename = "array-from")]
    ArrayFrom { value: Box<TypedExpr> },
    #[serde(rename = "array-flat")]
    ArrayFlat {
        value: Box<TypedExpr>,
        depth: Box<TypedExpr>,
    },
    #[serde(rename = "json-parse")]
    JsonParse { value: Box<TypedExpr> },
    #[serde(rename = "json-get")]
    JsonGet { value: Box<TypedExpr>, key: Box<TypedExpr> },
    #[serde(rename = "json-array-items")]
    JsonArrayItems { value: Box<TypedExpr> },
    #[serde(rename = "json-as-string")]
    JsonAsString { value: Box<TypedExpr> },
    #[serde(rename = "json-as-number")]
    JsonAsNumber { value: Box<TypedExpr> },
    #[serde(rename = "json-as-bool")]
    JsonAsBool { value: Box<TypedExpr> },
    #[serde(rename = "json-object")]
    JsonObject { fields: Vec<TypedObjectField> },
    #[serde(rename = "json-stringify")]
    JsonStringify { value: Box<TypedExpr> },
    #[serde(rename = "regex-test")]
    RegexTest {
        value: Box<TypedExpr>,
        pattern: String,
        flags: String,
    },
    #[serde(rename = "regex-match")]
    RegexMatch {
        value: Box<TypedExpr>,
        pattern: String,
        flags: String,
    },
    #[serde(rename = "regex-match-all")]
    RegexMatchAll {
        value: Box<TypedExpr>,
        pattern: String,
        flags: String,
    },
    #[serde(rename = "regex-replace")]
    RegexReplace {
        value: Box<TypedExpr>,
        pattern: String,
        flags: String,
        replacement: Box<TypedExpr>,
    },
    #[serde(rename = "regex-search")]
    RegexSearch {
        value: Box<TypedExpr>,
        pattern: String,
        flags: String,
    },
    #[serde(rename = "string-split-regex")]
    StringSplitRegex {
        value: Box<TypedExpr>,
        pattern: String,
        flags: String,
    },
    #[serde(rename = "string-split-limit")]
    StringSplitLimit {
        value: Box<TypedExpr>,
        separator: Box<TypedExpr>,
        limit: Box<TypedExpr>,
    },
    #[serde(rename = "string-index-of-from")]
    StringIndexOfFrom {
        value: Box<TypedExpr>,
        needle: Box<TypedExpr>,
        start: Box<TypedExpr>,
    },
    #[serde(rename = "array-index-of-from")]
    ArrayIndexOfFrom {
        value: Box<TypedExpr>,
        needle: Box<TypedExpr>,
        start: Box<TypedExpr>,
    },
    #[serde(rename = "math-max")]
    MathMax { values: Box<TypedExpr> },
    #[serde(rename = "string-set")]
    StringSet { value: Box<TypedExpr> },
    #[serde(rename = "string-set-add")]
    StringSetAdd { name: String, item: Box<TypedExpr> },
    #[serde(rename = "regex")]
    Regex { pattern: String, flags: String },
    #[serde(rename = "while")]
    While {
        condition: Box<TypedExpr>,
        body: Box<TypedExpr>,
    },
    #[serde(rename = "to-string")]
    ToString { value: Box<TypedExpr> },
    #[serde(rename = "locale-compare")]
    LocaleCompare {
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
    },
    #[serde(rename = "string-slice-range")]
    StringSliceRange {
        value: Box<TypedExpr>,
        start: Box<TypedExpr>,
        end: Box<TypedExpr>,
    },
    #[serde(rename = "exec")]
    Exec {
        command: Box<TypedExpr>,
        args: Box<TypedExpr>,
        cwd: Box<TypedExpr>,
        env: Box<TypedExpr>,
        timeout: Box<TypedExpr>,
    },
    #[serde(rename = "par")]
    Par { items: Vec<TypedExpr> },
    #[serde(rename = "ctx-cwd")]
    CtxCwd,
    #[serde(rename = "ctx-process")]
    CtxProcess,
    #[serde(rename = "ctx-ssh-prefix")]
    CtxSshPrefix,
    #[serde(rename = "ctx-env")]
    CtxEnv { name: Box<TypedExpr> },
    #[serde(rename = "ctx-environment")]
    CtxEnvironment,
    #[serde(rename = "ctx-search-term")]
    CtxSearchTerm,
    #[serde(rename = "ctx-is-dangerous")]
    CtxIsDangerous,
    #[serde(rename = "spec-object")]
    SpecObject { fields: Vec<TypedObjectField> },
    #[serde(rename = "throw")]
    Throw { class: String, message: Box<TypedExpr> },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TypedObjectField {
    key: String,
    value: TypedExpr,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypedHookIr {
    version: u64,
    kind: String,
    #[serde(rename = "sourceField")]
    source_field: String,
    #[serde(rename = "resultType")]
    result_type: TypedValueType,
    params: Vec<TypedHookParam>,
    expr: TypedExpr,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TypedHookContract {
    #[serde(rename = "irVersion")]
    ir_version: u64,
    params: Vec<TypedValueType>,
    #[serde(rename = "resultType")]
    result_type: TypedValueType,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TypedHookContracts {
    trigger: TypedHookContract,
    #[serde(rename = "getQueryTerm")]
    get_query_term: TypedHookContract,
    #[serde(rename = "postProcess")]
    post_process: TypedHookContract,
    script: TypedHookContract,
    #[serde(rename = "filterTemplateSuggestions")]
    filter_template_suggestions: TypedHookContract,
    custom: TypedHookContract,
    alias: TypedHookContract,
    #[serde(rename = "loadSpec")]
    load_spec: TypedHookContract,
    #[serde(rename = "generateSpec")]
    generate_spec: TypedHookContract,
}

/// One owned copy of a descriptor body. Identical raw JSON inside a single
/// catalog parse shares this allocation, so the first evaluation fills one
/// `TypedHookIr` for every hook that points at it.
#[derive(Debug)]
struct SharedDescriptor {
    raw: Box<RawValue>,
    parsed: OnceLock<TypedHookIr>,
}

thread_local! {
    static DESCRIPTOR_INTERN: RefCell<Option<HashMap<u64, Vec<Arc<SharedDescriptor>>>>> = const { RefCell::new(None) };
}

/// Installs the descriptor intern pool for one catalog parse and drops it
/// afterwards. The catalog keeps the `Arc`s; a later parse does not reuse them.
struct DescriptorInternGuard;

impl DescriptorInternGuard {
    fn enter() -> Self {
        DESCRIPTOR_INTERN.with(|slot| {
            let mut pool = slot.borrow_mut();
            assert!(pool.is_none(), "typed-hook descriptor intern pools must not nest");
            *pool = Some(HashMap::new());
        });
        Self
    }
}

impl Drop for DescriptorInternGuard {
    fn drop(&mut self) {
        DESCRIPTOR_INTERN.with(|slot| *slot.borrow_mut() = None);
    }
}

fn with_descriptor_intern<T>(body: impl FnOnce() -> T) -> T {
    let _guard = DescriptorInternGuard::enter();
    body()
}

fn intern_descriptor(raw: Box<RawValue>) -> Arc<SharedDescriptor> {
    DESCRIPTOR_INTERN.with(|slot| {
        let mut pool = slot.borrow_mut();
        let Some(pool) = pool.as_mut() else {
            return Arc::new(SharedDescriptor {
                raw,
                parsed: OnceLock::new(),
            });
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        raw.get().hash(&mut hasher);
        let hash = hasher.finish();
        let bucket = pool.entry(hash).or_default();
        if let Some(existing) = bucket.iter().find(|item| item.raw.get() == raw.get()) {
            return Arc::clone(existing);
        }
        let shared = Arc::new(SharedDescriptor {
            raw,
            parsed: OnceLock::new(),
        });
        bucket.push(Arc::clone(&shared));
        shared
    })
}

/// A catalog descriptor kept as raw JSON until the first evaluation.
///
/// Compile and `--check` still validate every body. The engine only builds
/// the `TypedHookIr` tree for hooks that actually run (G1 in
/// `docs/memory-goals.md`). Hooks whose raw JSON is byte-identical share one
/// [`SharedDescriptor`] for the life of that catalog.
#[derive(Debug)]
pub(crate) struct LazyTypedHookIr {
    shared: Arc<SharedDescriptor>,
}

impl LazyTypedHookIr {
    pub(crate) fn get(&self) -> TypedHookResult<&TypedHookIr> {
        if let Some(descriptor) = self.shared.parsed.get() {
            return Ok(descriptor);
        }
        let descriptor = parse_typed_hook_ir_bytes(self.shared.raw.get().as_bytes())?;
        let _ = self.shared.parsed.set(descriptor);
        Ok(self.shared.parsed.get().expect("descriptor was just stored"))
    }

    pub(crate) fn is_parsed(&self) -> bool {
        self.shared.parsed.get().is_some()
    }

    fn raw_json(&self) -> &str {
        self.shared.raw.get()
    }
}

impl From<TypedHookIr> for LazyTypedHookIr {
    fn from(descriptor: TypedHookIr) -> Self {
        let raw = serde_json::value::to_raw_value(&descriptor).expect("TypedHookIr always serializes to JSON");
        let parsed = OnceLock::new();
        let _ = parsed.set(descriptor);
        Self {
            shared: Arc::new(SharedDescriptor { raw, parsed }),
        }
    }
}

impl Clone for LazyTypedHookIr {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl PartialEq for LazyTypedHookIr {
    fn eq(&self, other: &Self) -> bool {
        if Arc::ptr_eq(&self.shared, &other.shared) {
            return true;
        }
        match (self.shared.parsed.get(), other.shared.parsed.get()) {
            (Some(left), Some(right)) => left == right,
            (Some(left), None) => parsed_eq_raw(left, &other.shared.raw),
            (None, Some(right)) => parsed_eq_raw(right, &self.shared.raw),
            (None, None) => self.shared.raw.get() == other.shared.raw.get(),
        }
    }
}

/// Semantic equality that must not write `parsed`. Catalog `assert_eq!` used
/// to call [`LazyTypedHookIr::get`] and materialize every descriptor (G1).
fn parsed_eq_raw(parsed: &TypedHookIr, raw: &RawValue) -> bool {
    match parse_typed_hook_ir_bytes(raw.get().as_bytes()) {
        Ok(other) => parsed == &other,
        Err(_) => false,
    }
}

impl Eq for LazyTypedHookIr {}

impl Serialize for LazyTypedHookIr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.shared.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LazyTypedHookIr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            shared: intern_descriptor(Box::<RawValue>::deserialize(deserializer)?),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypedHookCatalogEntry {
    module: String,
    #[serde(rename = "moduleSha256")]
    module_sha256: String,
    path: String,
    #[serde(rename = "sourceField")]
    pub(crate) source_field: String,
    #[serde(rename = "functionBodySha256")]
    pub(crate) function_body_sha256: String,
    pub(crate) descriptor: LazyTypedHookIr,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypedHookAdapterBinding {
    pub(crate) path: String,
    #[serde(rename = "sourceField")]
    pub(crate) source_field: String,
    #[serde(rename = "functionBodySha256")]
    pub(crate) function_body_sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypedHookCatalog {
    version: u64,
    kind: String,
    contracts: TypedHookContracts,
    pub(crate) hooks: BTreeMap<String, TypedHookCatalogEntry>,
    /// Named native adapters that compiled typed IR cannot represent.
    /// Absent from older sidecars; empty means every hook is typed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) adapters: BTreeMap<String, TypedHookAdapterBinding>,
}

impl TypedHookCatalog {
    pub(crate) fn parsed_descriptor_count(&self) -> usize {
        self.hooks.values().filter(|entry| entry.descriptor.is_parsed()).count()
    }

    /// Drop compile-time provenance that the engine never reads again.
    ///
    /// `module` / `moduleSha256` / `path` / per-hook `functionBodySha256` and
    /// the catalog `sourceField` exist so compile and `--check` can pin a
    /// body to a file. After index validation the worker only needs the
    /// lazy descriptor and the adapter SHA map (G2).
    pub(crate) fn into_runtime(self) -> RuntimeTypedHookCatalog {
        runtime_catalog(self.hooks, self.adapters)
    }
}

fn runtime_catalog(
    hooks: BTreeMap<String, TypedHookCatalogEntry>,
    adapters: BTreeMap<String, TypedHookAdapterBinding>,
) -> RuntimeTypedHookCatalog {
    RuntimeTypedHookCatalog {
        hooks: hooks.into_iter().map(|(id, entry)| (id, entry.descriptor)).collect(),
        adapters: adapters
            .into_iter()
            .map(|(id, entry)| {
                (
                    id,
                    RuntimeTypedHookAdapter {
                        source_field: entry.source_field,
                        function_body_sha256: entry.function_body_sha256,
                    },
                )
            })
            .collect(),
    }
}

/// Production view of [`TypedHookCatalog`]: typed bodies plus adapter SHAs.
#[derive(Debug)]
pub(crate) struct RuntimeTypedHookCatalog {
    pub(crate) hooks: BTreeMap<String, LazyTypedHookIr>,
    pub(crate) adapters: BTreeMap<String, RuntimeTypedHookAdapter>,
}

#[derive(Debug)]
pub(crate) struct RuntimeTypedHookAdapter {
    pub(crate) source_field: String,
    pub(crate) function_body_sha256: String,
}

impl RuntimeTypedHookCatalog {
    pub(crate) fn parsed_descriptor_count(&self) -> usize {
        self.hooks.values().filter(|descriptor| descriptor.is_parsed()).count()
    }

    pub(crate) fn descriptor_counts(&self) -> (usize, usize) {
        let mut seen = std::collections::HashSet::new();
        let mut parsed = 0;
        for descriptor in self.hooks.values() {
            if seen.insert(Arc::as_ptr(&descriptor.shared)) && descriptor.is_parsed() {
                parsed += 1;
            }
        }
        (seen.len(), parsed)
    }
}

/// Where each command's hooks sit in `typed-hooks.json`. The compiler sorts
/// hook ids, which read `<command>#<field>#<n>`, so a command's hooks, and its
/// adapters, are one contiguous run of members. A dispatch then reads and
/// parses its own command's run instead of the whole catalog, which cost
/// ~13 ms on CI (4.6 MB) on the first hook after every idle grace.
#[derive(Debug)]
pub(crate) struct TypedHookCatalogIndex {
    /// Sorted by command.
    blocks: Vec<TypedHookBlock>,
    hook_count: usize,
    adapter_count: usize,
}

/// A block's hook members and adapter members, as byte ranges.
type BlockRanges = (Option<Range<usize>>, Option<Range<usize>>);

#[derive(Debug)]
struct TypedHookBlock {
    command: Box<str>,
    /// `"id": {…}, …` members, without the enclosing braces.
    hooks: Option<Range<usize>>,
    adapters: Option<Range<usize>>,
}

/// The block a hook id belongs to.
pub(crate) fn typed_hook_command(hook_id: &str) -> &str {
    hook_id.split_once('#').map_or(hook_id, |(command, _)| command)
}

impl TypedHookCatalogIndex {
    pub(crate) fn len(&self) -> usize {
        self.blocks.len()
    }

    pub(crate) fn hook_count(&self) -> usize {
        self.hook_count
    }

    pub(crate) fn adapter_count(&self) -> usize {
        self.adapter_count
    }

    /// Position of the block that holds `hook_id`.
    pub(crate) fn position(&self, hook_id: &str) -> Option<usize> {
        let command = typed_hook_command(hook_id);
        self.blocks
            .binary_search_by(|block| block.command.as_ref().cmp(command))
            .ok()
    }

    /// Byte ranges of the block's hook members and adapter members.
    pub(crate) fn ranges(&self, position: usize) -> BlockRanges {
        let block = &self.blocks[position];
        (block.hooks.clone(), block.adapters.clone())
    }
}

/// Validate the header and every id, and record where each command's members
/// sit; entries are validated when their block is parsed. An escaped key, or
/// a command whose members are not contiguous, is an error, and the caller
/// then parses the catalog whole.
pub(crate) fn index_typed_hook_catalog_bytes(bytes: &[u8]) -> TypedHookResult<TypedHookCatalogIndex> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Outline<'a> {
        version: u64,
        kind: String,
        contracts: TypedHookContracts,
        #[serde(borrow)]
        hooks: BTreeMap<&'a str, &'a RawValue>,
        #[serde(default, borrow)]
        adapters: BTreeMap<&'a str, &'a RawValue>,
    }
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(TypedHookError::new(format!(
            "typed hook catalog exceeds {MAX_CATALOG_BYTES} bytes"
        )));
    }
    let outline = serde_json::from_slice::<Outline<'_>>(bytes)
        .map_err(|error| TypedHookError::new(format!("typed hook catalog schema: {error}")))?;
    validate_catalog_header(outline.version, &outline.kind, &outline.contracts)?;
    validate_catalog_hook_count(outline.hooks.len(), outline.adapters.len())?;
    for id in outline.hooks.keys().chain(outline.adapters.keys()) {
        validate_hook_id(id)?;
    }
    if let Some(id) = outline.adapters.keys().find(|id| outline.hooks.contains_key(*id)) {
        return Err(TypedHookError::new(format!(
            "typed hook {id:?} cannot be both a typed descriptor and an adapter binding"
        )));
    }
    let mut blocks: BTreeMap<&str, BlockRanges> = BTreeMap::new();
    for (command, range) in member_runs(bytes, &outline.hooks)? {
        if blocks.entry(command).or_default().0.replace(range).is_some() {
            return Err(TypedHookError::new(format!(
                "typed hooks for {command:?} are not contiguous"
            )));
        }
    }
    for (command, range) in member_runs(bytes, &outline.adapters)? {
        if blocks.entry(command).or_default().1.replace(range).is_some() {
            return Err(TypedHookError::new(format!(
                "typed hook adapters for {command:?} are not contiguous"
            )));
        }
    }
    Ok(TypedHookCatalogIndex {
        blocks: blocks
            .into_iter()
            .map(|(command, (hooks, adapters))| TypedHookBlock {
                command: command.into(),
                hooks,
                adapters,
            })
            .collect(),
        hook_count: outline.hooks.len(),
        adapter_count: outline.adapters.len(),
    })
}

/// One byte range per command, in file order: from the opening quote of the
/// first member's key to the end of the last member's value.
fn member_runs<'a>(
    bytes: &[u8],
    members: &BTreeMap<&'a str, &'a RawValue>,
) -> TypedHookResult<Vec<(&'a str, Range<usize>)>> {
    let base = bytes.as_ptr() as usize;
    let mut spans: Vec<(usize, usize, &'a str)> = members
        .iter()
        .map(|(id, value)| {
            let value = value.get();
            let start = (id.as_ptr() as usize - base).saturating_sub(1);
            (start, value.as_ptr() as usize - base + value.len(), *id)
        })
        .collect();
    spans.sort_unstable_by_key(|(start, _, _)| *start);
    let mut runs: Vec<(&'a str, Range<usize>)> = Vec::new();
    for (start, end, id) in spans {
        if bytes.get(start) != Some(&b'"') {
            return Err(TypedHookError::new("typed hook catalog key does not start at a quote"));
        }
        let command = typed_hook_command(id);
        match runs.last_mut() {
            Some((last, range)) if *last == command => range.end = end,
            _ => runs.push((command, start..end)),
        }
    }
    Ok(runs)
}

/// Parse one command's members as cut by [`index_typed_hook_catalog_bytes`],
/// with the per-entry checks [`parse_typed_hook_catalog_bytes`] applies.
pub(crate) fn parse_typed_hook_block(
    hooks: Option<&[u8]>,
    adapters: Option<&[u8]>,
) -> TypedHookResult<RuntimeTypedHookCatalog> {
    let hooks: BTreeMap<String, TypedHookCatalogEntry> = match hooks {
        Some(members) => with_descriptor_intern(|| parse_catalog_members(members))?,
        None => BTreeMap::new(),
    };
    let adapters: BTreeMap<String, TypedHookAdapterBinding> = match adapters {
        Some(members) => parse_catalog_members(members)?,
        None => BTreeMap::new(),
    };
    for (id, entry) in &hooks {
        validate_hook_entry(id, entry)?;
    }
    for (id, entry) in &adapters {
        validate_adapter_entry(id, entry, hooks.contains_key(id))?;
    }
    Ok(runtime_catalog(hooks, adapters))
}

fn parse_catalog_members<T: serde::de::DeserializeOwned>(members: &[u8]) -> TypedHookResult<BTreeMap<String, T>> {
    let mut object = Vec::with_capacity(members.len() + 2);
    object.push(b'{');
    object.extend_from_slice(members);
    object.push(b'}');
    serde_json::from_slice(&object).map_err(|error| TypedHookError::new(format!("typed hook catalog schema: {error}")))
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TypedTriggerReferenceCase {
    id: String,
    args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypedTriggerReferenceBaseline {
    version: u64,
    kind: String,
    #[serde(rename = "generatorSha256")]
    generator_sha256: String,
    #[serde(rename = "harnessSha256")]
    harness_sha256: String,
    #[serde(rename = "pairSha256")]
    pair_sha256: String,
    #[serde(rename = "hookManifestSha256")]
    hook_manifest_sha256: String,
    #[serde(rename = "sidecarSha256")]
    sidecar_sha256: String,
    cases: Vec<TypedTriggerReferenceCase>,
    catalog: TypedHookCatalog,
    expected: BTreeMap<String, Vec<bool>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TypedGetQueryTermReferenceCase {
    id: String,
    args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TypedGetQueryTermReferenceCandidate {
    source: String,
    #[serde(rename = "sourceSha256")]
    source_sha256: String,
    ir: String,
    #[serde(rename = "irSha256")]
    ir_sha256: String,
    #[serde(rename = "hookFile")]
    hook_file: String,
    #[serde(rename = "hookFileSha256")]
    hook_file_sha256: String,
    module: String,
    #[serde(rename = "moduleSha256")]
    module_sha256: String,
    path: String,
    #[serde(rename = "sourceField")]
    source_field: String,
    #[serde(rename = "functionBodySha256")]
    function_body_sha256: String,
    descriptor: TypedHookIr,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TypedFieldReferenceCase {
    id: String,
    args: Vec<JsonValue>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypedFieldReferenceBaseline {
    version: u64,
    kind: String,
    field: String,
    #[serde(rename = "generatorSha256")]
    generator_sha256: String,
    #[serde(rename = "harnessSha256")]
    harness_sha256: String,
    #[serde(rename = "pairSha256")]
    pair_sha256: String,
    #[serde(rename = "hookManifestSha256")]
    hook_manifest_sha256: String,
    #[serde(rename = "sidecarSha256")]
    sidecar_sha256: String,
    cases: Vec<TypedFieldReferenceCase>,
    catalog: TypedHookCatalog,
    expected: BTreeMap<String, Vec<JsonValue>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypedGetQueryTermReferenceBaseline {
    version: u64,
    kind: String,
    #[serde(rename = "generatorSha256")]
    generator_sha256: String,
    #[serde(rename = "harnessSha256")]
    harness_sha256: String,
    #[serde(rename = "pairSha256")]
    pair_sha256: String,
    #[serde(rename = "hookManifestSha256")]
    hook_manifest_sha256: String,
    cases: Vec<TypedGetQueryTermReferenceCase>,
    candidates: BTreeMap<String, TypedGetQueryTermReferenceCandidate>,
    expected: BTreeMap<String, Vec<String>>,
}

/// A parse, schema, or evaluation failure.  Callers intentionally receive a
/// single failure type so an unsupported sidecar can remain on the legacy
/// path without exposing serde implementation details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypedHookError {
    message: String,
    js_class: Option<String>,
    timed_out: bool,
    cancelled: bool,
}

impl TypedHookError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            js_class: None,
            timed_out: false,
            cancelled: false,
        }
    }

    /// The hook ran out of its wall-clock budget. QuickJS reported this
    /// through its interrupt handler; the evaluator carries the reason on the
    /// error so the diagnostic stays `Timeout` and not a generic failure.
    pub(crate) fn timed_out(message: impl Into<String>) -> Self {
        Self {
            timed_out: true,
            ..Self::new(message)
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self {
            cancelled: true,
            ..Self::new("completion cancelled")
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    pub(crate) fn is_timed_out(&self) -> bool {
        self.timed_out
    }

    fn throw(class: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            js_class: Some(class.into()),
            timed_out: false,
            cancelled: false,
        }
    }

    #[allow(dead_code)]
    fn js_class(&self) -> Option<&str> {
        self.js_class.as_deref()
    }
}

impl fmt::Display for TypedHookError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TypedHookError {}

type TypedHookResult<T> = Result<T, TypedHookError>;

mod eval;

/// A string represented as JavaScript UTF-16 code units.  Rust's `str` is
/// UTF-8, so keeping this representation through every string operation is
/// what preserves JavaScript's length, indexing, and surrogate-boundary
/// behavior.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Utf16String(pub(super) Vec<u16>);

impl Utf16String {
    pub(super) fn from_str(value: &str) -> Self {
        Self(value.encode_utf16().collect())
    }

    pub(super) fn from_units(value: impl Into<Vec<u16>>) -> Self {
        Self(value.into())
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    pub(super) fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TypedValue {
    String(Utf16String),
    Bool(bool),
    Integer(i64),
    StringArray(Vec<Utf16String>),
    Array(Vec<TypedValue>),
    /// Insertion-ordered own properties, matching JS `Object` enumeration.
    /// `BTreeMap` would sort keys and scramble `Object.entries` on JSON.parse
    /// results (package.json `scripts` is the bun/postProcess fixture).
    Object(Vec<(String, TypedValue)>),
    /// First-seen unique strings, matching JS `Set` enumeration order.
    /// `BTreeSet` would sort and scramble `Array.from(new Set(lines))`.
    StringSet(Vec<Utf16String>),
    Regex {
        pattern: String,
        flags: String,
    },
    Json(JsonValue),
    Null,
}

#[derive(Debug, Default)]
struct ValidationState {
    nodes: usize,
}

/// Parse and validate a generated descriptor from a JSON value.
pub(crate) fn parse_typed_hook_ir(value: &JsonValue) -> TypedHookResult<TypedHookIr> {
    ensure_descriptor_size(value)?;
    let descriptor = serde_json::from_value::<TypedHookIr>(value.clone())
        .map_err(|error| TypedHookError::new(format!("typed hook schema: {error}")))?;
    validate_typed_hook_ir(&descriptor)?;
    Ok(descriptor)
}

/// Parse and validate a generated descriptor directly from sidecar bytes.
pub(crate) fn parse_typed_hook_ir_bytes(bytes: &[u8]) -> TypedHookResult<TypedHookIr> {
    if bytes.len() > MAX_SERIALIZED_DESCRIPTOR_BYTES {
        return Err(TypedHookError::new(format!(
            "typed hook descriptor exceeds {MAX_SERIALIZED_DESCRIPTOR_BYTES} bytes"
        )));
    }
    let descriptor = serde_json::from_slice::<TypedHookIr>(bytes)
        .map_err(|error| TypedHookError::new(format!("typed hook schema: {error}")))?;
    validate_typed_hook_ir(&descriptor)?;
    Ok(descriptor)
}

/// Parse and validate the build-time catalog of typed hook descriptors.
pub(crate) fn parse_typed_hook_catalog(value: &JsonValue) -> TypedHookResult<TypedHookCatalog> {
    ensure_catalog_size(value)?;
    let catalog = with_descriptor_intern(|| serde_json::from_value::<TypedHookCatalog>(value.clone()))
        .map_err(|error| TypedHookError::new(format!("typed hook catalog schema: {error}")))?;
    validate_typed_hook_catalog(&catalog)?;
    Ok(catalog)
}

/// Parse and validate a catalog directly from the generated sidecar bytes.
pub(crate) fn parse_typed_hook_catalog_bytes(bytes: &[u8]) -> TypedHookResult<TypedHookCatalog> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(TypedHookError::new(format!(
            "typed hook catalog exceeds {MAX_CATALOG_BYTES} bytes"
        )));
    }
    let catalog = with_descriptor_intern(|| serde_json::from_slice::<TypedHookCatalog>(bytes))
        .map_err(|error| TypedHookError::new(format!("typed hook catalog schema: {error}")))?;
    // Production load validates the index only. Descriptor trees are parsed
    // on first use so an idle engine does not hold 3136 IR graphs (G1).
    validate_typed_hook_catalog_index(&catalog)?;
    Ok(catalog)
}

/// Parse and validate the immutable source/module differential baseline used
/// by the typed-hook migration tests.  This is deliberately test-only data:
/// the production engine does not load or consult this baseline.
pub(crate) fn parse_typed_trigger_reference(value: &JsonValue) -> TypedHookResult<TypedTriggerReferenceBaseline> {
    ensure_reference_baseline_size(value)?;
    let baseline = serde_json::from_value::<TypedTriggerReferenceBaseline>(value.clone())
        .map_err(|error| TypedHookError::new(format!("typed trigger reference schema: {error}")))?;
    validate_typed_trigger_reference(&baseline)?;
    Ok(baseline)
}

/// Parse and validate the checked-in typed-trigger reference baseline bytes.
pub(crate) fn parse_typed_trigger_reference_bytes(bytes: &[u8]) -> TypedHookResult<TypedTriggerReferenceBaseline> {
    if bytes.len() > MAX_REFERENCE_BASELINE_BYTES {
        return Err(TypedHookError::new(format!(
            "typed trigger reference exceeds {MAX_REFERENCE_BASELINE_BYTES} bytes"
        )));
    }
    let baseline = serde_json::from_slice::<TypedTriggerReferenceBaseline>(bytes)
        .map_err(|error| TypedHookError::new(format!("typed trigger reference schema: {error}")))?;
    validate_typed_trigger_reference(&baseline)?;
    Ok(baseline)
}

/// Parse the isolated asdf getQueryTerm differential baseline.  This is
/// research/test data only; production never loads this artifact.
pub(crate) fn parse_typed_get_query_term_reference(
    value: &JsonValue,
) -> TypedHookResult<TypedGetQueryTermReferenceBaseline> {
    ensure_reference_baseline_size(value)?;
    let baseline = serde_json::from_value::<TypedGetQueryTermReferenceBaseline>(value.clone())
        .map_err(|error| TypedHookError::new(format!("typed getQueryTerm reference schema: {error}")))?;
    validate_typed_get_query_term_reference(&baseline)?;
    Ok(baseline)
}

/// Parse the checked-in asdf getQueryTerm baseline bytes.
pub(crate) fn parse_typed_get_query_term_reference_bytes(
    bytes: &[u8],
) -> TypedHookResult<TypedGetQueryTermReferenceBaseline> {
    if bytes.len() > MAX_REFERENCE_BASELINE_BYTES {
        return Err(TypedHookError::new(format!(
            "typed getQueryTerm reference exceeds {MAX_REFERENCE_BASELINE_BYTES} bytes"
        )));
    }
    let baseline = serde_json::from_slice::<TypedGetQueryTermReferenceBaseline>(bytes)
        .map_err(|error| TypedHookError::new(format!("typed getQueryTerm reference schema: {error}")))?;
    validate_typed_get_query_term_reference(&baseline)?;
    Ok(baseline)
}

/// Parse a per-field typed-eval reference captured from sidecar descriptors.
pub(crate) fn parse_typed_field_reference(value: &JsonValue) -> TypedHookResult<TypedFieldReferenceBaseline> {
    ensure_reference_baseline_size(value)?;
    let baseline = serde_json::from_value::<TypedFieldReferenceBaseline>(value.clone())
        .map_err(|error| TypedHookError::new(format!("typed field reference schema: {error}")))?;
    validate_typed_field_reference(&baseline)?;
    Ok(baseline)
}

/// Parse checked-in per-field typed-eval reference bytes.
pub(crate) fn parse_typed_field_reference_bytes(bytes: &[u8]) -> TypedHookResult<TypedFieldReferenceBaseline> {
    if bytes.len() > MAX_REFERENCE_BASELINE_BYTES {
        return Err(TypedHookError::new(format!(
            "typed field reference exceeds {MAX_REFERENCE_BASELINE_BYTES} bytes"
        )));
    }
    let baseline = serde_json::from_slice::<TypedFieldReferenceBaseline>(bytes)
        .map_err(|error| TypedHookError::new(format!("typed field reference schema: {error}")))?;
    validate_typed_field_reference(&baseline)?;
    Ok(baseline)
}

fn ensure_catalog_size(value: &JsonValue) -> TypedHookResult<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| TypedHookError::new(format!("typed hook catalog schema: {error}")))?;
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(TypedHookError::new(format!(
            "typed hook catalog exceeds {MAX_CATALOG_BYTES} bytes"
        )));
    }
    Ok(())
}

fn ensure_reference_baseline_size(value: &JsonValue) -> TypedHookResult<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| TypedHookError::new(format!("typed trigger reference schema: {error}")))?;
    if bytes.len() > MAX_REFERENCE_BASELINE_BYTES {
        return Err(TypedHookError::new(format!(
            "typed trigger reference exceeds {MAX_REFERENCE_BASELINE_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_typed_trigger_reference(baseline: &TypedTriggerReferenceBaseline) -> TypedHookResult<()> {
    if baseline.version != REFERENCE_BASELINE_VERSION {
        return Err(TypedHookError::new(format!(
            "typed trigger reference version {} is unsupported",
            baseline.version
        )));
    }
    if baseline.kind != REFERENCE_BASELINE_KIND {
        return Err(TypedHookError::new(format!(
            "typed trigger reference kind {:?} is unsupported",
            baseline.kind
        )));
    }
    for (field, value) in [
        ("generatorSha256", &baseline.generator_sha256),
        ("harnessSha256", &baseline.harness_sha256),
        ("pairSha256", &baseline.pair_sha256),
        ("hookManifestSha256", &baseline.hook_manifest_sha256),
        ("sidecarSha256", &baseline.sidecar_sha256),
    ] {
        validate_sha256(value, field)?;
    }

    validate_reference_cases(&baseline.cases)?;
    validate_typed_hook_catalog(&baseline.catalog)?;
    if baseline.catalog.hooks.is_empty() {
        return Err(TypedHookError::new(
            "typed trigger reference catalog must contain at least one hook",
        ));
    }
    for (id, entry) in &baseline.catalog.hooks {
        if entry.source_field != SOURCE_FIELD {
            return Err(TypedHookError::new(format!(
                "typed trigger reference hook {id:?} sourceField must be {SOURCE_FIELD:?}"
            )));
        }
    }

    let catalog_ids: BTreeSet<_> = baseline.catalog.hooks.keys().collect();
    let expected_ids: BTreeSet<_> = baseline.expected.keys().collect();
    if catalog_ids != expected_ids {
        return Err(TypedHookError::new(
            "typed trigger reference expected ids do not match catalog ids",
        ));
    }
    for id in &expected_ids {
        let expected = baseline
            .expected
            .get(*id)
            .ok_or_else(|| TypedHookError::new("typed trigger reference expected id is missing"))?;
        if expected.len() != baseline.cases.len() {
            return Err(TypedHookError::new(format!(
                "typed trigger reference expected values for {id:?} have length {}, expected {}",
                expected.len(),
                baseline.cases.len()
            )));
        }
    }

    // sidecarSha256 is the full production sidecar file, which now contains
    // every side-effect-free field. The embedded catalog is the trigger-only
    // projection used by this baseline, so the digest must not be compared to
    // a re-serialization of that subset.
    Ok(())
}

fn validate_typed_get_query_term_reference(baseline: &TypedGetQueryTermReferenceBaseline) -> TypedHookResult<()> {
    if baseline.version != REFERENCE_BASELINE_VERSION {
        return Err(TypedHookError::new(format!(
            "typed getQueryTerm reference version {} is unsupported",
            baseline.version
        )));
    }
    if baseline.kind != GET_QUERY_TERM_REFERENCE_BASELINE_KIND {
        return Err(TypedHookError::new(format!(
            "typed getQueryTerm reference kind {:?} is unsupported",
            baseline.kind
        )));
    }
    for (field, value) in [
        ("generatorSha256", &baseline.generator_sha256),
        ("harnessSha256", &baseline.harness_sha256),
        ("pairSha256", &baseline.pair_sha256),
        ("hookManifestSha256", &baseline.hook_manifest_sha256),
    ] {
        validate_sha256(value, field)?;
    }
    if baseline.cases.is_empty() {
        return Err(TypedHookError::new(
            "typed getQueryTerm reference corpus must not be empty",
        ));
    }
    if baseline.cases.len() > MAX_REFERENCE_CASES {
        return Err(TypedHookError::new(format!(
            "typed getQueryTerm reference corpus exceeds {MAX_REFERENCE_CASES} cases"
        )));
    }
    let mut case_ids = BTreeSet::new();
    let mut corpus_bytes = 0usize;
    for case in &baseline.cases {
        validate_reference_case_id(&case.id)?;
        if !case_ids.insert(&case.id) {
            return Err(TypedHookError::new(format!(
                "typed getQueryTerm reference case id {:?} is duplicated",
                case.id
            )));
        }
        if case.args.len() != GET_QUERY_TERM_PARAM_COUNT {
            return Err(TypedHookError::new(format!(
                "typed getQueryTerm reference case {:?} must have exactly {GET_QUERY_TERM_PARAM_COUNT} argument",
                case.id
            )));
        }
        let serialized = serde_json::to_vec(case).map_err(|error| {
            TypedHookError::new(format!("typed getQueryTerm reference case serialization: {error}"))
        })?;
        if serialized
            .windows(REFERENCE_EXEC_MARKER.len())
            .any(|window| window == REFERENCE_EXEC_MARKER.as_bytes())
        {
            return Err(TypedHookError::new(
                "typed getQueryTerm reference corpus must not contain an executor marker",
            ));
        }
        corpus_bytes = corpus_bytes
            .checked_add(serialized.len())
            .ok_or_else(|| TypedHookError::new("typed getQueryTerm corpus size overflowed"))?;
    }
    if corpus_bytes > MAX_REFERENCE_CORPUS_BYTES {
        return Err(TypedHookError::new(format!(
            "typed getQueryTerm reference corpus exceeds {MAX_REFERENCE_CORPUS_BYTES} bytes"
        )));
    }

    let expected_candidate_ids: BTreeSet<_> = ASDF_GET_QUERY_TERM_CANDIDATE_IDS.into_iter().collect();
    let candidate_ids: BTreeSet<_> = baseline.candidates.keys().map(String::as_str).collect();
    if candidate_ids != expected_candidate_ids {
        return Err(TypedHookError::new(
            "typed getQueryTerm reference candidates must be exactly the two asdf hooks",
        ));
    }
    let expected_ids: BTreeSet<_> = baseline.expected.keys().map(String::as_str).collect();
    if candidate_ids != expected_ids {
        return Err(TypedHookError::new(
            "typed getQueryTerm reference expected ids do not match candidates",
        ));
    }
    for id in &expected_candidate_ids {
        let candidate = baseline
            .candidates
            .get(*id)
            .ok_or_else(|| TypedHookError::new(format!("typed getQueryTerm candidate {id:?} is missing")))?;
        validate_reference_file(&candidate.source, "source")?;
        validate_reference_file(&candidate.ir, "ir")?;
        validate_reference_file(&candidate.hook_file, "hookFile")?;
        validate_module_file(&candidate.module)?;
        validate_source_hook_path(&candidate.path)?;
        validate_sha256(&candidate.source_sha256, "sourceSha256")?;
        validate_sha256(&candidate.ir_sha256, "irSha256")?;
        validate_sha256(&candidate.hook_file_sha256, "hookFileSha256")?;
        validate_sha256(&candidate.module_sha256, "moduleSha256")?;
        validate_sha256(&candidate.function_body_sha256, "functionBodySha256")?;
        if candidate.source_field != GET_QUERY_TERM_SOURCE_FIELD {
            return Err(TypedHookError::new(format!(
                "typed getQueryTerm candidate {id:?} sourceField must be {GET_QUERY_TERM_SOURCE_FIELD:?}"
            )));
        }
        validate_typed_hook_ir(&candidate.descriptor)?;
        if candidate.descriptor.source_field != GET_QUERY_TERM_SOURCE_FIELD {
            return Err(TypedHookError::new(format!(
                "typed getQueryTerm candidate {id:?} descriptor sourceField is invalid"
            )));
        }
        let expected = baseline
            .expected
            .get(*id)
            .ok_or_else(|| TypedHookError::new(format!("typed getQueryTerm expected {id:?} is missing")))?;
        if expected.len() != baseline.cases.len() {
            return Err(TypedHookError::new(format!(
                "typed getQueryTerm expected values for {id:?} have length {}, expected {}",
                expected.len(),
                baseline.cases.len()
            )));
        }
    }
    Ok(())
}

fn validate_typed_field_reference(baseline: &TypedFieldReferenceBaseline) -> TypedHookResult<()> {
    if baseline.version != REFERENCE_BASELINE_VERSION {
        return Err(TypedHookError::new(format!(
            "typed field reference version {} is unsupported",
            baseline.version
        )));
    }
    if baseline.kind != FIELD_REFERENCE_BASELINE_KIND {
        return Err(TypedHookError::new(format!(
            "typed field reference kind {:?} is unsupported",
            baseline.kind
        )));
    }
    if typed_hook_contract(&baseline.field).is_none() {
        return Err(TypedHookError::new(format!(
            "typed field reference field {:?} is not a sidecar contract",
            baseline.field
        )));
    }
    for (name, value) in [
        ("generatorSha256", &baseline.generator_sha256),
        ("harnessSha256", &baseline.harness_sha256),
        ("pairSha256", &baseline.pair_sha256),
        ("hookManifestSha256", &baseline.hook_manifest_sha256),
        ("sidecarSha256", &baseline.sidecar_sha256),
    ] {
        validate_sha256(value, name)?;
    }
    if baseline.cases.is_empty() {
        return Err(TypedHookError::new("typed field reference corpus must not be empty"));
    }
    if baseline.cases.len() > MAX_REFERENCE_CASES {
        return Err(TypedHookError::new(format!(
            "typed field reference corpus exceeds {MAX_REFERENCE_CASES} cases"
        )));
    }
    let mut ids = BTreeSet::new();
    for case in &baseline.cases {
        validate_reference_case_id(&case.id)?;
        if !ids.insert(case.id.as_str()) {
            return Err(TypedHookError::new(format!(
                "typed field reference case id {:?} is duplicated",
                case.id
            )));
        }
        if case.args.iter().any(|value| value_contains_exec_marker(value)) {
            return Err(TypedHookError::new(format!(
                "typed field reference case {:?} contains an executor marker",
                case.id
            )));
        }
    }
    validate_typed_hook_catalog(&baseline.catalog)?;
    if baseline.catalog.hooks.is_empty() {
        return Err(TypedHookError::new(
            "typed field reference catalog must contain at least one hook",
        ));
    }
    for (id, entry) in &baseline.catalog.hooks {
        if entry.source_field != baseline.field {
            return Err(TypedHookError::new(format!(
                "typed field reference hook {id:?} sourceField must be {:?}",
                baseline.field
            )));
        }
    }
    let catalog_ids: BTreeSet<_> = baseline.catalog.hooks.keys().collect();
    let expected_ids: BTreeSet<_> = baseline.expected.keys().collect();
    if catalog_ids != expected_ids {
        return Err(TypedHookError::new(
            "typed field reference expected ids do not match catalog ids",
        ));
    }
    for id in &expected_ids {
        let expected = baseline
            .expected
            .get(*id)
            .ok_or_else(|| TypedHookError::new("typed field reference expected id is missing"))?;
        if expected.len() != baseline.cases.len() {
            return Err(TypedHookError::new(format!(
                "typed field reference expected values for {id:?} have length {}, expected {}",
                expected.len(),
                baseline.cases.len()
            )));
        }
    }
    Ok(())
}

fn value_contains_exec_marker(value: &JsonValue) -> bool {
    match value {
        JsonValue::String(text) => text.contains(REFERENCE_EXEC_MARKER),
        JsonValue::Array(items) => items.iter().any(value_contains_exec_marker),
        JsonValue::Object(fields) => fields
            .iter()
            .any(|(key, child)| key.contains(REFERENCE_EXEC_MARKER) || value_contains_exec_marker(child)),
        _ => false,
    }
}

fn validate_reference_cases(cases: &[TypedTriggerReferenceCase]) -> TypedHookResult<()> {
    if cases.is_empty() {
        return Err(TypedHookError::new("typed trigger reference corpus must not be empty"));
    }
    if cases.len() > MAX_REFERENCE_CASES {
        return Err(TypedHookError::new(format!(
            "typed trigger reference corpus exceeds {MAX_REFERENCE_CASES} cases"
        )));
    }

    let mut ids = BTreeSet::new();
    let mut corpus_bytes = 0usize;
    for case in cases {
        validate_reference_case_id(&case.id)?;
        if !ids.insert(&case.id) {
            return Err(TypedHookError::new(format!(
                "typed trigger reference case id {:?} is duplicated",
                case.id
            )));
        }
        if case.args.len() != TRIGGER_PARAM_COUNT {
            return Err(TypedHookError::new(format!(
                "typed trigger reference case {:?} must have exactly {TRIGGER_PARAM_COUNT} arguments",
                case.id
            )));
        }

        let serialized = serde_json::to_vec(case)
            .map_err(|error| TypedHookError::new(format!("typed trigger reference case serialization: {error}")))?;
        if serialized
            .windows(REFERENCE_EXEC_MARKER.len())
            .any(|window| window == REFERENCE_EXEC_MARKER.as_bytes())
        {
            return Err(TypedHookError::new(
                "typed trigger reference corpus must not contain an executor marker",
            ));
        }
        corpus_bytes = corpus_bytes
            .checked_add(serialized.len())
            .ok_or_else(|| TypedHookError::new("typed trigger reference corpus size overflowed"))?;
    }
    if corpus_bytes > MAX_REFERENCE_CORPUS_BYTES {
        return Err(TypedHookError::new(format!(
            "typed trigger reference corpus exceeds {MAX_REFERENCE_CORPUS_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_reference_case_id(id: &str) -> TypedHookResult<()> {
    if id.is_empty() {
        return Err(TypedHookError::new("typed trigger reference case id must not be empty"));
    }
    if id.len() > MAX_HOOK_ID_BYTES {
        return Err(TypedHookError::new(format!(
            "typed trigger reference case id exceeds {MAX_HOOK_ID_BYTES} UTF-8 bytes"
        )));
    }
    if id
        .chars()
        .any(|character| character.is_control() || matches!(character, '/' | '\\'))
    {
        return Err(TypedHookError::new(format!(
            "typed trigger reference case id {id:?} contains a forbidden control or path character"
        )));
    }
    Ok(())
}

fn validate_sidecar_contract(
    contract: &TypedHookContract,
    params: &[TypedValueType],
    result_type: TypedValueType,
    field: &str,
) -> TypedHookResult<()> {
    if contract.ir_version != IR_VERSION || contract.params != params || contract.result_type != result_type {
        return Err(TypedHookError::new(format!(
            "typed hook catalog {field} contract does not match the native contract"
        )));
    }
    Ok(())
}

fn validate_typed_hook_catalog(catalog: &TypedHookCatalog) -> TypedHookResult<()> {
    validate_typed_hook_catalog_index(catalog)?;
    for entry in catalog.hooks.values() {
        let descriptor = entry.descriptor.get()?;
        let descriptor_value = serde_json::to_value(descriptor)
            .map_err(|error| TypedHookError::new(format!("typed hook descriptor schema: {error}")))?;
        ensure_descriptor_size(&descriptor_value)?;
    }
    Ok(())
}

fn validate_typed_hook_catalog_index(catalog: &TypedHookCatalog) -> TypedHookResult<()> {
    validate_catalog_header(catalog.version, &catalog.kind, &catalog.contracts)?;
    validate_catalog_hook_count(catalog.hooks.len(), catalog.adapters.len())?;
    for (id, entry) in &catalog.hooks {
        validate_hook_entry(id, entry)?;
    }
    for (id, entry) in &catalog.adapters {
        validate_adapter_entry(id, entry, catalog.hooks.contains_key(id))?;
    }
    Ok(())
}

/// Everything about a catalog except its entries.
fn validate_catalog_header(version: u64, kind: &str, contracts: &TypedHookContracts) -> TypedHookResult<()> {
    if version != IR_VERSION {
        return Err(TypedHookError::new(format!(
            "typed hook catalog version {version} is unsupported"
        )));
    }
    if kind != CATALOG_KIND {
        return Err(TypedHookError::new(format!(
            "typed hook catalog kind {kind:?} is unsupported"
        )));
    }

    validate_sidecar_contract(
        &contracts.trigger,
        &[TypedValueType::String, TypedValueType::String],
        TypedValueType::Bool,
        "trigger",
    )?;
    validate_sidecar_contract(
        &contracts.get_query_term,
        &[TypedValueType::String],
        TypedValueType::String,
        "getQueryTerm",
    )?;
    validate_sidecar_contract(
        &contracts.post_process,
        &[TypedValueType::String, TypedValueType::StringArray],
        TypedValueType::SuggestionArray,
        "postProcess",
    )?;
    validate_sidecar_contract(
        &contracts.script,
        &[TypedValueType::StringArray],
        TypedValueType::StringArray,
        "script",
    )?;
    validate_sidecar_contract(
        &contracts.custom,
        &[
            TypedValueType::StringArray,
            TypedValueType::Exec,
            TypedValueType::Context,
        ],
        TypedValueType::SuggestionArray,
        "custom",
    )?;
    validate_sidecar_contract(
        &contracts.alias,
        &[TypedValueType::String, TypedValueType::Exec],
        TypedValueType::String,
        "alias",
    )?;
    validate_sidecar_contract(
        &contracts.load_spec,
        &[TypedValueType::String, TypedValueType::Exec],
        TypedValueType::Spec,
        "loadSpec",
    )?;
    validate_sidecar_contract(
        &contracts.generate_spec,
        &[TypedValueType::StringArray, TypedValueType::Exec],
        TypedValueType::Spec,
        "generateSpec",
    )?;
    validate_sidecar_contract(
        &contracts.filter_template_suggestions,
        &[TypedValueType::SuggestionArray],
        TypedValueType::SuggestionArray,
        "filterTemplateSuggestions",
    )?;
    Ok(())
}

fn validate_catalog_hook_count(hooks: usize, adapters: usize) -> TypedHookResult<()> {
    if hooks + adapters > MAX_CATALOG_HOOKS {
        return Err(TypedHookError::new(format!(
            "typed hook catalog contains more than {MAX_CATALOG_HOOKS} hooks"
        )));
    }
    Ok(())
}

fn validate_hook_entry(id: &str, entry: &TypedHookCatalogEntry) -> TypedHookResult<()> {
    validate_hook_id(id)?;
    validate_module_file(&entry.module)?;
    validate_source_hook_path(&entry.path)?;
    validate_sha256(&entry.module_sha256, "moduleSha256")?;
    validate_sha256(&entry.function_body_sha256, "functionBodySha256")?;
    if !SIDECAR_SOURCE_FIELDS.contains(&entry.source_field.as_str()) {
        return Err(TypedHookError::new(format!(
            "typed hook {id:?} sourceField must be one of the sidecar contracts"
        )));
    }
    if entry.descriptor.raw_json().len() > MAX_SERIALIZED_DESCRIPTOR_BYTES {
        return Err(TypedHookError::new(format!(
            "typed hook descriptor exceeds {MAX_SERIALIZED_DESCRIPTOR_BYTES} bytes"
        )));
    }
    Ok(())
}

/// `also_typed`: the same id has a typed descriptor, which is an error.
fn validate_adapter_entry(id: &str, entry: &TypedHookAdapterBinding, also_typed: bool) -> TypedHookResult<()> {
    validate_hook_id(id)?;
    if also_typed {
        return Err(TypedHookError::new(format!(
            "typed hook {id:?} cannot be both a typed descriptor and an adapter binding"
        )));
    }
    validate_source_hook_path(&entry.path)?;
    validate_sha256(&entry.function_body_sha256, "functionBodySha256")?;
    if !SIDECAR_SOURCE_FIELDS.contains(&entry.source_field.as_str()) {
        return Err(TypedHookError::new(format!(
            "typed hook adapter {id:?} sourceField must be one of the sidecar contracts"
        )));
    }
    Ok(())
}

fn validate_hook_id(id: &str) -> TypedHookResult<()> {
    if id.is_empty() {
        return Err(TypedHookError::new("typed hook id must not be empty"));
    }
    if id.len() > MAX_HOOK_ID_BYTES {
        return Err(TypedHookError::new(format!(
            "typed hook id exceeds {MAX_HOOK_ID_BYTES} UTF-8 bytes"
        )));
    }
    if id.contains(['\0', '\\']) {
        return Err(TypedHookError::new(
            "typed hook id contains a forbidden NUL or backslash",
        ));
    }
    Ok(())
}

fn validate_module_file(value: &str) -> TypedHookResult<()> {
    if value.is_empty()
        || value.len() > MAX_MODULE_BASENAME_BYTES
        || value.contains(['/', '\\', '\0'])
        || !value.ends_with(".js")
        || value == ".js"
        || value == "..js"
    {
        return Err(TypedHookError::new(format!(
            "invalid typed hook module basename {value:?}"
        )));
    }
    Ok(())
}

fn validate_reference_file(value: &str, field: &str) -> TypedHookResult<()> {
    if value.is_empty()
        || value.len() > MAX_HOOK_PATH_BYTES
        || value.starts_with('/')
        || value.contains(['\\', '\0'])
        || value
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(TypedHookError::new(format!(
            "typed getQueryTerm {field} must be a safe repository-relative file"
        )));
    }
    Ok(())
}

fn validate_source_hook_path(value: &str) -> TypedHookResult<()> {
    if value.len() > MAX_HOOK_PATH_BYTES || !is_safe_source_hook_path(value) {
        return Err(TypedHookError::new(format!("invalid typed hook source path {value:?}")));
    }
    Ok(())
}

fn is_safe_source_hook_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    if !bytes.starts_with(b"root") {
        return false;
    }
    let mut cursor = 4;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'.' => {
                cursor += 1;
                if cursor == bytes.len() || !is_identifier_start(bytes[cursor]) {
                    return false;
                }
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
            },
            b'[' => {
                cursor += 1;
                let start = cursor;
                while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                    cursor += 1;
                }
                if cursor == start || cursor == bytes.len() || bytes[cursor] != b']' {
                    return false;
                }
                cursor += 1;
            },
            _ => return false,
        }
    }
    true
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$'
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn validate_sha256(value: &str, field: &str) -> TypedHookResult<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(TypedHookError::new(format!(
            "typed hook {field} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;

        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

/// Find one catalog entry by its compiler-assigned hook id.
pub(crate) fn lookup_typed_hook<'a>(catalog: &'a TypedHookCatalog, hook_id: &str) -> Option<&'a TypedHookCatalogEntry> {
    catalog.hooks.get(hook_id)
}

/// Evaluate one catalog entry by id.  The catalog has already validated the
/// descriptor and provenance shape; a missing id remains an explicit error so
/// an orphaned IR reference cannot become a false negative trigger.
pub(crate) fn evaluate_typed_hook_by_id(
    catalog: &TypedHookCatalog,
    hook_id: &str,
    search_term: &str,
    previous_search_term: &str,
) -> TypedHookResult<bool> {
    let entry = lookup_typed_hook(catalog, hook_id)
        .ok_or_else(|| TypedHookError::new(format!("typed hook id {hook_id:?} is missing")))?;
    if entry.source_field != SOURCE_FIELD {
        return Err(TypedHookError::new(format!(
            "typed hook {hook_id:?} is not a trigger catalog entry"
        )));
    }
    evaluate_typed_trigger(entry.descriptor.get()?, search_term, previous_search_term)
}

fn ensure_descriptor_size(value: &JsonValue) -> TypedHookResult<()> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| TypedHookError::new(format!("typed hook schema: {error}")))?;
    if bytes.len() > MAX_SERIALIZED_DESCRIPTOR_BYTES {
        return Err(TypedHookError::new(format!(
            "typed hook descriptor exceeds {MAX_SERIALIZED_DESCRIPTOR_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_typed_hook_ir(descriptor: &TypedHookIr) -> TypedHookResult<()> {
    if descriptor.version != IR_VERSION {
        return Err(TypedHookError::new(format!(
            "typed hook version {} is unsupported",
            descriptor.version
        )));
    }
    if descriptor.kind != IR_KIND {
        return Err(TypedHookError::new(format!(
            "typed hook kind {:?} is unsupported",
            descriptor.kind
        )));
    }
    let (label, expected_result, expected_params) = typed_hook_contract(&descriptor.source_field).ok_or_else(|| {
        TypedHookError::new(format!(
            "typed hook source field {:?} is unsupported",
            descriptor.source_field
        ))
    })?;
    if descriptor.result_type != expected_result {
        return Err(TypedHookError::new(format!(
            "{label} typed hook resultType does not match its contract"
        )));
    }
    if descriptor.params.len() != expected_params.len() {
        return Err(TypedHookError::new(format!(
            "{label} typed hook requires exactly {} params",
            expected_params.len()
        )));
    }
    for (expected_index, param) in descriptor.params.iter().enumerate() {
        if param.index != expected_index as u64 || param.value_type != expected_params[expected_index] {
            return Err(TypedHookError::new(format!(
                "{label} parameter {expected_index} must be {:?} with matching index",
                expected_params[expected_index]
            )));
        }
    }

    let mut state = ValidationState::default();
    let result_type = validate_expr(
        &descriptor.expr,
        &descriptor.params,
        &mut state,
        0,
        "expr",
        Some(descriptor.result_type),
    )?;
    if result_type != descriptor.result_type && !types_compatible(result_type, descriptor.result_type) {
        return Err(TypedHookError::new(format!(
            "expression result type {result_type:?} does not match descriptor resultType {:?}",
            descriptor.result_type
        )));
    }
    Ok(())
}

fn types_compatible(actual: TypedValueType, expected: TypedValueType) -> bool {
    matches!(
        (actual, expected),
        (
            TypedValueType::ValueArray
                | TypedValueType::Json
                | TypedValueType::JsonArray
                | TypedValueType::Suggestion
                | TypedValueType::StringArray
                | TypedValueType::Null,
            TypedValueType::SuggestionArray
        ) | (
            TypedValueType::ValueArray | TypedValueType::Json | TypedValueType::JsonArray | TypedValueType::Null,
            TypedValueType::StringArray
        ) | (
            TypedValueType::Json | TypedValueType::Null | TypedValueType::StringRecord,
            TypedValueType::Suggestion
        ) | (TypedValueType::Suggestion, TypedValueType::Json)
            | (TypedValueType::StringSet, TypedValueType::StringArray)
            | (TypedValueType::StringArray, TypedValueType::ValueArray)
            | (
                TypedValueType::Json | TypedValueType::Null,
                TypedValueType::String
                    | TypedValueType::Integer
                    | TypedValueType::Bool
                    | TypedValueType::ValueArray
                    | TypedValueType::JsonArray
            )
            | (
                TypedValueType::String | TypedValueType::Integer | TypedValueType::Bool,
                TypedValueType::Json
            )
            | (
                TypedValueType::Json | TypedValueType::Null | TypedValueType::Suggestion,
                TypedValueType::Spec
            )
            | (TypedValueType::Spec, TypedValueType::Json)
            | (TypedValueType::ExecResult, TypedValueType::Json)
    )
}

fn typed_hook_contract(source_field: &str) -> Option<(&'static str, TypedValueType, &'static [TypedValueType])> {
    match source_field {
        SOURCE_FIELD => Some((
            "trigger",
            TypedValueType::Bool,
            &[TypedValueType::String, TypedValueType::String],
        )),
        GET_QUERY_TERM_SOURCE_FIELD => Some(("getQueryTerm", TypedValueType::String, &[TypedValueType::String])),
        "postProcess" => Some((
            "postProcess",
            TypedValueType::SuggestionArray,
            &[TypedValueType::String, TypedValueType::StringArray],
        )),
        "script" => Some(("script", TypedValueType::StringArray, &[TypedValueType::StringArray])),
        "filterTemplateSuggestions" => Some((
            "filterTemplateSuggestions",
            TypedValueType::SuggestionArray,
            &[TypedValueType::SuggestionArray],
        )),
        CUSTOM_SOURCE_FIELD => Some((
            "custom",
            TypedValueType::SuggestionArray,
            &[
                TypedValueType::StringArray,
                TypedValueType::Exec,
                TypedValueType::Context,
            ],
        )),
        ALIAS_SOURCE_FIELD => Some((
            "alias",
            TypedValueType::String,
            &[TypedValueType::String, TypedValueType::Exec],
        )),
        LOAD_SPEC_SOURCE_FIELD => Some((
            "loadSpec",
            TypedValueType::Spec,
            &[TypedValueType::String, TypedValueType::Exec],
        )),
        GENERATE_SPEC_SOURCE_FIELD => Some((
            "generateSpec",
            TypedValueType::Spec,
            &[TypedValueType::StringArray, TypedValueType::Exec],
        )),
        _ => None,
    }
}

fn is_string_literal(expression: &TypedExpr, expected: &str) -> bool {
    matches!(expression, TypedExpr::String { value } if value == expected)
}

fn validate_expr(
    expression: &TypedExpr,
    params: &[TypedHookParam],
    state: &mut ValidationState,
    depth: usize,
    path: &str,
    expected: Option<TypedValueType>,
) -> TypedHookResult<TypedValueType> {
    state.nodes += 1;
    if state.nodes > MAX_NODES || depth > MAX_DEPTH {
        return Err(TypedHookError::new(format!(
            "{path} exceeds typed hook complexity limits"
        )));
    }
    let child = |expression: &TypedExpr,
                 expected: Option<TypedValueType>,
                 name: &str,
                 state: &mut ValidationState|
     -> TypedHookResult<TypedValueType> {
        let result = validate_expr(
            expression,
            params,
            state,
            depth + 1,
            &format!("{path}.{name}"),
            expected,
        )?;
        if expected.is_some_and(|expected| expected != result && !types_compatible(result, expected)) {
            return Err(TypedHookError::new(format!(
                "{path}.{name} has type {result:?}, expected {expected:?}"
            )));
        }
        Ok(result)
    };
    match expression {
        TypedExpr::Arg { index } => {
            if *index > usize::MAX as u64 {
                return Err(TypedHookError::new(format!(
                    "{path}.index {index} cannot be represented"
                )));
            }
            let Some(param) = params.get(*index as usize) else {
                return Err(TypedHookError::new(format!(
                    "{path}.index {index} is outside the parameter contract"
                )));
            };
            Ok(param.value_type)
        },
        TypedExpr::String { value } => {
            if value.encode_utf16().count() > MAX_STRING_LITERAL_UNITS {
                return Err(TypedHookError::new(format!(
                    "{path} exceeds {MAX_STRING_LITERAL_UNITS} UTF-16 code units"
                )));
            }
            Ok(TypedValueType::String)
        },
        TypedExpr::Bool { .. } => Ok(TypedValueType::Bool),
        TypedExpr::Null => Ok(TypedValueType::Null),
        TypedExpr::Integer { value } => {
            ensure_safe_integer(*value, path)?;
            Ok(TypedValueType::Integer)
        },
        TypedExpr::Array { items } => {
            for (index, item) in items.iter().enumerate() {
                child(item, None, &format!("items[{index}]"), state)?;
            }
            Ok(TypedValueType::ValueArray)
        },
        TypedExpr::Length { value } => {
            child(value, None, "value", state)?;
            Ok(TypedValueType::Integer)
        },
        TypedExpr::StringIncludes { value, needle } => {
            child(value, Some(TypedValueType::String), "value", state)?;
            child(needle, Some(TypedValueType::String), "needle", state)?;
            Ok(TypedValueType::Bool)
        },
        TypedExpr::StringIndexOf { value, needle } | TypedExpr::StringLastIndexOf { value, needle } => {
            child(value, Some(TypedValueType::String), "value", state)?;
            child(needle, Some(TypedValueType::String), "needle", state)?;
            Ok(TypedValueType::Integer)
        },
        TypedExpr::StringSlice { value, start } => {
            child(value, Some(TypedValueType::String), "value", state)?;
            child(start, Some(TypedValueType::Integer), "start", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::StringSliceAfterFirst { value, needle } => {
            child(value, Some(TypedValueType::String), "value", state)?;
            child(needle, Some(TypedValueType::String), "needle", state)?;
            if !is_string_literal(needle, ":") {
                return Err(TypedHookError::new(format!(
                    "{path}.needle must be the closed getQueryTerm colon literal"
                )));
            }
            Ok(TypedValueType::String)
        },
        TypedExpr::StringSubstring { value, start, end } => {
            child(value, Some(TypedValueType::String), "value", state)?;
            child(start, Some(TypedValueType::Integer), "start", state)?;
            child(end, Some(TypedValueType::Integer), "end", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::StringSplit { value, separator } => {
            child(value, Some(TypedValueType::String), "value", state)?;
            child(separator, Some(TypedValueType::String), "separator", state)?;
            Ok(TypedValueType::StringArray)
        },
        TypedExpr::StringTrim { value }
        | TypedExpr::StringTrimStart { value }
        | TypedExpr::StringTrimEnd { value }
        | TypedExpr::StringToLower { value }
        | TypedExpr::StringToUpper { value } => {
            child(value, Some(TypedValueType::String), "value", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::StringReplace {
            value,
            needle,
            replacement,
        }
        | TypedExpr::StringReplaceAll {
            value,
            needle,
            replacement,
        } => {
            child(value, Some(TypedValueType::String), "value", state)?;
            child(needle, Some(TypedValueType::String), "needle", state)?;
            if !matches!(needle.as_ref(), TypedExpr::String { .. }) {
                return Err(TypedHookError::new(format!("{path}.needle must be a string literal")));
            }
            child(replacement, Some(TypedValueType::String), "replacement", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::StringStartsWith { value, needle } | TypedExpr::StringEndsWith { value, needle } => {
            child(value, Some(TypedValueType::String), "value", state)?;
            child(needle, Some(TypedValueType::String), "needle", state)?;
            Ok(TypedValueType::Bool)
        },
        TypedExpr::StringPadStart { value, target, pad } | TypedExpr::StringPadEnd { value, target, pad } => {
            child(value, Some(TypedValueType::String), "value", state)?;
            child(target, Some(TypedValueType::Integer), "target", state)?;
            child(pad, Some(TypedValueType::String), "pad", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::StringRepeat { value, count } => {
            child(value, Some(TypedValueType::String), "value", state)?;
            child(count, Some(TypedValueType::Integer), "count", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::StringConcat { parts } => {
            if parts.is_empty() {
                return Err(TypedHookError::new(format!("{path}.parts must be a non-empty array")));
            }
            for (index, part) in parts.iter().enumerate() {
                child(part, None, &format!("parts[{index}]"), state)?;
            }
            Ok(TypedValueType::String)
        },
        TypedExpr::StringCharAt { value, index } | TypedExpr::StringAt { value, index } => {
            child(value, Some(TypedValueType::String), "value", state)?;
            child(index, Some(TypedValueType::Integer), "index", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::ArrayIncludes { value, needle } => {
            child(value, None, "value", state)?;
            child(needle, None, "needle", state)?;
            Ok(TypedValueType::Bool)
        },
        TypedExpr::StrictEq { left, right } | TypedExpr::StrictNe { left, right } => {
            let left_type = child(left, None, "left", state)?;
            child(right, None, "right", state)?;
            if matches!(
                left_type,
                TypedValueType::StringArray
                    | TypedValueType::ValueArray
                    | TypedValueType::SuggestionArray
                    | TypedValueType::JsonArray
            ) {
                return Err(TypedHookError::new(format!("{path} cannot compare arrays by identity")));
            }
            Ok(TypedValueType::Bool)
        },
        TypedExpr::Add { left, right } | TypedExpr::Sub { left, right } | TypedExpr::Mul { left, right } => {
            child(left, Some(TypedValueType::Integer), "left", state)?;
            child(right, Some(TypedValueType::Integer), "right", state)?;
            Ok(TypedValueType::Integer)
        },
        TypedExpr::LessThan { left, right }
        | TypedExpr::LessThanOrEqual { left, right }
        | TypedExpr::GreaterThan { left, right }
        | TypedExpr::GreaterThanOrEqual { left, right } => {
            child(left, Some(TypedValueType::Integer), "left", state)?;
            child(right, Some(TypedValueType::Integer), "right", state)?;
            Ok(TypedValueType::Bool)
        },
        TypedExpr::Not { value } => {
            child(value, None, "value", state)?;
            Ok(TypedValueType::Bool)
        },
        TypedExpr::Nullish { left, right } => {
            let left_type = child(left, None, "left", state)?;
            let right_type = child(right, None, "right", state)?;
            Ok(if left_type == TypedValueType::Null {
                right_type
            } else if right_type == TypedValueType::Null || left_type == right_type {
                left_type
            } else {
                TypedValueType::Json
            })
        },
        TypedExpr::And { left, right } | TypedExpr::Or { left, right } => {
            let left_type = child(left, None, "left", state)?;
            let right_type = child(right, None, "right", state)?;
            Ok(expected.unwrap_or(if left_type == right_type {
                left_type
            } else {
                TypedValueType::Json
            }))
        },
        TypedExpr::If {
            condition,
            then_branch,
            else_branch,
        } => {
            child(condition, None, "condition", state)?;
            let then_type = child(then_branch, expected, "then", state)?;
            let else_type = child(else_branch, expected, "else", state)?;
            Ok(expected.unwrap_or(if then_type == else_type {
                then_type
            } else {
                TypedValueType::Json
            }))
        },
        TypedExpr::Lambda { params, body } => {
            if params.iter().any(String::is_empty) {
                return Err(TypedHookError::new(format!("{path}.params must be non-empty names")));
            }
            child(body, None, "body", state)?;
            Ok(TypedValueType::Json)
        },
        TypedExpr::Var { name } | TypedExpr::ArrayShift { name } => {
            if name.is_empty() {
                return Err(TypedHookError::new(format!("{path}.name must be a non-empty string")));
            }
            Ok(expected.unwrap_or(TypedValueType::Json))
        },
        TypedExpr::Let { name, value, body } => {
            if name.is_empty() {
                return Err(TypedHookError::new(format!("{path}.name must be a non-empty string")));
            }
            child(value, None, "value", state)?;
            child(body, expected, "body", state)
        },
        TypedExpr::Block { items } | TypedExpr::Seq { items } => {
            let mut result = TypedValueType::Null;
            for (index, item) in items.iter().enumerate() {
                result = child(item, None, &format!("items[{index}]"), state)?;
            }
            Ok(expected.unwrap_or(result))
        },
        TypedExpr::Return { value } => child(value, expected, "value", state),
        TypedExpr::CatchReturn { body } => child(body, expected, "body", state),
        TypedExpr::Break | TypedExpr::Continue => Ok(TypedValueType::Null),
        TypedExpr::Try { body, catch } => {
            child(body, expected, "body", state)?;
            child(catch, expected, "catch", state)
        },
        TypedExpr::ForOf { names, value, body } => {
            if names.is_empty() || names.iter().any(String::is_empty) {
                return Err(TypedHookError::new(format!(
                    "{path}.names must be a non-empty string array"
                )));
            }
            child(value, None, "value", state)?;
            child(body, None, "body", state)?;
            Ok(TypedValueType::Null)
        },
        TypedExpr::AssignVar { name, value } => {
            if name.is_empty() {
                return Err(TypedHookError::new(format!("{path}.name must be a non-empty string")));
            }
            child(value, None, "value", state)
        },
        TypedExpr::AssignProp { object, key, value } => {
            child(object, None, "object", state)?;
            child(key, None, "key", state)?;
            child(value, None, "value", state)
        },
        TypedExpr::Truthy { value } => {
            child(value, None, "value", state)?;
            Ok(TypedValueType::Bool)
        },
        TypedExpr::LooseEq { left, right } | TypedExpr::LooseNe { left, right } => {
            child(left, None, "left", state)?;
            child(right, None, "right", state)?;
            Ok(TypedValueType::Bool)
        },
        TypedExpr::Typeof { value } | TypedExpr::ToString { value } | TypedExpr::JsonStringify { value } => {
            child(value, None, "value", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::Object { fields } | TypedExpr::JsonObject { fields } => {
            for (index, field) in fields.iter().enumerate() {
                child(&field.value, None, &format!("fields[{index}].value"), state)?;
            }
            Ok(if matches!(expression, TypedExpr::JsonObject { .. }) {
                TypedValueType::Json
            } else {
                TypedValueType::Suggestion
            })
        },
        TypedExpr::Spread { value, fields } => {
            child(value, None, "value", state)?;
            for (index, field) in fields.iter().enumerate() {
                child(&field.value, None, &format!("fields[{index}].value"), state)?;
            }
            Ok(TypedValueType::Suggestion)
        },
        TypedExpr::ObjectAssign { parts } => {
            for (index, part) in parts.iter().enumerate() {
                child(part, None, &format!("parts[{index}]"), state)?;
            }
            Ok(expected.unwrap_or(TypedValueType::Suggestion))
        },
        TypedExpr::Get { value, key } | TypedExpr::JsonGet { value, key } => {
            child(value, None, "value", state)?;
            child(key, None, "key", state)?;
            Ok(expected.unwrap_or(TypedValueType::Json))
        },
        TypedExpr::ObjectKeys { value } => {
            child(value, None, "value", state)?;
            Ok(TypedValueType::StringArray)
        },
        TypedExpr::ObjectEntries { value }
        | TypedExpr::ObjectValues { value }
        | TypedExpr::ArrayEntries { value }
        | TypedExpr::ArrayFrom { value }
        | TypedExpr::JsonArrayItems { value } => {
            child(value, None, "value", state)?;
            Ok(TypedValueType::ValueArray)
        },
        TypedExpr::ArrayMap { value, callback }
        | TypedExpr::ArrayFilter { value, callback }
        | TypedExpr::ArrayFlatMap { value, callback }
        | TypedExpr::ArraySort { value, callback } => {
            child(value, None, "value", state)?;
            child(callback, None, "fn", state)?;
            Ok(expected.unwrap_or(TypedValueType::ValueArray))
        },
        TypedExpr::ArraySome { value, callback } | TypedExpr::ArrayEvery { value, callback } => {
            child(value, None, "value", state)?;
            child(callback, None, "fn", state)?;
            Ok(TypedValueType::Bool)
        },
        TypedExpr::ArrayFind { value, callback } => {
            child(value, None, "value", state)?;
            child(callback, None, "fn", state)?;
            Ok(TypedValueType::Json)
        },
        TypedExpr::ArrayFindIndex { value, callback } => {
            child(value, None, "value", state)?;
            child(callback, None, "fn", state)?;
            Ok(TypedValueType::Integer)
        },
        TypedExpr::ArraySlice { value, start, end } => {
            child(value, None, "value", state)?;
            child(start, None, "start", state)?;
            child(end, None, "end", state)?;
            Ok(expected.unwrap_or(TypedValueType::ValueArray))
        },
        TypedExpr::ArrayJoin { value, separator } => {
            child(value, None, "value", state)?;
            child(separator, None, "separator", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::ArrayIndexOf { value, needle } => {
            child(value, None, "value", state)?;
            child(needle, None, "needle", state)?;
            Ok(TypedValueType::Integer)
        },
        TypedExpr::ArrayIndexOfFrom { value, needle, start } => {
            child(value, None, "value", state)?;
            child(needle, None, "needle", state)?;
            child(start, None, "start", state)?;
            Ok(TypedValueType::Integer)
        },
        TypedExpr::ArrayConcat { parts } => {
            for (index, part) in parts.iter().enumerate() {
                child(part, None, &format!("parts[{index}]"), state)?;
            }
            Ok(expected.unwrap_or(TypedValueType::ValueArray))
        },
        TypedExpr::ArrayReverse { value } => {
            child(value, None, "value", state)?;
            Ok(expected.unwrap_or(TypedValueType::ValueArray))
        },
        TypedExpr::ArrayPop { value } => {
            child(value, None, "value", state)?;
            Ok(expected.unwrap_or(TypedValueType::Json))
        },
        TypedExpr::ArrayIndex { value, index } => {
            child(value, None, "value", state)?;
            child(index, None, "index", state)?;
            Ok(expected.unwrap_or(TypedValueType::Json))
        },
        TypedExpr::ArrayPush { name, item } | TypedExpr::StringSetAdd { name, item } => {
            if name.is_empty() {
                return Err(TypedHookError::new(format!("{path}.name must be a non-empty string")));
            }
            child(item, None, "item", state)?;
            Ok(if matches!(expression, TypedExpr::ArrayPush { .. }) {
                TypedValueType::Integer
            } else {
                TypedValueType::StringSet
            })
        },
        TypedExpr::ArrayFlat { value, depth } => {
            child(value, None, "value", state)?;
            child(depth, None, "depth", state)?;
            Ok(TypedValueType::ValueArray)
        },
        TypedExpr::JsonParse { value } => {
            child(value, None, "value", state)?;
            Ok(TypedValueType::Json)
        },
        TypedExpr::JsonAsString { value } => {
            child(value, None, "value", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::JsonAsNumber { value } => {
            child(value, None, "value", state)?;
            Ok(TypedValueType::Integer)
        },
        TypedExpr::JsonAsBool { value } => {
            child(value, None, "value", state)?;
            Ok(TypedValueType::Bool)
        },
        TypedExpr::RegexTest { value, pattern, flags }
        | TypedExpr::RegexMatch { value, pattern, flags }
        | TypedExpr::RegexMatchAll { value, pattern, flags }
        | TypedExpr::RegexSearch { value, pattern, flags }
        | TypedExpr::StringSplitRegex { value, pattern, flags } => {
            child(value, None, "value", state)?;
            validate_regex_literal(pattern, flags, path)?;
            Ok(match expression {
                TypedExpr::RegexTest { .. } => TypedValueType::Bool,
                TypedExpr::RegexSearch { .. } => TypedValueType::Integer,
                TypedExpr::StringSplitRegex { .. } => TypedValueType::StringArray,
                _ => TypedValueType::ValueArray,
            })
        },
        TypedExpr::RegexReplace {
            value,
            pattern,
            flags,
            replacement,
        } => {
            child(value, None, "value", state)?;
            validate_regex_literal(pattern, flags, path)?;
            child(replacement, None, "replacement", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::StringSplitLimit {
            value,
            separator,
            limit,
        } => {
            child(value, None, "value", state)?;
            child(separator, None, "separator", state)?;
            child(limit, None, "limit", state)?;
            Ok(TypedValueType::StringArray)
        },
        TypedExpr::StringIndexOfFrom { value, needle, start } => {
            child(value, None, "value", state)?;
            child(needle, None, "needle", state)?;
            child(start, None, "start", state)?;
            Ok(TypedValueType::Integer)
        },
        TypedExpr::MathMax { values } => {
            child(values, None, "values", state)?;
            Ok(TypedValueType::Integer)
        },
        TypedExpr::StringSet { value } => {
            child(value, None, "value", state)?;
            Ok(TypedValueType::StringSet)
        },
        TypedExpr::Regex { pattern, flags } => {
            validate_regex_literal(pattern, flags, path)?;
            Ok(TypedValueType::Regex)
        },
        TypedExpr::While { condition, body } => {
            child(condition, None, "condition", state)?;
            child(body, None, "body", state)?;
            Ok(TypedValueType::Null)
        },
        TypedExpr::LocaleCompare { left, right } => {
            child(left, None, "left", state)?;
            child(right, None, "right", state)?;
            Ok(TypedValueType::Integer)
        },
        TypedExpr::StringSliceRange { value, start, end } => {
            child(value, None, "value", state)?;
            child(start, None, "start", state)?;
            child(end, None, "end", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::Exec {
            command,
            args,
            cwd,
            env,
            timeout,
        } => {
            child(command, Some(TypedValueType::String), "command", state)?;
            child(args, Some(TypedValueType::StringArray), "args", state)?;
            child(cwd, None, "cwd", state)?;
            child(env, None, "env", state)?;
            child(timeout, None, "timeout", state)?;
            Ok(TypedValueType::ExecResult)
        },
        TypedExpr::Par { items } => {
            for (index, item) in items.iter().enumerate() {
                child(item, None, &format!("items[{index}]"), state)?;
            }
            Ok(TypedValueType::ValueArray)
        },
        TypedExpr::CtxCwd | TypedExpr::CtxProcess | TypedExpr::CtxSshPrefix | TypedExpr::CtxSearchTerm => {
            Ok(TypedValueType::String)
        },
        TypedExpr::CtxIsDangerous => Ok(TypedValueType::Bool),
        TypedExpr::CtxEnvironment => Ok(TypedValueType::StringRecord),
        TypedExpr::CtxEnv { name } => {
            child(name, Some(TypedValueType::String), "name", state)?;
            Ok(TypedValueType::String)
        },
        TypedExpr::SpecObject { fields } => {
            for (index, field) in fields.iter().enumerate() {
                child(&field.value, None, &format!("fields[{index}].value"), state)?;
            }
            Ok(TypedValueType::Spec)
        },
        TypedExpr::Throw { message, .. } => {
            child(message, Some(TypedValueType::String), "message", state)?;
            Ok(expected.unwrap_or(TypedValueType::Null))
        },
    }
}

fn validate_regex_literal(pattern: &str, flags: &str, path: &str) -> TypedHookResult<()> {
    if pattern.encode_utf16().count() > 4 * 1024 {
        return Err(TypedHookError::new(format!(
            "{path}.pattern exceeds the regex unit limit"
        )));
    }
    if flags.contains('v') {
        return Err(TypedHookError::new(format!("{path}.flags must not include v")));
    }
    fancy_regex::Regex::new(pattern)
        .map(|_| ())
        .map_err(|error| TypedHookError::new(format!("{path}.pattern is not a valid regex: {error}")))
}

fn ensure_safe_integer(value: i64, path: &str) -> TypedHookResult<()> {
    if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value) {
        return Err(TypedHookError::new(format!("{path} must be a JavaScript safe integer")));
    }
    Ok(())
}

/// Evaluate a validated trigger descriptor against its two string arguments.
/// The return type is deliberately boolean because the trigger contract is
/// fixed to `{ string, string } -> bool` in this first native slice.
pub(crate) fn evaluate_typed_trigger(
    descriptor: &TypedHookIr,
    search_term: &str,
    previous_search_term: &str,
) -> TypedHookResult<bool> {
    let arguments = [
        TypedValue::String(Utf16String::from_str(search_term)),
        TypedValue::String(Utf16String::from_str(previous_search_term)),
    ];
    let value = evaluate_expr(&descriptor.expr, &arguments)?;
    match value {
        TypedValue::Bool(value) => Ok(value),
        _ => Err(TypedHookError::new(
            "validated trigger expression did not evaluate to bool",
        )),
    }
}

/// Evaluate a getQueryTerm descriptor.
pub(crate) fn evaluate_typed_get_query_term(descriptor: &TypedHookIr, search_term: &str) -> TypedHookResult<String> {
    validate_typed_hook_ir(descriptor)?;
    if descriptor.source_field != GET_QUERY_TERM_SOURCE_FIELD {
        return Err(TypedHookError::new("descriptor is not a getQueryTerm hook"));
    }
    let arguments = [TypedValue::String(Utf16String::from_str(search_term))];
    let value = evaluate_expr(&descriptor.expr, &arguments)?;
    let TypedValue::String(value) = value else {
        return Err(TypedHookError::new(
            "validated getQueryTerm expression did not evaluate to string",
        ));
    };
    String::from_utf16(&value.0)
        .map_err(|error| TypedHookError::new(format!("getQueryTerm returned invalid UTF-16: {error}")))
}

/// Evaluate a postProcess descriptor.
pub(crate) fn evaluate_typed_post_process(
    descriptor: &TypedHookIr,
    stdout: &str,
    tokens: &[String],
) -> TypedHookResult<Vec<Suggestion>> {
    validate_typed_hook_ir(descriptor)?;
    if descriptor.source_field != POST_PROCESS_SOURCE_FIELD {
        return Err(TypedHookError::new("descriptor is not a postProcess hook"));
    }
    let json = evaluate_typed_hook_json(
        descriptor,
        &[
            JsonValue::String(stdout.to_owned()),
            JsonValue::Array(tokens.iter().map(|token| JsonValue::String(token.clone())).collect()),
        ],
    )?;
    suggestions_from_typed_json(&json)
}

/// Evaluate a script descriptor to the same `ScriptCommand` shape the native
/// backend returns.
pub(crate) fn evaluate_typed_script(descriptor: &TypedHookIr, tokens: &[String]) -> TypedHookResult<ScriptCommand> {
    validate_typed_hook_ir(descriptor)?;
    if descriptor.source_field != SCRIPT_SOURCE_FIELD {
        return Err(TypedHookError::new("descriptor is not a script hook"));
    }
    let json = evaluate_typed_hook_json(
        descriptor,
        &[JsonValue::Array(
            tokens.iter().map(|token| JsonValue::String(token.clone())).collect(),
        )],
    )?;
    script_command_from_typed_json(&json)
}

/// Evaluate a filterTemplateSuggestions descriptor. Test-only until T3.1.
pub(crate) fn evaluate_typed_filter_template_suggestions(
    descriptor: &TypedHookIr,
    suggestions: &[Suggestion],
) -> TypedHookResult<Vec<Suggestion>> {
    validate_typed_hook_ir(descriptor)?;
    if descriptor.source_field != FILTER_TEMPLATE_SUGGESTIONS_SOURCE_FIELD {
        return Err(TypedHookError::new(
            "descriptor is not a filterTemplateSuggestions hook",
        ));
    }
    let json = evaluate_typed_hook_json(descriptor, &[suggestions_to_typed_json(suggestions)])?;
    suggestions_from_typed_json(&json)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypedExecRequest {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypedExecResult {
    pub stdout: String,
    pub stderr: String,
    pub status: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypedHookContext {
    pub current_working_directory: String,
    pub current_process: String,
    pub ssh_prefix: String,
    pub environment_variables: crate::hook_types::HookEnvironment,
    pub search_term: String,
    pub is_dangerous: bool,
}

pub(crate) struct TypedHookEffects<'a> {
    pub context: &'a TypedHookContext,
    pub exec: &'a dyn Fn(TypedExecRequest) -> TypedHookResult<TypedExecResult>,
    pub deadline: Option<std::time::Instant>,
}

#[cfg(test)]
impl TypedHookContext {
    fn from_baseline(context: &crate::hook_baseline::HookContext) -> Self {
        Self {
            current_working_directory: context.current_working_directory.clone(),
            current_process: context.current_process.clone(),
            ssh_prefix: context.ssh_prefix.clone(),
            environment_variables: context.environment_variables.clone(),
            search_term: context.search_term.clone(),
            is_dangerous: context.is_dangerous,
        }
    }
}

/// Evaluate a custom descriptor. The Native backend calls this at runtime.
pub(crate) fn evaluate_typed_custom(
    descriptor: &TypedHookIr,
    tokens: &[String],
    context: &TypedHookContext,
    exec: &dyn Fn(TypedExecRequest) -> TypedHookResult<TypedExecResult>,
    deadline: Option<std::time::Instant>,
) -> TypedHookResult<Vec<Suggestion>> {
    validate_typed_hook_ir(descriptor)?;
    if descriptor.source_field != CUSTOM_SOURCE_FIELD {
        return Err(TypedHookError::new("descriptor is not a custom hook"));
    }
    let json = evaluate_typed_effect_json(
        descriptor,
        &[JsonValue::Array(
            tokens.iter().map(|token| JsonValue::String(token.clone())).collect(),
        )],
        context,
        exec,
        deadline,
    )?;
    suggestions_from_typed_json(&json)
}

pub(crate) fn evaluate_typed_alias(
    descriptor: &TypedHookIr,
    token: &str,
    context: &TypedHookContext,
    exec: &dyn Fn(TypedExecRequest) -> TypedHookResult<TypedExecResult>,
    deadline: Option<std::time::Instant>,
) -> TypedHookResult<String> {
    validate_typed_hook_ir(descriptor)?;
    if descriptor.source_field != ALIAS_SOURCE_FIELD {
        return Err(TypedHookError::new("descriptor is not an alias hook"));
    }
    let json = evaluate_typed_effect_json(
        descriptor,
        &[JsonValue::String(token.to_owned())],
        context,
        exec,
        deadline,
    )?;
    json.as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| TypedHookError::new("typed alias result must be a string"))
}

pub(crate) fn evaluate_typed_load_spec(
    descriptor: &TypedHookIr,
    token: &str,
    context: &TypedHookContext,
    exec: &dyn Fn(TypedExecRequest) -> TypedHookResult<TypedExecResult>,
    deadline: Option<std::time::Instant>,
) -> TypedHookResult<JsonValue> {
    validate_typed_hook_ir(descriptor)?;
    if descriptor.source_field != LOAD_SPEC_SOURCE_FIELD {
        return Err(TypedHookError::new("descriptor is not a loadSpec hook"));
    }
    evaluate_typed_effect_json(
        descriptor,
        &[JsonValue::String(token.to_owned())],
        context,
        exec,
        deadline,
    )
}

pub(crate) fn evaluate_typed_generate_spec(
    descriptor: &TypedHookIr,
    tokens: &[String],
    context: &TypedHookContext,
    exec: &dyn Fn(TypedExecRequest) -> TypedHookResult<TypedExecResult>,
    deadline: Option<std::time::Instant>,
) -> TypedHookResult<JsonValue> {
    validate_typed_hook_ir(descriptor)?;
    if descriptor.source_field != GENERATE_SPEC_SOURCE_FIELD {
        return Err(TypedHookError::new("descriptor is not a generateSpec hook"));
    }
    evaluate_typed_effect_json(
        descriptor,
        &[JsonValue::Array(
            tokens.iter().map(|token| JsonValue::String(token.clone())).collect(),
        )],
        context,
        exec,
        deadline,
    )
}

pub(crate) fn evaluate_typed_effect_json(
    descriptor: &TypedHookIr,
    args: &[JsonValue],
    context: &TypedHookContext,
    exec: &dyn Fn(TypedExecRequest) -> TypedHookResult<TypedExecResult>,
    deadline: Option<std::time::Instant>,
) -> TypedHookResult<JsonValue> {
    let runtime_params: Vec<_> = descriptor
        .params
        .iter()
        .filter(|param| !matches!(param.value_type, TypedValueType::Exec | TypedValueType::Context))
        .collect();
    if args.len() != runtime_params.len() {
        return Err(TypedHookError::new("evaluate args must match the field contract"));
    }
    let mut arguments = vec![TypedValue::Null; descriptor.params.len()];
    let mut arg_index = 0usize;
    for param in &descriptor.params {
        if matches!(param.value_type, TypedValueType::Exec | TypedValueType::Context) {
            continue;
        }
        arguments[param.index as usize] = json_to_typed_value(&args[arg_index], param.value_type)?;
        arg_index += 1;
    }
    let effects = TypedHookEffects {
        context,
        exec,
        deadline,
    };
    let value = typed_value_to_json(&evaluate_expr_with_effects(&descriptor.expr, &arguments, &effects)?)?;
    Ok(match descriptor.result_type {
        TypedValueType::SuggestionArray => normalize_suggestion_array(value),
        TypedValueType::Suggestion => normalize_suggestion_value(value),
        _ => value,
    })
}

fn suggestions_to_typed_json(suggestions: &[Suggestion]) -> JsonValue {
    JsonValue::Array(
        suggestions
            .iter()
            .map(|suggestion| {
                let mut object = serde_json::Map::new();
                object.insert("name".into(), JsonValue::String(suggestion.name.clone()));
                object.insert("type".into(), JsonValue::String(suggestion.kind.clone()));
                if !suggestion.description.is_empty() {
                    object.insert("description".into(), JsonValue::String(suggestion.description.clone()));
                }
                if let Some(insert) = suggestion.insert_value.clone() {
                    object.insert("insertValue".into(), JsonValue::String(insert));
                }
                if let Some(display) = suggestion.display_name.clone() {
                    object.insert("displayName".into(), JsonValue::String(display));
                }
                if let Some(icon) = suggestion.icon.clone() {
                    object.insert("icon".into(), JsonValue::String(icon));
                }
                if suggestion.hidden {
                    object.insert("hidden".into(), JsonValue::Bool(true));
                }
                if suggestion.is_dangerous {
                    object.insert("isDangerous".into(), JsonValue::Bool(true));
                }
                if suggestion.priority != 50 {
                    object.insert("priority".into(), JsonValue::Number(suggestion.priority.into()));
                }
                JsonValue::Object(object)
            })
            .collect(),
    )
}

pub(crate) fn suggestions_from_typed_json(value: &JsonValue) -> TypedHookResult<Vec<Suggestion>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let Some(items) = value.as_array() else {
        return Err(TypedHookError::new("typed suggestion result must be an array"));
    };
    let mut out = Vec::new();
    for item in items {
        if let Some(suggestion) = suggestion_from_typed_json(item) {
            out.push(suggestion);
        }
    }
    Ok(out)
}

fn suggestion_from_typed_json(item: &JsonValue) -> Option<Suggestion> {
    if let Some(name) = item.as_str() {
        if name.is_empty() {
            return None;
        }
        return Some(Suggestion::new(name, "", "arg").with_insert_value(name));
    }
    let object = item.as_object()?;
    let name = typed_json_name(object.get("name")?)?;
    if name.is_empty() {
        return None;
    }
    let description = object
        .get("description")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_string();
    let kind = object
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("arg")
        .to_string();
    let insert = object
        .get("insertValue")
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned);
    let display = object
        .get("displayName")
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned);
    let icon = object.get("icon").and_then(JsonValue::as_str).map(ToOwned::to_owned);
    let hidden = object.get("hidden").and_then(JsonValue::as_bool).unwrap_or(false);
    let dangerous = object.get("isDangerous").and_then(JsonValue::as_bool).unwrap_or(false);
    let priority = object.get("priority").and_then(JsonValue::as_i64);
    let mut suggestion = Suggestion::new(name.clone(), description, kind)
        .with_dangerous(dangerous)
        .with_meta(
            insert,
            display,
            None,
            object
                .get("shouldAddSpace")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
            hidden,
            priority,
            icon,
        );
    if suggestion.insert_value.is_none() {
        suggestion.insert_value = Some(name);
    }
    Some(suggestion)
}

fn typed_json_name(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(name) if !name.is_empty() => Some(name.clone()),
        JsonValue::Array(names) => names.iter().find_map(|item| item.as_str().map(ToOwned::to_owned)),
        _ => None,
    }
}

fn script_command_from_typed_json(value: &JsonValue) -> TypedHookResult<ScriptCommand> {
    if let Some(command) = value.as_str() {
        if command.trim().is_empty() {
            return Err(TypedHookError::new("typed script result is an empty command"));
        }
        return Ok(ScriptCommand {
            command: "sh".into(),
            args: vec!["-c".into(), command.to_string()],
            timeout_ms: None,
        });
    }
    if let Some(parts) = value.as_array() {
        let strings: Vec<String> = parts
            .iter()
            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
            .collect();
        let (command, args) = strings
            .split_first()
            .ok_or_else(|| TypedHookError::new("typed script result is an empty argv"))?;
        if command.is_empty() {
            return Err(TypedHookError::new("typed script result has an empty command"));
        }
        return Ok(ScriptCommand {
            command: command.clone(),
            args: args.to_vec(),
            timeout_ms: None,
        });
    }
    let object = value
        .as_object()
        .ok_or_else(|| TypedHookError::new("typed script result is not a command"))?;
    let command = object
        .get("command")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| TypedHookError::new("typed script result is missing command"))?
        .to_string();
    if command.is_empty() {
        return Err(TypedHookError::new("typed script result has an empty command"));
    }
    let args = object
        .get("args")
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Ok(ScriptCommand {
        command,
        args,
        timeout_ms: object.get("timeout").and_then(JsonValue::as_i64),
    })
}

fn evaluate_expr(expression: &TypedExpr, arguments: &[TypedValue]) -> TypedHookResult<TypedValue> {
    eval::evaluate(expression, arguments)
}

fn evaluate_expr_with_effects(
    expression: &TypedExpr,
    arguments: &[TypedValue],
    effects: &TypedHookEffects<'_>,
) -> TypedHookResult<TypedValue> {
    eval::evaluate_with_effects(expression, arguments, Some(effects))
}

#[allow(dead_code)]
fn safe_length(length: usize) -> TypedHookResult<TypedValue> {
    let value = i64::try_from(length).map_err(|error| TypedHookError::new(format!("length is too large: {error}")))?;
    ensure_safe_integer(value, "length")?;
    Ok(TypedValue::Integer(value))
}

#[allow(dead_code)]
fn evaluate_string(expression: &TypedExpr, arguments: &[TypedValue], label: &str) -> TypedHookResult<Utf16String> {
    match evaluate_expr(expression, arguments)? {
        TypedValue::String(value) => Ok(value),
        _ => Err(TypedHookError::new(format!("{label} is not a string"))),
    }
}

#[allow(dead_code)]
fn evaluate_array(expression: &TypedExpr, arguments: &[TypedValue], label: &str) -> TypedHookResult<Vec<Utf16String>> {
    match evaluate_expr(expression, arguments)? {
        TypedValue::StringArray(value) => Ok(value),
        _ => Err(TypedHookError::new(format!("{label} is not a string array"))),
    }
}

#[allow(dead_code)]
fn evaluate_integer(expression: &TypedExpr, arguments: &[TypedValue], label: &str) -> TypedHookResult<i64> {
    match evaluate_expr(expression, arguments)? {
        TypedValue::Integer(value) => Ok(value),
        _ => Err(TypedHookError::new(format!("{label} is not an integer"))),
    }
}

#[allow(dead_code)]
fn evaluate_bool(expression: &TypedExpr, arguments: &[TypedValue], label: &str) -> TypedHookResult<bool> {
    match evaluate_expr(expression, arguments)? {
        TypedValue::Bool(value) => Ok(value),
        _ => Err(TypedHookError::new(format!("{label} is not a bool"))),
    }
}

#[allow(dead_code)]
fn strict_equal(left: &TypedValue, right: &TypedValue) -> TypedHookResult<bool> {
    match (left, right) {
        (TypedValue::String(left), TypedValue::String(right)) => Ok(left == right),
        (TypedValue::Bool(left), TypedValue::Bool(right)) => Ok(left == right),
        (TypedValue::Integer(left), TypedValue::Integer(right)) => Ok(left == right),
        (TypedValue::Null, TypedValue::Null) => Ok(true),
        (TypedValue::StringArray(_), TypedValue::StringArray(_)) => {
            Err(TypedHookError::new("string-array strict equality is not representable"))
        },
        _ => Err(TypedHookError::new("strict equality operands have different types")),
    }
}

fn safe_arithmetic(value: Option<i64>, path: &str) -> TypedHookResult<i64> {
    let value =
        value.ok_or_else(|| TypedHookError::new(format!("{path} overflowed the JavaScript safe integer range")))?;
    ensure_safe_integer(value, path)?;
    Ok(value)
}

fn utf16_index_of_i64(value: &Utf16String, needle: &Utf16String) -> TypedHookResult<i64> {
    utf16_index_of(value, needle)
        .map(|index| {
            i64::try_from(index).map_err(|error| TypedHookError::new(format!("string index is too large: {error}")))
        })
        .transpose()
        .map(|index| index.unwrap_or(-1))
}

fn utf16_last_index_of(value: &Utf16String, needle: &Utf16String) -> Option<usize> {
    if needle.0.is_empty() {
        return Some(value.len());
    }
    if needle.0.len() > value.0.len() {
        return None;
    }
    value
        .0
        .windows(needle.0.len())
        .rposition(|window| window == needle.0.as_slice())
}

fn utf16_last_index_of_i64(value: &Utf16String, needle: &Utf16String) -> TypedHookResult<i64> {
    utf16_last_index_of(value, needle)
        .map(|index| {
            i64::try_from(index).map_err(|error| TypedHookError::new(format!("string index is too large: {error}")))
        })
        .transpose()
        .map(|index| index.unwrap_or(-1))
}

fn utf16_starts_with(value: &Utf16String, needle: &Utf16String) -> bool {
    value.0.starts_with(&needle.0)
}

fn utf16_ends_with(value: &Utf16String, needle: &Utf16String) -> bool {
    value.0.ends_with(&needle.0)
}

fn is_js_trim_unit(unit: u16) -> bool {
    matches!(
        unit,
        0x0009
            | 0x000A
            | 0x000B
            | 0x000C
            | 0x000D
            | 0x0020
            | 0x00A0
            | 0x1680
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
            | 0xFEFF
    ) || (0x2000..=0x200A).contains(&unit)
}

fn js_trim(value: &Utf16String, start: bool, end: bool) -> Utf16String {
    let mut units = value.0.as_slice();
    if start {
        while units.first().is_some_and(|unit| is_js_trim_unit(*unit)) {
            units = &units[1..];
        }
    }
    if end {
        while units.last().is_some_and(|unit| is_js_trim_unit(*unit)) {
            units = &units[..units.len() - 1];
        }
    }
    Utf16String::from_units(units.to_vec())
}

fn map_scalar_case(scalar: char, upper: bool) -> String {
    if upper {
        scalar.to_uppercase().collect()
    } else {
        scalar.to_lowercase().collect()
    }
}

fn js_map_case(value: &Utf16String, upper: bool) -> Utf16String {
    let mut out = Vec::with_capacity(value.0.len());
    let mut index = 0;
    while index < value.0.len() {
        let unit = value.0[index];
        if (0xD800..=0xDBFF).contains(&unit)
            && let Some(next) = value.0.get(index + 1)
            && (0xDC00..=0xDFFF).contains(next)
        {
            let scalar = char::decode_utf16([unit, *next])
                .next()
                .and_then(Result::ok)
                .expect("paired surrogates decode");
            out.extend(map_scalar_case(scalar, upper).encode_utf16());
            index += 2;
            continue;
        }
        if (0xD800..=0xDFFF).contains(&unit) {
            out.push(unit);
            index += 1;
            continue;
        }
        let scalar = char::from_u32(u32::from(unit)).expect("BMP code unit is a scalar");
        out.extend(map_scalar_case(scalar, upper).encode_utf16());
        index += 1;
    }
    Utf16String::from_units(out)
}

fn js_substring(value: &Utf16String, start: i64, end: i64) -> TypedHookResult<Utf16String> {
    let length =
        i64::try_from(value.len()).map_err(|error| TypedHookError::new(format!("string is too long: {error}")))?;
    let clamp = |index: i64| index.clamp(0, length);
    let mut from = clamp(start);
    let mut to = clamp(end);
    if from > to {
        std::mem::swap(&mut from, &mut to);
    }
    Ok(Utf16String::from_units(value.0[from as usize..to as usize].to_vec()))
}

fn js_replace(
    value: &Utf16String,
    needle: &Utf16String,
    replacement: &Utf16String,
    all: bool,
) -> TypedHookResult<Utf16String> {
    if needle.0.is_empty() {
        if !all {
            let mut out = replacement.0.clone();
            out.extend_from_slice(&value.0);
            return ensure_string_limit(out);
        }
        let mut out = replacement.0.clone();
        for unit in &value.0 {
            out.push(*unit);
            out.extend_from_slice(&replacement.0);
        }
        return ensure_string_limit(out);
    }
    if !all {
        let Some(index) = utf16_index_of(value, needle) else {
            return Ok(value.clone());
        };
        let mut out = value.0[..index].to_vec();
        out.extend_from_slice(&replacement.0);
        out.extend_from_slice(&value.0[index + needle.0.len()..]);
        return ensure_string_limit(out);
    }
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(relative) = value.0[start..]
        .windows(needle.0.len())
        .position(|window| window == needle.0.as_slice())
    {
        let end = start + relative;
        out.extend_from_slice(&value.0[start..end]);
        out.extend_from_slice(&replacement.0);
        start = end + needle.0.len();
    }
    out.extend_from_slice(&value.0[start..]);
    ensure_string_limit(out)
}

fn js_pad(value: &Utf16String, target: i64, pad: &Utf16String, end: bool) -> TypedHookResult<Utf16String> {
    let target = if target < 0 { 0 } else { target };
    let length =
        i64::try_from(value.len()).map_err(|error| TypedHookError::new(format!("string is too long: {error}")))?;
    if target <= length || pad.0.is_empty() {
        return Ok(value.clone());
    }
    if target > MAX_STRING_LITERAL_UNITS as i64 {
        return Err(TypedHookError::new("string-pad exceeds the UTF-16 code-unit limit"));
    }
    let needed = usize::try_from(target - length)
        .map_err(|error| TypedHookError::new(format!("pad target is too large: {error}")))?;
    let mut fill = Vec::with_capacity(needed);
    while fill.len() < needed {
        fill.extend_from_slice(&pad.0);
    }
    fill.truncate(needed);
    let out = if end {
        let mut out = value.0.clone();
        out.extend_from_slice(&fill);
        out
    } else {
        fill.extend_from_slice(&value.0);
        fill
    };
    ensure_string_limit(out)
}

fn js_repeat(value: &Utf16String, count: i64) -> TypedHookResult<Utf16String> {
    if count < 0 {
        return Err(TypedHookError::new("string-repeat count must be non-negative"));
    }
    let count = usize::try_from(count).map_err(|error| TypedHookError::new(format!("string-repeat count: {error}")))?;
    let units = value
        .len()
        .checked_mul(count)
        .ok_or_else(|| TypedHookError::new("string-repeat overflowed the JavaScript safe integer range"))?;
    if units > MAX_STRING_LITERAL_UNITS {
        return Err(TypedHookError::new("string-repeat exceeds the UTF-16 code-unit limit"));
    }
    let mut out = Vec::with_capacity(units);
    for _ in 0..count {
        out.extend_from_slice(&value.0);
    }
    Ok(Utf16String::from_units(out))
}

fn js_char_at(value: &Utf16String, index: i64) -> Utf16String {
    if index < 0 {
        return Utf16String::from_units(Vec::<u16>::new());
    }
    let Ok(index) = usize::try_from(index) else {
        return Utf16String::from_units(Vec::<u16>::new());
    };
    match value.0.get(index) {
        Some(unit) => Utf16String::from_units(vec![*unit]),
        None => Utf16String::from_units(Vec::<u16>::new()),
    }
}

fn js_at(value: &Utf16String, index: i64) -> TypedHookResult<Utf16String> {
    let length =
        i64::try_from(value.len()).map_err(|error| TypedHookError::new(format!("string is too long: {error}")))?;
    let actual = if index < 0 { length + index } else { index };
    Ok(js_char_at(value, actual))
}

fn ensure_string_limit(units: Vec<u16>) -> TypedHookResult<Utf16String> {
    if units.len() > MAX_STRING_LITERAL_UNITS {
        return Err(TypedHookError::new(format!(
            "string exceeds {MAX_STRING_LITERAL_UNITS} UTF-16 code units"
        )));
    }
    Ok(Utf16String::from_units(units))
}

fn json_to_typed_value(value: &JsonValue, expected: TypedValueType) -> TypedHookResult<TypedValue> {
    match expected {
        TypedValueType::String => {
            let Some(text) = value.as_str() else {
                return Err(TypedHookError::new("argument is not a string"));
            };
            Ok(TypedValue::String(Utf16String::from_str(text)))
        },
        TypedValueType::Bool => {
            let Some(flag) = value.as_bool() else {
                return Err(TypedHookError::new("argument is not a bool"));
            };
            Ok(TypedValue::Bool(flag))
        },
        TypedValueType::Integer => {
            let Some(number) = value.as_i64() else {
                return Err(TypedHookError::new("argument is not an integer"));
            };
            ensure_safe_integer(number, "argument")?;
            Ok(TypedValue::Integer(number))
        },
        TypedValueType::StringArray => {
            let Some(items) = value.as_array() else {
                return Err(TypedHookError::new("argument is not a string array"));
            };
            let values = items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(Utf16String::from_str)
                        .ok_or_else(|| TypedHookError::new("string-array item is not a string"))
                })
                .collect::<TypedHookResult<Vec<_>>>()?;
            Ok(TypedValue::StringArray(values))
        },
        TypedValueType::Null => {
            if value.is_null() {
                Ok(TypedValue::Null)
            } else {
                Err(TypedHookError::new("argument is not null"))
            }
        },
        TypedValueType::Json
        | TypedValueType::JsonArray
        | TypedValueType::Suggestion
        | TypedValueType::SuggestionArray
        | TypedValueType::StringRecord
        | TypedValueType::ValueArray => Ok(json_value_to_typed(value)),
        TypedValueType::StringSet => {
            let items = value
                .as_array()
                .ok_or_else(|| TypedHookError::new("argument is not a string set"))?;
            let mut set = Vec::new();
            for item in items {
                let text = item
                    .as_str()
                    .ok_or_else(|| TypedHookError::new("string-set item is not a string"))?;
                let text = Utf16String::from_str(text);
                if !set.iter().any(|existing| existing == &text) {
                    set.push(text);
                }
            }
            Ok(TypedValue::StringSet(set))
        },
        TypedValueType::Regex | TypedValueType::Exec | TypedValueType::Context | TypedValueType::ExecResult => {
            Err(TypedHookError::new("effect arguments are compile-only"))
        },
        TypedValueType::Spec => Ok(json_value_to_typed(value)),
    }
}

fn json_value_to_typed(value: &JsonValue) -> TypedValue {
    match value {
        JsonValue::Null => TypedValue::Null,
        JsonValue::Bool(flag) => TypedValue::Bool(*flag),
        JsonValue::Number(number) => number
            .as_i64()
            .map(TypedValue::Integer)
            .unwrap_or_else(|| TypedValue::Json(value.clone())),
        JsonValue::String(text) => TypedValue::String(Utf16String::from_str(text)),
        JsonValue::Array(items) => TypedValue::Array(items.iter().map(json_value_to_typed).collect()),
        JsonValue::Object(fields) => TypedValue::Object(
            fields
                .iter()
                .map(|(key, child)| (key.clone(), json_value_to_typed(child)))
                .collect(),
        ),
    }
}

fn typed_value_to_json(value: &TypedValue) -> TypedHookResult<JsonValue> {
    match value {
        TypedValue::String(value) => {
            let text = String::from_utf16(&value.0)
                .map_err(|error| TypedHookError::new(format!("evaluated string is not valid UTF-16: {error}")))?;
            Ok(JsonValue::String(text))
        },
        TypedValue::Bool(value) => Ok(JsonValue::Bool(*value)),
        TypedValue::Integer(value) => Ok(JsonValue::from(*value)),
        TypedValue::StringArray(items) => {
            let values = items
                .iter()
                .map(|item| {
                    String::from_utf16(&item.0)
                        .map(JsonValue::String)
                        .map_err(|error| TypedHookError::new(format!("evaluated string is not valid UTF-16: {error}")))
                })
                .collect::<TypedHookResult<Vec<_>>>()?;
            Ok(JsonValue::Array(values))
        },
        TypedValue::Null => Ok(JsonValue::Null),
        TypedValue::Array(items) => Ok(JsonValue::Array(
            items
                .iter()
                .map(typed_value_to_json)
                .collect::<TypedHookResult<Vec<_>>>()?,
        )),
        TypedValue::Object(fields) => {
            let mut object = serde_json::Map::new();
            for (key, child) in fields {
                object.insert(key.clone(), typed_value_to_json(child)?);
            }
            Ok(JsonValue::Object(object))
        },
        TypedValue::StringSet(items) => {
            let values = items
                .iter()
                .map(|item| {
                    String::from_utf16(&item.0)
                        .map(JsonValue::String)
                        .map_err(|error| TypedHookError::new(format!("evaluated string is not valid UTF-16: {error}")))
                })
                .collect::<TypedHookResult<Vec<_>>>()?;
            Ok(JsonValue::Array(values))
        },
        TypedValue::Regex { pattern, flags } => {
            let mut object = serde_json::Map::new();
            object.insert("pattern".to_string(), JsonValue::String(pattern.clone()));
            object.insert("flags".to_string(), JsonValue::String(flags.clone()));
            Ok(JsonValue::Object(object))
        },
        TypedValue::Json(value) => Ok(value.clone()),
    }
}

/// Evaluate a validated descriptor against JSON arguments that match its
/// field contract.  Used by the T2.1 cross-language golden.
pub(crate) fn evaluate_typed_hook_json(descriptor: &TypedHookIr, args: &[JsonValue]) -> TypedHookResult<JsonValue> {
    if args.len() != descriptor.params.len() {
        return Err(TypedHookError::new("evaluate args must match the field contract"));
    }
    let arguments = descriptor
        .params
        .iter()
        .zip(args)
        .map(|(param, value)| json_to_typed_value(value, param.value_type))
        .collect::<TypedHookResult<Vec<_>>>()?;
    let value = typed_value_to_json(&evaluate_expr(&descriptor.expr, &arguments)?)?;
    Ok(match descriptor.result_type {
        TypedValueType::SuggestionArray => normalize_suggestion_array(value),
        TypedValueType::Suggestion => normalize_suggestion_value(value),
        _ => value,
    })
}

fn normalize_suggestion_array(value: JsonValue) -> JsonValue {
    match value {
        JsonValue::Array(items) => JsonValue::Array(
            items
                .into_iter()
                .map(normalize_suggestion_value)
                .filter(|item| match item.get("name") {
                    Some(JsonValue::String(name)) => name != "undefined",
                    Some(JsonValue::Array(names)) => !names.is_empty(),
                    Some(_) => true,
                    None => false,
                })
                .collect(),
        ),
        other => other,
    }
}

fn normalize_suggestion_value(value: JsonValue) -> JsonValue {
    let JsonValue::Object(fields) = value else {
        return value;
    };
    let mut out = serde_json::Map::new();
    for (key, child) in fields {
        if child.is_null() {
            continue;
        }
        if key == "priority" && !child.is_number() {
            continue;
        }
        if child.as_array().is_some_and(Vec::is_empty) {
            continue;
        }
        if key != "name" && child.as_str().is_some_and(str::is_empty) {
            continue;
        }
        out.insert(key, child);
    }
    JsonValue::Object(out)
}

fn utf16_index_of(value: &Utf16String, needle: &Utf16String) -> Option<usize> {
    if needle.0.is_empty() {
        return Some(0);
    }
    if needle.0.len() > value.0.len() {
        return None;
    }
    value
        .0
        .windows(needle.0.len())
        .position(|window| window == needle.0.as_slice())
}

fn js_slice(value: &Utf16String, start: i64) -> TypedHookResult<Utf16String> {
    let length =
        i64::try_from(value.len()).map_err(|error| TypedHookError::new(format!("string is too long: {error}")))?;
    let index = if start < 0 {
        (length + start).max(0)
    } else {
        start.min(length)
    } as usize;
    Ok(Utf16String::from_units(value.0[index..].to_vec()))
}

/// JavaScript `String.prototype.slice(start, end)` — negative indexes count
/// from the end, and a reversed range is empty rather than swapped.
fn js_slice_range(value: &Utf16String, start: i64, end: i64) -> TypedHookResult<Utf16String> {
    let length =
        i64::try_from(value.len()).map_err(|error| TypedHookError::new(format!("string is too long: {error}")))?;
    let from = if start < 0 {
        (length + start).max(0)
    } else {
        start.min(length)
    };
    let to = if end < 0 {
        (length + end).max(0)
    } else {
        end.min(length)
    };
    if to <= from {
        return Ok(Utf16String::from_units(Vec::new()));
    }
    Ok(Utf16String::from_units(value.0[from as usize..to as usize].to_vec()))
}

fn js_split(value: &Utf16String, separator: &Utf16String) -> Vec<Utf16String> {
    if separator.0.is_empty() {
        return value
            .0
            .iter()
            .map(|unit| Utf16String::from_units(vec![*unit]))
            .collect();
    }

    let mut result = Vec::new();
    let mut start = 0;
    while let Some(relative) = value.0[start..]
        .windows(separator.0.len())
        .position(|window| window == separator.0.as_slice())
    {
        let end = start + relative;
        result.push(Utf16String::from_units(value.0[start..end].to_vec()));
        start = end + separator.0.len();
    }
    result.push(Utf16String::from_units(value.0[start..].to_vec()));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::fs::{self, Metadata, OpenOptions};
    use std::io::Read;
    use std::path::{Component, Path, PathBuf};

    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    const TYPED_TRIGGER_REFERENCE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/typed-hooks/reference.json"
    ));
    const TYPED_GET_QUERY_TERM_REFERENCE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/typed-hooks/asdf-get-query-term-reference.json"
    ));

    const ASDF_REFERENCE_HARNESS_FILES: &[&str] = &[
        "scripts/audit-spec-hooks.mjs",
        "scripts/capture-hook-reference.mjs",
        "scripts/capture-typed-reference.mjs",
        "scripts/capture-typed-trigger-reference.mjs",
        "scripts/filepaths-helper.mjs",
        "scripts/reference-audit-worker.mjs",
        "scripts/reference-hook-worker.mjs",
        "scripts/reference-safe-io.mjs",
        "scripts/spec-hook-contract.mjs",
        "scripts/spec-pair.mjs",
        "scripts/spec-versions.mjs",
        "scripts/typed-hook-inline.mjs",
        "scripts/typed-hook-ir.mjs",
        "scripts/typed-regex.mjs",
    ];

    fn arg(index: u64) -> TypedExpr {
        TypedExpr::Arg { index }
    }

    fn string(value: &str) -> TypedExpr {
        TypedExpr::String {
            value: value.to_owned(),
        }
    }

    fn integer(value: i64) -> TypedExpr {
        TypedExpr::Integer { value }
    }

    fn bool_value(value: bool) -> TypedExpr {
        TypedExpr::Bool { value }
    }

    fn length(value: TypedExpr) -> TypedExpr {
        TypedExpr::Length { value: Box::new(value) }
    }

    fn strict_eq(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        TypedExpr::StrictEq {
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn strict_ne(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        TypedExpr::StrictNe {
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn gt(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        TypedExpr::GreaterThan {
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn and(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        TypedExpr::And {
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn or(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        TypedExpr::Or {
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn trigger_descriptor(expression: TypedExpr) -> JsonValue {
        json!({
            "version": IR_VERSION,
            "kind": IR_KIND,
            "sourceField": SOURCE_FIELD,
            "resultType": "bool",
            "params": [
                {"index": 0, "type": "string"},
                {"index": 1, "type": "string"},
            ],
            "expr": serde_json::to_value(expression).expect("expression JSON"),
        })
    }

    fn parse_expression(expression: TypedExpr) -> TypedHookIr {
        parse_typed_hook_ir(&trigger_descriptor(expression)).expect("valid trigger descriptor")
    }

    fn get_query_term_descriptor() -> JsonValue {
        json!({
            "version": IR_VERSION,
            "kind": IR_KIND,
            "sourceField": GET_QUERY_TERM_SOURCE_FIELD,
            "resultType": "string",
            "params": [{"index": 0, "type": "string"}],
            "expr": {
                "op": "if",
                "condition": {
                    "op": "string-includes",
                    "value": {"op": "arg", "index": 0},
                    "needle": {"op": "string", "value": "latest"},
                },
                "then": {
                    "op": "string-slice-after-first",
                    "value": {"op": "arg", "index": 0},
                    "needle": {"op": "string", "value": ":"},
                },
                "else": {"op": "arg", "index": 0},
            },
        })
    }

    fn parse_get_query_term_descriptor() -> TypedHookIr {
        parse_typed_hook_ir(&get_query_term_descriptor()).expect("valid getQueryTerm descriptor")
    }

    fn evaluate(expression: TypedExpr, search: &str, previous: &str) -> bool {
        let descriptor = parse_expression(expression);
        evaluate_typed_trigger(&descriptor, search, previous).expect("evaluate trigger")
    }

    #[test]
    fn closed_get_query_term_preserves_js_utf16_and_first_index_of() {
        let descriptor = parse_get_query_term_descriptor();
        assert_eq!(
            evaluate_typed_get_query_term(&descriptor, "prefix:latest:tail").expect("query term"),
            "latest:tail"
        );
        assert_eq!(
            evaluate_typed_get_query_term(&descriptor, "latest").expect("missing colon"),
            "latest"
        );
        assert_eq!(
            evaluate_typed_get_query_term(&descriptor, "a:latest:b:latest").expect("first colon"),
            "latest:b:latest"
        );
        assert_eq!(
            evaluate_typed_get_query_term(&descriptor, "😀:latest").expect("UTF-16 query term"),
            "latest"
        );
        assert_eq!(
            evaluate_typed_get_query_term(&descriptor, "😀").expect("false branch"),
            "😀"
        );
    }

    #[test]
    fn parses_and_evaluates_all_nine_trigger_shapes() {
        let cases = vec![
            (
                or(
                    strict_eq(length(arg(0)), integer(0)),
                    and(strict_eq(length(arg(1)), integer(0)), gt(length(arg(0)), integer(0))),
                ),
                vec![("", "x", true), ("x", "", true), ("x", "y", false)],
            ),
            (
                strict_ne(
                    length(TypedExpr::StringSplit {
                        value: Box::new(arg(0)),
                        separator: Box::new(string(",")),
                    }),
                    length(TypedExpr::StringSplit {
                        value: Box::new(arg(1)),
                        separator: Box::new(string(",")),
                    }),
                ),
                vec![("a,b", "a", true), ("a,b", "a,c", false)],
            ),
            (bool_value(true), vec![("", "", true), ("x", "y", true)]),
            (
                strict_ne(
                    TypedExpr::StringIndexOf {
                        value: Box::new(arg(0)),
                        needle: Box::new(string("/")),
                    },
                    TypedExpr::StringIndexOf {
                        value: Box::new(arg(1)),
                        needle: Box::new(string("/")),
                    },
                ),
                vec![("a/b", "ab", true), ("a/b", "c/d", false), ("é/", "x/", false)],
            ),
            (
                or(strict_eq(arg(0), string("-g")), strict_eq(arg(0), string("--global"))),
                vec![("-g", "", true), ("--global", "", true), ("-x", "", false)],
            ),
            (
                strict_ne(
                    length(TypedExpr::StringSplit {
                        value: Box::new(arg(0)),
                        separator: Box::new(string(":")),
                    }),
                    length(TypedExpr::StringSplit {
                        value: Box::new(arg(1)),
                        separator: Box::new(string(":")),
                    }),
                ),
                vec![("a:b", "a", true), ("a:b", "a:c", false)],
            ),
            (
                or(strict_eq(arg(0), string("-g")), strict_eq(arg(0), string("--global"))),
                vec![("--global", "", true), ("--local", "", false)],
            ),
            (bool_value(true), vec![("", "", true)]),
            (
                and(strict_eq(length(arg(0)), integer(0)), gt(length(arg(1)), integer(0))),
                vec![("", "x", true), ("x", "x", false), ("", "", false)],
            ),
        ];

        for (expression, inputs) in cases {
            for (search, previous, expected) in inputs {
                assert_eq!(evaluate(expression.clone(), search, previous), expected);
            }
        }
    }

    #[test]
    fn every_operation_and_type_has_runtime_semantics() {
        let includes = TypedExpr::StringIncludes {
            value: Box::new(arg(0)),
            needle: Box::new(string("bc")),
        };
        assert!(evaluate(includes, "abcd", ""));

        let index_of = strict_eq(
            TypedExpr::StringIndexOf {
                value: Box::new(arg(0)),
                needle: Box::new(string("bc")),
            },
            integer(1),
        );
        assert!(evaluate(index_of, "abcd", ""));

        let slice = strict_eq(
            TypedExpr::StringSlice {
                value: Box::new(arg(0)),
                start: Box::new(integer(2)),
            },
            string("cd"),
        );
        assert!(evaluate(slice, "abcd", ""));

        let split = strict_eq(
            length(TypedExpr::StringSplit {
                value: Box::new(arg(0)),
                separator: Box::new(string(",")),
            }),
            integer(3),
        );
        assert!(evaluate(split, "a,b,", ""));

        let array_includes = TypedExpr::ArrayIncludes {
            value: Box::new(TypedExpr::Array {
                items: vec![string("bash"), string("-c")],
            }),
            needle: Box::new(string("-c")),
        };
        assert!(evaluate(array_includes, "", ""));

        assert!(evaluate(strict_eq(string("x"), string("x")), "", ""));
        assert!(evaluate(strict_ne(string("x"), string("y")), "", ""));

        let lazy = TypedExpr::If {
            condition: Box::new(bool_value(false)),
            then_branch: Box::new(TypedExpr::StringSlice {
                value: Box::new(string("unreachable")),
                start: Box::new(integer(MAX_SAFE_INTEGER)),
            }),
            else_branch: Box::new(string("lazy")),
        };
        let value = evaluate_expr(
            &parse_expression(strict_eq(bool_value(true), bool_value(true))).expr,
            &[
                TypedValue::String(Utf16String::from_str("")),
                TypedValue::String(Utf16String::from_str("")),
            ],
        )
        .expect("baseline expression");
        assert_eq!(value, TypedValue::Bool(true));
        assert_eq!(
            evaluate_expr(
                &lazy,
                &[
                    TypedValue::String(Utf16String::from_str("")),
                    TypedValue::String(Utf16String::from_str("")),
                ],
            )
            .expect("lazy branch"),
            TypedValue::String(Utf16String::from_str("lazy"))
        );

        let last_index = strict_eq(
            TypedExpr::StringLastIndexOf {
                value: Box::new(arg(0)),
                needle: Box::new(string(":")),
            },
            integer(3),
        );
        assert!(evaluate(last_index, "a:b:c", ""));

        let starts = TypedExpr::StringStartsWith {
            value: Box::new(arg(0)),
            needle: Box::new(string("ab")),
        };
        assert!(evaluate(starts, "abcd", ""));

        let ends = TypedExpr::StringEndsWith {
            value: Box::new(arg(0)),
            needle: Box::new(string("cd")),
        };
        assert!(evaluate(ends, "abcd", ""));

        let trimmed = strict_eq(
            TypedExpr::StringTrim {
                value: Box::new(arg(0)),
            },
            string("hi"),
        );
        assert!(evaluate(trimmed, "  hi  ", ""));

        let replaced = strict_eq(
            TypedExpr::StringReplaceAll {
                value: Box::new(arg(0)),
                needle: Box::new(string("-")),
                replacement: Box::new(string("_")),
            },
            string("a_b_c"),
        );
        assert!(evaluate(replaced, "a-b-c", ""));

        let lower = strict_eq(
            TypedExpr::StringToLower {
                value: Box::new(arg(0)),
            },
            string("abc"),
        );
        assert!(evaluate(lower, "AbC", ""));

        let padded = strict_eq(
            TypedExpr::StringPadStart {
                value: Box::new(arg(0)),
                target: Box::new(integer(4)),
                pad: Box::new(string("0")),
            },
            string("0012"),
        );
        assert!(evaluate(padded, "12", ""));

        let repeated = strict_eq(
            TypedExpr::StringRepeat {
                value: Box::new(arg(0)),
                count: Box::new(integer(3)),
            },
            string("ababab"),
        );
        assert!(evaluate(repeated, "ab", ""));

        let concat = strict_eq(
            TypedExpr::StringConcat {
                parts: vec![arg(0), string("!")],
            },
            string("hi!"),
        );
        assert!(evaluate(concat, "hi", ""));

        let char_at = strict_eq(
            TypedExpr::StringCharAt {
                value: Box::new(arg(0)),
                index: Box::new(integer(1)),
            },
            string("b"),
        );
        assert!(evaluate(char_at, "abc", ""));

        let at = strict_eq(
            TypedExpr::StringAt {
                value: Box::new(arg(0)),
                index: Box::new(integer(-1)),
            },
            string("c"),
        );
        assert!(evaluate(at, "abc", ""));

        let substring = strict_eq(
            TypedExpr::StringSubstring {
                value: Box::new(arg(0)),
                start: Box::new(integer(1)),
                end: Box::new(integer(3)),
            },
            string("bc"),
        );
        assert!(evaluate(substring, "abcd", ""));

        assert!(evaluate(
            strict_eq(
                TypedExpr::Sub {
                    left: Box::new(length(arg(0))),
                    right: Box::new(integer(1)),
                },
                integer(0),
            ),
            "x",
            "",
        ));
        assert!(evaluate(
            TypedExpr::LessThan {
                left: Box::new(length(arg(0))),
                right: Box::new(length(arg(1))),
            },
            "a",
            "bb",
        ));
        assert!(evaluate(
            TypedExpr::Not {
                value: Box::new(TypedExpr::StringStartsWith {
                    value: Box::new(arg(0)),
                    needle: Box::new(string("x")),
                }),
            },
            "hello",
            "",
        ));

        let nullish = evaluate_expr(
            &TypedExpr::Nullish {
                left: Box::new(TypedExpr::Null),
                right: Box::new(string("fallback")),
            },
            &[
                TypedValue::String(Utf16String::from_str("")),
                TypedValue::String(Utf16String::from_str("")),
            ],
        )
        .expect("nullish");
        assert_eq!(nullish, TypedValue::String(Utf16String::from_str("fallback")));

        let overflow = evaluate_expr(
            &TypedExpr::Add {
                left: Box::new(integer(MAX_SAFE_INTEGER)),
                right: Box::new(integer(1)),
            },
            &[],
        );
        assert!(overflow.is_err());

        let huge_pad = evaluate_expr(
            &TypedExpr::StringPadStart {
                value: Box::new(string("x")),
                target: Box::new(integer((MAX_STRING_LITERAL_UNITS as i64) + 1)),
                pad: Box::new(string("0")),
            },
            &[],
        );
        assert!(huge_pad.is_err());

        let empty_last = evaluate_expr(
            &TypedExpr::StringLastIndexOf {
                value: Box::new(string("ab")),
                needle: Box::new(string("")),
            },
            &[],
        )
        .expect("empty lastIndexOf");
        assert_eq!(empty_last, TypedValue::Integer(2));
    }

    #[test]
    fn malformed_schema_and_types_fail_closed() {
        let valid = trigger_descriptor(bool_value(true));
        let mut unknown_descriptor = valid.clone();
        unknown_descriptor["unexpected"] = json!(true);
        assert!(parse_typed_hook_ir(&unknown_descriptor).is_err());

        let mut unknown_operation = valid.clone();
        unknown_operation["expr"] = json!({"op": "future", "value": true});
        assert!(parse_typed_hook_ir(&unknown_operation).is_err());

        let mut generic_add = valid.clone();
        generic_add["expr"] = json!({
            "op": "add",
            "left": {"op": "integer", "value": 1},
            "right": {"op": "integer", "value": 1}
        });
        assert!(parse_typed_hook_ir(&generic_add).is_err());

        let mut extra_operation_field = valid.clone();
        extra_operation_field["expr"] = json!({"op": "bool", "value": true, "extra": 1});
        assert!(parse_typed_hook_ir(&extra_operation_field).is_err());

        let mut wrong_field = valid.clone();
        wrong_field["sourceField"] = json!("postProcess");
        assert!(parse_typed_hook_ir(&wrong_field).is_err());

        let mut wrong_params = valid.clone();
        wrong_params["params"] = json!([{"index": 0, "type": "bool"}, {"index": 1, "type": "string"}]);
        assert!(parse_typed_hook_ir(&wrong_params).is_err());

        let mut wrong_count = valid.clone();
        wrong_count["params"] = json!([{"index": 0, "type": "string"}]);
        assert!(parse_typed_hook_ir(&wrong_count).is_err());

        let mut out_of_range_arg = valid.clone();
        out_of_range_arg["expr"] = json!({"op": "arg", "index": 2});
        assert!(parse_typed_hook_ir(&out_of_range_arg).is_err());

        let mut wrong_type = valid.clone();
        wrong_type["expr"] = json!({
            "op": "string-includes",
            "value": {"op": "bool", "value": true},
            "needle": {"op": "string", "value": "x"}
        });
        assert!(parse_typed_hook_ir(&wrong_type).is_err());

        let mut unsafe_integer = valid.clone();
        unsafe_integer["expr"] = json!({"op": "integer", "value": 9_007_199_254_740_992_i64});
        assert!(parse_typed_hook_ir(&unsafe_integer).is_err());

        let mut array_identity = valid.clone();
        array_identity["expr"] = json!({
            "op": "strict-eq",
            "left": {"op": "array", "items": []},
            "right": {"op": "array", "items": []}
        });
        assert!(parse_typed_hook_ir(&array_identity).is_err());

        let mut query = get_query_term_descriptor();
        query["expr"]["then"]["needle"] = json!("not-an-expression");
        assert!(parse_typed_hook_ir(&query).is_err());
        let mut query_with_generic_add = get_query_term_descriptor();
        query_with_generic_add["expr"]["then"] = json!({
            "op": "string-slice",
            "value": {"op": "arg", "index": 0},
            "start": {
                "op": "add",
                "left": {"op": "string-index-of", "value": {"op": "arg", "index": 0}, "needle": {"op": "string", "value": ":"}},
                "right": {"op": "integer", "value": 1}
            }
        });
        let parsed =
            parse_typed_hook_ir(&query_with_generic_add).expect("add + string-slice is the v2 getQueryTerm shape");
        assert_eq!(
            evaluate_typed_get_query_term(&parsed, "nodejs:latest").expect("query term"),
            "latest"
        );
    }

    #[test]
    fn node_and_depth_limits_match_the_compiler() {
        let mut expression = bool_value(true);
        for _ in 0..32 {
            expression = or(bool_value(false), expression);
        }
        let descriptor = trigger_descriptor(expression);
        assert!(parse_typed_hook_ir(&descriptor).is_err());

        let mut expression = bool_value(true);
        for _ in 0..25 {
            expression = TypedExpr::If {
                condition: Box::new(bool_value(true)),
                then_branch: Box::new(expression),
                else_branch: Box::new(bool_value(false)),
            };
        }
        let descriptor = trigger_descriptor(expression);
        assert!(parse_typed_hook_ir(&descriptor).is_err());
    }

    #[test]
    fn utf16_length_index_slice_and_empty_split_match_javascript() {
        let length = strict_eq(length(arg(0)), integer(2));
        assert!(evaluate(length, "😀", ""));

        let index = strict_eq(
            TypedExpr::StringIndexOf {
                value: Box::new(arg(0)),
                needle: Box::new(string("😀")),
            },
            integer(1),
        );
        assert!(evaluate(index, "a😀", ""));

        let source = Utf16String::from_str("a😀b");
        let low_surrogate = TypedExpr::StringSlice {
            value: Box::new(string("a😀b")),
            start: Box::new(integer(2)),
        };
        let result = evaluate_expr(
            &low_surrogate,
            &[
                TypedValue::String(source.clone()),
                TypedValue::String(Utf16String::from_str("")),
            ],
        )
        .expect("slice");
        assert_eq!(
            result,
            TypedValue::String(Utf16String::from_units(vec![0xde00, b'b' as u16]))
        );

        let split = TypedExpr::StringSplit {
            value: Box::new(string("😀")),
            separator: Box::new(string("")),
        };
        let result = evaluate_expr(
            &split,
            &[
                TypedValue::String(source),
                TypedValue::String(Utf16String::from_str("")),
            ],
        )
        .expect("empty split");
        assert_eq!(
            result,
            TypedValue::StringArray(vec![
                Utf16String::from_units(vec![0xd83d]),
                Utf16String::from_units(vec![0xde00]),
            ])
        );

        let empty_nonempty_split = TypedExpr::StringSplit {
            value: Box::new(string("")),
            separator: Box::new(string(",")),
        };
        let result = evaluate_expr(
            &empty_nonempty_split,
            &[
                TypedValue::String(Utf16String::from_str("")),
                TypedValue::String(Utf16String::from_str("")),
            ],
        )
        .expect("empty source split");
        assert_eq!(
            result,
            TypedValue::StringArray(vec![Utf16String::from_units(Vec::<u16>::new())])
        );
    }

    #[test]
    fn logical_operators_are_lazy() {
        let unreachable = gt(integer(1), integer(0));
        let and_expression = and(bool_value(false), unreachable.clone());
        let or_expression = or(bool_value(true), unreachable);
        assert_eq!(
            evaluate_expr(
                &and_expression,
                &[
                    TypedValue::String(Utf16String::from_str("")),
                    TypedValue::String(Utf16String::from_str("")),
                ],
            )
            .expect("lazy and"),
            TypedValue::Bool(false)
        );
        assert_eq!(
            evaluate_expr(
                &or_expression,
                &[
                    TypedValue::String(Utf16String::from_str("")),
                    TypedValue::String(Utf16String::from_str("")),
                ],
            )
            .expect("lazy or"),
            TypedValue::Bool(true)
        );
    }

    fn catalog_entry(expression: TypedExpr) -> JsonValue {
        json!({
            "module": "typed-hooks.js",
            "moduleSha256": "a".repeat(64),
            "path": "root.args[0].generators[0].trigger",
            "sourceField": SOURCE_FIELD,
            "functionBodySha256": "b".repeat(64),
            "descriptor": trigger_descriptor(expression),
        })
    }

    fn sidecar_contracts_json() -> JsonValue {
        json!({
            "trigger": {
                "irVersion": IR_VERSION,
                "params": ["string", "string"],
                "resultType": "bool",
            },
            "getQueryTerm": {
                "irVersion": IR_VERSION,
                "params": ["string"],
                "resultType": "string",
            },
            "postProcess": {
                "irVersion": IR_VERSION,
                "params": ["string", "string-array"],
                "resultType": "suggestion-array",
            },
            "script": {
                "irVersion": IR_VERSION,
                "params": ["string-array"],
                "resultType": "string-array",
            },
            "filterTemplateSuggestions": {
                "irVersion": IR_VERSION,
                "params": ["suggestion-array"],
                "resultType": "suggestion-array",
            },
            "custom": {
                "irVersion": IR_VERSION,
                "params": ["string-array", "exec", "context"],
                "resultType": "suggestion-array",
            },
            "alias": {
                "irVersion": IR_VERSION,
                "params": ["string", "exec"],
                "resultType": "string",
            },
            "loadSpec": {
                "irVersion": IR_VERSION,
                "params": ["string", "exec"],
                "resultType": "spec",
            },
            "generateSpec": {
                "irVersion": IR_VERSION,
                "params": ["string-array", "exec"],
                "resultType": "spec",
            },
        })
    }

    fn catalog_value(hooks: serde_json::Map<String, JsonValue>) -> JsonValue {
        json!({
            "version": IR_VERSION,
            "kind": CATALOG_KIND,
            "contracts": sidecar_contracts_json(),
            "hooks": hooks,
        })
    }

    fn sidecar_contracts() -> TypedHookContracts {
        TypedHookContracts {
            trigger: TypedHookContract {
                ir_version: IR_VERSION,
                params: vec![TypedValueType::String, TypedValueType::String],
                result_type: TypedValueType::Bool,
            },
            get_query_term: TypedHookContract {
                ir_version: IR_VERSION,
                params: vec![TypedValueType::String],
                result_type: TypedValueType::String,
            },
            post_process: TypedHookContract {
                ir_version: IR_VERSION,
                params: vec![TypedValueType::String, TypedValueType::StringArray],
                result_type: TypedValueType::SuggestionArray,
            },
            script: TypedHookContract {
                ir_version: IR_VERSION,
                params: vec![TypedValueType::StringArray],
                result_type: TypedValueType::StringArray,
            },
            filter_template_suggestions: TypedHookContract {
                ir_version: IR_VERSION,
                params: vec![TypedValueType::SuggestionArray],
                result_type: TypedValueType::SuggestionArray,
            },
            custom: TypedHookContract {
                ir_version: IR_VERSION,
                params: vec![
                    TypedValueType::StringArray,
                    TypedValueType::Exec,
                    TypedValueType::Context,
                ],
                result_type: TypedValueType::SuggestionArray,
            },
            alias: TypedHookContract {
                ir_version: IR_VERSION,
                params: vec![TypedValueType::String, TypedValueType::Exec],
                result_type: TypedValueType::String,
            },
            load_spec: TypedHookContract {
                ir_version: IR_VERSION,
                params: vec![TypedValueType::String, TypedValueType::Exec],
                result_type: TypedValueType::Spec,
            },
            generate_spec: TypedHookContract {
                ir_version: IR_VERSION,
                params: vec![TypedValueType::StringArray, TypedValueType::Exec],
                result_type: TypedValueType::Spec,
            },
        }
    }

    #[test]
    fn parses_deterministic_catalog_and_evaluates_by_id() {
        let mut hooks = serde_json::Map::new();
        hooks.insert(
            "é/hooks#one".to_owned(),
            catalog_entry(strict_eq(arg(0), string("ready"))),
        );
        hooks.insert("z/hooks#two".to_owned(), catalog_entry(bool_value(true)));
        let value = catalog_value(hooks);
        let catalog = parse_typed_hook_catalog(&value).expect("valid typed hook catalog");
        let ids: Vec<_> = catalog.hooks.keys().map(String::as_str).collect();
        assert_eq!(ids, ["z/hooks#two", "é/hooks#one"]);

        let entry = lookup_typed_hook(&catalog, "é/hooks#one").expect("catalog entry");
        assert_eq!(entry.module, "typed-hooks.js");
        assert_eq!(entry.path, "root.args[0].generators[0].trigger");
        assert!(evaluate_typed_hook_by_id(&catalog, "é/hooks#one", "ready", "").expect("evaluate"));
        assert!(!evaluate_typed_hook_by_id(&catalog, "é/hooks#one", "waiting", "").expect("evaluate"));
        assert!(evaluate_typed_hook_by_id(&catalog, "z/hooks#two", "", "").expect("evaluate"));
        assert!(lookup_typed_hook(&catalog, "missing").is_none());
        assert!(evaluate_typed_hook_by_id(&catalog, "missing", "", "").is_err());

        let mut mixed = catalog_value(serde_json::Map::new());
        mixed["hooks"]["hook#query"] = json!({
            "module": "typed-hooks.js",
            "moduleSha256": "a".repeat(64),
            "path": "root.args[0].generators[0].getQueryTerm",
            "sourceField": GET_QUERY_TERM_SOURCE_FIELD,
            "functionBodySha256": "b".repeat(64),
            "descriptor": get_query_term_descriptor(),
        });
        assert!(parse_typed_hook_catalog(&mixed).is_ok());

        let bytes = serde_json::to_vec(&value).expect("catalog JSON");
        let from_bytes = parse_typed_hook_catalog_bytes(&bytes).expect("catalog bytes");
        assert_eq!(from_bytes.parsed_descriptor_count(), 0);
        assert_eq!(from_bytes, catalog);
        assert_eq!(
            from_bytes.parsed_descriptor_count(),
            0,
            "catalog equality must not materialize lazy descriptors"
        );

        let descriptor = parse_expression(bool_value(true));
        let descriptor_bytes = serde_json::to_vec(&descriptor).expect("descriptor JSON");
        assert_eq!(
            parse_typed_hook_ir_bytes(&descriptor_bytes).expect("descriptor bytes"),
            descriptor
        );
    }

    #[test]
    fn catalog_missing_tampered_unknown_and_orphan_shapes_fail_closed() {
        let mut valid = catalog_value(serde_json::Map::from_iter([(
            "hook#one".to_owned(),
            catalog_entry(bool_value(true)),
        )]));

        let mut missing = valid.clone();
        missing["hooks"]["hook#one"]
            .as_object_mut()
            .expect("entry object")
            .remove("descriptor");
        assert!(parse_typed_hook_catalog(&missing).is_err());

        let mut unknown_root = valid.clone();
        unknown_root["extra"] = json!(true);
        assert!(parse_typed_hook_catalog(&unknown_root).is_err());

        let mut unknown_entry = valid.clone();
        unknown_entry["hooks"]["hook#one"]["extra"] = json!(true);
        assert!(parse_typed_hook_catalog(&unknown_entry).is_err());

        let mut unknown_contract = valid.clone();
        unknown_contract["contracts"]["trigger"]["extra"] = json!(true);
        assert!(parse_typed_hook_catalog(&unknown_contract).is_err());

        let mut unknown_descriptor = valid.clone();
        unknown_descriptor["hooks"]["hook#one"]["descriptor"]["expr"]["extra"] = json!(true);
        assert!(parse_typed_hook_catalog(&unknown_descriptor).is_err());

        let mut tampered_module = valid.clone();
        tampered_module["hooks"]["hook#one"]["module"] = json!("source-modules/typed-hooks.js");
        assert!(parse_typed_hook_catalog(&tampered_module).is_err());

        let mut tampered_sha = valid.clone();
        tampered_sha["hooks"]["hook#one"]["moduleSha256"] = json!("A".repeat(64));
        assert!(parse_typed_hook_catalog(&tampered_sha).is_err());

        let mut tampered_path = valid.clone();
        tampered_path["hooks"]["hook#one"]["path"] = json!("root.args[0].generators[custom]");
        assert!(parse_typed_hook_catalog(&tampered_path).is_err());

        let mut tampered_field = valid.clone();
        tampered_field["hooks"]["hook#one"]["sourceField"] = json!("unknownField");
        assert!(parse_typed_hook_catalog(&tampered_field).is_err());

        let mut tampered_contract = valid.clone();
        tampered_contract["contracts"]["trigger"]["params"] = json!(["string"]);
        assert!(parse_typed_hook_catalog(&tampered_contract).is_err());

        let mut tampered_descriptor = valid.clone();
        tampered_descriptor["hooks"]["hook#one"]["descriptor"]["resultType"] = json!("string");
        assert!(parse_typed_hook_catalog(&tampered_descriptor).is_err());

        valid["hooks"]["hook#one"]["functionBodySha256"] = json!("0".repeat(63));
        assert!(parse_typed_hook_catalog(&valid).is_err());

        let mut unknown_on_load = catalog_value(serde_json::Map::from_iter([(
            "hook#one".to_owned(),
            catalog_entry(bool_value(true)),
        )]));
        unknown_on_load["hooks"]["hook#one"]["descriptor"]["expr"]["extra"] = json!(true);
        let delayed = serde_json::to_vec(&unknown_on_load).expect("catalog JSON");
        let catalog = parse_typed_hook_catalog_bytes(&delayed).expect("index accepts a raw descriptor");
        assert_eq!(catalog.parsed_descriptor_count(), 0);
        assert!(evaluate_typed_hook_by_id(&catalog, "hook#one", "", "").is_err());
        assert_eq!(catalog.parsed_descriptor_count(), 0);

        let unpaired_surrogate = br#"{"version":1,"kind":"typed-hook-expression","sourceField":"trigger","resultType":"bool","params":[{"index":0,"type":"string"},{"index":1,"type":"string"}],"expr":{"op":"strict-eq","left":{"op":"string","value":"\ud800"},"right":{"op":"string","value":"x"}}}"#;
        assert!(parse_typed_hook_ir_bytes(unpaired_surrogate).is_err());
    }

    #[test]
    fn catalog_path_id_module_and_count_limits_are_strict() {
        let mut hooks = serde_json::Map::new();
        hooks.insert("a/path#unicode-✓".to_owned(), catalog_entry(bool_value(true)));
        let valid = catalog_value(hooks);
        assert!(parse_typed_hook_catalog(&valid).is_ok());

        for invalid_id in ["", "bad\\id", "bad\0id", &"x".repeat(MAX_HOOK_ID_BYTES + 1)] {
            let mut value = catalog_value(serde_json::Map::from_iter([(
                invalid_id.to_owned(),
                catalog_entry(bool_value(true)),
            )]));
            assert!(parse_typed_hook_catalog(&value).is_err(), "id {invalid_id:?}");
            value["hooks"] = json!({});
        }

        for invalid_module in [
            "",
            ".js",
            "..js",
            "source-modules/x.js",
            "x\\y.js",
            &format!("{}.js", "x".repeat(MAX_MODULE_BASENAME_BYTES)),
        ] {
            let mut entry = catalog_entry(bool_value(true));
            entry["module"] = json!(invalid_module);
            let value = catalog_value(serde_json::Map::from_iter([("hook".to_owned(), entry)]));
            assert!(parse_typed_hook_catalog(&value).is_err(), "module {invalid_module:?}");
        }

        for invalid_path in [
            "root..args",
            "root.args[]",
            "root.args[0].bad-name",
            "root.args[0].generators[custom]",
        ] {
            let mut entry = catalog_entry(bool_value(true));
            entry["path"] = json!(invalid_path);
            let value = catalog_value(serde_json::Map::from_iter([("hook".to_owned(), entry)]));
            assert!(parse_typed_hook_catalog(&value).is_err(), "path {invalid_path:?}");
        }

        let mut oversized_path_entry = catalog_entry(bool_value(true));
        oversized_path_entry["path"] = json!(format!("root.{}", "a".repeat(MAX_HOOK_PATH_BYTES)));
        let oversized_path = catalog_value(serde_json::Map::from_iter([("hook".to_owned(), oversized_path_entry)]));
        assert!(parse_typed_hook_catalog(&oversized_path).is_err());

        let mut too_many = BTreeMap::new();
        for index in 0..=MAX_CATALOG_HOOKS {
            too_many.insert(
                format!("hook-{index}"),
                TypedHookCatalogEntry {
                    module: "typed-hooks.js".to_owned(),
                    module_sha256: "a".repeat(64),
                    path: "root.args[0]".to_owned(),
                    source_field: SOURCE_FIELD.to_owned(),
                    function_body_sha256: "b".repeat(64),
                    descriptor: parse_expression(bool_value(true)).into(),
                },
            );
        }
        let catalog = TypedHookCatalog {
            version: IR_VERSION,
            kind: CATALOG_KIND.to_owned(),
            contracts: sidecar_contracts(),
            hooks: too_many,
            adapters: BTreeMap::new(),
        };
        assert!(validate_typed_hook_catalog(&catalog).is_err());
    }

    #[test]
    fn production_sidecar_stays_unparsed_until_a_hook_runs() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bundle/specs-ir/typed-hooks.json");
        let Ok(bytes) = fs::read(&path) else {
            return;
        };
        let catalog = parse_typed_hook_catalog_bytes(&bytes).expect("production sidecar");
        assert!(
            catalog.hooks.len() > 1_000,
            "expected the bundled catalog, got {}",
            catalog.hooks.len()
        );
        assert_eq!(
            catalog.parsed_descriptor_count(),
            0,
            "loading typed-hooks.json must not materialize every IR tree"
        );
        let hook_count = catalog.hooks.len();
        let hook_id = catalog.hooks.keys().next().expect("catalog is non-empty").clone();
        let runtime = catalog.into_runtime();
        assert_eq!(runtime.hooks.len(), hook_count);
        assert_eq!(
            runtime.parsed_descriptor_count(),
            0,
            "into_runtime must not parse descriptors"
        );
        let descriptor = runtime.hooks.get(&hook_id).expect("first hook");
        let first_body = descriptor.raw_json().to_owned();
        let _ = descriptor.get();
        let same_body = runtime
            .hooks
            .values()
            .filter(|hook| hook.raw_json() == first_body)
            .count();
        assert_eq!(
            runtime.parsed_descriptor_count(),
            same_body,
            "evaluating one body parses every hook that shares it, and no other"
        );
        assert!(runtime.hooks.values().any(|hook| !hook.is_parsed()));
        let unique = runtime
            .hooks
            .values()
            .map(|hook| Arc::as_ptr(&hook.shared))
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(
            unique < hook_count,
            "identical descriptor bodies must share one allocation ({unique} unique of {hook_count})"
        );
    }

    #[test]
    fn duplicate_descriptor_bodies_share_one_allocation_inside_one_parse() {
        let mut hooks = serde_json::Map::new();
        hooks.insert("hook#a".to_owned(), catalog_entry(bool_value(true)));
        hooks.insert("hook#b".to_owned(), catalog_entry(bool_value(true)));
        hooks.insert("hook#c".to_owned(), catalog_entry(bool_value(false)));
        let bytes = serde_json::to_vec(&catalog_value(hooks)).expect("catalog JSON");
        let catalog = parse_typed_hook_catalog_bytes(&bytes).expect("index");
        assert_eq!(catalog.parsed_descriptor_count(), 0);
        let a = &catalog.hooks["hook#a"].descriptor;
        let b = &catalog.hooks["hook#b"].descriptor;
        let c = &catalog.hooks["hook#c"].descriptor;
        assert!(Arc::ptr_eq(&a.shared, &b.shared));
        assert!(!Arc::ptr_eq(&a.shared, &c.shared));
        assert!(a.get().is_ok());
        assert!(b.is_parsed());
        assert!(!c.is_parsed());
        assert_eq!(catalog.parsed_descriptor_count(), 2);

        let again = parse_typed_hook_catalog_bytes(&bytes).expect("second index");
        assert!(!Arc::ptr_eq(
            &catalog.hooks["hook#a"].descriptor.shared,
            &again.hooks["hook#a"].descriptor.shared
        ));
        assert_eq!(again.parsed_descriptor_count(), 0);
        assert_eq!(catalog, again);
        assert_eq!(again.parsed_descriptor_count(), 0);
        assert_eq!(catalog.into_runtime().descriptor_counts(), (2, 1));
        assert_eq!(again.into_runtime().descriptor_counts(), (2, 0));
    }

    #[test]
    fn runtime_catalog_drops_compile_time_provenance_without_parsing() {
        let mut hooks = serde_json::Map::new();
        hooks.insert("hook#one".to_owned(), catalog_entry(bool_value(true)));
        let bytes = serde_json::to_vec(&catalog_value(hooks)).expect("catalog JSON");
        let catalog = parse_typed_hook_catalog_bytes(&bytes).expect("index");
        let entry = catalog.hooks.get("hook#one").expect("entry");
        assert_eq!(entry.module, "typed-hooks.js");
        assert!(!entry.module_sha256.is_empty());
        assert!(!entry.path.is_empty());
        assert_eq!(catalog.parsed_descriptor_count(), 0);

        let runtime = catalog.into_runtime();
        assert_eq!(runtime.hooks.len(), 1);
        assert_eq!(runtime.parsed_descriptor_count(), 0);
        assert!(evaluate_typed_hook_by_id_runtime(&runtime, "hook#one", "", "").expect("runtime evaluate"));
        assert_eq!(runtime.parsed_descriptor_count(), 1);
    }

    fn evaluate_typed_hook_by_id_runtime(
        catalog: &RuntimeTypedHookCatalog,
        hook_id: &str,
        search_term: &str,
        previous_search_term: &str,
    ) -> TypedHookResult<bool> {
        let descriptor = catalog
            .hooks
            .get(hook_id)
            .ok_or_else(|| TypedHookError::new(format!("typed hook id {hook_id:?} is missing")))?;
        evaluate_typed_trigger(descriptor.get()?, search_term, previous_search_term)
    }

    fn typed_trigger_reference_value() -> JsonValue {
        serde_json::from_slice(TYPED_TRIGGER_REFERENCE).expect("reference JSON")
    }

    #[test]
    fn checked_in_typed_trigger_reference_matches_native_evaluator() {
        let baseline =
            parse_typed_trigger_reference_bytes(TYPED_TRIGGER_REFERENCE).expect("checked-in typed trigger reference");
        assert_eq!(baseline.catalog.hooks.len(), baseline.expected.len());
        assert!(!baseline.cases.is_empty());

        for (hook_id, expected_values) in &baseline.expected {
            let entry = baseline.catalog.hooks.get(hook_id).expect("catalog hook");
            assert_eq!(expected_values.len(), baseline.cases.len());
            for (index, case) in baseline.cases.iter().enumerate() {
                let actual =
                    evaluate_typed_trigger(entry.descriptor.get().expect("lazy ir"), &case.args[0], &case.args[1])
                        .expect("native typed trigger evaluation");
                assert_eq!(actual, expected_values[index], "hook {hook_id}, case {}", case.id);
            }
        }
    }

    #[test]
    fn typed_trigger_reference_schema_and_provenance_fail_closed() {
        let mut unknown = typed_trigger_reference_value();
        unknown["unexpected"] = json!(true);
        assert!(parse_typed_trigger_reference(&unknown).is_err());

        for (field, invalid) in [("version", json!(2)), ("kind", json!("future-reference"))] {
            let mut value = typed_trigger_reference_value();
            value[field] = invalid;
            assert!(parse_typed_trigger_reference(&value).is_err(), "field {field}");
        }

        for field in [
            "generatorSha256",
            "harnessSha256",
            "pairSha256",
            "hookManifestSha256",
            "sidecarSha256",
        ] {
            let mut value = typed_trigger_reference_value();
            value[field] = json!("A".repeat(64));
            assert!(parse_typed_trigger_reference(&value).is_err(), "field {field}");
        }

        let mut wrong_sidecar_digest = typed_trigger_reference_value();
        wrong_sidecar_digest["sidecarSha256"] = json!("0".repeat(64));
        assert!(
            parse_typed_trigger_reference(&wrong_sidecar_digest).is_ok(),
            "sidecarSha256 is the full sidecar file hash, not a digest of the trigger-only catalog"
        );

        let mut wrong_case_field = typed_trigger_reference_value();
        wrong_case_field["cases"][0]["extra"] = json!(true);
        assert!(parse_typed_trigger_reference(&wrong_case_field).is_err());

        let mut wrong_args_count = typed_trigger_reference_value();
        wrong_args_count["cases"][0]["args"] = json!([""]);
        assert!(parse_typed_trigger_reference(&wrong_args_count).is_err());

        let mut wrong_args_type = typed_trigger_reference_value();
        wrong_args_type["cases"][0]["args"] = json!(["", false]);
        assert!(parse_typed_trigger_reference(&wrong_args_type).is_err());
    }

    #[test]
    fn typed_trigger_reference_case_ids_and_limits_are_strict() {
        let mut empty_id = typed_trigger_reference_value();
        empty_id["cases"][0]["id"] = json!("");
        assert!(parse_typed_trigger_reference(&empty_id).is_err());

        let mut duplicate_id = typed_trigger_reference_value();
        let first_id = duplicate_id["cases"][0]["id"].clone();
        duplicate_id["cases"][1]["id"] = first_id;
        assert!(parse_typed_trigger_reference(&duplicate_id).is_err());

        for dangerous_id in ["case/name", "case\\name", "case\0name"] {
            let mut value = typed_trigger_reference_value();
            value["cases"][0]["id"] = json!(dangerous_id);
            assert!(parse_typed_trigger_reference(&value).is_err(), "id {dangerous_id:?}");
        }

        let mut too_many_cases = typed_trigger_reference_value();
        let case = too_many_cases["cases"][0].clone();
        too_many_cases["cases"] = JsonValue::Array(vec![case; MAX_REFERENCE_CASES + 1]);
        assert!(parse_typed_trigger_reference(&too_many_cases).is_err());

        let mut oversized_corpus = typed_trigger_reference_value();
        oversized_corpus["cases"][0]["args"][0] = json!("x".repeat(MAX_REFERENCE_CORPUS_BYTES));
        assert!(parse_typed_trigger_reference(&oversized_corpus).is_err());

        let mut marker = typed_trigger_reference_value();
        marker["cases"][0]["args"][0] = json!(REFERENCE_EXEC_MARKER);
        assert!(parse_typed_trigger_reference(&marker).is_err());
    }

    #[test]
    fn typed_trigger_reference_expected_ids_lengths_and_types_are_strict() {
        let mut missing_expected = typed_trigger_reference_value();
        let first_id = missing_expected["expected"]
            .as_object_mut()
            .expect("expected map")
            .keys()
            .next()
            .cloned()
            .expect("expected hook");
        missing_expected["expected"]
            .as_object_mut()
            .expect("expected map")
            .remove(&first_id);
        assert!(parse_typed_trigger_reference(&missing_expected).is_err());

        let mut extra_expected = typed_trigger_reference_value();
        extra_expected["expected"]["not-a-catalog-hook"] = json!([]);
        assert!(parse_typed_trigger_reference(&extra_expected).is_err());

        let first_id = typed_trigger_reference_value()["expected"]
            .as_object()
            .expect("expected map")
            .keys()
            .next()
            .cloned()
            .expect("expected hook");

        let mut wrong_length = typed_trigger_reference_value();
        wrong_length["expected"][&first_id] = json!([]);
        assert!(parse_typed_trigger_reference(&wrong_length).is_err());

        let mut wrong_type = typed_trigger_reference_value();
        wrong_type["expected"][&first_id] = json!([true, "not-a-bool"]);
        assert!(parse_typed_trigger_reference(&wrong_type).is_err());
    }

    fn typed_get_query_term_reference_value() -> JsonValue {
        serde_json::from_slice(TYPED_GET_QUERY_TERM_REFERENCE).expect("getQueryTerm reference JSON")
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ReferenceFileIdentity {
        length: u64,
        #[cfg(unix)]
        device: u64,
        #[cfg(unix)]
        inode: u64,
        #[cfg(unix)]
        mode: u32,
    }

    fn reference_file_identity(metadata: &Metadata) -> ReferenceFileIdentity {
        ReferenceFileIdentity {
            length: metadata.len(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            mode: metadata.mode(),
        }
    }

    fn safe_repo_relative_path(value: &str) -> bool {
        !value.is_empty()
            && !value.contains(['\\', '\0'])
            && Path::new(value)
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            && value
                .split('/')
                .all(|component| !component.is_empty() && component != "." && component != "..")
    }

    fn reject_symlink_ancestors(path: &Path) -> Result<(), String> {
        let mut current = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => current.push(prefix.as_os_str()),
                Component::RootDir => current.push(Path::new("/")),
                Component::CurDir => {},
                Component::ParentDir => current.push(".."),
                Component::Normal(name) => current.push(name),
            }
            let metadata =
                fs::symlink_metadata(&current).map_err(|error| format!("inspect {}: {error}", current.display()))?;
            if metadata.file_type().is_symlink() {
                return Err(format!("{} contains a symbolic-link ancestor", path.display()));
            }
        }
        Ok(())
    }

    /// Read one checked-in provenance file without following a symlink or
    /// accepting a path outside the repository.  The descriptor only carries
    /// relative names; this helper is deliberately test-only and never
    /// executes or parses JavaScript.
    fn read_reference_file(repo_root: &Path, base: &str, relative: &str) -> Result<Vec<u8>, String> {
        if !safe_repo_relative_path(base) || !safe_repo_relative_path(relative) {
            return Err(format!(
                "unsafe repository-relative provenance path {base:?}/{relative:?}"
            ));
        }
        reject_symlink_ancestors(repo_root)?;
        let root_metadata = fs::symlink_metadata(repo_root)
            .map_err(|error| format!("inspect repository root {}: {error}", repo_root.display()))?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err(format!(
                "repository root is not a real directory: {}",
                repo_root.display()
            ));
        }
        let canonical_root = fs::canonicalize(repo_root)
            .map_err(|error| format!("canonicalize repository root {}: {error}", repo_root.display()))?;
        let mut path = repo_root.to_path_buf();
        for component in base.split('/').chain(relative.split('/')) {
            path.push(component);
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("inspect provenance file {}: {error}", path.display()))?;
            if metadata.file_type().is_symlink() {
                return Err(format!("provenance file {} is a symbolic link", path.display()));
            }
        }
        let before = fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect provenance file {}: {error}", path.display()))?;
        if before.file_type().is_symlink() || !before.is_file() {
            return Err(format!("provenance path is not a regular file: {}", path.display()));
        }
        let canonical_file = fs::canonicalize(&path)
            .map_err(|error| format!("canonicalize provenance file {}: {error}", path.display()))?;
        if canonical_file.strip_prefix(&canonical_root).is_err() {
            return Err(format!("provenance file escapes repository root: {}", path.display()));
        }
        let before_identity = reference_file_identity(&before);

        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        let mut file = options
            .open(&path)
            .map_err(|error| format!("open provenance file {}: {error}", path.display()))?;
        let opened = file
            .metadata()
            .map_err(|error| format!("stat provenance file {}: {error}", path.display()))?;
        if opened.file_type().is_symlink() || !opened.is_file() || reference_file_identity(&opened) != before_identity {
            return Err(format!("provenance file changed while opening: {}", path.display()));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| format!("read provenance file {}: {error}", path.display()))?;
        let after_open = file
            .metadata()
            .map_err(|error| format!("restat provenance file {}: {error}", path.display()))?;
        if reference_file_identity(&after_open) != before_identity {
            return Err(format!("provenance file drifted while reading: {}", path.display()));
        }
        let after = fs::symlink_metadata(&path)
            .map_err(|error| format!("restat provenance path {}: {error}", path.display()))?;
        if after.file_type().is_symlink() || reference_file_identity(&after) != before_identity {
            return Err(format!("provenance path drifted while reading: {}", path.display()));
        }
        Ok(bytes)
    }

    fn expected_reference_hook_file(id: &str) -> String {
        let mut name = String::with_capacity(id.len() + 4);
        for character in id.chars() {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                name.push(character);
            } else {
                name.push('_');
            }
        }
        name.push_str(".js");
        name
    }

    fn json_string<'a>(value: &'a JsonValue, field: &str) -> Result<&'a str, String> {
        value
            .get(field)
            .and_then(JsonValue::as_str)
            .ok_or_else(|| format!("manifest field {field:?} is not a string"))
    }

    fn assert_current_asdf_get_query_term_provenance(
        baseline: &TypedGetQueryTermReferenceBaseline,
    ) -> Result<(), String> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let source_root = repo_root.join("bundle/specs");
        let ir_root = repo_root.join("bundle/specs-ir");
        crate::spec_pair::verify_pair(Some(&source_root), &ir_root)
            .map_err(|error| format!("source/IR pair verification failed: {error:#}"))?;

        let marker = read_reference_file(&repo_root, "bundle/specs-ir", ".spec-pair.json")?;
        let marker: JsonValue =
            serde_json::from_slice(&marker).map_err(|error| format!("pair marker JSON is invalid: {error}"))?;
        let marker_pair_sha256 = json_string(&marker, "pairSha256")?;
        if marker_pair_sha256 != baseline.pair_sha256 {
            return Err("pairSha256 differs from the checked-in pair marker".to_owned());
        }

        let generator = read_reference_file(&repo_root, "scripts", "compile-spec-ir.mjs")?;
        if sha256_hex(&generator) != baseline.generator_sha256 {
            return Err("generatorSha256 differs from scripts/compile-spec-ir.mjs".to_owned());
        }

        let mut harness_records = Vec::with_capacity(ASDF_REFERENCE_HARNESS_FILES.len());
        for file in ASDF_REFERENCE_HARNESS_FILES {
            let (base, relative) = file
                .rsplit_once('/')
                .ok_or_else(|| format!("harness path has no repository directory: {file}"))?;
            let bytes = read_reference_file(&repo_root, base, relative)?;
            harness_records.push(((*file).to_owned(), sha256_hex(&bytes)));
        }
        harness_records.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        let mut harness_canonical = String::new();
        for (file, digest) in harness_records {
            harness_canonical.push_str(&file);
            harness_canonical.push('\0');
            harness_canonical.push_str(&digest);
            harness_canonical.push('\n');
        }
        if sha256_hex(harness_canonical.as_bytes()) != baseline.harness_sha256 {
            return Err("harnessSha256 differs from the current reference harness".to_owned());
        }

        let sidecar = read_reference_file(&repo_root, "bundle/specs-ir", "typed-hooks.json")?;
        if sha256_hex(&sidecar) != baseline.hook_manifest_sha256 {
            return Err("hookManifestSha256 differs from typed-hooks.json".to_owned());
        }
        let sidecar: JsonValue =
            serde_json::from_slice(&sidecar).map_err(|error| format!("typed-hooks.json is invalid JSON: {error}"))?;
        let sidecar_hooks = sidecar
            .get("hooks")
            .and_then(JsonValue::as_object)
            .ok_or_else(|| "typed-hooks.json has no hooks object".to_owned())?;

        for id in ASDF_GET_QUERY_TERM_CANDIDATE_IDS {
            let candidate = baseline
                .candidates
                .get(id)
                .ok_or_else(|| format!("baseline candidate {id:?} is missing"))?;
            if candidate.hook_file != expected_reference_hook_file(id) {
                return Err(format!("candidate {id:?} has an unexpected hook artifact name"));
            }
            let source = read_reference_file(&repo_root, "bundle/specs", &candidate.source)?;
            if sha256_hex(&source) != candidate.source_sha256 {
                return Err(format!("candidate {id:?} source bytes drifted"));
            }
            let ir = read_reference_file(&repo_root, "bundle/specs-ir", &candidate.ir)?;
            if sha256_hex(&ir) != candidate.ir_sha256 {
                return Err(format!("candidate {id:?} IR bytes drifted"));
            }

            let descriptor = sidecar_hooks
                .get(id)
                .ok_or_else(|| format!("typed sidecar entry {id:?} is missing"))?;
            for (field, expected) in [
                ("module", candidate.module.as_str()),
                ("moduleSha256", candidate.module_sha256.as_str()),
                ("path", candidate.path.as_str()),
                ("sourceField", candidate.source_field.as_str()),
                ("functionBodySha256", candidate.function_body_sha256.as_str()),
            ] {
                if json_string(descriptor, field)? != expected {
                    return Err(format!("candidate {id:?} differs from typed sidecar {field}"));
                }
            }
            if json_string(descriptor, "moduleSha256")? != candidate.module_sha256 {
                return Err(format!("candidate {id:?} typed sidecar moduleSha256 differs"));
            }
        }
        Ok(())
    }

    #[test]
    fn checked_in_asdf_get_query_term_reference_matches_native_evaluator() {
        let baseline = parse_typed_get_query_term_reference_bytes(TYPED_GET_QUERY_TERM_REFERENCE)
            .expect("checked-in getQueryTerm reference");
        assert_current_asdf_get_query_term_provenance(&baseline).expect("current getQueryTerm provenance");
        assert_eq!(baseline.candidates.len(), 2);
        assert_eq!(baseline.candidates.len(), baseline.expected.len());
        for (hook_id, expected_values) in &baseline.expected {
            let candidate = baseline.candidates.get(hook_id).expect("candidate");
            assert_eq!(expected_values.len(), baseline.cases.len());
            for (index, case) in baseline.cases.iter().enumerate() {
                let actual = evaluate_typed_get_query_term(&candidate.descriptor, &case.args[0])
                    .expect("native getQueryTerm evaluation");
                assert_eq!(actual, expected_values[index], "hook {hook_id}, case {}", case.id);
            }
        }
    }

    #[test]
    fn asdf_reference_file_reader_rejects_escape_missing_and_symlink_paths() {
        let root = tempfile::tempdir().expect("reference root");
        let root_path = fs::canonicalize(root.path()).expect("canonical reference root");
        let base = root_path.join("base");
        fs::create_dir(&base).expect("reference base");
        fs::write(base.join("ok.txt"), b"stable").expect("reference file");
        assert_eq!(
            read_reference_file(&root_path, "base", "ok.txt").expect("safe reference file"),
            b"stable"
        );
        assert!(read_reference_file(&root_path, "base", "../ok.txt").is_err());
        assert!(read_reference_file(&root_path, "base", "missing.txt").is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(base.join("ok.txt"), base.join("link.txt")).expect("file symlink");
            assert!(read_reference_file(&root_path, "base", "link.txt").is_err());
            std::os::unix::fs::symlink(&base, root_path.join("linked-base")).expect("directory symlink");
            assert!(read_reference_file(&root_path, "linked-base", "ok.txt").is_err());
        }
    }

    #[test]
    fn asdf_reference_provenance_rejects_baseline_hash_drift() {
        let mut baseline = parse_typed_get_query_term_reference_bytes(TYPED_GET_QUERY_TERM_REFERENCE)
            .expect("checked-in getQueryTerm reference");
        baseline
            .candidates
            .get_mut("asdf#getQueryTerm#7")
            .expect("asdf candidate")
            .module_sha256 = "0".repeat(64);
        let error =
            assert_current_asdf_get_query_term_provenance(&baseline).expect_err("module hash drift must fail closed");
        assert!(
            error.contains("typed sidecar moduleSha256") || error.contains("differs from typed sidecar"),
            "{error}"
        );
    }

    #[test]
    fn asdf_get_query_term_reference_is_closed_and_fail_closed() {
        let mut unknown = typed_get_query_term_reference_value();
        unknown["unexpected"] = json!(true);
        assert!(parse_typed_get_query_term_reference(&unknown).is_err());

        for (field, invalid) in [("version", json!(2)), ("kind", json!("future-reference"))] {
            let mut value = typed_get_query_term_reference_value();
            value[field] = invalid;
            assert!(parse_typed_get_query_term_reference(&value).is_err(), "field {field}");
        }

        let mut extra_candidate = typed_get_query_term_reference_value();
        extra_candidate["candidates"]["not-asdf"] = extra_candidate["candidates"]["asdf#getQueryTerm#7"].clone();
        assert!(parse_typed_get_query_term_reference(&extra_candidate).is_err());

        let mut wrong_source = typed_get_query_term_reference_value();
        wrong_source["candidates"]["asdf#getQueryTerm#7"]["sourceField"] = json!("trigger");
        assert!(parse_typed_get_query_term_reference(&wrong_source).is_err());

        let mut unknown_then = typed_get_query_term_reference_value();
        unknown_then["candidates"]["asdf#getQueryTerm#7"]["descriptor"]["expr"]["then"] = json!({
            "op": "future-slice",
            "value": {"op": "arg", "index": 0}
        });
        assert!(parse_typed_get_query_term_reference(&unknown_then).is_err());

        let mut wrong_expected = typed_get_query_term_reference_value();
        wrong_expected["expected"]["asdf#getQueryTerm#7"] = json!(["only-one"]);
        assert!(parse_typed_get_query_term_reference(&wrong_expected).is_err());
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TypedIrV2OpsGolden {
        version: u64,
        kind: String,
        cases: Vec<TypedIrV2OpsCase>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TypedIrV2OpsCase {
        id: String,
        #[serde(rename = "sourceField")]
        source_field: String,
        #[allow(dead_code)]
        body: Option<String>,
        args: Vec<JsonValue>,
        expected: JsonValue,
        descriptor: JsonValue,
    }

    #[test]
    fn typed_ir_v2_ops_golden_matches_javascript_evaluator() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/native-hooks/typed-ir-v2-ops.json");
        let bytes = fs::read(&path).expect("typed-ir-v2-ops golden");
        let golden: TypedIrV2OpsGolden = serde_json::from_slice(&bytes).expect("typed-ir-v2-ops schema");
        assert_eq!(golden.version, 1);
        assert_eq!(golden.kind, "typed-ir-v2-ops");
        assert!(
            golden.cases.len() >= 30,
            "v2 op golden must cover the new string/numeric/nullish ops, got {}",
            golden.cases.len()
        );
        for case in &golden.cases {
            let descriptor = parse_typed_hook_ir(&case.descriptor)
                .unwrap_or_else(|error| panic!("case {} failed to parse: {error}", case.id));
            assert_eq!(descriptor.source_field, case.source_field, "{}", case.id);
            let actual = evaluate_typed_hook_json(&descriptor, &case.args)
                .unwrap_or_else(|error| panic!("case {} failed to evaluate: {error}", case.id));
            assert_eq!(actual, case.expected, "{}", case.id);
        }
    }

    #[test]
    fn script_and_filter_contracts_parse_without_entering_production_sidecars() {
        let script = json!({
            "version": IR_VERSION,
            "kind": IR_KIND,
            "sourceField": "script",
            "resultType": "string-array",
            "params": [{"index": 0, "type": "string-array"}],
            "expr": {
                "op": "array",
                "items": [
                    {"op": "string", "value": "echo"},
                    {"op": "string", "value": "-n"}
                ]
            }
        });
        let descriptor = parse_typed_hook_ir(&script).expect("script descriptor");
        assert_eq!(
            evaluate_typed_hook_json(&descriptor, &[json!([])]).expect("script eval"),
            json!(["echo", "-n"])
        );

        let filter = json!({
            "version": IR_VERSION,
            "kind": IR_KIND,
            "sourceField": "filterTemplateSuggestions",
            "resultType": "suggestion-array",
            "params": [{"index": 0, "type": "suggestion-array"}],
            "expr": {"op": "arg", "index": 0}
        });
        assert!(parse_typed_hook_ir(&filter).is_ok());
        assert_eq!(
            evaluate_typed_hook_json(
                &parse_typed_hook_ir(&filter).expect("filter"),
                &[json!([{"name": "a"}])]
            )
            .expect("filter eval"),
            json!([{"name": "a"}])
        );

        let command = evaluate_typed_script(&descriptor, &[]).expect("typed script command");
        assert_eq!(command.command, "echo");
        assert_eq!(command.args, vec!["-n"]);

        let rows = evaluate_typed_filter_template_suggestions(
            &parse_typed_hook_ir(&filter).expect("filter"),
            &[Suggestion::new("a", "", "arg")],
        )
        .expect("typed filter");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "a");
    }

    #[test]
    fn effect_ops_evaluate_exec_context_and_spec_object() {
        let load = json!({
            "version": IR_VERSION,
            "kind": IR_KIND,
            "sourceField": "loadSpec",
            "resultType": "spec",
            "params": [
                {"index": 0, "type": "string"},
                {"index": 1, "type": "exec"}
            ],
            "expr": {
                "op": "spec-object",
                "fields": [
                    {
                        "key": "name",
                        "value": {
                            "op": "string-concat",
                            "parts": [
                                {"op": "string", "value": "create-"},
                                {"op": "arg", "index": 0}
                            ]
                        }
                    },
                    {"key": "type", "value": {"op": "string", "value": "global"}}
                ]
            }
        });
        let descriptor = parse_typed_hook_ir(&load).expect("loadSpec descriptor");
        let context = TypedHookContext {
            current_working_directory: "/repo".into(),
            current_process: "zsh".into(),
            ssh_prefix: String::new(),
            environment_variables: std::sync::Arc::new(vec![
                ("Z".into(), "tail".into()),
                ("HOME".into(), "obsolete home".into()),
                ("HOME".into(), "/Users/x".into()),
                ("A".into(), "first".into()),
            ])
            .into(),
            search_term: String::new(),
            is_dangerous: false,
        };
        let exec = |_request: TypedExecRequest| -> TypedHookResult<TypedExecResult> {
            panic!("exec should not run");
        };
        assert_eq!(
            evaluate_typed_load_spec(&descriptor, "react-app", &context, &exec, None).expect("loadSpec"),
            json!({"name": "create-react-app", "type": "global"})
        );

        let custom = json!({
            "version": IR_VERSION,
            "kind": IR_KIND,
            "sourceField": "custom",
            "resultType": "suggestion-array",
            "params": [
                {"index": 0, "type": "string-array"},
                {"index": 1, "type": "exec"},
                {"index": 2, "type": "context"}
            ],
            "expr": {
                "op": "array-map",
                "value": {"op": "object-values", "value": {"op": "ctx-environment"}},
                "fn": {
                    "op": "lambda",
                    "params": ["n"],
                    "body": {
                        "op": "object",
                        "fields": [
                            {"key": "name", "value": {"op": "var", "name": "n"}},
                            {"key": "description", "value": {"op": "string", "value": "Environment variable"}}
                        ]
                    }
                }
            }
        });
        let rows = evaluate_typed_custom(
            &parse_typed_hook_ir(&custom).expect("custom"),
            &["env".into()],
            &context,
            &exec,
            None,
        )
        .expect("custom");
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["first", "/Users/x", "tail"]
        );
        let mut single = custom;
        single["expr"] = json!({
            "op": "array",
            "items": [{
                "op": "object",
                "fields": [{
                    "key": "name",
                    "value": {"op": "ctx-env", "name": {"op": "string", "value": "HOME"}}
                }]
            }]
        });
        let rows = evaluate_typed_custom(
            &parse_typed_hook_ir(&single).expect("single env lookup"),
            &[],
            &context,
            &exec,
            None,
        )
        .expect("single env lookup");
        assert_eq!(
            rows[0].name, "/Users/x",
            "lookup and full object use the same last value"
        );
        let result = single["expr"].clone();
        single["expr"] = json!({
            "op": "seq",
            "items": [{
                "op": "exec",
                "command": {"op": "string", "value": "fixture"},
                "args": {"op": "array", "items": []},
                "cwd": {"op": "ctx-cwd"},
                "env": {"op": "ctx-environment"},
                "timeout": {"op": "null"}
            }, result]
        });
        let calls = std::cell::Cell::new(0);
        let exec = |request: TypedExecRequest| {
            calls.set(calls.get() + 1);
            assert_eq!(request.command, "fixture");
            assert_eq!(request.cwd.as_deref(), Some("/repo"));
            assert_eq!(
                request.env,
                [
                    ("A".into(), "first".into()),
                    ("HOME".into(), "/Users/x".into()),
                    ("Z".into(), "tail".into())
                ]
            );
            Ok(TypedExecResult {
                stdout: String::new(),
                stderr: String::new(),
                status: 0,
            })
        };
        evaluate_typed_custom(
            &parse_typed_hook_ir(&single).expect("exec environment"),
            &[],
            &context,
            &exec,
            None,
        )
        .expect("exec environment");
        assert_eq!(calls.get(), 1);
    }

    fn typed_eval_error_shape(value: &JsonValue) -> bool {
        value.get("kind").and_then(JsonValue::as_str) == Some("error")
            && value.get("name").and_then(JsonValue::as_str).is_some()
            && value.get("message").and_then(JsonValue::as_str).is_some()
    }

    fn load_typed_field_reference(field: &str) -> TypedFieldReferenceBaseline {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/typed-hooks")
            .join(format!("{field}-reference.json"));
        let bytes = fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        parse_typed_field_reference_bytes(&bytes).unwrap_or_else(|error| panic!("parse {field} reference: {error}"))
    }

    fn assert_field_reference_matches_evaluator(field: &str) {
        let baseline = load_typed_field_reference(field);
        assert_eq!(baseline.field, field);
        assert!(!baseline.cases.is_empty());
        assert_eq!(baseline.catalog.hooks.len(), baseline.expected.len());
        let mut compared = 0usize;
        let mut errors = 0usize;
        for (hook_id, expected_values) in &baseline.expected {
            let entry = baseline.catalog.hooks.get(hook_id).expect("catalog hook");
            assert_eq!(entry.source_field, field);
            assert_eq!(expected_values.len(), baseline.cases.len());
            for (index, case) in baseline.cases.iter().enumerate() {
                let expected = &expected_values[index];
                match evaluate_typed_hook_json(entry.descriptor.get().expect("lazy ir"), &case.args) {
                    Ok(actual) => {
                        assert!(
                            !typed_eval_error_shape(expected),
                            "hook {hook_id} case {} succeeded in Rust but the JS typed eval recorded an error: {expected}",
                            case.id
                        );
                        assert_eq!(actual, *expected, "hook {hook_id}, case {}", case.id);
                        compared += 1;
                    },
                    Err(error) => {
                        assert!(
                            typed_eval_error_shape(expected),
                            "hook {hook_id} case {} failed in Rust ({error}) but JS typed eval succeeded: {expected}",
                            case.id
                        );
                        errors += 1;
                    },
                }
            }
        }
        assert!(compared > 0, "{field} reference compared no successful rows");
        let _ = errors;
    }

    #[test]
    fn checked_in_typed_field_references_match_native_evaluator() {
        for field in ["getQueryTerm", "script", "postProcess", "filterTemplateSuggestions"] {
            assert_field_reference_matches_evaluator(field);
        }
    }

    #[test]
    fn typed_field_reference_schema_rejects_unknown_fields() {
        let mut value = serde_json::from_slice::<JsonValue>(
            &fs::read(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/typed-hooks/getQueryTerm-reference.json"),
            )
            .expect("getQueryTerm field reference"),
        )
        .expect("json");
        value["unexpected"] = json!(true);
        assert!(parse_typed_field_reference(&value).is_err());
        value.as_object_mut().expect("object").remove("unexpected");
        value["field"] = json!("custom");
        assert!(parse_typed_field_reference(&value).is_err());
    }

    #[test]
    fn suggestions_from_typed_json_treats_null_as_empty() {
        assert_eq!(
            suggestions_from_typed_json(&JsonValue::Null).expect("null"),
            Vec::<Suggestion>::new()
        );
        assert!(suggestions_from_typed_json(&json!({"name": "x"})).is_err());
    }

    #[test]
    fn typed_post_process_evaluator_matches_js_host_suggestion_shape() {
        let descriptor = parse_typed_hook_ir(&json!({
            "version": IR_VERSION,
            "kind": IR_KIND,
            "sourceField": "postProcess",
            "resultType": "suggestion-array",
            "params": [
                {"index": 0, "type": "string"},
                {"index": 1, "type": "string-array"}
            ],
            "expr": {
                "op": "array",
                "items": [
                    {
                        "op": "object",
                        "fields": [
                            {"key": "name", "value": {"op": "arg", "index": 0}},
                            {"key": "type", "value": {"op": "string", "value": "arg"}}
                        ]
                    }
                ]
            }
        }))
        .expect("postProcess descriptor");
        let rows = evaluate_typed_post_process(&descriptor, "main", &["git".into()]).expect("postProcess");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "main");
        assert_eq!(rows[0].kind, "arg");
    }

    #[derive(Debug, Deserialize)]
    struct TypedBaselineDescriptors {
        counts: BTreeMap<String, TypedBaselineCount>,
        descriptors: BTreeMap<String, JsonValue>,
    }

    #[derive(Debug, Deserialize)]
    struct TypedBaselineCount {
        total: u64,
        ok: u64,
    }

    fn expected_baseline_json(expected: &crate::hook_baseline::Expected) -> Option<JsonValue> {
        match expected {
            crate::hook_baseline::Expected::Suggestions { value } => serde_json::to_value(value).ok(),
            crate::hook_baseline::Expected::String { value } => Some(JsonValue::String(value.clone())),
            crate::hook_baseline::Expected::Bool { value } => Some(JsonValue::Bool(*value)),
            crate::hook_baseline::Expected::Argv { value } => serde_json::to_value(value).ok(),
            crate::hook_baseline::Expected::Spec { value } => Some(decode_baseline_undefined(value.clone())),
            crate::hook_baseline::Expected::Error { .. } | crate::hook_baseline::Expected::Timeout { .. } => None,
        }
    }

    fn decode_baseline_undefined(value: JsonValue) -> JsonValue {
        match value {
            JsonValue::Object(fields)
                if fields.len() == 1 && fields.get("kind").and_then(JsonValue::as_str) == Some("undefined") =>
            {
                JsonValue::Null
            },
            JsonValue::Object(fields) => JsonValue::Object(
                fields
                    .into_iter()
                    .map(|(key, child)| (key, decode_baseline_undefined(child)))
                    .collect(),
            ),
            JsonValue::Array(items) => JsonValue::Array(items.into_iter().map(decode_baseline_undefined).collect()),
            other => other,
        }
    }

    #[test]
    fn array_map_callbacks_keep_outer_array_pushes() {
        let descriptor = parse_typed_hook_ir(&json!({
            "version": IR_VERSION,
            "kind": IR_KIND,
            "sourceField": "postProcess",
            "resultType": "suggestion-array",
            "params": [{"index": 0, "type": "string"}, {"index": 1, "type": "string-array"}],
            "expr": {
                "op": "let",
                "name": "a",
                "value": {"op": "array", "items": []},
                "body": {
                    "op": "seq",
                    "items": [
                        {
                            "op": "array-map",
                            "value": {
                                "op": "string-split",
                                "value": {"op": "arg", "index": 0},
                                "separator": {"op": "string", "value": "\n"}
                            },
                            "fn": {
                                "op": "lambda",
                                "params": ["s"],
                                "body": {
                                    "op": "if",
                                    "condition": {"op": "truthy", "value": {"op": "var", "name": "s"}},
                                    "then": {
                                        "op": "array-push",
                                        "name": "a",
                                        "item": {
                                            "op": "object",
                                            "fields": [{"key": "name", "value": {"op": "var", "name": "s"}}]
                                        }
                                    },
                                    "else": {"op": "null"}
                                }
                            }
                        },
                        {"op": "var", "name": "a"}
                    ]
                }
            }
        }))
        .expect("foreach descriptor");
        assert_eq!(
            evaluate_typed_hook_json(&descriptor, &[json!("main\nfeature\n"), json!([])]).expect("eval"),
            json!([{ "name": "main" }, { "name": "feature" }])
        );
    }

    #[test]
    fn return_inside_try_aborts_the_fallback_empty_array() {
        let descriptor = parse_typed_hook_ir(&json!({
            "version": IR_VERSION,
            "kind": IR_KIND,
            "sourceField": "postProcess",
            "resultType": "suggestion-array",
            "params": [{"index": 0, "type": "string"}, {"index": 1, "type": "string-array"}],
            "expr": {
                "op": "seq",
                "items": [
                    {
                        "op": "try",
                        "body": {
                            "op": "return",
                            "value": {
                                "op": "array",
                                "items": [{
                                    "op": "object",
                                    "fields": [{"key": "name", "value": {"op": "string", "value": "build"}}]
                                }]
                            }
                        },
                        "catch": {"op": "array", "items": []}
                    },
                    {"op": "array", "items": []}
                ]
            }
        }))
        .expect("try return descriptor");
        assert_eq!(
            evaluate_typed_hook_json(&descriptor, &[json!("{}"), json!([])]).expect("eval"),
            json!([{ "name": "build" }])
        );
    }

    #[test]
    fn json_parse_object_entries_keep_source_key_order() {
        let descriptor = parse_typed_hook_ir(&json!({
            "version": IR_VERSION,
            "kind": IR_KIND,
            "sourceField": "postProcess",
            "resultType": "suggestion-array",
            "params": [{"index": 0, "type": "string"}, {"index": 1, "type": "string-array"}],
            "expr": {
                "op": "array-map",
                "value": {
                    "op": "object-entries",
                    "value": {"op": "json-parse", "value": {"op": "arg", "index": 0}}
                },
                "fn": {
                    "op": "lambda",
                    "params": ["entry"],
                    "body": {
                        "op": "object",
                        "fields": [{
                            "key": "name",
                            "value": {
                                "op": "array-index",
                                "value": {"op": "var", "name": "entry"},
                                "index": {"op": "integer", "value": 0}
                            }
                        }]
                    }
                }
            }
        }))
        .expect("object-entries descriptor");
        assert_eq!(
            evaluate_typed_hook_json(&descriptor, &[json!("{\"build\":1,\"b\":2,\"format\":3}"), json!([])])
                .expect("eval"),
            json!([{ "name": "build" }, { "name": "b" }, { "name": "format" }])
        );
    }

    #[test]
    fn js_slice_range_keeps_negative_end() {
        assert_eq!(
            js_slice_range(&Utf16String::from_str("main"), 0, -1)
                .expect("slice")
                .to_string_lossy(),
            "mai"
        );
        assert_eq!(
            js_slice_range(&Utf16String::from_str("not json"), 0, -1)
                .expect("slice")
                .to_string_lossy(),
            "not jso"
        );
        assert_eq!(
            js_substring(&Utf16String::from_str("main"), 0, -1)
                .expect("substring")
                .to_string_lossy(),
            ""
        );
    }

    /// `emit-typed-hook-descriptors` takes the spec-pair publication lock, so
    /// two tests running it at once fail on the lock instead of on a real
    /// difference. Emit once per test binary and share the catalog.
    fn typed_baseline_descriptors() -> &'static TypedBaselineDescriptors {
        static CATALOG: std::sync::OnceLock<TypedBaselineDescriptors> = std::sync::OnceLock::new();
        CATALOG.get_or_init(|| {
            let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
            let script = repo.join("scripts/emit-typed-hook-descriptors.mjs");
            let output = std::process::Command::new("node")
                .arg(&script)
                .current_dir(&repo)
                .output()
                .expect("emit typed hook descriptors");
            assert!(
                output.status.success(),
                "emit-typed-hook-descriptors failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(&output.stdout).expect("typed baseline descriptor catalog")
        })
    }

    #[test]
    fn typed_hook_baseline_parity() {
        let catalog = typed_baseline_descriptors();
        let require = |field: &str, minimum: u64, exact_total: u64| {
            let count = catalog.counts.get(field).expect(field);
            assert_eq!(count.total, exact_total, "{field} unique-body total");
            assert!(
                count.ok >= minimum,
                "{field} typed compile {ok}/{total} is below {minimum}",
                ok = count.ok,
                total = count.total
            );
        };
        require("postProcess", 330, 376);
        require("script", 31, 31);
        require("filterTemplateSuggestions", 5, 5);
        require("trigger", 30, 30);
        require("getQueryTerm", 16, 16);

        let adapter_keys = registered_adapter_keys();
        let baselines = crate::hook_baseline::load_all().expect("T1.2 baselines");
        let mut compared = 0usize;
        let mut failures = Vec::new();
        for baseline in &baselines {
            if matches!(
                baseline.field.as_str(),
                "custom" | "alias" | "loadSpec" | "generateSpec"
            ) {
                continue;
            }
            let key = format!("{}:{}", baseline.field, baseline.body_sha256);
            if adapter_keys.contains(&key) {
                continue;
            }
            let Some(descriptor_json) = catalog.descriptors.get(&key) else {
                continue;
            };
            let descriptor = match parse_typed_hook_ir(descriptor_json) {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    failures.push(format!("{key}: parse {error}"));
                    continue;
                },
            };
            for case in &baseline.cases {
                let Some(expected) = expected_baseline_json(&case.expected) else {
                    continue;
                };
                match evaluate_typed_hook_json(&descriptor, &case.args) {
                    Ok(actual) if actual == expected => compared += 1,
                    Ok(actual) => failures.push(format!("{key} {} actual={actual} expected={expected}", case.id)),
                    Err(error) => failures.push(format!("{key} {} eval {error}", case.id)),
                }
            }
        }
        assert!(
            compared >= 330,
            "typed baseline parity compared {compared} cases; failures: {}",
            failures.iter().take(12).cloned().collect::<Vec<_>>().join(" | ")
        );
        assert!(
            failures.is_empty(),
            "typed baseline parity mismatches ({}): {}",
            failures.len(),
            failures.iter().take(20).cloned().collect::<Vec<_>>().join(" | ")
        );
    }

    fn registered_adapter_keys() -> BTreeSet<String> {
        let path = crate::native_adapter_catalog_path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let catalog: JsonValue = serde_json::from_str(&text).unwrap_or(JsonValue::Null);
        catalog
            .get("adapters")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                Some(format!(
                    "{}:{}",
                    row.get("field")?.as_str()?,
                    row.get("bodySha256")?.as_str()?
                ))
            })
            .collect()
    }

    fn mock_typed_exec(
        rules: &[crate::hook_baseline::ExecRule],
    ) -> impl Fn(TypedExecRequest) -> TypedHookResult<TypedExecResult> + '_ {
        move |request| {
            let env_is_default = request.env.is_empty();
            for rule in rules {
                let command = rule.command.as_deref().unwrap_or("");
                if command.is_empty() {
                    continue;
                }
                let args = rule.args.clone().unwrap_or_default();
                if request.command == command
                    && request.args == args
                    && request.cwd.is_none()
                    && env_is_default
                    && request.timeout_ms.is_none()
                {
                    return Ok(TypedExecResult {
                        stdout: rule.stdout.clone().unwrap_or_default(),
                        stderr: rule.stderr.clone().unwrap_or_default(),
                        status: rule.status.unwrap_or(0),
                    });
                }
            }
            Err(TypedHookError::throw("UnmockedCommand", "unmocked command"))
        }
    }

    #[test]
    fn typed_effect_baseline_parity() {
        let catalog = typed_baseline_descriptors();
        let adapter_keys = registered_adapter_keys();
        let baselines = crate::hook_baseline::load_all().expect("T1.2 baselines");
        let mut compared = 0usize;
        let mut seen = 0usize;
        let mut failures = Vec::new();
        for baseline in &baselines {
            if !matches!(
                baseline.field.as_str(),
                "custom" | "alias" | "loadSpec" | "generateSpec"
            ) {
                continue;
            }
            let key = format!("{}:{}", baseline.field, baseline.body_sha256);
            if adapter_keys.contains(&key) {
                continue;
            }
            let Some(descriptor_json) = catalog.descriptors.get(&key) else {
                continue;
            };
            seen += 1;
            let descriptor = match parse_typed_hook_ir(descriptor_json) {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    failures.push(format!("{key}: parse {error}"));
                    continue;
                },
            };
            for case in &baseline.cases {
                let Some(expected) = expected_baseline_json(&case.expected) else {
                    continue;
                };
                let context = TypedHookContext::from_baseline(&case.context);
                let exec = mock_typed_exec(&case.exec);
                match evaluate_typed_effect_json(&descriptor, &case.args, &context, &exec, None) {
                    Ok(actual) if actual == expected => compared += 1,
                    Ok(actual) => failures.push(format!("{key} {} actual={actual} expected={expected}", case.id)),
                    Err(error) => failures.push(format!("{key} {} eval {error}", case.id)),
                }
            }
        }
        assert!(
            seen >= 4,
            "typed effect catalog should include compiled custom/alias/loadSpec/generateSpec bodies"
        );
        assert!(
            failures.is_empty(),
            "typed effect baseline mismatches ({}): {}",
            failures.len(),
            failures.iter().take(20).cloned().collect::<Vec<_>>().join(" | ")
        );
        assert!(
            compared > 0,
            "typed effect baseline parity compared {compared} cases across {seen} bodies"
        );
    }
}

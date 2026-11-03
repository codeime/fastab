//! Headless completion engine.
//!
//! Bundled Fig specs are compiled to static JSON IR at build time. Lookup and
//! generators run in Rust.

mod cancellation;
mod cobra;
mod diagnostics;
#[cfg(test)]
mod engine_golden;
mod fig_spec;
mod filegen;
mod generate;
mod history;
mod hook_backend;
#[cfg(test)]
mod hook_baseline;
mod hook_cache;
mod hook_types;
mod ir;
mod lookup;
mod native_adapters;
mod process;
mod public_ai;
mod query;
mod rank;
mod runtime;
mod shell_history;
mod snapshot;
mod spec_pair;
mod typed_hook;
mod versioned;
mod worker;

pub use cancellation::CompletionCancelled;
pub use diagnostics::{
    CacheMapDiagnostics, EngineClientDiagnostics, EngineDiagnostics, GeneratorSessionDiagnostics, HistoryDiagnostics,
    HookCatalogDiagnostics, HookDiagnostics, HostResourceDiagnostics, RegistryDiagnostics, RequestDiagnostics,
    WorkerResourceDiagnostics,
};
pub use ir::{ArgSpec, Builtin, OptionSpec, Registry, Spec, Template};
pub use lookup::{completion_buffer, current_command_slice, tokenize};
pub use native_adapters::{dump_native_adapter_catalog, native_adapter_catalog_path};
pub use public_ai::{PublicAiCandidate, PublicAiContext};
pub use rank::{ACCEPTANCE_STATE_KEY, AcceptanceIndex};
pub use runtime::{CompleteRequest, CompleteResult, CurrentArg, Engine, Suggestion, ranking_root_command};
pub use spec_pair::public_ai_baseline_ok;
pub use worker::{
    CompletionTask, EngineClient, EngineClientOptions, SessionId, default_specs_dir, engine_attempt_timeout,
    ui_completion_deadline,
};

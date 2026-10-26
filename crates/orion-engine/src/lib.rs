//! The engine layer: what a workflow may call, how the engine is assembled
//! from stored rows, and how one channel's run is driven.
//!
//! F45: this file used to carry all three of those plus the glob matcher, the
//! rollout arithmetic and the lock accessors in one 468-line module. They are
//! now three:
//!
//! - [`handlers`] — the function-name vocabulary and the handler construction
//!   behind Orion's own entries.
//! - [`loader`] — stored channels + workflows → the engine's workflow set,
//!   dataflow-rs 4's verdict deciding which load (I182),
//!   with the per-channel quarantine.
//! - [`runner`] — the trace-capture policy every transport shares.
//! - [`observer`] — always-on per-task timing, including the sync built-ins no
//!   host can otherwise reach.
//! - [`kit`] — what every dataflow-rs 4 function shares: the error mapping's
//!   readers and field types, the connector shell, inert dependencies.
//!
//! Everything public is re-exported here, so `crate::…` paths are
//! unchanged.

/// Where the engine records the **code** of each failed task, for workflows
/// that must branch on *why* a step failed (#280).
///
/// `metadata`, not `data` or `temp_data`, and that placement is the point:
/// `metadata` is stripped from the trace-read projection and never reaches the
/// sync response body, whereas `temp_data` would ride out through the
/// persisted trace's `result_json` and `data` would go straight to the caller.
///
/// The `_orion_` prefix follows the reserved namespace (`_orion_call_depth`,
/// `_orion_call_chain`) rather than the bare `metadata.errors` dataflow-rs
/// would otherwise default to — that name is both a plausible caller-supplied
/// key and outside the namespace Orion documents as its own.
///
/// **The prefix is a convention, not an enforced namespace.** A caller can put
/// `_orion_call_depth` in an envelope today and nothing strips it, so this key
/// is force-cleared at every ingress — see `build_request_metadata` — and
/// reset on `channel_call`, where the child would
/// otherwise inherit and report the parent's failures as its own.
///
/// Records carry `{workflow_id, task_id, code, status}` and **never a
/// message**: see the note in `docs/src/reference/errors.md`.
pub const ERROR_CONTEXT_PATH: &str = "metadata._orion_errors";

/// The bare key under `metadata`, for the ingress stamping sites.
pub const ERROR_CONTEXT_KEY: &str = "_orion_errors";

/// [`ERROR_CONTEXT_PATH`] as dataflow-rs 4's error context, keeping its
/// default number of records: the one place Orion configures it. Every
/// dataflow-rs 4 engine Orion builds takes it
/// (`EngineConfig::default().error_context(ERROR_CONTEXT.clone())`), and
/// `channel_call` strips its key from the metadata it hands a target.
pub static ERROR_CONTEXT: std::sync::LazyLock<dataflow_rs::run::ErrorContext> =
    std::sync::LazyLock::new(|| {
        dataflow_rs::run::ErrorContext::new(
            ERROR_CONTEXT_PATH
                .parse()
                .expect("the error context's path is a path"),
            dataflow_rs::run::ErrorContext::DEFAULT_LIMIT,
        )
        .expect("the error context's path is below none of the engine's own slots")
    });

/// Clear the engine-owned error records from a metadata object.
///
/// Lives beside the constant so a new ingress has one call to make rather than
/// a three-line idiom to copy — and so widening this to the whole `_orion_`
/// prefix later is a one-line change here instead of an audit of call sites.
pub fn clear_error_context(metadata: &mut serde_json::Value) {
    if let Some(map) = metadata.as_object_mut() {
        map.remove(ERROR_CONTEXT_KEY);
    }
}

/// The metadata key describing what started a scheduled run.
///
/// Lives here beside [`VARS_KEY`] and [`ERROR_CONTEXT_KEY`] rather than in
/// `cron`, because it is a *reserved metadata key* and this is where the
/// reserved namespace is declared — and because the offline metadata validator
/// in `engine::utils` has to know the name, which it could not if the name
/// lived a layer above.
///
/// Platform-owned: the cron worker stamps it from the occurrence being run, and
/// nothing else may supply it. See `cron::metadata` for the object's shape.
pub const TRIGGER_KEY: &str = "trigger";

/// The metadata key verified `jwt` claims are stamped under, as
/// `{"claims": …}`.
///
/// Platform-reserved: the guard chain writes it only when a token verified,
/// and the HTTP ingress strips any caller-supplied value first, so a workflow
/// never reads an identity the caller wrote.
pub const AUTH_KEY: &str = "auth";

/// The metadata key allowlisted request cookies are stamped under (#270).
///
/// Platform-reserved: stamped from the channel's `cookies_to_metadata`
/// allowlist, and stripped when there is none so a caller cannot forge cookie
/// values via the request envelope.
pub const COOKIES_KEY: &str = "cookies";

/// The metadata key verified OAuth2 grants are stamped under (#307).
///
/// Platform-reserved: stamped by the `oauth2_login` callback guard after state
/// verification and code exchange, and stripped at ingress so a caller cannot
/// pre-seed an access token or claims via the request envelope.
pub const OAUTH_KEY: &str = "oauth";

/// The metadata key normalized OAuth2/OIDC identities are stamped under (#355).
///
/// Platform-reserved: stamped by the `oauth2_login` callback guard and stripped
/// at ingress so a caller cannot pre-seed an identity.
pub const IDENTITY_KEY: &str = "identity";

/// Strip all caller-supplied keys that belong to the platform's reserved
/// ingress metadata set (`_orion_errors`, `cookies`, `oauth`, `identity`,
/// `auth`, `trigger`).
///
/// Centralizing this list here ensures every HTTP/envelope ingress strips the
/// exact same set of platform-stamped keys before guards or workflows read
/// `metadata`. Note that `_orion_call_depth` and `_orion_call_chain` are
/// intentionally not stripped at HTTP ingress because entry-level cycle/depth
/// refusal is explicitly exercised over HTTP in integration tests.
pub fn strip_caller_reserved_metadata(metadata: &mut serde_json::Value) {
    if let Some(map) = metadata.as_object_mut() {
        for key in [
            ERROR_CONTEXT_KEY,
            COOKIES_KEY,
            OAUTH_KEY,
            IDENTITY_KEY,
            AUTH_KEY,
            TRIGGER_KEY,
        ] {
            map.remove(key);
        }
    }
}

/// The metadata key the `[vars]` config section is stamped under.
///
/// Platform-reserved, in the same sense as `channel` and `cookies`: an
/// operator declares the values and every ingress stamps them, overwriting
/// whatever the caller sent. Envelope mode merges caller-supplied `metadata`
/// wholesale, so without that a request could forge the topic prefix its own
/// run publishes to.
pub const VARS_KEY: &str = "vars";

/// Force `metadata.vars` to the instance's declared vars.
///
/// `None` — the instance declares no vars — *removes* the key rather than
/// writing an empty object. Both halves matter: the removal is what makes the
/// key unforgeable on an instance that declares nothing, and leaving the key
/// absent means a workflow reading `metadata.vars.x` sees the same missing
/// value whether the section is empty or the entry is.
///
/// Vars are stamped rather than held beside the message on purpose. They are
/// deployment configuration, so they *should* appear in the trace — an
/// operator asking "which topic did this run publish to?" is asking to see
/// them. That is the whole distinction between this and
/// [`secrets`], which the engine holds precisely so it
/// cannot record them.
///
/// A non-object `metadata` is replaced by an object rather than skipped when
/// there are vars to stamp. Every ingress builds metadata differently — the
/// HTTP route normalises the caller's value first, cron builds its own, and
/// the admin test endpoint's field defaults to `null` when the caller omits it
/// — so a helper that silently did nothing for `null` made "which ingress?"
/// decide whether `metadata.vars` exists. It reads as absent either way to a
/// caller who sent no metadata, and the workflow sees the same object it would
/// have seen through the data route.
pub fn stamp_vars(metadata: &mut serde_json::Value, vars: Option<&serde_json::Value>) {
    match vars {
        Some(values) => {
            if !metadata.is_object() {
                *metadata = serde_json::Value::Object(serde_json::Map::new());
            }
            if let Some(map) = metadata.as_object_mut() {
                map.insert(VARS_KEY.to_string(), values.clone());
            }
        }
        None => {
            if let Some(map) = metadata.as_object_mut() {
                map.remove(VARS_KEY);
            }
        }
    }
}

pub mod connector;
pub mod jwt;
pub mod query;

// The leaf crates under their `crate::` names, so the engine's paths read as
// they did inside `orion-server`.
pub(crate) use orion_common::{
    config, crypto, errors, http_body, metrics, text, trace_context, version,
};
pub(crate) use orion_storage as storage;

pub mod budget;
mod error;
pub mod execute;
pub mod functions;
pub mod handlers;
pub mod intake;
pub mod kit;
pub mod loader;
pub mod observer;
pub mod offline;
pub mod operators;
pub mod profile;
pub mod refs;
pub mod retry;
pub mod runner;
pub mod secrets;
pub mod steps;
pub mod utils;

pub use budget::{RunBudgetLimits, RunBudgetSnapshot, SharedRunAccountant};
pub use error::{ErrorClass, HTTP_ERROR, HandlerError, HttpFailure, http_error, http_status};
pub use execute::{ExecOpts, Execution, RunOutcome, execute_on};
pub use functions::registry::{
    CatalogueEntry, FieldSpec, FunctionEntry, FunctionRegistry, PluginBinding,
};
pub use functions::schema::ConnectorRule;
pub use handlers::{
    HandlerDeps, NamedFunction, is_reserved_function_name, register_functions,
    register_plugin_functions, serving_registry,
};
pub use loader::{
    ChannelLoadIssue, EngineLoad, IssueNotes, LoadedWorkflows, NoIssueNotes,
    build_engine_workflows, compile_row, filter_channels, name_v3_spellings, parse_options,
    quarantines, serving_engine, serving_engine_with_policy, workflow_json,
};
pub use observer::MetricsObserver;
pub use refs::{
    ConnectorFacts, ConnectorRef, RefProblem, channel_call_targets, check_connector_refs,
    connector_refs,
};
pub use runner::TraceCapture;
pub use secrets::ResolvedSecrets;
pub use steps::{MAX_STEP_DEPTH, is_group, leaf_tasks, walk_steps};

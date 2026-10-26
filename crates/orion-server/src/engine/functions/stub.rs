//! Canned responses for connector-backed tasks, so a workflow can be run
//! offline.
//!
//! `orion-server dry-run` used to build its engine with an empty custom-function
//! map, which meant every connector-backed function — `http_call`,
//! `channel_call`, `db_read`, `db_write`, `data_query`, `data_write`,
//! `cache_read`, `cache_write`, `mongo_read`, `publish_kafka` — failed with
//! `Connector '…' not found`. Only a pure mapping workflow could be dry-run,
//! which is not many of them.
//!
//! The alternative was `POST /definitions/test`, whose own documentation
//! warns that it runs against **live** connectors: it will POST to real
//! webhooks, write to real databases and publish to real topics. So the
//! offline option could not run a realistic workflow and the realistic option
//! had real side effects.
//!
//! [`StubLayer`] closes that. An offline run registers every real function
//! over inert dependencies, so the workflow is compiled exactly as a node
//! compiles it, and the layer answers each connector-backed call from a file
//! before the function is reached: nothing is dialled, and no pool opens.
//!
//! ## Stub file format
//!
//! ```json
//! {
//!   "http_call":    { "crm": { "name": "Ada" } },
//!   "data_query":   { "orders-db": [ { "id": 1, "total": 10 } ] },
//!   "channel_call": { "inventory-check": { "in_stock": true } },
//!   "db_write":     { "*": { "rows_affected": 1 } }
//! }
//! ```
//!
//! The outer key is the function name and the inner key is the *target* — the
//! task's `connector`, or its `channel` for `channel_call`. `"*"` matches any
//! target.
//!
//! ## What is never stubbed
//!
//! The self-contained functions (`SELF_CONTAINED` below) always run for real. A
//! plugin function always runs for real too — its component is loaded from a
//! `--plugin-dir` — and is refused by name when the component is absent.

use std::collections::HashMap;
use std::sync::Arc;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::function::{BoxFuture, CallLayer, Next};
use dataflow_rs::{Ctx, Outcome};
use serde_json::Value;

use crate::engine::kit;
use crate::engine::{ErrorClass, HandlerError};

/// Wildcard target: matches whatever connector or channel the task names.
const ANY_TARGET: &str = "*";

/// Parsed stub file: `function -> target -> response`.
pub type StubTable = HashMap<String, HashMap<String, Value>>;

/// The functions that execute for real offline rather than through a stub, and
/// are therefore absent from the [`CallLog`].
///
/// They are deterministic and make no egress, so stubbing them would only hide
/// behaviour — and their inputs can carry inline key material and passwords,
/// which a recorded payload has no business holding. The boundary this draws is
/// also the honest description of the log: it records the calls that *would
/// have left the process*.
const SELF_CONTAINED: [&str; 3] = ["crypto", "jwt_sign", "jwt_verify"];

/// One connector call a stubbed run would have made.
///
/// `input` is the task's input as the function would have read it: every
/// field its config resolves, resolved against the message, and a field that
/// reads a secret recorded as `null` — so it is what *would be sent*, not
/// what was typed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecordedCall {
    /// Position in the run, across all functions.
    pub seq: usize,
    /// The task that made the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub function: &'static str,
    /// The key that matched in the stub table: the task's `connector` (or
    /// `connection`), or its `channel` for `channel_call`.
    ///
    /// Spelled `stub_target` rather than `target` because `target` is already a
    /// `data_write` input field naming the entity written to — a recorded
    /// `data_write` would otherwise carry `target` twice, at two depths, with
    /// two meanings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stub_target: Option<String>,
    pub input: Value,
}

/// The calls a stubbed run made, in order.
///
/// A `std::sync::Mutex` rather than a `tokio` one: nothing awaits while the
/// lock is held.
#[derive(Debug, Default)]
pub struct CallLog(std::sync::Mutex<Vec<RecordedCall>>);

impl CallLog {
    pub fn new() -> Self {
        Self::default()
    }

    fn record(
        &self,
        task_id: &str,
        function: &'static str,
        stub_target: Option<String>,
        input: Value,
    ) {
        let mut calls = self.0.lock().expect("call log mutex poisoned");
        let seq = calls.len();
        calls.push(RecordedCall {
            seq,
            task_id: Some(task_id.to_string()),
            function,
            stub_target,
            input,
        });
    }

    /// Every recorded call, in execution order.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.0.lock().expect("call log mutex poisoned").clone()
    }

    /// The log grouped by function name, in execution order within each group —
    /// the shape a case's `calls.<function>[i]` path and its `expect_calls`
    /// block both read.
    pub fn grouped(&self) -> serde_json::Map<String, Value> {
        let mut grouped: serde_json::Map<String, Value> = serde_json::Map::new();
        for call in self.0.lock().expect("call log mutex poisoned").iter() {
            let entry = grouped
                .entry(call.function.to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(list) = entry.as_array_mut()
                && let Ok(value) = serde_json::to_value(call)
            {
                list.push(value);
            }
        }
        grouped
    }
}

/// The documents an offline run publishes, by the name that addresses each one.
///
/// This is the vocabulary a `*.case.json` `expect` path is rooted at and the
/// key set `dry-run` prints, and it is deliberately **one** list feeding
/// **one** builder ([`run_documents`]). When the names lived in a const and the
/// two output sites built their own object literals, a root could exist for the
/// path validator and not for the lookup — and the result of that is a path
/// that validates, resolves to nothing, and (because an expected `null` matches
/// an absent path) *passes*. That is the exact failure the rooting rule was
/// added to remove, so it must not be reachable one level up.
///
/// Adding a root is one edit here plus one arm in [`run_documents`];
/// `every_root_is_published` fails if only one of the two happens.
pub const RUN_DOCUMENTS: [&str; 5] = ["data", "metadata", "temp_data", "calls", "audit_trail"];

/// Whether a dotted path names one of [`RUN_DOCUMENTS`].
///
/// Reads the first segment, stopping at a `.` or a `[` so `calls[0]` and
/// `calls.db_write` are both rooted at `calls`.
pub fn is_rooted(path: &str) -> bool {
    RUN_DOCUMENTS.contains(&path_root(path))
}

/// The first segment of an `expect` path: `audit_trail` for
/// `audit_trail[1].changes`.
pub fn path_root(path: &str) -> &str {
    path.split(['.', '[']).next().unwrap_or(path)
}

/// Everything an offline run leaves behind, keyed by [`RUN_DOCUMENTS`].
///
/// `calls` is grouped by function — the shape `calls.<function>[i]` addresses —
/// on both surfaces. Chronological order across functions is not lost by the
/// grouping: every record carries its `seq`.
pub fn run_documents(
    message: &dataflow_rs::Message,
    log: &CallLog,
) -> serde_json::Map<String, Value> {
    let context = message.context();
    let mut docs = serde_json::Map::new();
    docs.insert("data".to_string(), Value::from(context.data()));
    docs.insert("metadata".to_string(), Value::from(context.metadata()));
    docs.insert("temp_data".to_string(), Value::from(context.temp_data()));
    docs.insert("calls".to_string(), Value::Object(log.grouped()));
    docs.insert(
        "audit_trail".to_string(),
        serde_json::to_value(message.audit()).unwrap_or(Value::Null),
    );
    docs
}

/// Parse a stub file, rejecting shapes that would silently stub nothing.
///
/// A stub file is written by hand under time pressure, and the two easy
/// mistakes — naming a function that does not exist, and putting the response
/// where the target map belongs — both produce a file that parses fine and
/// matches nothing. Catching them here beats a dry run that reports a missing
/// stub for a stub you are looking at.
pub fn parse_stubs(raw: &str, path: &str) -> Result<StubTable, String> {
    let root: Value =
        serde_json::from_str(raw).map_err(|e| format!("'{path}' is not valid JSON: {e}"))?;
    parse_stub_value(&root, path)
}

/// [`parse_stubs`] over an already-parsed value.
///
/// Every check below runs against the parsed tree, so a caller that already has
/// one — the `test` runner, whose cases carry their stubs inline — has no
/// reason to serialize it back to a string just to have it parsed again.
pub fn parse_stub_value(root: &Value, path: &str) -> Result<StubTable, String> {
    let Some(object) = root.as_object() else {
        return Err(format!(
            "'{path}' must be a JSON object mapping function names to \
             {{target: response}} maps"
        ));
    };

    let mut table = StubTable::new();
    for (function, targets) in object {
        if !stubbable_functions().any(|name| name == function) {
            return Err(format!(
                "'{path}' stubs '{function}', which is not a connector-backed function. \
                 Stubbable functions: {}",
                stubbable_functions().collect::<Vec<_>>().join(", ")
            ));
        }
        let Some(map) = targets.as_object() else {
            return Err(format!(
                "'{path}': the value of '{function}' must be a map of \
                 connector (or channel) name to response, e.g. \
                 {{\"{function}\": {{\"my-connector\": <response>}}}} — or use \
                 \"{ANY_TARGET}\" to match any target"
            ));
        };
        table.insert(
            function.clone(),
            map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        );
    }
    Ok(table)
}

/// Find the canned response, or explain which stub to add.
///
/// Failing loudly is the point. A stub file that under-covers a workflow would
/// otherwise produce a dry run reporting success while several tasks quietly
/// did nothing — worse than having no stubs at all, because it looks like a
/// passing test.
fn resolve<'a>(
    stubs: &'a StubTable,
    function: &str,
    target: Option<&str>,
) -> Result<&'a Value, HandlerError> {
    stubs
        .get(function)
        .and_then(|targets| {
            target
                .and_then(|t| targets.get(t))
                .or_else(|| targets.get(ANY_TARGET))
        })
        .ok_or_else(|| {
            let named = target.unwrap_or("<none>");
            HandlerError::new(
                ErrorClass::Backend,
                format!(
                    "dry-run: no stub for '{function}' on target '{named}'. Add it to the \
                     stubs file: {{\"{function}\": {{\"{named}\": <response>}}}}"
                ),
            )
        })
}

/// Every function a stub file may name: the Orion functions, which are the
/// entries a real engine dispatches to code in this binary. An engine
/// built-in needs no stub, and a plugin function is executed for real by the
/// loaded component rather than stood in for.
pub fn stubbable_functions() -> impl Iterator<Item = &'static str> {
    crate::engine::FunctionRegistry::builtin()
        .entries()
        .filter(|e| e.source == super::schema::Source::Orion)
        .map(crate::engine::FunctionEntry::label)
}

/// Answers every connector-backed call of an offline run from a [`StubTable`],
/// recording each into a [`CallLog`], before the function is reached.
///
/// A layer rather than a stub function per name: the workflow compiles
/// against the real functions, so its configs are parsed and checked as a
/// node checks them, and a call the layer answers writes where the real
/// function would — through the call's own primary output. Every other call
/// (the self-contained functions and a plugin's) passes through to the real
/// function.
pub struct StubLayer {
    stubs: StubTable,
    log: Arc<CallLog>,
    stubbed: Vec<&'static str>,
}

impl StubLayer {
    /// Stubs every connector-backed function.
    pub fn new(stubs: StubTable, log: Arc<CallLog>) -> Self {
        let stubbed = stubbable_functions()
            .filter(|name| !SELF_CONTAINED.contains(name))
            .collect();
        Self {
            stubs,
            log,
            stubbed,
        }
    }

    /// The canned answer to one call, written at its output.
    fn answer(&self, ctx: &mut Ctx<'_>, function: &'static str) -> dataflow_rs::Result<Outcome> {
        // The destination first, since it can be an expression like any other
        // field and a call whose output names no path fails before it is made.
        let output = match ctx.primary_output() {
            Some(out) => Some(kit::output_path(ctx, out, on_null(function))?.into_owned()),
            None => None,
        };
        let input = Value::from(&ctx.input()?);
        let target_key = if function == super::channel_call::NAME {
            "channel"
        } else if input.get("connector").is_some() {
            "connector"
        } else {
            "connection"
        };
        let target = input
            .get(target_key)
            .and_then(Value::as_str)
            .map(str::to_string);
        let namespaces = input
            .get("namespaces")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        // Recorded before the stub is resolved, so a call that fails for want
        // of a stub still appears in the log — that is the run you most want
        // to see the payload of.
        self.log
            .record(ctx.info().task_id, function, target.clone(), input);
        let response = match resolve(&self.stubs, function, target.as_deref()) {
            Ok(response) => response.clone(),
            // Offline there is no response cache to invalidate, so the call
            // has no effect to stand in for and needs no stub: it reaches no
            // store. A stub, when given, still wins.
            Err(_) if function == "cache_invalidate" => {
                serde_json::json!({ "namespaces": namespaces, "stores": 0 })
            }
            Err(e) => return Err(e.into()),
        };
        if let Some(path) = output {
            ctx.set(&path, OwnedDataValue::from(&response))?;
        }
        Ok(Outcome::Ok)
    }
}

/// Where a computed output that evaluates to `null` writes, as the real
/// function decides it: nowhere for the two whose output is optional, the
/// data root for the rest.
fn on_null(function: &str) -> Option<&'static dataflow_rs::Path> {
    match function {
        "http_call" | "channel_call" => None,
        _ => Some(&kit::DATA),
    }
}

impl CallLayer for StubLayer {
    fn call<'a>(
        &'a self,
        ctx: &'a mut Ctx<'_>,
        next: Next<'a>,
    ) -> BoxFuture<'a, dataflow_rs::Result<Outcome>> {
        let called = ctx.info().function;
        let Some(function) = self.stubbed.iter().copied().find(|name| *name == called) else {
            return next.run(ctx);
        };
        Box::pin(async move { self.answer(ctx, function) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_well_formed_stub_file_parses() {
        let table = parse_stubs(
            r#"{ "http_call": { "crm": {"name": "Ada"} }, "db_read": { "*": [] } }"#,
            "stubs.json",
        )
        .expect("parses");
        assert_eq!(table["http_call"]["crm"], json!({"name": "Ada"}));
        assert_eq!(table["db_read"]["*"], json!([]));
    }

    /// A function name that is not stubbable is a typo, not a request to stub
    /// something else.
    #[test]
    fn an_unknown_function_is_refused() {
        let err = parse_stubs(r#"{ "htp_call": { "crm": {} } }"#, "stubs.json")
            .expect_err("unknown function must be refused");
        assert!(err.contains("htp_call"), "{err}");
        assert!(
            err.contains("http_call"),
            "the error lists the real names: {err}"
        );
    }

    /// The response written where the target map belongs parses as JSON and
    /// matches nothing, so it is caught here rather than at run time.
    #[test]
    fn a_response_in_place_of_a_target_map_is_refused() {
        let err = parse_stubs(r#"{ "http_call": [1, 2] }"#, "stubs.json")
            .expect_err("a non-object target map must be refused");
        assert!(err.contains("http_call"), "{err}");
    }

    /// One workflow compiled on the offline registry and run through a
    /// [`StubLayer`] over `stubs`, from `data`: the message, the calls and
    /// the run's error.
    async fn run(
        tasks: Value,
        stubs: StubTable,
        data: Value,
    ) -> (
        dataflow_rs::Message,
        Vec<RecordedCall>,
        Option<dataflow_rs::Error>,
    ) {
        use crate::engine::offline::{OfflineRun, run_single, single_engine};

        let offline = crate::engine::kit::OfflineDeps::new();
        let compiler = crate::engine::serving_registry(offline.deps())
            .compiler(dataflow_rs::program::CompileOptions::default());
        let workflow = json!({
            "id": "w", "name": "w", "channel": "c", "status": "active",
            "condition": true, "tasks": tasks,
        });
        let engine = single_engine(&compiler, &workflow).expect("the workflow compiles");
        let log = Arc::new(CallLog::new());
        let layers: Vec<Arc<dyn CallLayer>> = vec![Arc::new(StubLayer::new(stubs, log.clone()))];
        let result = run_single(
            &engine,
            &data,
            &json!({}),
            OfflineRun {
                trace: None,
                max_snapshot_bytes: 0,
                capture_changes: false,
                layers: &layers,
                observer: None,
            },
        )
        .await;
        (result.message, log.calls(), result.error)
    }

    fn stub(function: &str, target: &str, response: Value) -> StubTable {
        let mut stubs = StubTable::new();
        stubs.insert(
            function.to_string(),
            [(target.to_string(), response)].into_iter().collect(),
        );
        stubs
    }

    fn data(message: &dataflow_rs::Message) -> Value {
        Value::from(message.context().data())
    }

    /// Every connector-backed function is answered by the layer; the
    /// self-contained ones are not.
    #[test]
    fn the_layer_stubs_every_connector_backed_function_and_only_those() {
        let layer = StubLayer::new(StubTable::new(), Arc::new(CallLog::new()));
        let stubbable: Vec<&str> = stubbable_functions().collect();
        assert_eq!(stubbable.len(), 15, "every Orion function is stubbable");
        for name in &stubbable {
            assert_eq!(
                layer.stubbed.contains(name),
                !SELF_CONTAINED.contains(name),
                "{name}"
            );
        }
    }

    /// A stubbed answer lands where the real function writes: the task's
    /// `output`, its default when it names none, and nowhere for a function
    /// whose output is optional and unset.
    #[tokio::test]
    async fn a_stubbed_answer_is_written_where_the_real_function_writes() {
        let rows = json!([{"id": 1}]);
        let (message, _, error) = run(
            json!([{"id": "read", "name": "read", "function": {"name": "db_read",
                "input": {"connector": "orders", "query": "SELECT 1", "output": "data.rows"}}}]),
            stub("db_read", "orders", rows.clone()),
            json!({}),
        )
        .await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(data(&message)["rows"], rows);

        let (message, _, error) = run(
            json!([{"id": "read", "name": "read", "function": {"name": "cache_read",
                "input": {"connector": "cache", "key": "k"}}}]),
            stub("cache_read", "cache", json!({"count": 1})),
            json!({}),
        )
        .await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(
            data(&message)["count"],
            1,
            "an omitted output is the data root"
        );

        let (message, calls, error) = run(
            json!([{"id": "forget", "name": "forget", "function": {"name": "cache_delete",
                "input": {"connector": "cache", "keys": ["k"]}}}]),
            stub("cache_delete", "cache", json!({"deleted": true})),
            json!({"kept": true}),
        )
        .await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            data(&message),
            json!({"kept": true}),
            "an unset optional output writes nothing"
        );
    }

    /// The path validator and the lookup must agree about what a root is.
    ///
    /// A root the validator accepts but the builder does not publish resolves
    /// to nothing, and an expected `null` matches nothing — so the assertion
    /// passes. That is the silent pass the rooting rule exists to remove.
    #[test]
    fn every_root_is_published() {
        let message = dataflow_rs::Message::from_json(&json!({}));
        let docs = run_documents(&message, &CallLog::new());

        for root in RUN_DOCUMENTS {
            assert!(
                docs.contains_key(root),
                "'{root}' is accepted by is_rooted but never published — a case \
                 path under it would resolve to <absent> and pass on null"
            );
            assert!(is_rooted(root), "'{root}' must validate as a root");
        }
        assert_eq!(
            docs.len(),
            RUN_DOCUMENTS.len(),
            "run_documents publishes {:?}, RUN_DOCUMENTS declares {:?}",
            docs.keys().collect::<Vec<_>>(),
            RUN_DOCUMENTS
        );
        assert!(!is_rooted("order.flagged"), "a bare path is not rooted");
        assert!(!is_rooted("dat.order"), "a typo'd root is not rooted");
        assert!(
            is_rooted("calls[0].input"),
            "a bracket ends the root segment"
        );
    }

    /// The recorded payload must be what *would be sent*, not what was typed —
    /// otherwise the log asserts the workflow file back at you.
    #[tokio::test]
    async fn a_recorded_call_carries_the_resolved_payload() {
        let (_, calls, error) = run(
            json!([{"id": "persist", "name": "Persist", "function": {"name": "db_write",
                "input": {"connector": "orders", "query": "UPDATE t SET a = :a WHERE id = :id",
                    "params": {"a": 1, "id": {"var": "data.sid"}}}}}]),
            stub("db_write", "orders", json!({"rows_affected": 1})),
            json!({"sid": "sess-1"}),
        )
        .await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(calls.len(), 1, "one write, one record");
        assert_eq!(calls[0].function, "db_write");
        assert_eq!(calls[0].stub_target.as_deref(), Some("orders"));
        assert_eq!(calls[0].task_id.as_deref(), Some("persist"));
        assert_eq!(
            calls[0].input["params"]["id"], "sess-1",
            "a resolvable field is folded against the message"
        );
        assert_eq!(calls[0].input["connector"], "orders");
    }

    /// A call that fails for want of a stub is the run you most want the
    /// payload of, so recording happens before the lookup.
    #[tokio::test]
    async fn an_unstubbed_call_is_still_recorded() {
        let (message, calls, _) = run(
            json!([{"id": "read", "name": "Read", "function": {"name": "db_read",
                "input": {"connector": "orders", "query": "SELECT 1", "output": "data.rows"}}}]),
            StubTable::new(),
            json!({}),
        )
        .await;
        assert_eq!(calls.len(), 1, "the call is recorded even with no stub");
        assert_eq!(calls[0].input["query"], "SELECT 1");
        assert_eq!(calls[0].task_id.as_deref(), Some("read"));
        let errors: Vec<&str> = message
            .errors()
            .iter()
            .map(|e| e.message.as_str())
            .collect();
        assert!(
            errors
                .iter()
                .any(|m| m.contains("no stub for 'db_read' on target 'orders'")),
            "the missing stub is named: {errors:?}"
        );
    }

    /// Their inputs can carry inline key material, and they run for real
    /// rather than through a stub — so they are outside what the log
    /// describes.
    #[tokio::test]
    async fn a_self_contained_function_runs_for_real_and_is_not_recorded() {
        let (message, calls, error) = run(
            json!([{"id": "digest", "name": "Digest", "function": {"name": "crypto",
                "input": {"op": "hash", "data": "abc", "output": "data.digest"}}}]),
            StubTable::new(),
            json!({}),
        )
        .await;
        assert!(error.is_none(), "{error:?}");
        assert!(message.errors().is_empty(), "{:?}", message.errors());
        assert!(data(&message)["digest"].is_string(), "crypto ran for real");
        assert!(
            calls.is_empty(),
            "a self-contained function must stay out of the call log"
        );
    }

    /// A typed config dispatches through the layer and its answer reaches the
    /// task's output.
    #[tokio::test]
    async fn a_typed_function_dispatches_through_the_layer() {
        let (message, calls, error) = run(
            json!([{"id": "call", "name": "Call", "function": {"name": "http_call",
                "input": {"connector": "crm", "method": "GET", "path": "/x",
                    "output": "data.customer"}}}]),
            stub("http_call", "crm", json!({"name": "Ada"})),
            json!({}),
        )
        .await;
        assert!(error.is_none(), "{error:?}");
        assert!(message.errors().is_empty(), "{:?}", message.errors());
        assert_eq!(data(&message)["customer"], json!({"name": "Ada"}));
        assert_eq!(calls[0].input["path"], "/x");
    }
}

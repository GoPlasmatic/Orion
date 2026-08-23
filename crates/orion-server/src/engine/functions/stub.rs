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
//! The alternative was `POST /workflows/{id}/test`, whose own documentation
//! warns that it runs against **live** connectors: it will POST to real
//! webhooks, write to real databases and publish to real topics. So the
//! offline option could not run a realistic workflow and the realistic option
//! had real side effects.
//!
//! A stub handler closes that. It deliberately does **not** reconstruct
//! [`HandlerDeps`](crate::engine::HandlerDeps) — those are `AppState`-level
//! pools and registries with no business existing in a CLI — it just answers
//! from a file.
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

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::connector_helpers::apply_output;

/// Wildcard target: matches whatever connector or channel the task names.
const ANY_TARGET: &str = "*";

/// Parsed stub file: `function -> target -> response`.
pub type StubTable = HashMap<String, HashMap<String, Value>>;

/// The three functions that execute for real offline rather than through a
/// stub, and are therefore not recorded in the [`CallLog`].
///
/// They are deterministic and make no egress, so stubbing them would only hide
/// behaviour — and their inputs can carry inline key material and passwords,
/// which a recorded payload has no business holding. The boundary this draws is
/// also the honest description of the log: it records the calls that *would
/// have left the process*.
pub const UNSTUBBED_FUNCTIONS: [&str; 3] = ["crypto", "jwt_sign", "jwt_verify"];

/// Whether an offline run records calls to this function.
pub fn is_recorded_function(function: &str) -> bool {
    crate::engine::CUSTOM_HANDLER_FUNCTIONS.contains(&function)
        && !UNSTUBBED_FUNCTIONS.contains(&function)
}

/// One connector call a stubbed run would have made.
///
/// `input` is the task's authored input with every field the real handler folds
/// `{"var": ..}` nodes in (per the schema registry's `resolvable` flag) already
/// resolved — so it is what *would be sent*, not what was typed. That is the
/// whole point: a `mongo_write` whose `document` carries an unresolvable
/// JSONLogic node shows the node here, where an assertion can see it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecordedCall {
    /// Position in the run, across all functions.
    pub seq: usize,
    /// The task that made the call. Correlated from the execution trace after
    /// the run — `TaskContext` does not carry it — so it is `None` when the
    /// correlation cannot be made confidently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub function: &'static str,
    /// The key that matched in the stub table: the task's `connector`, or its
    /// `channel` for `channel_call`.
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
/// Shared by every stub handler through an `Arc`. A `std::sync::Mutex` rather
/// than a `tokio` one: nothing awaits while the lock is held, and the handlers
/// need it from a `&self` async method.
#[derive(Debug, Default)]
pub struct CallLog(std::sync::Mutex<Vec<RecordedCall>>);

impl CallLog {
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, function: &'static str, stub_target: Option<String>, input: Value) {
        let mut calls = self.0.lock().expect("call log mutex poisoned");
        let seq = calls.len();
        calls.push(RecordedCall {
            seq,
            task_id: None,
            function,
            stub_target,
            input,
        });
    }

    /// Every recorded call, in execution order.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.0.lock().expect("call log mutex poisoned").clone()
    }

    /// Attach task ids by walking the executed steps of `trace` in step order
    /// and pairing each recorded-function task with the next recorded call.
    ///
    /// `task_functions` maps a task id to the function it names, which only the
    /// caller (holding the workflow JSON) can build.
    ///
    /// Best-effort by construction: if the run died partway and the two
    /// sequences no longer line up, the surplus calls keep `task_id: None`
    /// rather than being given a wrong one. A missing label is a gap; a wrong
    /// one is a lie.
    pub fn correlate(
        &self,
        trace: &dataflow_rs::ExecutionTrace,
        task_functions: &HashMap<String, String>,
    ) {
        let mut calls = self.0.lock().expect("call log mutex poisoned");
        let mut next = 0usize;
        for step in &trace.steps {
            if !matches!(step.result, dataflow_rs::StepResult::Executed) {
                continue;
            }
            let Some(task_id) = step.task_id.as_deref() else {
                continue;
            };
            let Some(function) = task_functions.get(task_id) else {
                continue;
            };
            if !is_recorded_function(function) {
                continue;
            }
            let Some(call) = calls.get_mut(next) else {
                break;
            };
            // A desync means the pairing is no longer trustworthy for anything
            // after it either, so stop rather than mislabel the rest.
            if call.function != function {
                break;
            }
            call.task_id = Some(task_id.to_string());
            next += 1;
        }
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

/// A task's authored input with its resolvable fields folded against the
/// message, per the schema registry.
///
/// Non-resolvable fields (`connector`, `database`, `op`, an output path) are
/// left exactly as authored, because that is what the real handler reads.
fn resolved_input(function: &str, input: &Value, ctx: &TaskContext<'_>) -> Value {
    let Some(obj) = input.as_object() else {
        return input.clone();
    };
    Value::Object(
        obj.iter()
            .map(|(key, value)| {
                let value = if crate::engine::functions::schema::is_resolvable_field(function, key)
                {
                    super::connector_helpers::resolve_value(value, ctx)
                } else {
                    value.clone()
                };
                (key.clone(), value)
            })
            .collect(),
    )
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
        if !crate::engine::CUSTOM_HANDLER_FUNCTIONS.contains(&function.as_str()) {
            return Err(format!(
                "'{path}' stubs '{function}', which is not a connector-backed function. \
                 Stubbable functions: {}",
                crate::engine::CUSTOM_HANDLER_FUNCTIONS.join(", ")
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

/// The lookup every stub variant shares: find the canned response, or explain
/// which stub to add.
///
/// Failing loudly is the point. A stub file that under-covers a workflow would
/// otherwise produce a dry run reporting success while several tasks quietly
/// did nothing — worse than having no stubs at all, because it looks like a
/// passing test.
fn resolve<'a>(
    stubs: &'a StubTable,
    function: &str,
    target: Option<&str>,
) -> dataflow_rs::Result<&'a Value> {
    stubs
        .get(function)
        .and_then(|targets| {
            target
                .and_then(|t| targets.get(t))
                .or_else(|| targets.get(ANY_TARGET))
        })
        .ok_or_else(|| {
            let named = target.unwrap_or("<none>");
            DataflowError::function_execution(
                format!(
                    "dry-run: no stub for '{function}' on target '{named}'. Add it to the \
                     stubs file: {{\"{function}\": {{\"{named}\": <response>}}}}"
                ),
                None,
            )
        })
}

/// Stands in for one of the connector-backed functions whose input is plain
/// JSON — every one but the three with a typed config below.
///
/// The split is not a style choice: dataflow-rs precompiles each task's `input`
/// into the *registered* handler's `Input` type, so a stub declaring
/// `Input = Value` where the real handler declares a struct fails the whole run
/// with "Handler input type mismatch". The stub surface therefore has to mirror
/// the real one type for type.
pub struct StubHandler {
    /// The function this instance is registered under, for error messages.
    pub function: &'static str,
    pub stubs: Arc<StubTable>,
    pub log: Arc<CallLog>,
}

impl StubHandler {
    /// Where this task's result would be written — mirroring the real
    /// handlers exactly, because an offline run that writes to a different
    /// place than production reports the wrong verdict in both directions.
    ///
    /// Every function this generic stub serves resolves its destination via
    /// `extract_output_path`: the `output` field, defaulting to `"data"` (as
    /// their published schemas document). `cache_write` is the one that
    /// writes nothing — its stub exists only to keep the task from failing.
    /// `response_path` is deliberately not consulted: none of these
    /// functions' production handlers read it (the pre-1.0 spelling survives
    /// only where the real config declares it as an alias — `http_call` and
    /// `channel_call`, which have their own typed stubs).
    fn output_path(&self, input: &Value) -> Option<String> {
        if self.function == "cache_write" {
            return None;
        }
        Some(
            input
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or("data")
                .to_string(),
        )
    }
}

#[async_trait]
impl AsyncFunctionHandler for StubHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        let target = input.get("connector").and_then(Value::as_str);
        // Recorded before the stub is resolved, so a call that fails for want
        // of a stub still appears in the log — that is the run you most want to
        // see the payload of.
        self.log.record(
            self.function,
            target.map(str::to_string),
            resolved_input(self.function, input, ctx),
        );
        let response = resolve(&self.stubs, self.function, target)?.clone();
        if let Some(path) = self.output_path(input) {
            apply_output(ctx, &path, response);
        }
        Ok(TaskOutcome::Success)
    }
}

/// `http_call`'s stub. Its input is dataflow-rs's `HttpCallConfig`, whose
/// destination field is `response_path` (with `output` as an accepted alias).
pub struct HttpCallStub {
    pub stubs: Arc<StubTable>,
    pub log: Arc<CallLog>,
}

#[async_trait]
impl AsyncFunctionHandler for HttpCallStub {
    type Input = dataflow_rs::engine::functions::HttpCallConfig;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Self::Input,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // Upstream's own resolvers, so the record carries the real templated
        // path and body rather than an Orion-side approximation of them.
        self.log.record(
            "http_call",
            Some(input.connector.clone()),
            serde_json::json!({
                "connector": input.connector,
                "method": input.method.as_str(),
                "path": input.resolve_path(ctx)?,
                "body": input.resolve_body(ctx)?,
            }),
        );
        let response = resolve(&self.stubs, "http_call", Some(&input.connector))?.clone();
        if let Some(ref path) = input.response_path {
            apply_output(ctx, path, response);
        }
        Ok(TaskOutcome::Success)
    }
}

/// `publish_kafka`'s stub. A publish has no result to write, so the stub's only
/// job is to let the task succeed without a broker.
pub struct PublishKafkaStub {
    pub stubs: Arc<StubTable>,
    pub log: Arc<CallLog>,
}

#[async_trait]
impl AsyncFunctionHandler for PublishKafkaStub {
    type Input = dataflow_rs::engine::functions::PublishKafkaConfig;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Self::Input,
    ) -> dataflow_rs::Result<TaskOutcome> {
        self.log.record(
            "publish_kafka",
            Some(input.connector.clone()),
            serde_json::json!({
                "connector": input.connector,
                "topic": input.topic,
                "key": input.resolve_key(ctx)?,
                "value": input.resolve_value(ctx)?,
            }),
        );
        resolve(&self.stubs, "publish_kafka", Some(&input.connector))?;
        Ok(TaskOutcome::Success)
    }
}

/// `channel_call`'s stub. Targets the *channel*, not a connector.
///
/// Stubbing it is what lets a composed workflow be dry-run at all: the real
/// handler reaches into the live engine for another channel's workflow, which
/// a CLI has no way to supply.
///
/// Unlike [`HttpCallStub`] and [`PublishKafkaStub`], the typed `Input` is not
/// forced here — `channel_call` is a `FunctionConfig::Custom`, so its input is
/// parsed by whichever handler is registered and a `Value` stub could not
/// mismatch. Mirroring the real type anyway means the dry-run engine rejects a
/// malformed `channel_call` input at build, the same as the serving engine
/// does, rather than accepting it and stubbing past the mistake.
pub struct ChannelCallStub {
    pub stubs: Arc<StubTable>,
    pub log: Arc<CallLog>,
}

#[async_trait]
impl AsyncFunctionHandler for ChannelCallStub {
    type Input = super::channel_call::ChannelCallInput;

    /// The same two templates the real handler compiles
    /// (`ChannelCallHandler::compile_input`). Without this the stub holds
    /// uncompiled `Template`s, and reading one to record the call is an
    /// "eval before compile" error rather than the payload.
    fn compile_input(
        input: &mut Self::Input,
        c: &dataflow_rs::engine::functions::TemplateCompiler,
    ) -> dataflow_rs::Result<()> {
        if let Some(t) = input.channel_logic.as_mut() {
            t.compile(c, "channel_call.channel_logic")?;
        }
        if let Some(t) = input.data_logic.as_mut() {
            t.compile(c, "channel_call.data_logic")?;
        }
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Self::Input,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // `channel_logic` computes the target per message and is not resolvable
        // without running it, so a dynamic call falls back to the `"*"` entry.
        let target = (!input.channel.is_empty()).then_some(input.channel.as_str());
        self.log.record(
            "channel_call",
            target.map(str::to_string),
            serde_json::json!({
                "channel": target,
                "data": match input.data_logic {
                    Some(ref logic) => Some(logic.eval_into::<Value>(ctx)?),
                    None => input.data.clone(),
                },
            }),
        );
        let response = resolve(&self.stubs, "channel_call", target)?.clone();
        if let Some(ref path) = input.output {
            apply_output(ctx, path, response);
        }
        Ok(TaskOutcome::Success)
    }
}

/// Register a stub for every connector-backed function.
///
/// Every name in `CUSTOM_HANDLER_FUNCTIONS` gets one, whether or not the stub
/// file mentions it: a workflow calling an unstubbed function should be told
/// which stub to add, and that only happens if a handler is there to say so.
/// Registering none would reproduce the `FunctionNotFound` the old dry run gave.
/// `every_stubbable_function_gets_a_handler` pins the coverage.
pub fn build_stub_functions(
    stubs: StubTable,
) -> HashMap<String, dataflow_rs::BoxedFunctionHandler> {
    build_stub_functions_with_log(stubs, Arc::new(CallLog::new()))
}

/// [`build_stub_functions`] writing every call it answers into `log`.
///
/// The two exist separately so a caller that does not read the log — anything
/// but the `test` runner and `dry-run` — does not have to construct one.
pub fn build_stub_functions_with_log(
    stubs: StubTable,
    log: Arc<CallLog>,
) -> HashMap<String, dataflow_rs::BoxedFunctionHandler> {
    let stubs = Arc::new(stubs);
    let mut out: HashMap<String, dataflow_rs::BoxedFunctionHandler> = HashMap::new();

    for &function in crate::engine::CUSTOM_HANDLER_FUNCTIONS {
        let handler: dataflow_rs::BoxedFunctionHandler = match function {
            "http_call" => Box::new(HttpCallStub {
                stubs: stubs.clone(),
                log: log.clone(),
            }),
            "publish_kafka" => Box::new(PublishKafkaStub {
                stubs: stubs.clone(),
                log: log.clone(),
            }),
            "channel_call" => Box::new(ChannelCallStub {
                stubs: stubs.clone(),
                log: log.clone(),
            }),
            // Deterministic and offline — dry-run executes it for real, so a
            // stub would only hide behavior. (An env:// key still resolves
            // from the local environment; a missing variable is an honest
            // failure, not a gap in stubbing.)
            "crypto" => Box::new(super::crypto::CryptoHandler),
            // Same reasoning: deterministic given their inputs (jwt_verify
            // with a JWKS does fetch keys — an offline dry-run without them
            // fails honestly rather than fabricating a verification).
            "jwt_sign" => Box::new(super::jwt_sign::JwtSignHandler),
            "jwt_verify" => Box::new(super::jwt_verify::JwtVerifyHandler),
            _ => Box::new(StubHandler {
                function,
                stubs: stubs.clone(),
                log: log.clone(),
            }),
        };
        out.insert(function.to_string(), handler);
    }
    out
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

    #[test]
    fn every_stubbable_function_gets_a_handler() {
        let fns = build_stub_functions(StubTable::new());
        for name in crate::engine::CUSTOM_HANDLER_FUNCTIONS {
            assert!(fns.contains_key(*name), "no stub handler for {name}");
        }
    }

    #[test]
    fn the_output_path_follows_each_functions_convention() {
        let stub = |function| StubHandler {
            function,
            stubs: Arc::new(StubTable::new()),
            log: Arc::new(CallLog::new()),
        };

        let read = stub("db_read");
        assert_eq!(
            read.output_path(&json!({"output": "data.x"})),
            Some("data.x".to_string())
        );
        // An omitted `output` defaults to the data root, exactly like the
        // real handler (`extract_output_path`) and its published schema —
        // a stub writing nothing here fails workflows that pass in
        // production.
        assert_eq!(read.output_path(&json!({})), Some("data".to_string()));
        // `response_path` is not a spelling the real connector handlers
        // read; honoring it here passed workflows offline that production
        // does not run that way.
        assert_eq!(
            read.output_path(&json!({"response_path": "data.y"})),
            Some("data".to_string())
        );

        for function in [
            "cache_read",
            "mongo_read",
            "db_write",
            "data_query",
            "data_write",
        ] {
            assert_eq!(
                stub(function).output_path(&json!({})),
                Some("data".to_string()),
                "{function} defaults to the data root"
            );
        }

        // The one generic-stubbed function whose real handler writes nothing.
        assert_eq!(stub("cache_write").output_path(&json!({})), None);
    }

    /// The recorded payload must be what *would be sent*, not what was typed —
    /// otherwise the log asserts the workflow file back at you.
    #[tokio::test]
    async fn a_recorded_call_carries_the_resolved_payload() {
        let workflow: dataflow_rs::Workflow = serde_json::from_value(json!({
            "id": "w", "name": "w", "condition": true,
            "tasks": [{
                "id": "persist", "name": "Persist",
                "function": {"name": "mongo_write", "input": {
                    "connector": "sessions-db", "database": "app",
                    "collection": "sessions", "op": "update_one",
                    "filter": {"_id": {"var": "data.sid"}},
                    // Not folded — only `{"var": ..}` is. Recording it verbatim
                    // is exactly what makes the bug assertable.
                    "update": {"$set": {"generation": {"if": [true, 2, 1]}}}
                }}
            }]
        }))
        .expect("workflow parses");

        let mut stubs = StubTable::new();
        stubs.insert(
            "mongo_write".to_string(),
            [("sessions-db".to_string(), json!({"modified": 1}))]
                .into_iter()
                .collect(),
        );

        let log = Arc::new(CallLog::new());
        let engine = dataflow_rs::Engine::new(
            vec![workflow],
            build_stub_functions_with_log(stubs, log.clone()),
        )
        .expect("engine builds");
        // Seed `data` directly: a case's `input` is the *payload*, and a real
        // workflow copies it into `data` with a first `parse_json`/`map` task.
        // This test is about the recorder, not that copy.
        let mut message = dataflow_rs::Message::builder()
            .payload_json(&json!({"sid": "sess-1"}))
            .data_json(&json!({"sid": "sess-1"}))
            .build();
        engine.process_message(&mut message).await.expect("runs");

        let calls = log.calls();
        assert_eq!(calls.len(), 1, "one write, one record");
        assert_eq!(calls[0].function, "mongo_write");
        assert_eq!(calls[0].stub_target.as_deref(), Some("sessions-db"));
        assert_eq!(
            calls[0].input["filter"]["_id"], "sess-1",
            "a resolvable field is folded against the message"
        );
        assert_eq!(
            calls[0].input["collection"], "sessions",
            "a literal field is left as authored"
        );
        assert_eq!(
            calls[0].input["update"]["$set"]["generation"],
            json!({"if": [true, 2, 1]}),
            "an unresolvable JSONLogic node is recorded verbatim — which is how \
             a case sees that Mongo would have stored the object, not the number"
        );
    }

    /// A call that fails for want of a stub is the run you most want the
    /// payload of, so recording happens before the lookup.
    #[tokio::test]
    async fn an_unstubbed_call_is_still_recorded() {
        let workflow: dataflow_rs::Workflow = serde_json::from_value(json!({
            "id": "w", "name": "w", "condition": true,
            "tasks": [{
                "id": "read", "name": "Read",
                "function": {"name": "db_read", "input": {
                    "connector": "orders", "sql": "SELECT 1"}}
            }]
        }))
        .expect("workflow parses");

        let log = Arc::new(CallLog::new());
        let engine = dataflow_rs::Engine::new(
            vec![workflow],
            build_stub_functions_with_log(StubTable::new(), log.clone()),
        )
        .expect("engine builds");
        let mut message = dataflow_rs::Message::from_value(&json!({}));
        let _ = engine.process_message(&mut message).await;

        let calls = log.calls();
        assert_eq!(calls.len(), 1, "the call is recorded even with no stub");
        assert_eq!(calls[0].input["sql"], "SELECT 1");
    }

    /// Their inputs can carry inline key material, and they run for real rather
    /// than through a stub — so they are outside what the log describes.
    #[test]
    fn the_unstubbed_functions_are_not_recorded() {
        for function in UNSTUBBED_FUNCTIONS {
            assert!(
                !is_recorded_function(function),
                "{function} must stay out of the call log"
            );
        }
        assert!(is_recorded_function("mongo_write"));
        assert!(
            !is_recorded_function("map"),
            "a dataflow-rs built-in is not a connector call"
        );
    }

    /// A stub whose `Input` type does not match the real handler's fails the
    /// *run*, not the build — so this asserts by running.
    ///
    /// dataflow-rs precompiles each task's `input` into the registered
    /// handler's `Input` type. `http_call` and `publish_kafka` are typed
    /// `FunctionConfig` variants, so a `Value` stub standing in for either
    /// produces "Handler input type mismatch" at dispatch. Asserting only that
    /// a handler is *registered* would stay green through exactly that
    /// regression, which is what this test previously did.
    #[tokio::test]
    async fn a_typed_function_dispatches_through_its_stub() {
        let mut stubs = StubTable::new();
        stubs.insert(
            "http_call".to_string(),
            [("crm".to_string(), json!({"name": "Ada"}))]
                .into_iter()
                .collect(),
        );

        let workflow: dataflow_rs::Workflow = serde_json::from_value(json!({
            "id": "typed", "name": "typed", "condition": true,
            "tasks": [{
                "id": "call", "name": "Call",
                "function": {"name": "http_call", "input": {
                    "connector": "crm", "method": "GET", "path": "/x",
                    "output": "data.customer"}}
            }]
        }))
        .expect("workflow parses");

        let engine = dataflow_rs::Engine::new(vec![workflow], build_stub_functions(stubs))
            .expect("engine builds");
        let mut message = dataflow_rs::Message::from_value(&json!({}));
        engine
            .process_message(&mut message)
            .await
            .expect("a typed stub must dispatch, not mismatch");

        let out: Value = message.data().into();
        assert_eq!(
            out["customer"],
            json!({"name": "Ada"}),
            "the stubbed response must reach the task's output path"
        );
    }
}

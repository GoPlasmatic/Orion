use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::{Ctx, Expr, Function, Issue, Outcome};
use dataflow_rs_v3::engine::task_context::TaskContext;
use serde_json::{Map, Value};

use super::connector_handler::ConnectorHandler;
use super::connector_helpers::{
    ConnectorCall, require_op, resolve_required_str, resolve_required_str_list, to_connect_error,
    to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::{Scope, TemplatedInput};
use crate::connector::cache_backend::{CachePool, CachePurpose};
use crate::connector::{CacheConnectorConfig, ConnectorRegistry};
use crate::engine::HandlerError;
use crate::engine::kit::{self, read};
use dataflow_rs_v3::engine::error::DataflowError;

/// This function's name, for the free functions below (F48).
const NAME: &str = <CacheReadHandler as ConnectorHandler>::NAME;

/// Workflow function handler for reading values from a cache backend.
pub struct CacheReadHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl ConnectorHandler for CacheReadHandler {
    const NAME: &'static str = "cache_read";
    type Kind = crate::connector::kind::Cache;
    type Input = TemplatedInput;
    /// The key or keys, resolved against the message. `{"var": "data.id"}` is
    /// the whole point of a per-request cache lookup, so it has to be folded
    /// before the body takes `ctx` mutably.
    type Parsed = CacheReadKeys;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &Scope<'_, '_>,
    ) -> Result<Self::Parsed, crate::engine::HandlerError> {
        // Exactly one of the two; `validate_static_input` says so at authoring
        // time, and this is the same rule for a hand-built input.
        match (input.get("key").is_some(), input.get("keys").is_some()) {
            (true, false) => Ok(CacheReadKeys::One(resolve_required_str(
                input, "key", call.name, ctx,
            )?)),
            (false, true) => Ok(CacheReadKeys::Many(resolve_required_str_list(
                input, "keys", call.name, ctx, MAX_KEYS,
            )?)),
            (true, true) => Err(not_one_key_choice(call.name, true).into()),
            (false, false) => Err(not_one_key_choice(call.name, false).into()),
        }
    }

    fn gate(
        _keys: &CacheReadKeys,
        conn: &crate::connector::CacheConnectorConfig,
        connector: &str,
    ) -> Result<(), crate::engine::HandlerError> {
        // F22e: a cache connector can be made write-only in its config.
        Ok(require_op(conn.operations.read, "read", connector)?)
    }

    async fn run(
        &self,
        keys: CacheReadKeys,
        conn: &crate::connector::CacheConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<super::connector_handler::Produced, crate::engine::HandlerError> {
        let lookup = match &keys {
            CacheReadKeys::One(key) => Lookup::One(Cow::Borrowed(key)),
            CacheReadKeys::Many(keys) => Lookup::Many(Cow::Borrowed(keys)),
        };
        Ok(look_up(&self.cache_pool, call.connector, conn, &lookup)
            .await?
            .into())
    }
}

/// The refusal of a task naming both `key` and `keys`, or neither, in
/// 1.12's words: `both` says which. dataflow-rs 4's config refuses both
/// shapes at load, as 1.12's authoring did, so only a dataflow-rs 3 call
/// meets it.
fn not_one_key_choice(name: &str, both: bool) -> DataflowError {
    DataflowError::Validation(if both {
        format!("{name} takes 'key' or 'keys', not both")
    } else {
        format!("{name} requires 'key' or 'keys'")
    })
}

/// One lookup, borrowed from the keys either engine read.
enum Lookup<'k> {
    One(Cow<'k, str>),
    Many(Cow<'k, [String]>),
}

/// The lookup, against a resolved and gated cache connector: the value one
/// key holds, or an array of the values several hold, in the order asked,
/// `null` for a miss. Shared by the dataflow-rs 3 handler and the
/// dataflow-rs 4 function, so both read the same store the same way.
async fn look_up(
    pool: &CachePool,
    connector: &str,
    conn: &CacheConnectorConfig,
    lookup: &Lookup<'_>,
) -> Result<Value, HandlerError> {
    // Workflow-purpose namespace (S19) — the mirror of `cache_write`, so a
    // workflow reads exactly what workflows wrote and never the dedup store
    // or response cache.
    let backend = pool
        .get_backend(CachePurpose::Workflow, connector, conn)
        .await
        .map_err(to_connect_error)?;

    match lookup {
        Lookup::One(key) => {
            let value = backend.get(key).await.map_err(to_exec_error)?;
            Ok(decode(value))
        }
        // One `MGET`: a route reading two generation counters pays one
        // round trip, not two.
        Lookup::Many(keys) => {
            let values = backend.get_many(keys).await.map_err(to_exec_error)?;
            Ok(Value::Array(values.into_iter().map(decode).collect()))
        }
    }
}

/// I185's Defect: `keys` answers an array, which no context root takes
/// (break 21), so an `output` that is one fails every call.
const ROOT_OUTPUT: &str = "`cache_read` with `keys` writes an array of values, and a context \
    root takes only an object (break 21): set `output` to a path below it, such as \
    `data.values`";

/// `cache_read`'s input on dataflow-rs 4, in its field table's order (I179).
/// The connector is a literal, required at load (I183); `key` and `keys`
/// are expressions, read by `kit::read`'s twins of `resolve_required_str`
/// and `resolve_required_str_list` (I183), the bound of 1000 applied per
/// call, as 1.12 applied it. Its `check` is 1.12's authoring-time rule,
/// exactly one of `key` and `keys`.
#[derive(Debug, FunctionConfig)]
#[config(check = check)]
pub struct CacheReadConfig {
    /// The cache connector read from.
    connector: String,
    /// The key looked up.
    key: Option<Expr<OwnedDataValue>>,
    /// Several keys looked up in one round trip, answered as an array in
    /// the same order.
    keys: Option<Expr<OwnedDataValue>>,
    /// Where the value, or the array of values, is written.
    #[output(default = "data")]
    output: Output,
}

/// 1.12's authoring-time rule for `cache_read` ([`validate_static_input`],
/// by which create, update, import, validate and `lint` refused a task),
/// on the config at load: exactly one of `key` and `keys`, refused at the
/// field 1.12 named, in its words. Every call 1.12 made with either shape
/// failed, so no task that ran is refused.
fn check(cfg: &CacheReadConfig, cx: &mut CompileCx<'_>) {
    let mut input = Map::new();
    for (field, expr) in [("key", &cfg.key), ("keys", &cfg.keys)] {
        if let Some(expr) = expr {
            input.insert(field.to_string(), expr.raw().clone());
        }
    }
    super::mongo_common::report_static(validate_static_input(&input), cx);
}

/// `cache_read` as a dataflow-rs 4 function: the lookup `CacheReadHandler`
/// makes, read in the same order.
pub struct CacheReadFunction {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

impl Function for CacheReadFunction {
    type Config = CacheReadConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    /// I185: with `keys` set, an `output` that is a literal context root,
    /// the default `data` included, is a Defect at `output`. One `key`'s
    /// value may be an object, so a root is judged per call.
    fn prepare(&self, cfg: &mut CacheReadConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        match cfg.keys {
            Some(_) => kit::root_output_defect(&cfg.output, ROOT_OUTPUT),
            None => Ok(()),
        }
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &CacheReadConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, before the keys are read: an output that
        // names no path reads nothing.
        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
        let key = cfg.key.as_ref().map(|key| ctx.eval(key)).transpose()?;
        let keys = cfg.keys.as_ref().map(|keys| ctx.eval(keys)).transpose()?;
        let lookup = match (&key, &keys) {
            (Some(key), None) => Lookup::One(read::required_str(Some(key), "key", NAME)?),
            (None, Some(keys)) => Lookup::Many(Cow::Owned(read::required_str_list(
                Some(keys),
                "keys",
                NAME,
                MAX_KEYS,
            )?)),
            (key, _) => {
                return Err(HandlerError::from(not_one_key_choice(NAME, key.is_some())).into());
            }
        };
        let mut value = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let conn =
                kit::require_connector::<crate::connector::kind::Cache>(&config, &cfg.connector)?;
            // F22e: a cache connector can be made write-only in its config.
            kit::require_op(conn.operations.read, "read", &cfg.connector)?;
            value = Some(look_up(&self.cache_pool, &cfg.connector, conn, &lookup).await?);
            Ok(Outcome::Ok)
        })
        .await?;
        if let Some(value) = value {
            ctx.set(&output, OwnedDataValue::from(value))?;
        }
        Ok(Outcome::Ok)
    }
}

/// Most keys one `cache_read` or `cache_delete` may name.
pub(super) const MAX_KEYS: usize = 1000;

/// What a `cache_read` looks up: one `key`, answered as a value, or `keys`,
/// answered as an array in the same order.
pub enum CacheReadKeys {
    One(String),
    Many(Vec<String>),
}

/// A stored string back to the value it was written as.
///
/// `cache_write` JSON-encodes everything, so parsing is its exact inverse. The
/// raw-string fallback is kept deliberately: a key written by something other
/// than Orion may hold a bare string, and surfacing that as a string beats
/// failing the task. A miss is a result — `null` — not an absence of one.
fn decode(value: Option<String>) -> Value {
    match value {
        Some(v) => serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v)),
        None => Value::Null,
    }
}

// -- Authoring-time validation (shared with schema::validate_input) --

/// `key` and `keys` are each optional in the field table, because either one
/// does; exactly one of them is the rule.
pub(super) fn validate_static_input(
    obj: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    match (obj.contains_key("key"), obj.contains_key("keys")) {
        (true, true) => vec![(
            "keys",
            "INVALID",
            "cache_read takes 'key' (one value) or 'keys' (an array of values), not both"
                .to_string(),
        )],
        (false, false) => vec![(
            "key",
            "REQUIRED",
            "cache_read requires 'key', or 'keys' to read several at once".to_string(),
        )],
        _ => Vec::new(),
    }
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const CACHE_READ_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the cache connector to read from.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key",
        description: "Cache key to look up. JSONLogic: a literal, or an expression over the message. One of `key` and `keys` is required.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "keys",
        description: "Several keys to look up in one round trip (at most 1000). The result is an array in the same order, null for a miss. JSONLogic: an array of literals or expressions, or an expression evaluating to one.",
        kind: FieldKind::Array,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where the result is stored. Defaults to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_connector(read: bool) -> crate::connector::CacheConnectorConfig {
        crate::connector::CacheConnectorConfig {
            backend: "memory".to_string(),
            url: None,
            allow_private_urls: false,
            operations: crate::connector::CacheOperationGates { read, write: true },
        }
    }

    fn handler() -> CacheReadHandler {
        CacheReadHandler {
            cache_pool: Arc::new(CachePool::new(4, 60, 128)),
            registry: Arc::new(ConnectorRegistry::new(Default::default())),
        }
    }

    /// The seam the trait exists for.
    ///
    /// `run` receives a connector that is already resolved, already
    /// type-checked and already gated, so a handler body can be exercised
    /// in-process against a real backend — no registry entry, no engine, no
    /// workflow. The same shape is what would let the Mongo, Redis, SMTP and ES
    /// bodies out of their container-gated `#[ignore]`s, which is the reason to
    /// prefer the trait over a cheaper conformance fix.
    #[tokio::test]
    async fn the_run_seam_is_reachable_without_an_engine() {
        let h = handler();
        let datalogic = std::sync::Arc::new(dataflow_rs_v3::datalogic_rs::Engine::new());
        let mut message = dataflow_rs_v3::Message::from_value(&serde_json::json!({}));
        let mut ctx =
            dataflow_rs_v3::engine::task_context::TaskContext::new(&mut message, &datalogic);
        let call = ConnectorCall {
            name: CacheReadHandler::NAME,
            connector: "c",
            channel: "ch".to_string(),
            output: "data".to_string(),
        };

        let value = h
            .run(
                CacheReadKeys::One("absent-key".to_string()),
                &memory_connector(true),
                &call,
                &TemplatedInput::from(serde_json::json!({"connector": "c", "key": "absent-key"})),
                &mut ctx,
            )
            .await
            .expect("a miss is not an error");
        assert_eq!(
            value.value,
            Some(Value::Null),
            "a cache miss reads as null, not as a failure"
        );
    }

    /// The gate is the connector's answer rather than the backend's, which is
    /// why it is a separate method: it has to be decided before anything is
    /// dialled. It is also now callable on its own.
    #[test]
    fn a_write_only_connector_refuses_a_read() {
        let err = <CacheReadHandler as ConnectorHandler>::gate(
            &CacheReadKeys::One("k".to_string()),
            &memory_connector(false),
            "c",
        )
        .expect_err("a read must be refused when the gate is off");
        // The specific refusal is the operator-facing detail; `msg` is the
        // caller-safe half. Both must survive the conversion.
        assert_eq!(err.msg, "Request validation failed");
        let detail = err.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("operation 'read' is disabled"),
            "the refusal must name the gate it hit: {detail:?}"
        );
    }

    // -- dataflow-rs 4: the config and its check, I185's Defect, and twins
    // of the handler over an in-memory cache --

    use crate::engine::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::json;

    fn v3_handler(cache: &twin::Cache) -> dataflow_rs_v3::BoxedFunctionHandler {
        Box::new(super::super::connector_handler::Connector(
            CacheReadHandler {
                cache_pool: cache.pool.clone(),
                registry: cache.registry.clone(),
            },
        ))
    }

    fn function(cache: &twin::Cache) -> CacheReadFunction {
        CacheReadFunction {
            cache_pool: cache.pool.clone(),
            registry: cache.registry.clone(),
        }
    }

    /// One task through each engine, each against a cache of its own
    /// holding `seed`, its connector configured with `config`.
    async fn both(input: Value, data: Value, config: Value, seed: &[(&str, &str)]) -> (Seen, Seen) {
        let cache = twin::cache(config.clone(), seed).await;
        let v3 = twin::v3(NAME, v3_handler(&cache), input.clone(), data.clone()).await;
        let cache = twin::cache(config, seed).await;
        let v4 = twin::v4(NAME, function(&cache), input, data).await;
        (v3, v4)
    }

    fn compile(input: Value) -> Result<CacheReadConfig, Vec<dataflow_rs::Issue>> {
        testing::config_with::<CacheReadConfig>(&kit::test_exprs(), input)
    }

    fn rejected(input: Value) -> Vec<(String, IssueCode, String)> {
        compile(input)
            .expect_err("refused")
            .into_iter()
            .filter(|i| i.severity == Severity::Rejected)
            .map(|i| (i.path.to_string(), i.code, i.message.clone()))
            .collect()
    }

    /// Declaration order is the field table's (I179); `key` and `keys` are
    /// expressions, an authored `null` among them, as 1.12's presence check
    /// read it; `connector` is a required literal (I183); an unknown key is
    /// refused (break 2a); `output` defaults to `data`.
    #[test]
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = CacheReadConfig::fields().iter().map(|f| &*f.name).collect();
        let table: Vec<&str> = CACHE_READ_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

        let cfg = compile(json!({"connector": "c", "key": {"cat": ["user:", {"var": "data.id"}]}}))
            .expect("compiles");
        assert!(cfg.key.as_ref().expect("set").constant().is_none());
        assert!(cfg.keys.is_none());
        assert_eq!(
            cfg.output.constant().map(ToString::to_string).as_deref(),
            Some("data")
        );
        let cfg =
            compile(json!({"connector": "c", "keys": ["a", {"var": "data.k"}]})).expect("compiles");
        assert!(cfg.key.is_none() && cfg.keys.is_some());
        let cfg = compile(json!({"connector": "c", "key": null})).expect("compiles");
        assert!(
            cfg.key.is_some(),
            "an authored null is the key, refused per call"
        );

        let paths = |input: Value| -> Vec<String> {
            rejected(input).into_iter().map(|(path, ..)| path).collect()
        };
        assert_eq!(paths(json!({"key": "k"})), ["connector"]);
        assert_eq!(
            paths(json!({"connector": {"var": "data.c"}, "key": "k"})),
            ["connector"]
        );
        assert_eq!(
            paths(json!({"connector": "c", "key": "k", "ttl": 5})),
            ["ttl"]
        );
    }

    /// The check is 1.12's authoring rule, exactly one of `key` and `keys`:
    /// each refusal at the field and in the words `validate_static_input`
    /// gives, computed from it. 1.12 failed every call of either shape.
    #[test]
    fn the_check_is_validate_static_input() {
        for input in [
            json!({"connector": "c"}),
            json!({"connector": "c", "key": "k", "keys": ["k"]}),
            json!({"connector": "c", "key": null, "keys": null}),
            json!({"connector": "c", "key": {"var": "data.k"}, "keys": {"var": "data.ks"}}),
        ] {
            let expected: Vec<(String, IssueCode, String)> =
                validate_static_input(input.as_object().expect("an object"))
                    .into_iter()
                    .map(|(field, _, message)| (field.to_string(), IssueCode::InputParse, message))
                    .collect();
            assert_eq!(expected.len(), 1, "{input}");
            assert_eq!(rejected(input.clone()), expected, "{input}");
        }
        for input in [
            json!({"connector": "c", "key": "k"}),
            json!({"connector": "c", "keys": []}),
        ] {
            let object = input.as_object().expect("an object");
            assert!(validate_static_input(object).is_empty());
            compile(input).expect("one of the two compiles");
        }
    }

    /// I185: with `keys`, the result is an array, so a literal root
    /// `output`, the default `data` included, is a Defect at `output`,
    /// reported by `prepare`, which only a compile through the registry
    /// runs; a path below a root is not, and neither is one `key`, whose
    /// value may be an object. The call still runs, and an array's write at
    /// a root is refused (break 21), where 1.12 replaced the root.
    #[tokio::test]
    async fn keys_at_a_root_output_is_i185s_defect() {
        for (input, defect) in [
            (json!({"connector": "c", "keys": ["a"]}), true),
            (
                json!({"connector": "c", "keys": ["a"], "output": "data"}),
                true,
            ),
            (
                json!({"connector": "c", "keys": ["a"], "output": "temp_data"}),
                true,
            ),
            (
                json!({"connector": "c", "keys": ["a"], "output": null}),
                true,
            ),
            (
                json!({"connector": "c", "keys": ["a"], "output": "data.values"}),
                false,
            ),
            (
                json!({"connector": "c", "keys": ["a"], "output": {"cat": ["data", ".values"]}}),
                false,
            ),
            (json!({"connector": "c", "key": "a"}), false),
            (
                json!({"connector": "c", "key": "a", "output": "data"}),
                false,
            ),
        ] {
            let cache = twin::cache(json!({}), &[("a", r#"{"n":1}"#)]).await;
            let called = testing::call(function(&cache), input.clone())
                .exprs(kit::test_exprs())
                .name(NAME)
                .await;
            let defects: Vec<_> = called
                .issues
                .iter()
                .filter(|i| i.severity == Severity::Defect)
                .map(|i| (i.path.to_string(), i.code, i.message.as_str()))
                .collect();
            if defect {
                assert_eq!(
                    defects,
                    [("output".to_string(), IssueCode::InputParse, ROOT_OUTPUT)],
                    "{input}"
                );
                let error = called.outcome.expect_err("an array at a root is refused");
                assert_eq!(error.code(), "CONFIG_ERROR", "{input}");
            } else {
                assert!(defects.is_empty(), "{input}: {defects:?}");
                assert_eq!(called.outcome.as_ref().ok(), Some(&Outcome::Ok), "{input}");
            }
        }
    }

    /// `the_run_seam_is_reachable_without_an_engine` on v4: a miss is a
    /// result, `null`, not a failure.
    #[tokio::test]
    async fn a_miss_reads_as_null_on_v4() {
        let input = json!({"connector": "c", "key": "absent-key", "output": "data.cached"});
        let (v3, v4) = both(input, json!({}), json!({}), &[]).await;
        assert_eq!(v4, v3);
        assert_eq!(v4.expect("a miss is not an error"), json!({"cached": null}));
    }

    /// `a_write_only_connector_refuses_a_read` on v4: the gate's refusal,
    /// its caller-safe message and its detail, as 1.12's.
    #[tokio::test]
    async fn a_write_only_connector_refuses_a_read_on_v4() {
        let input = json!({"connector": "c", "key": "k", "output": "data.v"});
        let gated = json!({"operations": {"read": false, "write": true}});
        let (v3, v4) = both(input, json!({}), gated, &[("k", "1")]).await;
        assert_eq!(v4, v3);
        let failure = v4.expect_err("a read must be refused when the gate is off");
        let detail = failure.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("operation 'read' is disabled"),
            "{failure:?}"
        );
    }

    /// Every reading 1.12 made, on both engines: a computed key, a number
    /// and a boolean spelled out as keys, `keys` as an array of expressions
    /// or one expression, the decode with its raw-string fallback, a miss
    /// in the middle; and every refusal, in order: a computed output that
    /// names no path (before anything is read), a key or keys that resolve
    /// to no key, the bound of 1000, a connector that is not there or not a
    /// cache.
    #[tokio::test]
    async fn reads_and_refuses_as_on_v3() {
        let seed: &[(&str, &str)] = &[
            ("user:7", r#"{"name":"Ada","tags":["a","b"]}"#),
            ("n", "123"),
            ("s", r#""123""#),
            ("raw", "not json"),
            ("7", "true"),
            ("true", "1.5"),
        ];
        let data = json!({
            "id": 7, "list": ["n", "s"], "obj": {"a": 1}, "n": 3, "at": "data.out",
            "many": (0..1001).map(|i| format!("k{i}")).collect::<Vec<_>>(),
        });
        let cases = [
            json!({"connector": "c", "key": {"cat": ["user:", {"var": "data.id"}]}, "output": "data.out"}),
            json!({"connector": "c", "key": 7, "output": "data.out"}),
            json!({"connector": "c", "key": true, "output": "temp_data.out"}),
            json!({"connector": "c", "key": "raw", "output": {"var": "data.at"}}),
            json!({"connector": "c", "keys": ["n", {"cat": ["user:", {"var": "data.id"}]}, "missing", "raw", "s"], "output": "data.out"}),
            json!({"connector": "c", "keys": {"var": "data.list"}, "output": "data.out"}),
            json!({"connector": "c", "keys": [], "output": "data.out"}),
            json!({"connector": "c", "keys": [7, true], "output": "data.out"}),
            // The default `data` takes one key's object value.
            json!({"connector": "c", "key": "user:7"}),
            json!({"connector": "c", "key": "user:7", "output": {"var": "data.missing"}}),
            // Refusals.
            json!({"connector": "c", "key": "n", "output": {"var": "data.n"}}),
            json!({"connector": "c", "key": "n", "output": {"var": "data.obj"}}),
            json!({"connector": "c", "key": {"var": "data.none"}, "output": "data.out"}),
            json!({"connector": "c", "key": null, "output": "data.out"}),
            json!({"connector": "c", "key": {"var": "data.obj"}, "output": "data.out"}),
            json!({"connector": "c", "keys": "n", "output": "data.out"}),
            json!({"connector": "c", "keys": ["n", {"var": "data.obj"}], "output": "data.out"}),
            json!({"connector": "c", "keys": ["n", null], "output": "data.out"}),
            json!({"connector": "c", "keys": {"var": "data.many"}, "output": "data.out"}),
            json!({"connector": "absent", "key": "n", "output": "data.out"}),
            // An expression that fails, as 1.12's evaluation failed it.
            json!({"connector": "c", "key": {"throw": "boom"}, "output": "data.out"}),
            json!({"connector": "c", "keys": [{"throw": "boom"}], "output": "data.out"}),
        ];
        for input in cases {
            let (v3, v4) = both(input.clone(), data.clone(), json!({}), seed).await;
            assert_eq!(v4, v3, "{input}");
        }
        // A spot check of what the twins agree on.
        let (_, v4) = both(
            json!({"connector": "c", "keys": ["n", "missing", "raw", "s"], "output": "data.out"}),
            json!({}),
            json!({}),
            seed,
        )
        .await;
        assert_eq!(
            v4.expect("read")["out"],
            json!([123, null, "not json", "123"])
        );
    }

    /// A connector of another kind is refused as 1.12 refused it.
    #[tokio::test]
    async fn a_connector_that_is_not_a_cache_is_refused_on_v4() {
        let input = json!({"connector": "api", "key": "k", "output": "data.v"});
        let http = json!({"type": "http", "url": "http://127.0.0.1:1"});
        let mut sides = Vec::new();
        for v4 in [false, true] {
            let cache = twin::cache(json!({}), &[]).await;
            let registry = twin::connectors(&[("api", http.clone())]).await;
            let cache = twin::Cache { registry, ..cache };
            sides.push(if v4 {
                twin::v4(NAME, function(&cache), input.clone(), json!({})).await
            } else {
                twin::v3(NAME, v3_handler(&cache), input.clone(), json!({})).await
            });
        }
        assert_eq!(sides[1], sides[0]);
        let failure = sides[1].as_ref().expect_err("not a cache");
        assert!(
            failure
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("is not"),
            "{failure:?}"
        );
    }

    /// Break 21 at call time, named for O11b: one key's value written at a
    /// context root, the default `data` included, is refused on v4 unless
    /// it is an object, a miss's `null` included, where 1.12 replaced the
    /// root with it. An object is written on both.
    #[tokio::test]
    async fn one_value_at_a_root_is_written_only_when_it_is_an_object() {
        let seed: &[(&str, &str)] = &[("n", "123"), ("o", r#"{"a":1}"#)];
        for (key, scalar) in [("n", true), ("missing", true), ("o", false)] {
            let input = json!({"connector": "c", "key": key});
            let (v3, v4) = both(input, json!({"kept": true}), json!({}), seed).await;
            if scalar {
                assert!(v3.is_ok(), "1.12 replaced `data`: {v3:?}");
                let failure = v4.expect_err("a root takes only an object");
                assert_eq!(failure.code, "CONFIG_ERROR", "{key}");
            } else {
                assert_eq!(v4, v3);
                assert_eq!(v4.expect("an object"), json!({"a": 1}));
            }
        }
    }
}

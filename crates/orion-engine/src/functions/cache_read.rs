use super::definition::{Definition, FieldOverlay, OrionFunction};
use std::borrow::Cow;
use std::sync::Arc;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::{Ctx, Expr, Function, Issue, Outcome};
use serde_json::{Map, Value};

use super::connector_helpers::{to_connect_error, to_exec_error};
use super::schema::{FieldKind, RetrySafety};
use crate::connector::cache_backend::{CachePool, CachePurpose};
use crate::connector::{CacheConnectorConfig, ConnectorRegistry};
use crate::kit::{self, read};
use crate::{ErrorClass, HandlerError};

/// This function's name, for the free functions below (F48).
pub(crate) const NAME: &str = "cache_read";

/// The refusal of a task naming both `key` and `keys`, or neither, in
/// 1.12's words: `both` says which. dataflow-rs 4's config refuses both
/// shapes at load, as 1.12's authoring did, so a call meets it only as a
/// defensive fallback.
fn not_one_key_choice(name: &str, both: bool) -> HandlerError {
    HandlerError::new(
        ErrorClass::CallerInput,
        if both {
            format!("{name} takes 'key' or 'keys', not both")
        } else {
            format!("{name} requires 'key' or 'keys'")
        },
    )
}

/// One lookup, borrowed from the keys either engine read.
enum Lookup<'k> {
    One(Cow<'k, str>),
    Many(Cow<'k, [String]>),
}

/// The lookup, against a resolved and gated cache connector: the value one
/// key holds, or an array of the values several hold, in the order asked,
/// `null` for a miss.
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

/// One key's value may be an object or not, so a literal root `output` is
/// an Advisory: a miss or a non-object value fails the call (break 21).
const ONE_AT_ROOT: &str = "`cache_read` writes one key's cached value at `output`, and a \
    context root takes only an object (break 21): a miss (`null`) or a non-object value fails \
    the call; set `output` to a path below it, such as `data.cached`";

/// `cache_read`'s input on dataflow-rs 4, in its field table's order (I179).
/// The connector is a literal, required at load (I183); `key` and `keys`
/// are expressions, read by `kit::read`'s twins of `resolve_required_str`
/// and `resolve_required_str_list` (I183), the bound of 1000 applied per
/// call, as 1.12 applied it. Its `check` is 1.12's authoring-time rule,
/// exactly one of `key` and `keys`.
#[derive(Debug, FunctionConfig)]
#[config(check = check)]
pub struct CacheReadConfig {
    /// Name of the cache connector to read from.
    connector: String,
    /// Cache key to look up. JSONLogic: a literal, or an expression over the message. One of
    /// `key` and `keys` is required.
    key: Option<Expr<OwnedDataValue>>,
    /// Several keys to look up in one round trip (at most 1000). The result is an array in the
    /// same order, null for a miss. JSONLogic: an array of literals or expressions, or an
    /// expression evaluating to one.
    keys: Option<Expr<OwnedDataValue>>,
    /// Dotted path in the message where the result is stored. Defaults to "data".
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
    super::report_static(validate_static_input(&input), cx);
}

/// `cache_read` as a dataflow-rs 4 function: the lookup `CacheReadHandler`
/// makes, read in the same order.
pub struct CacheReadFunction {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

impl OrionFunction for CacheReadFunction {
    type Connector = crate::connector::kind::Cache;

    const DEFINITION: Definition = Definition {
        name: NAME,
        description: "Read a value from a cache connector (per-node, in memory).",
        category: "connector",
        retry_safety: RetrySafety::Read,
        fields: &[
            FieldOverlay {
                name: "key",
                kind: Some(FieldKind::String),
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "keys",
                kind: Some(FieldKind::Array),
                ..FieldOverlay::DEFAULT
            },
        ],
        validate_static: Some(validate_static_input),
        deny_unknown: false,
        stub_target: Some("connector"),
        ..Definition::DEFAULT
    };

    fn from_deps(deps: &crate::HandlerDeps<'_>) -> Self {
        Self {
            cache_pool: deps.cache_pool.clone(),
            registry: deps.registry.clone(),
        }
    }
}

impl Function for CacheReadFunction {
    type Config = CacheReadConfig;

    fn spec(&self) -> FunctionSpec {
        super::definition::spec::<Self>()
    }

    /// An `output` that is a literal context root, the default `data`
    /// included: with `keys` set, I185's Defect at `output`, since the
    /// array never fits; with one `key`, an Advisory there, since the value
    /// may be an object, and a miss or anything else fails the call (break
    /// 21). A computed output is judged per call.
    fn prepare(&self, cfg: &mut CacheReadConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        match (&cfg.key, &cfg.keys) {
            (_, Some(_)) => kit::root_output_defect(&cfg.output, ROOT_OUTPUT),
            (Some(_), None) => kit::root_output_advisory(&cfg.output, ONE_AT_ROOT),
            (None, None) => Ok(()),
        }
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &CacheReadConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, before the keys are read: an output that
        // names no path reads nothing.
        let output = Self::DEFINITION.output_path(ctx, &cfg.output)?;
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
                return Err(not_one_key_choice(NAME, key.is_some()).into());
            }
        };
        let mut value = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let conn = kit::require_connector::<<Self as OrionFunction>::Connector>(
                &config,
                &cfg.connector,
            )?;
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

#[cfg(test)]
mod tests {
    use super::*;

    // -- the config and its check, I185's Defect, and calls over an
    // in-memory cache, against 1.12's records --

    use crate::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::json;

    fn function(cache: &twin::Cache) -> CacheReadFunction {
        CacheReadFunction {
            cache_pool: cache.pool.clone(),
            registry: cache.registry.clone(),
        }
    }

    /// One task against a cache of its own holding `seed`, its connector
    /// configured with `config`.
    async fn run(input: Value, data: Value, config: Value, seed: &[(&str, &str)]) -> Seen {
        let cache = twin::cache(config, seed).await;
        twin::v4(NAME, function(&cache), input, data).await
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
    fn the_config_compiles_and_checks_its_inputs() {
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

    /// One `key` at a literal root `output`, the default `data` included, is
    /// an Advisory at `output`: a miss or a non-object value fails the call
    /// (break 21), and a cached object is written, so the function loads
    /// and runs. None below a root, for a computed output, or with `keys`,
    /// which the Defect covers.
    #[tokio::test]
    async fn one_key_at_a_root_output_is_an_advisory() {
        for (input, advisory) in [
            (json!({"connector": "c", "key": "a"}), true),
            (
                json!({"connector": "c", "key": "a", "output": "data"}),
                true,
            ),
            (
                json!({"connector": "c", "key": "a", "output": "temp_data"}),
                true,
            ),
            (json!({"connector": "c", "key": "a", "output": null}), true),
            (
                json!({"connector": "c", "key": "a", "output": "data.cached"}),
                false,
            ),
            (
                json!({"connector": "c", "key": "a", "output": {"cat": ["data", ".cached"]}}),
                false,
            ),
            (
                json!({"connector": "c", "key": "a", "output": {"var": "data.missing"}}),
                false,
            ),
            (
                json!({"connector": "c", "keys": ["a"], "output": "data.values"}),
                false,
            ),
            (json!({"connector": "c", "keys": ["a"]}), false),
        ] {
            let cache = twin::cache(json!({}), &[("a", r#"{"n":1}"#)]).await;
            let called = testing::call(function(&cache), input.clone())
                .exprs(kit::test_exprs())
                .name(NAME)
                .await;
            let advisories: Vec<_> = called
                .issues
                .iter()
                .filter(|i| i.severity == Severity::Advisory)
                .map(|i| (i.path.to_string(), i.code, i.message.as_str()))
                .collect();
            if advisory {
                assert_eq!(
                    advisories,
                    [("output".to_string(), IssueCode::InputParse, ONE_AT_ROOT)],
                    "{input}"
                );
                assert_eq!(called.issues.len(), 1, "{input}: {:?}", called.issues);
                assert_eq!(called.outcome.as_ref().ok(), Some(&Outcome::Ok), "{input}");
                // The cached object, written at the root.
                assert_eq!(called.writes.len(), 1, "{input}: {:?}", called.writes);
            } else {
                assert!(advisories.is_empty(), "{input}: {advisories:?}");
            }
        }
    }

    /// A miss is a result, `null`, not a failure.
    #[tokio::test]
    async fn a_miss_reads_as_null() {
        let input = json!({"connector": "c", "key": "absent-key", "output": "data.cached"});
        let v4 = run(input, json!({}), json!({}), &[]).await;
        twin::pinned(&v4, "a_miss_reads_as_null");
        assert_eq!(v4.expect("a miss is not an error"), json!({"cached": null}));
    }

    /// The gate's refusal, its caller-safe message and its detail, as
    /// 1.12's. The gate is the connector's answer rather than the backend's,
    /// so it is decided before anything is dialled.
    #[tokio::test]
    async fn a_write_only_connector_refuses_a_read() {
        let input = json!({"connector": "c", "key": "k", "output": "data.v"});
        let gated = json!({"operations": {"read": false, "write": true}});
        let v4 = run(input, json!({}), gated, &[("k", "1")]).await;
        twin::pinned(&v4, "a_write_only_connector_refuses_a_read");
        let failure = v4.expect_err("a read must be refused when the gate is off");
        let detail = failure.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("operation 'read' is disabled"),
            "{failure:?}"
        );
    }

    /// Every reading 1.12 made: a computed key, a number
    /// and a boolean spelled out as keys, `keys` as an array of expressions
    /// or one expression, the decode with its raw-string fallback, a miss
    /// in the middle; and every refusal, in order: a computed output that
    /// names no path (before anything is read), a key or keys that resolve
    /// to no key, the bound of 1000, a connector that is not there or not a
    /// cache.
    #[tokio::test]
    async fn reads_and_refuses_as_1_12_did() {
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
            let v4 = run(input.clone(), data.clone(), json!({}), seed).await;
            twin::pinned(&v4, format!("reads_and_refuses/{input}"));
        }
        // A spot check of what the records hold.
        let v4 = run(
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
    async fn a_connector_that_is_not_a_cache_is_refused() {
        let input = json!({"connector": "api", "key": "k", "output": "data.v"});
        let http = json!({"type": "http", "url": "http://127.0.0.1:1"});
        let cache = twin::cache(json!({}), &[]).await;
        let registry = twin::connectors(&[("api", http)]).await;
        let cache = twin::Cache { registry, ..cache };
        let v4 = twin::v4(NAME, function(&cache), input, json!({})).await;
        twin::pinned(&v4, "a_connector_that_is_not_a_cache_is_refused");
        let failure = v4.as_ref().expect_err("not a cache");
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
    /// root with it. An object is written.
    #[tokio::test]
    async fn one_value_at_a_root_is_written_only_when_it_is_an_object() {
        let seed: &[(&str, &str)] = &[("n", "123"), ("o", r#"{"a":1}"#)];
        for (key, scalar) in [("n", true), ("missing", true), ("o", false)] {
            let input = json!({"connector": "c", "key": key});
            let v4 = run(input, json!({"kept": true}), json!({}), seed).await;
            if scalar {
                let failure = v4.expect_err("a root takes only an object");
                assert_eq!(failure.code, "CONFIG_ERROR", "{key}");
            } else {
                assert_eq!(v4.expect("an object"), json!({"a": 1}));
            }
        }
    }
}

use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::Interpolated;
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use dataflow_rs_v3::engine::error::DataflowError;
use dataflow_rs_v3::engine::task_context::TaskContext;
use serde_json::Value;

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, json_type_name, require_op, resolve_required_str, resolve_value,
    to_connect_error, to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::{Scope, TemplatedInput};
use crate::connector::cache_backend::{CachePool, CachePurpose};
use crate::connector::{CacheConnectorConfig, ConnectorRegistry};
use crate::engine::HandlerError;
use crate::engine::kit::{self, read};

/// This function's name, for the free functions below (F48).
const NAME: &str = <CacheWriteHandler as ConnectorHandler>::NAME;

/// Workflow function handler for writing values to a cache backend.
pub struct CacheWriteHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

/// Everything the write needs from the task and the message, folded before the
/// body takes `ctx` mutably — without which the task could only ever write one
/// constant, to one constant key.
pub struct CacheWrite {
    key: String,
    value: Value,
    ttl: Option<u64>,
}

#[async_trait]
impl ConnectorHandler for CacheWriteHandler {
    const NAME: &'static str = "cache_write";
    type Kind = crate::connector::kind::Cache;
    type Input = TemplatedInput;
    type Parsed = CacheWrite;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &Scope<'_, '_>,
    ) -> Result<Self::Parsed, HandlerError> {
        let key = resolve_required_str(input, "key", call.name, ctx)?;
        let value = match input.get("value") {
            Some(v) => resolve_value(v, ctx),
            None => {
                return Err(
                    DataflowError::Validation(format!("{} requires 'value'", call.name)).into(),
                );
            }
        };
        Ok(CacheWrite {
            key,
            value,
            ttl: resolve_ttl_secs(input, call.name, ctx)?,
        })
    }

    fn gate(
        _parsed: &Self::Parsed,
        conn: &crate::connector::CacheConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        // F22e: a cache connector can be made read-only in its config.
        Ok(require_op(conn.operations.write, "write", connector)?)
    }

    async fn run(
        &self,
        write: Self::Parsed,
        conn: &crate::connector::CacheConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        let encoded = encode(&write.value)?;
        store(
            &self.cache_pool,
            call.connector,
            conn,
            &write.key,
            &encoded,
            write.ttl,
        )
        .await?;

        // The write is the whole effect: this handler declares no `output`.
        Ok(Produced::nothing())
    }
}

/// The value as the string the store holds.
///
/// Always JSON-encode, including strings, so `cache_read` is the exact
/// inverse. Storing strings raw made the round-trip lossy: writing "123"
/// stored `123`, which read back as the *number* 123, and "true"/"null"
/// likewise changed type — silently breaking any downstream JSONLogic
/// comparison (proposal N13).
///
/// Generic over what is encoded, so dataflow-rs 4 serializes the rendered
/// document straight into the string, with no tree of its own.
fn encode<V: serde::Serialize + ?Sized>(value: &V) -> Result<String, DataflowError> {
    serde_json::to_string(value)
        .map_err(|e| DataflowError::Validation(format!("Failed to serialize value for cache: {e}")))
}

/// The write, against a resolved and gated cache connector: `encoded` at
/// `key`, expiring after `ttl` seconds when it is set. Shared by the
/// dataflow-rs 3 handler and the dataflow-rs 4 function.
async fn store(
    pool: &CachePool,
    connector: &str,
    conn: &CacheConnectorConfig,
    key: &str,
    encoded: &str,
    ttl: Option<u64>,
) -> Result<(), HandlerError> {
    // Workflow-purpose namespace (S19): a memory backend here can never
    // alias the dedup store or response cache, so a crafted
    // `dedup:{channel}:…` key is just an ordinary workflow key.
    let backend = pool
        .get_backend(CachePurpose::Workflow, connector, conn)
        .await
        .map_err(to_connect_error)?;

    match ttl {
        Some(ttl) => backend
            .set_ex(key, encoded, ttl)
            .await
            .map_err(to_exec_error)?,
        None => backend.set(key, encoded).await.map_err(to_exec_error)?,
    }

    tracing::debug!(key = %key, ttl = ?ttl, "Wrote value to cache");
    Ok(())
}

/// Resolve `ttl_secs` to whole seconds. Absent or null means "no expiry"; a
/// value that resolves to something uninterpretable is an error rather than a
/// silent fall-through to a key that never expires.
///
/// The field is folded, not evaluated: 1.12's table declares it a template,
/// but this reads the authored value through [`resolve_value`]'s `{"var"}`
/// walk, which is the reading every stored task runs under (I183).
pub(super) fn resolve_ttl_secs(
    input: &TemplatedInput,
    name: &str,
    ctx: &TaskContext<'_>,
) -> Result<Option<u64>, DataflowError> {
    let Some(raw) = input.get("ttl_secs") else {
        return Ok(None);
    };
    ttl_secs(resolve_value(raw, ctx), name)
}

/// `ttl_secs` on dataflow-rs 4: the document rendered for this call, read
/// as [`resolve_ttl_secs`] reads its fold; `None` when the task leaves the
/// field unset.
///
/// # Errors
///
/// As [`ttl_secs`].
pub(super) fn rendered_ttl_secs(
    ctx: &Ctx<'_>,
    doc: Option<&Interpolated>,
    name: &str,
) -> Result<Option<u64>, HandlerError> {
    let Some(doc) = doc else {
        return Ok(None);
    };
    Ok(ttl_secs(kit::rendered(ctx, doc)?, name)?)
}

/// A resolved `ttl_secs` as whole seconds, by [`resolve_ttl_secs`]' rules:
/// `null` is no expiry; a non-negative whole number, written as an integer
/// or a float, is the seconds; anything else is refused.
fn ttl_secs(value: Value, name: &str) -> Result<Option<u64>, DataflowError> {
    match value {
        Value::Null => Ok(None),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Ok(Some(u))
            } else if let Some(f) = n.as_f64()
                && f >= 0.0
                && f.fract() == 0.0
            {
                Ok(Some(f as u64))
            } else {
                Err(DataflowError::Validation(format!(
                    "{name} 'ttl_secs' must be a non-negative whole number of seconds, got {n}"
                )))
            }
        }
        other => Err(DataflowError::Validation(format!(
            "{name} 'ttl_secs' must resolve to a number, got {}",
            json_type_name(&other)
        ))),
    }
}

/// `cache_write`'s input on dataflow-rs 4, in its field table's order
/// (I179). The connector is a literal; the key an expression, read by
/// `kit::read`'s twin of `resolve_required_str`; the value and `ttl_secs`
/// documents of `var` holes, `$` keys kept as data, as 1.12 folded them
/// (I183). The connector, the key and the value are required at load
/// (I183). It has no `output`: the write is the whole effect.
#[derive(Debug, FunctionConfig)]
pub struct CacheWriteConfig {
    /// The cache connector written to.
    connector: String,
    /// The key set.
    key: Expr<OwnedDataValue>,
    /// The value stored, any JSON.
    value: Interpolated,
    /// The seconds the key lives; unset, it never expires.
    ttl_secs: Option<Interpolated>,
}

/// `cache_write` as a dataflow-rs 4 function: the write `CacheWriteHandler`
/// makes, read in the same order.
pub struct CacheWriteFunction {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

impl Function for CacheWriteFunction {
    type Config = CacheWriteConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &CacheWriteConfig,
    ) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // `parse`'s order: the key, then the expiry. The value is a document,
        // whose rendering cannot fail, so it is rendered where it is encoded.
        let key = ctx.eval(&cfg.key)?;
        let key = read::required_str(Some(&key), "key", NAME)?;
        let ttl = rendered_ttl_secs(ctx, cfg.ttl_secs.as_ref(), NAME)?;
        let ctx = &*ctx;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let conn =
                kit::require_connector::<crate::connector::kind::Cache>(&config, &cfg.connector)?;
            // F22e: a cache connector can be made read-only in its config.
            kit::require_op(conn.operations.write, "write", &cfg.connector)?;
            let encoded = encode(&ctx.render(&cfg.value)).map_err(HandlerError::from)?;
            store(&self.cache_pool, &cfg.connector, conn, &key, &encoded, ttl).await?;
            Ok(Outcome::Ok)
        })
        .await
    }
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const CACHE_WRITE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the cache connector to write to.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key",
        description: "Cache key to set. JSONLogic: a literal, or an expression over the message.",
        kind: FieldKind::String,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "value",
        description: "Value to store. May be any JSON value. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::Any,
        required: true,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "ttl_secs",
        description: "Time-to-live in seconds. Omit for no expiry. JSONLogic: a literal, or an expression over the message.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::json;

    /// The keys the twins look at after a write.
    const LOOK: &[&str] = &["k", "7", "true", "user:7"];

    /// One task through each engine, each against a cache of its own
    /// holding `seed`, its connector configured with `config`: what each
    /// saw, and what each cache holds at [`LOOK`] after.
    async fn both(
        input: Value,
        data: Value,
        config: Value,
        seed: &[(&str, &str)],
    ) -> ((Seen, Vec<Option<String>>), (Seen, Vec<Option<String>>)) {
        let cache = twin::cache(config.clone(), seed).await;
        let handler = Box::new(super::super::connector_handler::Connector(
            CacheWriteHandler {
                cache_pool: cache.pool.clone(),
                registry: cache.registry.clone(),
            },
        ));
        let v3 = twin::v3(NAME, handler, input.clone(), data.clone()).await;
        let v3 = (v3, cache.held(LOOK).await);
        let cache = twin::cache(config, seed).await;
        let f = CacheWriteFunction {
            cache_pool: cache.pool.clone(),
            registry: cache.registry.clone(),
        };
        let v4 = twin::v4(NAME, f, input, data).await;
        (v3, (v4, cache.held(LOOK).await))
    }

    /// Declaration order is the field table's (I179). The value and
    /// `ttl_secs` are documents of `var` holes, `$` keys kept as data (I183);
    /// the connector, the key and the value are required (I183), the
    /// connector a literal; an unknown key, an `output` among them, is
    /// refused (break 2a).
    #[test]
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = CacheWriteConfig::fields()
            .iter()
            .map(|f| &*f.name)
            .collect();
        let table: Vec<&str> = CACHE_WRITE_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

        let compile =
            |input: Value| testing::config_with::<CacheWriteConfig>(&kit::test_exprs(), input);
        let value =
            json!({"$set": {"name": {"var": "data.name"}}, "n": [1, {"var": ["data.n", 0]}]});
        let cfg = compile(json!({
            "connector": "c", "key": {"var": "data.k"}, "value": value.clone(),
            "ttl_secs": {"var": "data.ttl"}
        }))
        .expect("compiles");
        assert!(cfg.key.constant().is_none());
        assert_eq!(cfg.value.raw(), &value, "the document as authored");
        let holes: Vec<String> = cfg.value.holes().map(ToString::to_string).collect();
        assert_eq!(holes, ["data.name", "data.n"]);
        let ttl: Vec<String> = cfg
            .ttl_secs
            .as_ref()
            .expect("set")
            .holes()
            .map(ToString::to_string)
            .collect();
        assert_eq!(ttl, ["data.ttl"]);
        // A `null` value is a value, as 1.12 stored it.
        compile(json!({"connector": "c", "key": "k", "value": null})).expect("compiles");

        let rejected = |input: Value| -> Vec<(String, IssueCode)> {
            compile(input)
                .expect_err("refused")
                .into_iter()
                .filter(|i| i.severity == Severity::Rejected)
                .map(|i| (i.path.to_string(), i.code))
                .collect()
        };
        assert_eq!(
            rejected(json!({"connector": "c"})),
            [
                ("key".to_string(), IssueCode::InputParse),
                ("value".to_string(), IssueCode::InputParse)
            ]
        );
        assert_eq!(
            rejected(json!({"key": "k", "value": 1})),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        assert_eq!(
            rejected(json!({"connector": {"var": "data.c"}, "key": "k", "value": 1})),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        let issues =
            rejected(json!({"connector": "c", "key": "k", "value": 1, "output": "data.x"}));
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].0, "output");
    }

    /// Every write 1.12 made, the stored strings compared byte for byte:
    /// strings JSON-encoded so a read gives the type back, a document's
    /// holes filled and its `$` keys kept, a computed key, a number and a
    /// boolean spelled out as keys; and `ttl_secs` read as 1.12 read its
    /// fold, a whole float accepted and a duration string, a fraction, a
    /// negative, an object (an operator is data here) or a boolean refused.
    #[tokio::test]
    async fn writes_and_refuses_as_on_v3() {
        let data = json!({
            "id": 7, "name": "Ada", "obj": {"a": [1, {"b": null}]}, "ttl": 30,
            "two": 2.0, "half": 2.5, "neg": -1, "f": 1.25
        });
        let cases = [
            json!({"connector": "c", "key": "k", "value": "123"}),
            json!({"connector": "c", "key": "k", "value": 123}),
            json!({"connector": "c", "key": "k", "value": null}),
            json!({"connector": "c", "key": "k", "value": {"var": "data.obj"}}),
            json!({"connector": "c", "key": "k", "value": {"var": "data.f"}}),
            json!({"connector": "c", "key": "k", "value": [1.5, 2.0, {"var": "data.two"}]}),
            json!({
                "connector": "c", "key": {"cat": ["user:", {"var": "data.id"}]},
                "value": {"$set": {"name": {"var": "data.name"}}, "n": [1, {"var": ["data.missing", "dflt"]}]},
                "ttl_secs": 60
            }),
            json!({"connector": "c", "key": "k", "value": {"+": [1, 2]}}),
            json!({"connector": "c", "key": 7, "value": true}),
            json!({"connector": "c", "key": true, "value": false}),
            json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": {"var": "data.ttl"}}),
            json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": null}),
            json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": {"var": "data.missing"}}),
            json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": {"var": "data.two"}}),
            json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": 0}),
            // Refusals.
            json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": {"var": "data.half"}}),
            json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": {"var": "data.neg"}}),
            json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": "5m"}),
            json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": {"+": [1, 2]}}),
            json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": [30]}),
            json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": true}),
            json!({"connector": "c", "key": {"var": "data.none"}, "value": 1}),
            json!({"connector": "c", "key": {"var": "data.obj"}, "value": 1}),
            json!({"connector": "absent", "key": "k", "value": 1}),
        ];
        for input in cases {
            let (v3, v4) = both(input.clone(), data.clone(), json!({}), &[]).await;
            assert_eq!(v4, v3, "{input}");
        }
        // A spot check: the string a read decodes back to the same type.
        let (_, (seen, held)) = both(
            json!({"connector": "c", "key": "k", "value": "123"}),
            json!({}),
            json!({}),
            &[],
        )
        .await;
        assert_eq!(
            seen.expect("written"),
            json!({}),
            "a write writes no output"
        );
        assert_eq!(held[0].as_deref(), Some(r#""123""#));
    }

    /// The gate refuses a write before anything is stored, as 1.12's did.
    #[tokio::test]
    async fn a_read_only_connector_refuses_a_write_on_v4() {
        let input = json!({"connector": "c", "key": "k", "value": 1});
        let gated = json!({"operations": {"read": true, "write": false}});
        let (v3, v4) = both(input, json!({}), gated, &[("k", "0")]).await;
        assert_eq!(v4, v3);
        let failure = v4.0.expect_err("writes are off");
        assert!(
            failure
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("operation 'write' is disabled"),
            "{failure:?}"
        );
        assert_eq!(v4.1[0].as_deref(), Some("0"), "nothing was stored");
    }

    /// `ttl_secs` reaches the store as 1.12 sent it: a key written with one
    /// is gone once it passes, on both engines, and one written without is
    /// kept.
    #[tokio::test(start_paused = true)]
    async fn a_ttl_expires_the_key_on_both() {
        for v4 in [false, true] {
            let cache = twin::cache(json!({}), &[]).await;
            for (key, ttl) in [("k", json!(1)), ("7", json!(null))] {
                let input = json!({"connector": "c", "key": key, "value": 1, "ttl_secs": ttl});
                let seen = if v4 {
                    let f = CacheWriteFunction {
                        cache_pool: cache.pool.clone(),
                        registry: cache.registry.clone(),
                    };
                    twin::v4(NAME, f, input, json!({})).await
                } else {
                    let handler = Box::new(super::super::connector_handler::Connector(
                        CacheWriteHandler {
                            cache_pool: cache.pool.clone(),
                            registry: cache.registry.clone(),
                        },
                    ));
                    twin::v3(NAME, handler, input, json!({})).await
                };
                seen.expect("written");
            }
            assert_eq!(
                cache.held(&["k", "7"]).await,
                [Some("1".to_string()), Some("1".to_string())]
            );
            tokio::time::advance(std::time::Duration::from_secs(2)).await;
            assert_eq!(
                cache.held(&["k", "7"]).await,
                [None, Some("1".to_string())],
                "v4: {v4}"
            );
        }
    }

    /// The call writes nothing to the message: the store is its effect.
    #[tokio::test]
    async fn a_write_writes_nothing_to_the_message() {
        let cache = twin::cache(json!({}), &[]).await;
        let f = CacheWriteFunction {
            cache_pool: cache.pool.clone(),
            registry: cache.registry.clone(),
        };
        let called = testing::call(f, json!({"connector": "c", "key": "k", "value": {"a": 1}}))
            .exprs(kit::test_exprs())
            .name(NAME)
            .await;
        assert_eq!(called.outcome.as_ref().ok(), Some(&Outcome::Ok));
        assert!(called.writes.is_empty(), "{:?}", called.writes);
        assert_eq!(cache.held(&["k"]).await, [Some(r#"{"a":1}"#.to_string())]);
    }
}

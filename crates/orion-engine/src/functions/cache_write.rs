use super::definition::{Definition, FieldOverlay, OrionFunction};
use std::sync::Arc;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::Interpolated;
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use serde_json::Value;

use super::connector_helpers::{json_type_name, to_connect_error, to_exec_error};
use super::schema::{FieldKind, RetrySafety};
use crate::connector::cache_backend::{CachePool, CachePurpose};
use crate::connector::{CacheConnectorConfig, ConnectorRegistry};
use crate::kit::{self, read};
use crate::{ErrorClass, HandlerError};

/// This function's name, for the free functions below (F48).
pub(crate) const NAME: &str = "cache_write";

/// The value as the string the store holds.
///
/// Always JSON-encode, including strings, so `cache_read` is the exact
/// inverse. Storing strings raw made the round-trip lossy: writing "123"
/// stored `123`, which read back as the *number* 123, and "true"/"null"
/// likewise changed type — silently breaking any downstream JSONLogic
/// comparison (proposal N13).
///
/// Generic over what is encoded, so the function serializes the rendered
/// document straight into the string, with no tree of its own.
fn encode<V: serde::Serialize + ?Sized>(value: &V) -> Result<String, HandlerError> {
    serde_json::to_string(value).map_err(|e| {
        HandlerError::new(
            ErrorClass::CallerInput,
            format!("Failed to serialize value for cache: {e}"),
        )
    })
}

/// The write, against a resolved and gated cache connector: `encoded` at
/// `key`, expiring after `ttl` seconds when it is set.
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
    ttl_secs(kit::rendered(ctx, doc)?, name)
}

/// A resolved `ttl_secs` as whole seconds, by [`resolve_ttl_secs`]' rules:
/// `null` is no expiry; a non-negative whole number, written as an integer
/// or a float, is the seconds; anything else is refused.
fn ttl_secs(value: Value, name: &str) -> Result<Option<u64>, HandlerError> {
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
                Err(HandlerError::new(
                    ErrorClass::CallerInput,
                    format!(
                        "{name} 'ttl_secs' must be a non-negative whole number of seconds, got {n}"
                    ),
                ))
            }
        }
        other => Err(HandlerError::new(
            ErrorClass::CallerInput,
            format!(
                "{name} 'ttl_secs' must resolve to a number, got {}",
                json_type_name(&other)
            ),
        )),
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
    /// Name of the cache connector to write to.
    connector: String,
    /// Cache key to set. JSONLogic: a literal, or an expression over the message.
    key: Expr<OwnedDataValue>,
    /// Value to store. May be any JSON value. Accepts {"var": "path"} to read the value from the message.
    value: Interpolated,
    /// Time-to-live in seconds. Omit for no expiry. JSONLogic: a literal, or an expression over the message.
    ttl_secs: Option<Interpolated>,
}

/// `cache_write` as a dataflow-rs 4 function: the write `CacheWriteHandler`
/// makes, read in the same order.
pub struct CacheWriteFunction {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

impl OrionFunction for CacheWriteFunction {
    type Connector = crate::connector::kind::Cache;

    const DEFINITION: Definition = Definition {
        name: NAME,
        description: "Write a value to a cache connector.",
        category: "connector",
        retry_safety: RetrySafety::IdempotentWrite,
        fields: &[
            FieldOverlay {
                name: "key",
                kind: Some(FieldKind::String),
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "value",
                resolvable: true,
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "ttl_secs",
                kind: Some(FieldKind::Number),
                template_at: Some(&[""]),
                ..FieldOverlay::DEFAULT
            },
        ],
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

impl Function for CacheWriteFunction {
    type Config = CacheWriteConfig;

    fn spec(&self) -> FunctionSpec {
        super::definition::spec::<Self>()
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
            let conn = kit::require_connector::<<Self as OrionFunction>::Connector>(
                &config,
                &cfg.connector,
            )?;
            // F22e: a cache connector can be made read-only in its config.
            kit::require_op(conn.operations.write, "write", &cfg.connector)?;
            let encoded = encode(&ctx.render(&cfg.value))?;
            store(&self.cache_pool, &cfg.connector, conn, &key, &encoded, ttl).await?;
            Ok(Outcome::Ok)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::json;

    /// The keys a test looks at after a write.
    const LOOK: &[&str] = &["k", "7", "true", "user:7"];

    /// One task against a cache of its own holding `seed`, its connector
    /// configured with `config`: what it saw, and what the cache holds at
    /// [`LOOK`] after.
    async fn run(
        input: Value,
        data: Value,
        config: Value,
        seed: &[(&str, &str)],
    ) -> (Seen, Vec<Option<String>>) {
        let cache = twin::cache(config, seed).await;
        let f = CacheWriteFunction {
            cache_pool: cache.pool.clone(),
            registry: cache.registry.clone(),
        };
        let v4 = twin::v4(NAME, f, input, data).await;
        (v4, cache.held(LOOK).await)
    }

    /// Pin what a call saw, and what the cache held after, to 1.12's records.
    #[track_caller]
    fn pin(seen: &(Seen, Vec<Option<String>>), key: impl std::fmt::Display) {
        twin::pinned(&seen.0, &key);
        twin::pinned(&Ok(json!(seen.1)), format!("{key}/held"));
    }

    /// Declaration order is the field table's (I179). The value and
    /// `ttl_secs` are documents of `var` holes, `$` keys kept as data (I183);
    /// the connector, the key and the value are required (I183), the
    /// connector a literal; an unknown key, an `output` among them, is
    /// refused (break 2a).
    #[test]
    fn the_config_compiles_and_checks_its_inputs() {
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
    async fn writes_and_refuses_as_1_12_did() {
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
            let v4 = run(input.clone(), data.clone(), json!({}), &[]).await;
            pin(&v4, format!("writes_and_refuses/{input}"));
        }
        // A spot check: the string a read decodes back to the same type.
        let (seen, held) = run(
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
    async fn a_read_only_connector_refuses_a_write() {
        let input = json!({"connector": "c", "key": "k", "value": 1});
        let gated = json!({"operations": {"read": true, "write": false}});
        let v4 = run(input, json!({}), gated, &[("k", "0")]).await;
        pin(&v4, "a_read_only_connector_refuses_a_write");
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
    /// is gone once it passes, and one written without is kept.
    #[tokio::test(start_paused = true)]
    async fn a_ttl_expires_the_key() {
        let cache = twin::cache(json!({}), &[]).await;
        for (key, ttl) in [("k", json!(1)), ("7", json!(null))] {
            let input = json!({"connector": "c", "key": key, "value": 1, "ttl_secs": ttl});
            let f = CacheWriteFunction {
                cache_pool: cache.pool.clone(),
                registry: cache.registry.clone(),
            };
            twin::v4(NAME, f, input, json!({})).await.expect("written");
        }
        assert_eq!(
            cache.held(&["k", "7"]).await,
            [Some("1".to_string()), Some("1".to_string())]
        );
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(cache.held(&["k", "7"]).await, [None, Some("1".to_string())]);
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

use std::sync::Arc;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::Output;
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Expr, Function, Outcome};

use super::cache_read::MAX_KEYS;
use super::connector_helpers::{to_connect_error, to_exec_error};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::cache_backend::{CachePool, CachePurpose};
use crate::connector::{CacheConnectorConfig, ConnectorRegistry};
use crate::engine::HandlerError;
use crate::engine::kit::{self, read};

/// This function's name, for the free functions below (F48).
pub(crate) const NAME: &str = "cache_delete";

/// The delete, against a resolved and gated cache connector: how many of
/// `keys` existed. Shared by the dataflow-rs 3 handler and the dataflow-rs
/// 4 function.
async fn delete(
    pool: &CachePool,
    connector: &str,
    conn: &CacheConnectorConfig,
    keys: &[String],
) -> Result<u64, HandlerError> {
    // Workflow-purpose namespace (S19), like `cache_read` and
    // `cache_write`: a memory backend here can never reach the dedup store
    // or the response cache.
    let backend = pool
        .get_backend(CachePurpose::Workflow, connector, conn)
        .await
        .map_err(to_connect_error)?;
    let deleted = backend.remove_many(keys).await.map_err(to_exec_error)?;
    tracing::debug!(keys = keys.len(), deleted, "Deleted cache keys");
    Ok(deleted)
}

/// `cache_delete`'s input on dataflow-rs 4, in its field table's order
/// (I179). The connector is a literal and `keys` an expression, read by
/// `kit::read`'s twin of `resolve_required_str_list` with 1.12's bound of
/// 1000 per call; both are required at load (I183). `output` is optional:
/// unset, nothing is written (I190's primary output).
#[derive(Debug, FunctionConfig)]
pub struct CacheDeleteConfig {
    /// The cache connector deleted from.
    connector: String,
    /// The exact keys deleted.
    keys: Expr<OwnedDataValue>,
    /// Where `{"deleted": n}`, the number of keys that existed, is written;
    /// unset, nothing is.
    output: Option<Output>,
}

/// `cache_delete` as a dataflow-rs 4 function: the delete
/// `CacheDeleteHandler` makes, read in the same order.
pub struct CacheDeleteFunction {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

impl Function for CacheDeleteFunction {
    type Config = CacheDeleteConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &CacheDeleteConfig,
    ) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, when one is set, before the keys are read.
        // A computed one that resolves to `null` is `data`, as 1.12's was.
        let output = cfg
            .output
            .as_ref()
            .map(|out| kit::output_path(ctx, out, Some(&kit::DATA)))
            .transpose()?;
        let keys = ctx.eval(&cfg.keys)?;
        let keys = read::required_str_list(Some(&keys), "keys", NAME, MAX_KEYS)?;
        let mut deleted = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let conn =
                kit::require_connector::<crate::connector::kind::Cache>(&config, &cfg.connector)?;
            // A delete changes what later reads see, so a read-only connector
            // refuses it exactly as it refuses `cache_write`.
            kit::require_op(conn.operations.write, "write", &cfg.connector)?;
            deleted = Some(delete(&self.cache_pool, &cfg.connector, conn, &keys).await?);
            Ok(Outcome::Ok)
        })
        .await?;
        if let (Some(path), Some(deleted)) = (output, deleted) {
            let answer = OwnedDataValue::Object(vec![("deleted".to_string(), deleted.into())]);
            ctx.set(&path, answer)?;
        }
        Ok(Outcome::Ok)
    }
}

// -- Input schema (F53) --

pub(super) const CACHE_DELETE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the cache connector to delete from.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "keys",
        description: "Exact keys to delete (at most 1000). A key that is not present is not an error. JSONLogic: an array of literals or expressions, or an expression evaluating to one.",
        kind: FieldKind::Array,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where `{\"deleted\": n}`, the number of keys that existed, is stored. Omit to record nothing.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- the config, and calls over an in-memory cache, against 1.12's
    // records --

    use crate::engine::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::Value;

    /// The keys a test looks at after a delete.
    const LOOK: &[&str] = &["a", "b", "c"];

    fn function(cache: &twin::Cache) -> CacheDeleteFunction {
        CacheDeleteFunction {
            cache_pool: cache.pool.clone(),
            registry: cache.registry.clone(),
        }
    }

    /// One task against a cache of its own holding `a`, `b` and `c`, its
    /// connector configured with `config`: what it saw, and what the cache
    /// holds at [`LOOK`] after.
    async fn run(input: Value, data: Value, config: Value) -> (Seen, Vec<Option<String>>) {
        let seed: &[(&str, &str)] = &[("a", "1"), ("b", "2"), ("c", "3")];
        let cache = twin::cache(config, seed).await;
        let v4 = twin::v4(NAME, function(&cache), input, data).await;
        (v4, cache.held(LOOK).await)
    }

    /// Pin what a call saw, and what the cache held after, to 1.12's records.
    #[track_caller]
    fn pin(seen: &(Seen, Vec<Option<String>>), key: impl std::fmt::Display) {
        twin::pinned(&seen.0, &key);
        twin::pinned(&Ok(json!(seen.1)), format!("{key}/held"));
    }

    /// Declaration order is the field table's (I179); `keys` is an
    /// expression and, with the connector, required (I183); `output` is
    /// optional and the primary output, unset when absent or `null`; an
    /// unknown key is refused (break 2a).
    #[test]
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = CacheDeleteConfig::fields()
            .iter()
            .map(|f| &*f.name)
            .collect();
        let table: Vec<&str> = CACHE_DELETE_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

        let compile =
            |input: Value| testing::config_with::<CacheDeleteConfig>(&kit::test_exprs(), input);
        let cfg =
            compile(json!({"connector": "c", "keys": ["a", {"var": "data.k"}]})).expect("compiles");
        assert!(cfg.output.is_none());
        assert!(cfg.primary_output().is_none(), "unset, nothing is written");
        let cfg = compile(json!({"connector": "c", "keys": [], "output": null})).expect("compiles");
        assert!(
            cfg.output.is_none(),
            "an authored null is unset, as 1.12 read it"
        );
        let cfg =
            compile(json!({"connector": "c", "keys": [], "output": "data.del"})).expect("compiles");
        assert!(cfg.primary_output().is_some());

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
            [("keys".to_string(), IssueCode::InputParse)]
        );
        assert_eq!(
            rejected(json!({"keys": ["a"]})),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        let issues = rejected(json!({"connector": "c", "keys": ["a"], "key": "a"}));
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].0, "key");
    }

    /// The count is of the keys that existed, written only where the task
    /// names an output; the keys go either way.
    #[tokio::test]
    async fn deletes_report_what_existed() {
        let keys = json!(["a", "b", "absent"]);
        let v4 = run(
            json!({"connector": "c", "keys": keys.clone(), "output": "data.del"}),
            json!({}),
            json!({}),
        )
        .await;
        pin(&v4, "deletes_report_what_existed/output");
        assert_eq!(v4.0.expect("deleted"), json!({"del": {"deleted": 2}}));
        assert_eq!(v4.1, [None, None, Some("3".to_string())]);

        let v4 = run(
            json!({"connector": "c", "keys": keys}),
            json!({}),
            json!({}),
        )
        .await;
        pin(&v4, "deletes_report_what_existed/no_output");
        assert_eq!(
            v4.0.expect("deleted"),
            json!({}),
            "no output, nothing written"
        );
        assert_eq!(v4.1, [None, None, Some("3".to_string())]);
    }

    /// A read-only connector refuses a delete before anything goes.
    #[tokio::test]
    async fn a_read_only_connector_refuses_a_delete() {
        let input = json!({"connector": "c", "keys": ["a"], "output": "data.del"});
        let gated = json!({"operations": {"read": true, "write": false}});
        let v4 = run(input, json!({}), gated).await;
        pin(&v4, "a_read_only_connector_refuses_a_delete");
        let failure = v4.0.expect_err("writes are off");
        assert!(
            failure
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("operation 'write' is disabled"),
            "{failure:?}"
        );
        assert_eq!(v4.1[0].as_deref(), Some("1"), "nothing was deleted");
    }

    /// The rest of 1.12's readings: computed keys, numbers spelled out, an
    /// output computed to `null` writing at `data` (an object, which a
    /// root takes) and an authored `null` writing nothing; and every
    /// refusal, a computed output that names no path before anything is
    /// deleted included.
    #[tokio::test]
    async fn deletes_and_refuses_as_1_12_did() {
        let data = json!({
            "ks": ["a", "c"], "n": 3, "obj": {"x": 1},
            "many": (0..1001).map(|i| format!("k{i}")).collect::<Vec<_>>(),
        });
        let cases = [
            json!({"connector": "c", "keys": {"var": "data.ks"}, "output": "temp_data.del"}),
            json!({"connector": "c", "keys": [{"cat": ["", "b"]}, 7], "output": "data.del"}),
            json!({"connector": "c", "keys": [], "output": "data.del"}),
            json!({"connector": "c", "keys": ["a"], "output": {"var": "data.missing"}}),
            json!({"connector": "c", "keys": ["a"], "output": null}),
            // Refusals.
            json!({"connector": "c", "keys": ["a"], "output": {"var": "data.n"}}),
            json!({"connector": "c", "keys": "a", "output": "data.del"}),
            json!({"connector": "c", "keys": ["a", {"var": "data.obj"}]}),
            json!({"connector": "c", "keys": ["a", null]}),
            json!({"connector": "c", "keys": {"var": "data.many"}}),
            json!({"connector": "absent", "keys": ["a"]}),
        ];
        for input in cases {
            let v4 = run(input.clone(), data.clone(), json!({})).await;
            pin(&v4, format!("deletes_and_refuses/{input}"));
        }
    }

    /// The call's writes: none with no output, the count with one.
    #[tokio::test]
    async fn writes_nothing_without_an_output_and_the_count_with_one() {
        for (input, writes) in [
            (json!({"connector": "c", "keys": ["a"]}), 0),
            (
                json!({"connector": "c", "keys": ["a"], "output": "data.del"}),
                1,
            ),
        ] {
            let cache = twin::cache(json!({}), &[("a", "1")]).await;
            let called = testing::call(function(&cache), input.clone())
                .exprs(kit::test_exprs())
                .name(NAME)
                .await;
            assert_eq!(called.outcome.as_ref().ok(), Some(&Outcome::Ok), "{input}");
            assert_eq!(called.writes.len(), writes, "{input}: {:?}", called.writes);
            assert_eq!(cache.held(&["a"]).await, [None], "{input}");
        }
    }
}

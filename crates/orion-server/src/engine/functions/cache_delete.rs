use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::Output;
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use dataflow_rs_v3::engine::task_context::TaskContext;
use serde_json::json;

use super::cache_read::MAX_KEYS;
use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, output_declared, require_op, resolve_required_str_list, to_connect_error,
    to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::{Scope, TemplatedInput};
use crate::connector::cache_backend::{CachePool, CachePurpose};
use crate::connector::{CacheConnectorConfig, ConnectorRegistry};
use crate::engine::HandlerError;
use crate::engine::kit::{self, read};

/// This function's name, for the free functions below (F48).
const NAME: &str = <CacheDeleteHandler as ConnectorHandler>::NAME;

/// Workflow function handler for deleting exact keys from a cache backend.
///
/// The half of "cache a read until a write invalidates it" that `cache_read`
/// and `cache_write` could not express: a workflow that changes the data can
/// now drop the entries it made stale instead of waiting out their TTL.
/// Exact keys only. A prefix or pattern form would be a `SCAN` over the whole
/// keyspace on Redis; a generation counter (`cache_incr`) is the scalable way
/// to retire a family of keys.
pub struct CacheDeleteHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl ConnectorHandler for CacheDeleteHandler {
    const NAME: &'static str = "cache_delete";
    type Kind = crate::connector::kind::Cache;
    type Input = TemplatedInput;
    type Parsed = Vec<String>;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &Scope<'_, '_>,
    ) -> Result<Self::Parsed, HandlerError> {
        Ok(resolve_required_str_list(
            input, "keys", call.name, ctx, MAX_KEYS,
        )?)
    }

    fn gate(
        _keys: &Self::Parsed,
        conn: &crate::connector::CacheConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        // A delete changes what later reads see, so a read-only connector
        // refuses it exactly as it refuses `cache_write`.
        Ok(require_op(conn.operations.write, "write", connector)?)
    }

    async fn run(
        &self,
        keys: Self::Parsed,
        conn: &crate::connector::CacheConnectorConfig,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        let deleted = delete(&self.cache_pool, call.connector, conn, &keys).await?;

        // Recorded only where the task asks: the delete is the effect, and a
        // default of `data` would overwrite the message with a count.
        Ok(if output_declared(input) {
            json!({ "deleted": deleted }).into()
        } else {
            Produced::nothing()
        })
    }
}

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

    fn memory_connector(write: bool) -> crate::connector::CacheConnectorConfig {
        crate::connector::CacheConnectorConfig {
            backend: "memory".to_string(),
            url: None,
            allow_private_urls: false,
            operations: crate::connector::CacheOperationGates { read: true, write },
        }
    }

    #[test]
    fn a_read_only_connector_refuses_a_delete() {
        let err = <CacheDeleteHandler as ConnectorHandler>::gate(
            &vec!["k".to_string()],
            &memory_connector(false),
            "c",
        )
        .expect_err("a delete must be refused when writes are off");
        assert!(
            err.detail
                .as_deref()
                .unwrap_or_default()
                .contains("operation 'write' is disabled")
        );
    }

    /// The count is of keys that existed, and the output is recorded only
    /// where the task names one.
    #[tokio::test]
    async fn deletes_report_what_existed() {
        let pool = Arc::new(CachePool::new(4, 60, 128));
        let conn = memory_connector(true);
        let backend = pool
            .get_backend(CachePurpose::Workflow, "c", &conn)
            .await
            .expect("test");
        backend.set("a", "1").await.expect("test");
        backend.set("b", "2").await.expect("test");
        let h = CacheDeleteHandler {
            cache_pool: pool,
            registry: Arc::new(ConnectorRegistry::new(Default::default())),
        };
        let datalogic = Arc::new(dataflow_rs_v3::datalogic_rs::Engine::new());
        let mut message = dataflow_rs_v3::Message::from_value(&json!({}));
        let mut ctx = TaskContext::new(&mut message, &datalogic);
        let call = ConnectorCall {
            name: CacheDeleteHandler::NAME,
            connector: "c",
            channel: "ch".to_string(),
            output: "data.del".to_string(),
        };
        let keys = vec!["a".to_string(), "b".to_string(), "absent".to_string()];

        let with_output =
            TemplatedInput::from(json!({"connector": "c", "keys": [], "output": "data.del"}));
        let produced = h
            .run(keys.clone(), &conn, &call, &with_output, &mut ctx)
            .await
            .expect("test");
        assert_eq!(produced.value, Some(json!({"deleted": 2})));
        assert_eq!(backend.get("a").await.expect("test"), None);

        let without = TemplatedInput::from(json!({"connector": "c", "keys": []}));
        let produced = h
            .run(keys, &conn, &call, &without, &mut ctx)
            .await
            .expect("test");
        assert_eq!(produced.value, None, "no output declared, nothing recorded");
    }

    // -- dataflow-rs 4: the config, and twins of the handler over an
    // in-memory cache --

    use crate::engine::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::Value;

    /// The keys the twins look at after a delete.
    const LOOK: &[&str] = &["a", "b", "c"];

    fn function(cache: &twin::Cache) -> CacheDeleteFunction {
        CacheDeleteFunction {
            cache_pool: cache.pool.clone(),
            registry: cache.registry.clone(),
        }
    }

    /// One task through each engine, each against a cache of its own
    /// holding `a`, `b` and `c`, its connector configured with `config`:
    /// what each saw, and what each cache holds at [`LOOK`] after.
    async fn both(
        input: Value,
        data: Value,
        config: Value,
    ) -> ((Seen, Vec<Option<String>>), (Seen, Vec<Option<String>>)) {
        let seed: &[(&str, &str)] = &[("a", "1"), ("b", "2"), ("c", "3")];
        let cache = twin::cache(config.clone(), seed).await;
        let handler = Box::new(super::super::connector_handler::Connector(
            CacheDeleteHandler {
                cache_pool: cache.pool.clone(),
                registry: cache.registry.clone(),
            },
        ));
        let v3 = twin::v3(NAME, handler, input.clone(), data.clone()).await;
        let v3 = (v3, cache.held(LOOK).await);
        let cache = twin::cache(config, seed).await;
        let v4 = twin::v4(NAME, function(&cache), input, data).await;
        (v3, (v4, cache.held(LOOK).await))
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

    /// `deletes_report_what_existed` on v4: the count is of the keys that
    /// existed, written only where the task names an output; the keys go
    /// either way.
    #[tokio::test]
    async fn deletes_report_what_existed_on_v4() {
        let keys = json!(["a", "b", "absent"]);
        let (v3, v4) = both(
            json!({"connector": "c", "keys": keys.clone(), "output": "data.del"}),
            json!({}),
            json!({}),
        )
        .await;
        assert_eq!(v4, v3);
        assert_eq!(v4.0.expect("deleted"), json!({"del": {"deleted": 2}}));
        assert_eq!(v4.1, [None, None, Some("3".to_string())]);

        let (v3, v4) = both(
            json!({"connector": "c", "keys": keys}),
            json!({}),
            json!({}),
        )
        .await;
        assert_eq!(v4, v3);
        assert_eq!(
            v4.0.expect("deleted"),
            json!({}),
            "no output, nothing written"
        );
        assert_eq!(v4.1, [None, None, Some("3".to_string())]);
    }

    /// `a_read_only_connector_refuses_a_delete` on v4: refused before
    /// anything goes.
    #[tokio::test]
    async fn a_read_only_connector_refuses_a_delete_on_v4() {
        let input = json!({"connector": "c", "keys": ["a"], "output": "data.del"});
        let gated = json!({"operations": {"read": true, "write": false}});
        let (v3, v4) = both(input, json!({}), gated).await;
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
        assert_eq!(v4.1[0].as_deref(), Some("1"), "nothing was deleted");
    }

    /// The rest of 1.12's readings: computed keys, numbers spelled out, an
    /// output computed to `null` writing at `data` (an object, which a
    /// root takes) and an authored `null` writing nothing; and every
    /// refusal, a computed output that names no path before anything is
    /// deleted included.
    #[tokio::test]
    async fn deletes_and_refuses_as_on_v3() {
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
            let (v3, v4) = both(input.clone(), data.clone(), json!({})).await;
            assert_eq!(v4, v3, "{input}");
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

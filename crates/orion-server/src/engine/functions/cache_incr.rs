use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::{Ctx, Expr, Function, Issue, Outcome};
use dataflow_rs_v3::engine::task_context::TaskContext;
use serde_json::Value;

use super::cache_write::{rendered_ttl_secs, resolve_ttl_secs};
use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, output_declared, require_op, resolve_optional_i64, resolve_required_str,
    to_connect_error, to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::{Scope, TemplatedInput};
use crate::connector::cache_backend::{CachePool, CachePurpose};
use crate::connector::{CacheConnectorConfig, ConnectorRegistry};
use crate::engine::HandlerError;
use crate::engine::kit::{self, read};

/// This function's name, for the free functions below (F48).
const NAME: &str = <CacheIncrHandler as ConnectorHandler>::NAME;

/// Workflow function handler for an atomic increment on a cache backend.
///
/// The primitive a generation counter needs: each bump yields a value no other
/// writer can be handed, with no clock or random suffix to improvise, and a
/// cached key that embeds the generation it read retires itself the moment the
/// counter moves. It also serves plain counters and idle markers.
pub struct CacheIncrHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

/// The resolved increment.
pub struct CacheIncr {
    key: String,
    by: i64,
    ttl: Option<u64>,
}

#[async_trait]
impl ConnectorHandler for CacheIncrHandler {
    const NAME: &'static str = "cache_incr";
    type Kind = crate::connector::kind::Cache;
    type Input = TemplatedInput;
    type Parsed = CacheIncr;

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
        let by = resolve_optional_i64(input, "by", call.name, ctx)?.unwrap_or(1);
        Ok(CacheIncr {
            key,
            by,
            ttl: resolve_ttl_secs(input, call.name, ctx)?,
        })
    }

    fn gate(
        _parsed: &Self::Parsed,
        conn: &crate::connector::CacheConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        Ok(require_op(conn.operations.write, "write", connector)?)
    }

    async fn run(
        &self,
        incr: Self::Parsed,
        conn: &crate::connector::CacheConnectorConfig,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        let value = bump(
            &self.cache_pool,
            call.connector,
            conn,
            &incr.key,
            incr.by,
            incr.ttl,
        )
        .await?;

        // As `cache_delete`: recorded only where the task asks, so a bare
        // counter bump never overwrites `data` with a number.
        Ok(if output_declared(input) {
            Value::from(value).into()
        } else {
            Produced::nothing()
        })
    }
}

/// The increment, against a resolved and gated cache connector: the new
/// value. `ttl` applies only when the call creates the key. Shared by the
/// dataflow-rs 3 handler and the dataflow-rs 4 function.
async fn bump(
    pool: &CachePool,
    connector: &str,
    conn: &CacheConnectorConfig,
    key: &str,
    by: i64,
    ttl: Option<u64>,
) -> Result<i64, HandlerError> {
    let backend = pool
        .get_backend(CachePurpose::Workflow, connector, conn)
        .await
        .map_err(to_connect_error)?;
    let value = backend.incr_by(key, by, ttl).await.map_err(to_exec_error)?;
    tracing::debug!(key = %key, by, value, "Incremented cache key");
    Ok(value)
}

/// I185's Defect: the new value is a number, which no context root takes
/// (break 21), so an `output` that is one fails every call.
const ROOT_OUTPUT: &str = "`cache_incr` writes a number, and a context root takes only an \
    object (break 21): set `output` to a path below it, such as `data.count`";

/// `cache_incr`'s input on dataflow-rs 4, in its field table's order
/// (I179). The connector is a literal; `key` and `by` are expressions, read
/// by `kit::read`'s twins of `resolve_required_str` and
/// `resolve_optional_i64` (I183), so a `by` that resolves to `null` is the
/// default 1; `ttl_secs` is a document of `var` holes, as 1.12 folded it
/// (I183). The connector and the key are required at load (I183). `output`
/// is optional: unset, nothing is written (I190's primary output).
#[derive(Debug, FunctionConfig)]
pub struct CacheIncrConfig {
    /// The cache connector holding the counter.
    connector: String,
    /// The counter's key; a missing key counts as 0 and is created.
    key: Expr<OwnedDataValue>,
    /// The whole number added, 1 when unset; a negative one decrements.
    by: Option<Expr<OwnedDataValue>>,
    /// The seconds a key this call creates lives; later bumps keep it.
    ttl_secs: Option<Interpolated>,
    /// Where the new value is written; unset, nothing is.
    output: Option<Output>,
}

/// `cache_incr` as a dataflow-rs 4 function: the increment
/// `CacheIncrHandler` makes, read in the same order.
pub struct CacheIncrFunction {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

impl Function for CacheIncrFunction {
    type Config = CacheIncrConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    /// I185: the value is never an object, so an `output` set to a literal
    /// context root is a Defect at `output`. Unset, nothing is written, so
    /// there is no default root to judge.
    fn prepare(&self, cfg: &mut CacheIncrConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        match &cfg.output {
            Some(output) => kit::root_output_defect(output, ROOT_OUTPUT),
            None => Ok(()),
        }
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &CacheIncrConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, when one is set, then `parse`'s order: the
        // key, the step, the expiry. A computed output that resolves to
        // `null` is `data`, as 1.12's was.
        let output = cfg
            .output
            .as_ref()
            .map(|out| kit::output_path(ctx, out, Some(&kit::DATA)))
            .transpose()?;
        let key = ctx.eval(&cfg.key)?;
        let key = read::required_str(Some(&key), "key", NAME)?;
        let by = cfg.by.as_ref().map(|by| ctx.eval(by)).transpose()?;
        let by = read::optional_i64(by.as_deref(), "by", NAME)?.unwrap_or(1);
        let ttl = rendered_ttl_secs(ctx, cfg.ttl_secs.as_ref(), NAME)?;
        let mut value = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let conn =
                kit::require_connector::<crate::connector::kind::Cache>(&config, &cfg.connector)?;
            kit::require_op(conn.operations.write, "write", &cfg.connector)?;
            value = Some(bump(&self.cache_pool, &cfg.connector, conn, &key, by, ttl).await?);
            Ok(Outcome::Ok)
        })
        .await?;
        if let (Some(path), Some(value)) = (output, value) {
            ctx.set(&path, OwnedDataValue::from(value))?;
        }
        Ok(Outcome::Ok)
    }
}

// -- Input schema (F53) --

pub(super) const CACHE_INCR_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the cache connector holding the counter.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key",
        description: "Counter key. A missing key counts as 0 and is created. JSONLogic: a literal, or an expression over the message.",
        kind: FieldKind::String,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "by",
        description: "Amount to add, a whole number; negative decrements. Defaults to 1. JSONLogic: a literal, or an expression over the message.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "ttl_secs",
        description: "Time-to-live in seconds, applied only when this call creates the key; later bumps keep its expiry. Omit for no expiry. JSONLogic: a literal, or an expression over the message.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the new value is stored. Omit to record nothing.",
        kind: FieldKind::String,
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

    /// The keys the twins look at after a bump.
    const LOOK: &[&str] = &["n", "fresh", "7"];

    fn function(cache: &twin::Cache) -> CacheIncrFunction {
        CacheIncrFunction {
            cache_pool: cache.pool.clone(),
            registry: cache.registry.clone(),
        }
    }

    fn handler(cache: &twin::Cache) -> dataflow_rs_v3::BoxedFunctionHandler {
        Box::new(super::super::connector_handler::Connector(
            CacheIncrHandler {
                cache_pool: cache.pool.clone(),
                registry: cache.registry.clone(),
            },
        ))
    }

    /// One task through each engine, each against a cache of its own
    /// holding `n` at 41 and `text` at a string, its connector configured
    /// with `config`: what each saw, and what each cache holds at [`LOOK`]
    /// after.
    async fn both(
        input: Value,
        data: Value,
        config: Value,
    ) -> ((Seen, Vec<Option<String>>), (Seen, Vec<Option<String>>)) {
        let seed: &[(&str, &str)] = &[("n", "41"), ("text", "abc")];
        let cache = twin::cache(config.clone(), seed).await;
        let v3 = twin::v3(NAME, handler(&cache), input.clone(), data.clone()).await;
        let v3 = (v3, cache.held(LOOK).await);
        let cache = twin::cache(config, seed).await;
        let v4 = twin::v4(NAME, function(&cache), input, data).await;
        (v3, (v4, cache.held(LOOK).await))
    }

    /// Declaration order is the field table's (I179); `key` and `by` are
    /// expressions, `ttl_secs` a document of `var` holes (I183); the
    /// connector and the key are required (I183); `output` is optional and
    /// the primary output; an unknown key is refused (break 2a).
    #[test]
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = CacheIncrConfig::fields().iter().map(|f| &*f.name).collect();
        let table: Vec<&str> = CACHE_INCR_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

        let compile =
            |input: Value| testing::config_with::<CacheIncrConfig>(&kit::test_exprs(), input);
        let cfg = compile(json!({
            "connector": "c", "key": "n", "by": {"var": "data.by"},
            "ttl_secs": {"var": "data.ttl"}
        }))
        .expect("compiles");
        assert!(cfg.by.as_ref().expect("set").constant().is_none());
        let holes: Vec<String> = cfg
            .ttl_secs
            .as_ref()
            .expect("set")
            .holes()
            .map(ToString::to_string)
            .collect();
        assert_eq!(holes, ["data.ttl"]);
        assert!(cfg.output.is_none() && cfg.primary_output().is_none());
        let cfg = compile(json!({"connector": "c", "key": "n", "by": null})).expect("compiles");
        assert!(
            cfg.by.is_some(),
            "an authored null is a value, read as absent per call"
        );

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
            [("key".to_string(), IssueCode::InputParse)]
        );
        assert_eq!(
            rejected(json!({"key": "n"})),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        let issues = rejected(json!({"connector": "c", "key": "n", "step": 2}));
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].0, "step");
    }

    /// Every bump 1.12 made, the counters compared after: `by` defaulting
    /// to 1, negative, computed, and a `null` result read as absent, so 1
    /// (I183); the value written only where an output is named; a key
    /// created by the bump; and every refusal: a `by` that is not a whole
    /// number in 64 bits, a key that is no key, a stored value that is not
    /// an integer, a computed output that names no path before anything is
    /// bumped, a connector that is not there.
    #[tokio::test]
    async fn bumps_and_refuses_as_on_v3() {
        let data =
            json!({"by": 5, "neg": -2, "half": 2.5, "obj": {"a": 1}, "n": 3, "at": "data.v"});
        let cases = [
            json!({"connector": "c", "key": "n"}),
            json!({"connector": "c", "key": "n", "output": "data.v"}),
            json!({"connector": "c", "key": "n", "by": 5, "output": "data.v"}),
            json!({"connector": "c", "key": "n", "by": -50, "output": "temp_data.v"}),
            json!({"connector": "c", "key": "n", "by": {"var": "data.by"}, "output": {"var": "data.at"}}),
            json!({"connector": "c", "key": "n", "by": {"var": "data.neg"}, "output": "data.v"}),
            json!({"connector": "c", "key": "n", "by": null, "output": "data.v"}),
            json!({"connector": "c", "key": "n", "by": {"var": "data.missing"}, "output": "data.v"}),
            json!({"connector": "c", "key": "fresh", "ttl_secs": 60, "output": "data.v"}),
            json!({"connector": "c", "key": "fresh", "ttl_secs": {"var": "data.n"}}),
            json!({"connector": "c", "key": 7, "output": "data.v"}),
            json!({"connector": "c", "key": "n", "output": null}),
            // Refusals.
            json!({"connector": "c", "key": "n", "by": "2"}),
            json!({"connector": "c", "key": "n", "by": {"var": "data.half"}}),
            json!({"connector": "c", "key": "n", "by": 1e20}),
            json!({"connector": "c", "key": "n", "by": {"var": "data.obj"}}),
            json!({"connector": "c", "key": "n", "ttl_secs": "5m"}),
            json!({"connector": "c", "key": {"var": "data.none"}}),
            json!({"connector": "c", "key": "text", "output": "data.v"}),
            json!({"connector": "c", "key": "n", "output": {"var": "data.n"}}),
            json!({"connector": "absent", "key": "n"}),
            json!({"connector": "c", "key": "n", "by": {"throw": "boom"}}),
        ];
        for input in cases {
            let (v3, v4) = both(input.clone(), data.clone(), json!({})).await;
            assert_eq!(v4, v3, "{input}");
        }
        // A spot check: the bump's value, where asked, and the counter.
        let (_, (seen, held)) = both(
            json!({"connector": "c", "key": "n", "output": "data.v"}),
            json!({}),
            json!({}),
        )
        .await;
        assert_eq!(seen.expect("bumped"), json!({"v": 42}));
        assert_eq!(held[0].as_deref(), Some("42"));
    }

    /// The gate refuses a bump before the counter moves, as 1.12's did.
    #[tokio::test]
    async fn a_read_only_connector_refuses_a_bump_on_v4() {
        let input = json!({"connector": "c", "key": "n", "output": "data.v"});
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
        assert_eq!(v4.1[0].as_deref(), Some("41"), "nothing was bumped");
    }

    /// The call's writes: none with no output, the new value with one.
    #[tokio::test]
    async fn writes_nothing_without_an_output_and_the_value_with_one() {
        for (input, writes) in [
            (json!({"connector": "c", "key": "n"}), 0),
            (json!({"connector": "c", "key": "n", "output": "data.v"}), 1),
        ] {
            let cache = twin::cache(json!({}), &[("n", "1")]).await;
            let called = testing::call(function(&cache), input.clone())
                .exprs(kit::test_exprs())
                .name(NAME)
                .await;
            assert_eq!(called.outcome.as_ref().ok(), Some(&Outcome::Ok), "{input}");
            assert_eq!(called.writes.len(), writes, "{input}: {:?}", called.writes);
            assert_eq!(cache.held(&["n"]).await, [Some("2".to_string())], "{input}");
        }
    }

    /// I185, which the plan's table does not list for `cache_incr`: the
    /// value is a number, never an object, so an `output` set to a literal
    /// root is a Defect at `output`, and its write is refused at the call
    /// (break 21), where 1.12 replaced the root with the number. A
    /// computed output is judged per call: one resolving to `null` is
    /// `data`, refused at the call too. Unset, or below a root, no Defect.
    #[tokio::test]
    async fn a_root_output_is_i185s_defect() {
        // The output, whether it is a Defect, whether the call writes at a
        // root.
        for (output, defect, at_root) in [
            (Some(json!("data")), true, true),
            (Some(json!("temp_data")), true, true),
            (Some(json!({"var": "data.missing"})), false, true),
            (Some(json!("data.v")), false, false),
            (None, false, false),
        ] {
            let mut input = json!({"connector": "c", "key": "n"});
            if let Some(output) = &output {
                input["output"] = output.clone();
            }
            let cache = twin::cache(json!({}), &[]).await;
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
            } else {
                assert!(defects.is_empty(), "{input}: {defects:?}");
            }
            if at_root {
                let error = called.outcome.expect_err("a number at a root is refused");
                assert_eq!(error.code(), "CONFIG_ERROR", "{input}");
                // 1.12 replaced the root.
                let cache = twin::cache(json!({}), &[]).await;
                let v3 = twin::v3(NAME, handler(&cache), input.clone(), json!({})).await;
                assert!(v3.is_ok(), "{input}: {v3:?}");
            } else {
                assert_eq!(called.outcome.as_ref().ok(), Some(&Outcome::Ok), "{input}");
            }
        }
    }
}

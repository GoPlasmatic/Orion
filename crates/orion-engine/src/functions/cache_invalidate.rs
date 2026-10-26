//! `cache_invalidate`: bump response-cache namespaces from a workflow.
//!
//! The write-side half of `cache.namespaces` (see `channel::cache_namespace`).
//! A workflow that changes the data a namespace covers bumps it, and every
//! channel response cached under that namespace stops matching — on this node
//! and, in a cluster, through the shared store on every node.
//!
//! It takes no connector, deliberately. A response-cache entry can live in any
//! store a channel resolves — the default one or any cache connector — and the
//! author invalidating "the ladder" should not have to know which. The handler
//! bumps the namespace in every store an entry could be in. It also cannot
//! write a response body: the response cache's keyspace stays one only Orion
//! writes to (S19), and a workflow can move a counter in it, nothing else.

use super::definition::{Definition, FieldOverlay, NoConnector, OrionFunction};
use std::borrow::Cow;
use std::sync::Arc;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::Output;
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Expr, Function, Outcome, Path};

use super::connector_helpers::to_exec_error;
use super::schema::{FieldKind, RetrySafety};
use crate::connector::cache_backend::CachePool;
use crate::connector::cache_namespace;
#[cfg(test)]
use crate::kit;
use crate::kit::read;
use crate::{ErrorClass, HandlerError};

/// This function's name, written once (F48): both engines register it
/// under this, and its errors carry it.
pub const NAME: &str = "cache_invalidate";

/// Most namespaces one call may bump.
const MAX_PER_CALL: usize = 64;

/// The namespaces' own rules, after the bound: at least one, and each a
/// name a channel's `cache.namespaces` could declare.
fn check_namespaces(namespaces: &[String]) -> Result<(), HandlerError> {
    if namespaces.is_empty() {
        return Err(HandlerError::new(
            ErrorClass::CallerInput,
            "'namespaces' resolved to an empty list; name at least one",
        ));
    }
    for ns in namespaces {
        cache_namespace::check_name(ns)
            .map_err(|e| HandlerError::new(ErrorClass::CallerInput, e))?;
    }
    Ok(())
}

/// The bump `cache_invalidate` and the admin route both make, in every store a
/// response-cache entry could be in. The channel estate implements it: only it
/// knows which stores its channels cache in.
#[async_trait::async_trait]
pub trait NamespaceInvalidator: Send + Sync {
    /// Bump `namespaces` in every store, answering how many stores it reached.
    async fn invalidate_namespaces(
        &self,
        cache_pool: &CachePool,
        namespaces: &[String],
        source: &'static str,
    ) -> Result<usize, crate::errors::OrionError>;
}

/// The bump, in every store a response-cache entry could be in: how many
/// stores it reached.
async fn invalidate(
    loader: &dyn NamespaceInvalidator,
    pool: &CachePool,
    namespaces: &[String],
) -> Result<usize, HandlerError> {
    let stores = loader
        .invalidate_namespaces(pool, namespaces, "workflow")
        .await
        .map_err(to_exec_error)?;
    tracing::debug!(namespaces = ?namespaces, stores, "Invalidated response-cache namespaces");
    Ok(stores)
}

/// `cache_invalidate`'s input on dataflow-rs 4, in its field table's order
/// (I179). `namespaces` is an expression, read by `kit::read`'s twin of
/// `resolve_required_str_list` with 1.12's bound of 64 per call, and
/// required at load (I183). `output` is optional: unset, nothing is written
/// (I190's primary output).
#[derive(Debug, FunctionConfig)]
pub struct CacheInvalidateConfig {
    /// Response-cache namespaces to invalidate (at most 64), as declared in channels'
    /// `cache.namespaces`. JSONLogic: an array of literals or expressions, or an expression
    /// evaluating to one.
    namespaces: Expr<OwnedDataValue>,
    /// Dotted path where `{"namespaces": n, "stores": m}` is stored. Omit to record nothing.
    output: Option<Output>,
}

/// `cache_invalidate` as a dataflow-rs 4 function: the bump
/// `CacheInvalidateHandler` makes, read in the same order, its errors under
/// its name as 1.12's were.
pub struct CacheInvalidateFunction {
    /// Which stores a namespace's entries could be in: the serving
    /// channels', which only the channel estate knows.
    pub channel_loader: Arc<dyn NamespaceInvalidator>,
    pub cache_pool: Arc<CachePool>,
}

impl CacheInvalidateFunction {
    /// The handler's `run` up to its write: the namespaces, the output and
    /// the bump, then where the answer goes and the answer.
    async fn bump<'c>(
        &self,
        ctx: &Ctx<'_>,
        cfg: &'c CacheInvalidateConfig,
    ) -> Result<Option<(Cow<'c, Path>, OwnedDataValue)>, HandlerError> {
        let namespaces = ctx.eval(&cfg.namespaces)?;
        let namespaces =
            read::required_str_list(Some(&namespaces), "namespaces", NAME, MAX_PER_CALL)?;
        check_namespaces(&namespaces)?;
        // Resolved before anything is bumped, so a bad `output` does not leave
        // an invalidation behind a failed task. A computed one that resolves
        // to `null` is `data`, as 1.12's was.
        let output = cfg
            .output
            .as_ref()
            .map(|out| Self::DEFINITION.output_path(ctx, out))
            .transpose()?;
        let stores = invalidate(&*self.channel_loader, &self.cache_pool, &namespaces).await?;
        Ok(output.map(|path| {
            let answer = OwnedDataValue::Object(vec![
                ("namespaces".to_string(), namespaces.len().into()),
                ("stores".to_string(), stores.into()),
            ]);
            (path, answer)
        }))
    }
}

impl OrionFunction for CacheInvalidateFunction {
    type Connector = NoConnector;

    const DEFINITION: Definition = Definition {
        name: NAME,
        description: "Invalidate channel response-cache namespaces, on every store and every node.",
        category: "utility",
        retry_safety: RetrySafety::IdempotentWrite,
        fields: &[FieldOverlay {
            name: "namespaces",
            kind: Some(FieldKind::Array),
            ..FieldOverlay::DEFAULT
        }],
        stub_target: Some(""),
        stub_default: Some(offline_result),
        ..Definition::DEFAULT
    };

    fn from_deps(deps: &crate::HandlerDeps<'_>) -> Self {
        Self {
            channel_loader: deps.namespaces.clone(),
            cache_pool: deps.cache_pool.clone(),
        }
    }
}

// An offline run has no response-cache stores to invalidate.
fn offline_result(input: &serde_json::Value) -> serde_json::Value {
    let namespaces = input
        .get("namespaces")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    serde_json::json!({ "namespaces": namespaces, "stores": 0 })
}

impl Function for CacheInvalidateFunction {
    type Config = CacheInvalidateConfig;

    fn spec(&self) -> FunctionSpec {
        super::definition::spec::<Self>()
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &CacheInvalidateConfig,
    ) -> dataflow_rs::Result<Outcome> {
        let written = self
            .bump(ctx, cfg)
            .await
            .map_err(|e| dataflow_rs::Error::from(e.prefixed(NAME)))?;
        if let Some((path, answer)) = written {
            ctx.set(&path, answer)?;
        }
        Ok(Outcome::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::cache_backend::CachePurpose;
    use crate::connector::cache_namespace::version_key;
    use crate::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::Value;
    use serde_json::json;

    /// The namespaces a test looks at after a bump.
    const LOOK: &[&str] = &["ladder", "season", "a", "7"];

    /// What a node with no cache connector bumps: the built-in response
    /// cache, made as a channel's first cached response makes it.
    struct Stores {
        pool: Arc<CachePool>,
        loader: Arc<kit::LocalNamespaces>,
    }

    fn stores() -> Stores {
        let pool = Arc::new(CachePool::new(60, 128));
        pool.default_memory(CachePurpose::ResponseCache);
        Stores {
            pool,
            loader: Arc::new(kit::LocalNamespaces),
        }
    }

    impl Stores {
        fn function(&self) -> CacheInvalidateFunction {
            CacheInvalidateFunction {
                channel_loader: self.loader.clone(),
                cache_pool: self.pool.clone(),
            }
        }

        /// Each of `namespaces`' version in the built-in store, in order.
        async fn versions(&self, namespaces: &[&str]) -> Vec<Option<String>> {
            let store = self.pool.default_memory(CachePurpose::ResponseCache);
            let mut versions = Vec::with_capacity(namespaces.len());
            for ns in namespaces {
                versions.push(store.get(&version_key(ns)).await.expect("a memory read"));
            }
            versions
        }
    }

    /// One task against stores of its own: what it saw, and each namespace
    /// of [`LOOK`]'s version after.
    async fn run(input: Value, data: Value) -> (Seen, Vec<Option<String>>) {
        let stores = stores();
        let v4 = twin::v4(NAME, stores.function(), input, data).await;
        (v4, stores.versions(LOOK).await)
    }

    /// Declaration order is the field table's (I179); `namespaces` is an
    /// expression, required (I183); `output` is optional and the primary
    /// output; an unknown key is refused, as 1.12's `deny_unknown` refused it
    /// at authoring.
    #[test]
    fn the_config_compiles_and_checks_its_inputs() {
        let compile =
            |input: Value| testing::config_with::<CacheInvalidateConfig>(&kit::test_exprs(), input);
        let cfg = compile(json!({"namespaces": {"var": "data.ns"}})).expect("compiles");
        assert!(cfg.namespaces.constant().is_none());
        assert!(cfg.output.is_none() && cfg.primary_output().is_none());
        let cfg = compile(json!({"namespaces": ["a"], "output": "data.inv"})).expect("compiles");
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
            rejected(json!({})),
            [("namespaces".to_string(), IssueCode::InputParse)]
        );
        let issues = rejected(json!({"namespaces": ["a"], "connector": "c"}));
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].0, "connector");
    }

    /// Every bump 1.12 made, the versions compared after, and every
    /// refusal under the function's name, as 1.12 prefixed it: an empty
    /// list, a name a channel could not declare, more than 64, a list that
    /// is not one, and a computed output that names no path, refused before
    /// anything is bumped.
    #[tokio::test]
    async fn bumps_and_refuses_as_1_12_did() {
        let data = json!({
            "ns": ["a", "season"], "at": "data.inv", "n": 3,
            "many": (0..65).map(|i| format!("n{i}")).collect::<Vec<_>>(),
        });
        let cases = [
            json!({"namespaces": ["ladder"], "output": "data.inv"}),
            json!({"namespaces": {"var": "data.ns"}}),
            json!({"namespaces": [{"cat": ["sea", "son"]}, "ladder", 7], "output": {"var": "data.at"}}),
            json!({"namespaces": ["ladder"], "output": {"var": "data.missing"}}),
            json!({"namespaces": ["ladder"], "output": null}),
            // Refusals.
            json!({"namespaces": []}),
            json!({"namespaces": ["Ladder"]}),
            json!({"namespaces": ["a b"]}),
            json!({"namespaces": "ladder"}),
            json!({"namespaces": ["ladder", null]}),
            json!({"namespaces": {"var": "data.many"}}),
            json!({"namespaces": ["ladder"], "output": {"var": "data.n"}}),
            json!({"namespaces": {"throw": "boom"}}),
            json!({"namespaces": ["ladder"], "output": {"throw": "boom"}}),
        ];
        for input in cases {
            let (seen, versions) = run(input.clone(), data.clone()).await;
            twin::pinned(&seen, format!("bumps_and_refuses/{input}"));
            twin::pinned(
                &Ok(json!(versions)),
                format!("bumps_and_refuses/{input}/versions"),
            );
        }
        // Spot checks: the answer, and a refusal under the function's name.
        let (seen, versions) = run(
            json!({"namespaces": ["ladder"], "output": "data.inv"}),
            json!({}),
        )
        .await;
        assert_eq!(
            seen.expect("bumped"),
            json!({"inv": {"namespaces": 1, "stores": 1}})
        );
        assert_eq!(versions[0].as_deref(), Some("1"));
        let (seen, versions) = run(json!({"namespaces": []}), json!({})).await;
        let failure = seen.expect_err("an empty list");
        assert_eq!(failure.code, "VALIDATION_ERROR");
        assert_eq!(
            failure.message,
            "Validation error: cache_invalidate: 'namespaces' resolved to an empty list; \
             name at least one"
        );
        assert!(versions.iter().all(Option::is_none));
    }

    /// The call's writes: none with no output, the answer with one; the
    /// bump either way.
    #[tokio::test]
    async fn writes_nothing_without_an_output_and_the_answer_with_one() {
        for (input, writes) in [
            (json!({"namespaces": ["ladder"]}), 0),
            (json!({"namespaces": ["ladder"], "output": "data.inv"}), 1),
        ] {
            let stores = stores();
            let called = testing::call(stores.function(), input.clone())
                .exprs(kit::test_exprs())
                .name(NAME)
                .await;
            assert_eq!(called.outcome.as_ref().ok(), Some(&Outcome::Ok), "{input}");
            assert_eq!(called.writes.len(), writes, "{input}: {:?}", called.writes);
            assert_eq!(
                stores.versions(&["ladder"]).await,
                [Some("1".to_string())],
                "{input}"
            );
        }
    }
}

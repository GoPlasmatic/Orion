//! `connector_call` — unified v2 connector invocation function for `dataflow-rs` 4 (`V2-10.1`, `V2-11.1`, `V2-14`).
//!
//! Dispatches any registered [`ProviderBackend`] (`rust`, `workflow`, or `wasm`)
//! through [`ProviderRegistry`] under an owned [`RunContext`] / [`AttemptContext`],
//! enforcing reserved operation denial (`orion.internal.*`), deadline/cancellation
//! propagation, and normalized [`ConnectorResult`] / [`ConnectorError`] outcomes.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Reference, RetrySafety};
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use serde_json::{Value, json};

use crate::engine::kit::{self, read};
use crate::engine::{ErrorClass, HandlerError};
use crate::extensions::{
    AdmittedRun, AuthorityGrant, BudgetLimits, ConnectionReference, ConnectorErrorClass,
    EffectiveConnectionIdentity, ImplementationKind, ProviderReference, ProviderRegistry,
    RetryAdvice, RunContext,
};

pub const NAME: &str = "connector_call";

tokio::task_local! {
    /// Active v2 `RunContext` for the current engine execution, when started
    /// under a pinned v2 generation (`V2-05.3`).
    pub static ACTIVE_RUN_CONTEXT: RunContext;
}

#[derive(Debug, FunctionConfig)]
pub struct ConnectorCallConfig {
    /// Logical connection ID or driver identifier.
    pub connection: String,
    /// Public operation name (e.g. `"http.request"`, `"sql.query"`, `"cache.get"`).
    pub operation: String,
    /// Optional implementation kind (`"rust"`, `"workflow"`, `"wasm"`). Defaults to `"rust"`.
    pub kind: Option<String>,
    /// Optional provider target ID (`driver_id` for `rust`, `package_digest` for `workflow`/`wasm`).
    /// Defaults to `connection`.
    pub target_id: Option<String>,
    /// Operation input object with JSONLogic/`var` interpolation.
    pub input: Option<Interpolated>,
    /// Optional idempotency key expression for retry-capable write operations.
    pub idempotency_key: Option<Expr<OwnedDataValue>>,
    /// Dotted context path where the operation's `data` result is written.
    #[output(default = "data")]
    pub output: Output,
}

#[derive(Clone)]
pub struct ConnectorCallFunction {
    pub providers: ProviderRegistry,
}

impl Function for ConnectorCallFunction {
    type Config = ConnectorCallConfig;

    fn spec(&self) -> FunctionSpec {
        FunctionSpec::default()
            .with_description("Invoke a v2 connector provider operation under an owned RunContext.")
            .with_category("connector")
            .with_retry(RetrySafety::Unknown)
            .with_references(vec![Reference::new("connection", NAME)])
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &ConnectorCallConfig,
    ) -> dataflow_rs::Result<Outcome> {
        if cfg.operation.starts_with("orion.internal.") {
            return Err(HandlerError::new(
                ErrorClass::CallerInput,
                format!(
                    "CONNECTOR_OPERATION_FORBIDDEN: operation '{}' is reserved for host lifecycle/source facets",
                    cfg.operation
                ),
            )
            .into());
        }

        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
        let input_val = match &cfg.input {
            Some(doc) => kit::rendered(ctx, doc)?,
            None => json!({}),
        };
        let idempotency_key = match &cfg.idempotency_key {
            Some(expr) => {
                let evaluated = ctx.eval(expr)?;
                read::optional_str(Some(&evaluated), "idempotency_key", NAME)?
                    .map(|c| c.into_owned())
            }
            None => None,
        };

        let impl_kind = match cfg.kind.as_deref().unwrap_or("rust") {
            "workflow" => ImplementationKind::Workflow,
            "wasm" => ImplementationKind::Wasm,
            _ => ImplementationKind::Rust,
        };
        let target_id = cfg
            .target_id
            .as_deref()
            .unwrap_or(&cfg.connection)
            .to_string();

        let Some(backend) = self.providers.get(impl_kind, &target_id) else {
            return Err(HandlerError::new(
                ErrorClass::Backend,
                format!(
                    "CONNECTOR_PROVIDER_NOT_FOUND: provider ({impl_kind:?}, '{target_id}') is not registered"
                ),
            )
            .into());
        };

        let run_ctx = ACTIVE_RUN_CONTEXT
            .try_with(Clone::clone)
            .unwrap_or_else(|_| default_run_context_for_call(&cfg.connection, &cfg.operation, &target_id));

        let conn_ref = ConnectionReference {
            connection_id: cfg.connection.clone(),
            revision: 1,
            provider: ProviderReference {
                provider_id: target_id.clone(),
                version: "2.0.0".to_string(),
                package_digest: if target_id.starts_with("sha256:") {
                    target_id.clone()
                } else {
                    "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string()
                },
            },
        };
        let effective_conn = EffectiveConnectionIdentity {
            provider_digest: conn_ref.provider.package_digest.clone(),
            connection_id: cfg.connection.clone(),
            revision: 1,
            secret_hash: "sha256:none".to_string(),
        };

        let invocation = run_ctx
            .start_invocation(conn_ref, effective_conn, &cfg.operation, idempotency_key, 2)
            .map_err(connector_err_to_dataflow)?;
        let attempt = invocation
            .next_attempt()
            .map_err(connector_err_to_dataflow)?;

        let result = backend.invoke(&attempt, &cfg.operation, input_val).await;
        match result {
            Ok(out) => {
                ctx.set(&output, OwnedDataValue::from(&out.data))?;
                Ok(Outcome::Ok)
            }
            Err(error) => Err(connector_err_to_dataflow(error)),
        }
    }
}

fn default_run_context_for_call(
    connection_id: &str,
    operation: &str,
    target_id: &str,
) -> RunContext {
    let mut allowed_operations = BTreeMap::new();
    let ops_set: BTreeSet<String> = [operation.to_string()].into_iter().collect();
    allowed_operations.insert(connection_id.to_string(), ops_set.clone());
    if target_id != connection_id {
        allowed_operations.insert(target_id.to_string(), ops_set);
    }
    let grant = AuthorityGrant {
        allowed_operations,
        declared_dependencies: BTreeMap::new(),
        allowed_channels: BTreeSet::new(),
    };
    let admitted = AdmittedRun::admit(
        "workflow",
        1,
        Instant::now() + Duration::from_secs(30),
        BudgetLimits::default(),
        grant,
    );
    admitted.context()
}

fn connector_err_to_dataflow(err: crate::extensions::ConnectorError) -> dataflow_rs::Error {
    let class = match err.class {
        ConnectorErrorClass::InvalidInput
        | ConnectorErrorClass::Unauthorized
        | ConnectorErrorClass::Forbidden
        | ConnectorErrorClass::NotFound
        | ConnectorErrorClass::Conflict => ErrorClass::CallerInput,
        ConnectorErrorClass::RateLimited | ConnectorErrorClass::Unavailable => {
            if err.retry_advice == RetryAdvice::Transient {
                ErrorClass::Connector
            } else {
                ErrorClass::Backend
            }
        }
        ConnectorErrorClass::Deadline | ConnectorErrorClass::Cancelled => ErrorClass::Timeout,
        ConnectorErrorClass::ProviderFailure | ConnectorErrorClass::ContractViolation => {
            ErrorClass::Backend
        }
    };
    let msg = format!("{}: {}", err.code, err.message);
    HandlerError::new(class, msg).into()
}

/// Register `connector_call` on a `dataflow-rs` 4 [`dataflow_rs::Registry`].
pub fn register_connector_call(
    registry: &mut dataflow_rs::Registry,
    providers: ProviderRegistry,
) {
    registry.register(NAME, ConnectorCallFunction { providers });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::CompositionRoot;
    use dataflow_rs::function::testing;

    #[tokio::test]
    async fn connector_call_dispatches_memory_cache_provider_in_v4_engine() {
        let root = CompositionRoot::community();
        let func = ConnectorCallFunction {
            providers: root.providers.clone(),
        };

        // 1. Set a value in org.orion.memory-cache via connector_call
        let set_called = testing::call(
            func.clone(),
            json!({
                "connection": "local-cache",
                "target_id": "org.orion.memory-cache",
                "operation": "cache.set",
                "input": { "key": "greeting", "value": "hello v2", "ttl_ms": 60000 },
                "output": "data.set_res"
            }),
        )
        .exprs(crate::engine::kit::test_exprs())
        .name(NAME)
        .await;
        assert!(set_called.outcome.is_ok(), "{:?}", set_called.outcome);

        // 2. Read it back via connector_call
        let get_called = testing::call(
            func.clone(),
            json!({
                "connection": "local-cache",
                "target_id": "org.orion.memory-cache",
                "operation": "cache.get",
                "input": { "key": "greeting" },
                "output": "data.get_res"
            }),
        )
        .exprs(crate::engine::kit::test_exprs())
        .name(NAME)
        .await;
        assert!(get_called.outcome.is_ok(), "{:?}", get_called.outcome);
        let data = Value::from(get_called.message.context().data());
        assert_eq!(data["get_res"]["hit"], true);
        assert_eq!(data["get_res"]["value"], "hello v2");

        // 3. Verify reserved operation is rejected
        let reserved_called = testing::call(
            func,
            json!({
                "connection": "local-cache",
                "target_id": "org.orion.memory-cache",
                "operation": "orion.internal.poll",
                "output": "data.err"
            }),
        )
        .exprs(crate::engine::kit::test_exprs())
        .name(NAME)
        .await;
        let err = reserved_called.outcome.expect_err("reserved operation must fail");
        assert!(err.message().contains("CONNECTOR_OPERATION_FORBIDDEN"));
    }
}

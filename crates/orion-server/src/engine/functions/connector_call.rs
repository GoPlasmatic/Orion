//! `connector_call` — unified v2 connector invocation function for `dataflow-rs` 4 (`V2-10.1`, `V2-11.1`, `V2-14`).
//!
//! Dispatches any registered [`ProviderBackend`] (`rust`, `workflow`, or `wasm`)
//! through [`ProviderRegistry`] under an owned [`RunContext`] / [`AttemptContext`],
//! enforcing reserved operation denial (`orion.internal.*`), deadline/cancellation
//! propagation, and normalized [`ConnectorResult`] / [`ConnectorError`] outcomes.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use dataflow_rs_v3::engine::functions::AsyncFunctionHandler;
use dataflow_rs_v3::engine::task_context::TaskContext;
use dataflow_rs_v3::engine::task_outcome::TaskOutcome;
use serde_json::{Value, json};

use super::connector_helpers::{apply_output, resolve_output_path, resolve_value};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::TemplatedInput;
use crate::engine::kit::{self, read};
use crate::engine::{ErrorClass, HandlerError};
use crate::extensions::{
    AdmittedRun, AuthorityGrant, BudgetLimits, ConnectionReference, ConnectorErrorClass,
    ConnectorResult, EffectiveConnectionIdentity, ImplementationKind, ProviderReference,
    ProviderRegistry, RetryAdvice, RunContext, sha256_digest,
};

pub const NAME: &str = "connector_call";

pub(super) const CONNECTOR_CALL_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connection",
        description: "Logical connection ID or driver identifier",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "operation",
        description: "Public operation name (e.g. `http.request`, `sql.query`, `cache.get`)",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "kind",
        description: "Provider implementation kind (`rust`, `workflow`, or `wasm`; default `rust`)",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "target_id",
        description: "Optional provider target ID (`driver_id` for `rust`, `package_digest` for `workflow`/`wasm`; default `connection`)",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "input",
        description: "Operation input object (`{\"var\": ...}` references are resolved from the message context)",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "idempotency_key",
        description: "Optional idempotency key expression for retry-capable write operations",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted context path where the operation result is written (default `data`)",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

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
        kit::spec(NAME)
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &ConnectorCallConfig,
    ) -> dataflow_rs::Result<Outcome> {
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

        let data = execute_connector_call(
            &self.providers,
            &cfg.connection,
            &cfg.operation,
            cfg.kind.as_deref(),
            cfg.target_id.as_deref(),
            input_val,
            idempotency_key,
        )
        .await?;

        ctx.set(&output, OwnedDataValue::from(&data))?;
        Ok(Outcome::Ok)
    }
}

/// `dataflow-rs` 3 handler twin for `connector_call` so `build_custom_functions`
/// and `register_functions` register identical function sets (`Claim 6`).
pub struct ConnectorCallHandler {
    pub providers: ProviderRegistry,
}

#[async_trait]
impl AsyncFunctionHandler for ConnectorCallHandler {
    type Input = TemplatedInput;

    fn compile_input(
        input: &mut Self::Input,
        c: &dataflow_rs_v3::engine::functions::TemplateCompiler,
    ) -> dataflow_rs_v3::Result<()> {
        input.compile(NAME, c)
    }

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &TemplatedInput,
    ) -> dataflow_rs_v3::Result<TaskOutcome> {
        run_v3(&self.providers, ctx, input).await
    }
}

async fn run_v3(
    providers: &ProviderRegistry,
    ctx: &mut TaskContext<'_>,
    input: &TemplatedInput,
) -> dataflow_rs_v3::Result<TaskOutcome> {
    let validation_err =
        |msg: &str| -> dataflow_rs_v3::DataflowError { HandlerError::new(ErrorClass::CallerInput, msg).prefixed(NAME).into() };
    let connection = match input.get("connection").and_then(Value::as_str) {
        Some(c) if !c.is_empty() => c,
        _ => return Err(validation_err("requires 'connection' (string)")),
    };
    let operation = match input.get("operation").and_then(Value::as_str) {
        Some(op) if !op.is_empty() => op,
        _ => return Err(validation_err("requires 'operation' (string)")),
    };
    let kind = match input.get("kind") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return Err(validation_err("'kind' must be a string")),
    };
    let target_id = match input.get("target_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return Err(validation_err("'target_id' must be a string")),
    };
    let output = resolve_output_path(input, NAME, ctx)?;
    let input_val = match input.get("input") {
        Some(raw) if !raw.is_null() => resolve_value(raw, ctx),
        _ => json!({}),
    };
    let idempotency_key = match input.value_of("idempotency_key", NAME, ctx) {
        None => None,
        Some(res) => match res? {
            Value::Null => None,
            Value::String(s) => Some(s),
            _ => return Err(validation_err("'idempotency_key' must resolve to a string")),
        },
    };

    let data = execute_connector_call(
        providers,
        connection,
        operation,
        kind,
        target_id,
        input_val,
        idempotency_key,
    )
    .await
    .map_err(|e| -> dataflow_rs_v3::DataflowError { e.prefixed(NAME).into() })?;

    apply_output(ctx, &output, data);
    Ok(TaskOutcome::Success)
}

async fn execute_connector_call(
    providers: &ProviderRegistry,
    connection: &str,
    operation: &str,
    kind: Option<&str>,
    target_id: Option<&str>,
    input_val: Value,
    idempotency_key: Option<String>,
) -> Result<Value, HandlerError> {
    if operation.starts_with("orion.internal.") {
        return Err(HandlerError::new(
            ErrorClass::CallerInput,
            format!(
                "CONNECTOR_OPERATION_FORBIDDEN: operation '{operation}' is reserved for host lifecycle/source facets"
            ),
        ));
    }

    let impl_kind = match kind.unwrap_or("rust") {
        "workflow" => ImplementationKind::Workflow,
        "wasm" => ImplementationKind::Wasm,
        _ => ImplementationKind::Rust,
    };
    let target_id = target_id.unwrap_or(connection).to_string();

    let Some(_backend) = providers.get(impl_kind, &target_id) else {
        return Err(HandlerError::new(
            ErrorClass::Backend,
            format!(
                "CONNECTOR_PROVIDER_NOT_FOUND: provider ({impl_kind:?}, '{target_id}') is not registered"
            ),
        ));
    };

    let run_ctx = ACTIVE_RUN_CONTEXT
        .try_with(Clone::clone)
        .unwrap_or_else(|_| default_run_context_for_call(connection, operation, &target_id));

    // Resolve real stored connection revision and package admission status when available (`TD-25`).
    let mut resolved_revision = 1u64;
    let mut resolved_package_digest = if target_id.starts_with("sha256:") {
        target_id.clone()
    } else {
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string()
    };
    let mut resolved_provider_id = target_id.clone();
    let mut resolved_version = "2.0.0".to_string();
    let mut resolved_secret_hash = "sha256:none".to_string();

    if let Some(store) = providers.deployment_store() {
        if let Some(conn_row) = store
            .list_connections()
            .into_iter()
            .filter(|c| c.connection_id == connection)
            .max_by_key(|c| c.revision)
        {
            resolved_revision = conn_row.revision;
            resolved_package_digest = conn_row.provider_digest.clone();
            if let Some(loc) = conn_row.secret_locator.as_deref() {
                resolved_secret_hash = sha256_digest(loc.as_bytes());
            }
        }
        if let Some(pkg) = store
            .list_packages()
            .into_iter()
            .find(|p| p.package_digest == resolved_package_digest || p.provider_id == target_id)
        {
            let is_revoked = store
                .list_admissions()
                .into_iter()
                .any(|a| a.package_digest == pkg.package_digest && a.revoked);
            if is_revoked {
                return Err(HandlerError::new(
                    ErrorClass::CallerInput,
                    format!(
                        "CONNECTOR_PACKAGE_REVOKED: provider package '{}' ({}) has been revoked",
                        pkg.provider_id, pkg.package_digest
                    ),
                ));
            }
            resolved_provider_id = pkg.provider_id;
            resolved_version = pkg.version;
            resolved_package_digest = pkg.package_digest;
        }
    }

    let conn_ref = ConnectionReference {
        connection_id: connection.to_string(),
        revision: resolved_revision,
        provider: ProviderReference {
            provider_id: resolved_provider_id,
            version: resolved_version,
            package_digest: resolved_package_digest.clone(),
        },
    };
    let effective_conn = EffectiveConnectionIdentity {
        provider_digest: resolved_package_digest,
        connection_id: connection.to_string(),
        revision: resolved_revision,
        secret_hash: resolved_secret_hash,
    };

    let invocation = run_ctx
        .start_invocation(conn_ref, effective_conn, operation, idempotency_key, 2)
        .map_err(connector_err_to_handler)?;

    let admission = providers.admission_policy();
    match providers
        .dispatch_attempt(
            admission.as_ref(),
            &invocation,
            impl_kind,
            &target_id,
            input_val,
        )
        .await
    {
        ConnectorResult::Success { data, .. } => Ok(data),
        ConnectorResult::Error { error } => Err(connector_err_to_handler(error)),
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

fn connector_err_to_handler(err: crate::extensions::ConnectorError) -> HandlerError {
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
    HandlerError::new(class, msg)
}

/// Register `connector_call` on a `dataflow-rs` 4 [`dataflow_rs::Registry`].
pub fn register_connector_call(registry: &mut dataflow_rs::Registry, providers: ProviderRegistry) {
    registry.register(NAME, ConnectorCallFunction { providers });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::CompositionRoot;
    use dataflow_rs::function::testing;
    use serde_json::Value;

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
            func.clone(),
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
        let err = reserved_called
            .outcome
            .expect_err("reserved operation must fail");
        assert!(err.message().contains("CONNECTOR_OPERATION_FORBIDDEN"));

        // 4. TD-25: Verify revoked provider/connection is blocked by dispatch_attempt admission check!
        let custom_policy = std::sync::Arc::new(crate::extensions::CommunityAdmissionPolicy::new());
        custom_policy.revoke_provider("org.orion.memory-cache");
        root.providers.set_admission_policy(custom_policy);
        let revoked_called = testing::call(
            func,
            json!({
                "connection": "local-cache",
                "target_id": "org.orion.memory-cache",
                "operation": "cache.get",
                "input": { "key": "greeting" },
                "output": "data.revoked_res"
            }),
        )
        .exprs(crate::engine::kit::test_exprs())
        .name(NAME)
        .await;
        let rev_err = revoked_called
            .outcome
            .expect_err("revoked provider must be blocked by dispatch_attempt");
        assert!(
            rev_err.message().contains("CONNECTOR_CONNECTION_FORBIDDEN"),
            "unexpected error: {}",
            rev_err.message()
        );
    }

    #[tokio::test]
    async fn claim6_connector_call_registered_across_serving_validation_and_offline_stubbing() {
        let builtin = crate::engine::FunctionRegistry::builtin();
        assert!(builtin.contains(NAME));
        assert!(
            builtin
                .validate_input(
                    NAME,
                    &json!({
                        "connection": "local-cache",
                        "operation": "cache.get",
                        "input": { "key": "k" },
                        "output": "data.out"
                    }),
                    "tasks[0]"
                )
                .is_empty()
        );

        let offline = crate::engine::kit::OfflineDeps::new();
        let custom_fns = crate::engine::build_custom_functions(offline.deps());
        assert!(custom_fns.contains_key(NAME));

        let serving_reg = crate::engine::serving_registry(offline.deps(), None);
        assert!(serving_reg.catalogue().any(|i| i.name == NAME));

        let stub_log = std::sync::Arc::new(crate::engine::functions::stub::CallLog::new());
        let mut stubs = crate::engine::functions::stub::StubTable::new();
        stubs.insert(
            NAME.to_string(),
            [("local-cache".to_string(), json!({"hit": true, "value": 99}))]
                .into_iter()
                .collect(),
        );
        let stub_fns = crate::engine::functions::stub::build_stub_functions_with_log(
            stubs,
            stub_log.clone(),
        );
        assert!(stub_fns.contains_key(NAME));
    }
}

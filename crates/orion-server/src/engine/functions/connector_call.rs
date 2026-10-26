//! `connector_call` — unified v2 connector invocation function for `dataflow-rs` 4 (`V2-10.1`, `V2-11.1`, `V2-14`).
//!
//! Dispatches any registered [`ProviderBackend`](crate::extensions::ProviderBackend) (`rust`, `workflow`, or `wasm`)
//! through [`ProviderRegistry`] under an owned [`RunContext`] / [`AttemptContext`](crate::extensions::context::AttemptContext),
//! enforcing reserved operation denial (`orion.internal.*`), deadline/cancellation
//! propagation, and normalized [`ConnectorResult`] / [`ConnectorError`](crate::extensions::ConnectorError) outcomes.

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::program::{AuthoredPath, IssueCode};
use dataflow_rs::{Ctx, Expr, Function, Issue, Outcome};
use serde_json::{Value, json};

use super::schema::{FieldKind, FieldSchema};
use crate::engine::kit::{self, read};
use crate::engine::{ErrorClass, HandlerError};
use crate::extensions::{
    ConnectionReference, ConnectorErrorClass, ConnectorResult, EffectiveConnectionIdentity,
    ImplementationKind, ProviderReference, ProviderRegistry, RetryAdvice, RunContext,
    sha256_digest,
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
    /// Where the circuit breakers live, keyed by (channel, connection) as
    /// every connector function's are.
    pub registry: std::sync::Arc<crate::connector::ConnectorRegistry>,
}

impl Function for ConnectorCallFunction {
    type Config = ConnectorCallConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    /// The input keys the provider names as statements
    /// ([`ProviderBackend::literal_input_keys`](crate::extensions::ProviderBackend::literal_input_keys))
    /// must be authored as literal strings, so the message reaches the
    /// backend only through the values bound beside them. A provider not
    /// registered on this node is judged when the call runs.
    fn prepare(&self, cfg: &mut ConnectorCallConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        let Some(kind) = implementation_kind(cfg.kind.as_deref()) else {
            let mut at = AuthoredPath::root();
            at.push("kind");
            return Err(vec![Issue::new(
                IssueCode::InputParse,
                at,
                unknown_kind(cfg.kind.as_deref()).to_string(),
            )]);
        };
        let target = cfg.target_id.as_deref().unwrap_or(&cfg.connection);
        let Some(backend) = self.providers.get(kind, target) else {
            return Ok(());
        };
        let Some(input) = cfg.input.as_ref() else {
            return Ok(());
        };
        let issues: Vec<Issue> = backend
            .literal_input_keys(&cfg.operation)
            .iter()
            .filter(|key| input.raw().get(**key).is_some_and(|v| !v.is_string()))
            .map(|key| {
                let mut at = AuthoredPath::root();
                at.push("input");
                at.push(*key);
                Issue::new(
                    IssueCode::InputParse,
                    at,
                    format!(
                        "`input.{key}` must be a literal string: the statement is authored in \
                         the definition, and the message reaches it only through `input.params`"
                    ),
                )
            })
            .collect();
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
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

        // The connector shell every connector function runs in: the breaker
        // for this (channel, connection) pair, and the connector metrics.
        let channel = kit::channel(ctx);
        let workflow_id = ctx.info().workflow_id;
        let mut data = None;
        kit::guarded(NAME, &self.registry, &cfg.connection, channel, async {
            data = Some(
                execute_connector_call(
                    &self.providers,
                    workflow_id,
                    cfg,
                    input_val,
                    idempotency_key,
                )
                .await?,
            );
            Ok(Outcome::Ok)
        })
        .await?;
        let data = data.unwrap_or(Value::Null);

        ctx.set(&output, OwnedDataValue::from(&data))?;
        Ok(Outcome::Ok)
    }
}

/// One `connector_call`, refused unless every fact it rests on is governed:
/// the run's context, the workflow's grant, the connection's revision and
/// the provider's identity. Nothing is assumed when one is missing.
async fn execute_connector_call(
    providers: &ProviderRegistry,
    workflow_id: &str,
    cfg: &ConnectorCallConfig,
    input_val: Value,
    idempotency_key: Option<String>,
) -> Result<Value, HandlerError> {
    let (connection, operation, kind) = (
        cfg.connection.as_str(),
        cfg.operation.as_str(),
        cfg.kind.as_deref(),
    );
    if operation.starts_with("orion.internal.") {
        return Err(HandlerError::new(
            ErrorClass::CallerInput,
            format!(
                "CONNECTOR_OPERATION_FORBIDDEN: operation '{operation}' is reserved for host lifecycle/source facets"
            ),
        ));
    }

    let impl_kind = implementation_kind(kind).ok_or_else(|| unknown_kind(kind))?;
    let mut target_id = cfg.target_id.as_deref().unwrap_or(connection).to_string();
    // An uploaded `workflow` or `wasm` package registers under its digest,
    // which the connection's revision names: with no `target_id` the target
    // is resolved from it below.
    let target_from_connection = impl_kind != ImplementationKind::Rust && cfg.target_id.is_none();

    // A target named in the task is resolved now, and the backend found
    // here is the one dispatched to: one lookup per call. A target taken
    // from the connection's package is resolved once that is read.
    let mut backend = None;
    if !target_from_connection {
        backend = Some(
            providers
                .get(impl_kind, &target_id)
                .ok_or_else(|| not_registered(impl_kind, &target_id))?,
        );
    }

    let run_ctx = ACTIVE_RUN_CONTEXT.try_with(Clone::clone).map_err(|_| {
        forbidden(format!(
            "CONNECTOR_CONNECTION_FORBIDDEN: connector_call runs only inside a served run; \
             '{operation}' on '{connection}' was called outside one"
        ))
    })?;
    let granted = crate::runtime::generation::PINNED_GENERATION
        .try_with(|generation| {
            generation
                .connector_authority
                .for_workflow(workflow_id)
                .is_some_and(|grant| grant.allows(connection, operation))
        })
        .unwrap_or(false);
    if !granted {
        return Err(forbidden(format!(
            "CONNECTOR_CONNECTION_FORBIDDEN: workflow '{workflow_id}' may not call '{operation}' \
             on '{connection}': its deployment manifest member does not require that connection"
        )));
    }

    let store = providers.deployment_store();
    // The revision the manifest deploys, never one stored since.
    let deployed = crate::runtime::generation::PINNED_GENERATION
        .try_with(|generation| {
            generation
                .connector_authority
                .connection_revision(connection)
        })
        .ok()
        .flatten();
    let Some(conn_row) = deployed.and_then(|revision| store.get_connection(connection, revision))
    else {
        return Err(forbidden(format!(
            "CONNECTOR_CONNECTION_FORBIDDEN: connection '{connection}' is not a member of the \
             deployed manifest"
        )));
    };
    let provider = match store.get_package(&conn_row.provider_digest) {
        Some(pkg) => {
            let revoked = store
                .get_admission(&pkg.package_digest)
                .is_some_and(|a| a.revoked);
            if revoked {
                return Err(HandlerError::new(
                    ErrorClass::CallerInput,
                    format!(
                        "CONNECTOR_PACKAGE_REVOKED: provider package '{}' ({}) has been revoked",
                        pkg.provider_id, pkg.package_digest
                    ),
                ));
            }
            if impl_kind != ImplementationKind::Rust {
                if pkg.implementation != impl_kind {
                    return Err(forbidden(format!(
                        "CONNECTOR_CONNECTION_FORBIDDEN: connection '{connection}' runs on the \
                         {} package '{}', not a {} one",
                        pkg.implementation.as_str(),
                        pkg.provider_id,
                        impl_kind.as_str()
                    )));
                }
                if !target_from_connection && target_id != pkg.backend_id() {
                    return Err(forbidden(format!(
                        "CONNECTOR_CONNECTION_FORBIDDEN: connection '{connection}' runs on \
                         package '{}' ({}), not '{target_id}'",
                        pkg.provider_id,
                        pkg.backend_id()
                    )));
                }
                target_id = pkg.backend_id().to_string();
            }
            // The input the package declares for this operation, checked
            // before anything is dispatched to the provider.
            if let Some(Err(finding)) = store.check_package_instance(
                &pkg.package_digest,
                crate::extensions::deployment_store::PackageSchema::OperationInput(operation),
                &input_val,
            ) {
                let field = finding.instance_field();
                let at = if field.is_empty() {
                    "the input".to_string()
                } else {
                    format!("`input.{field}`")
                };
                return Err(connector_error(
                    crate::extensions::ConnectorError::invalid_input(
                        format!(
                            "'{operation}' on '{connection}': {at} does not satisfy provider \
                             '{}''s input_schema ({} refused it)",
                            pkg.provider_id, finding.location
                        ),
                        Some(json!({
                            "pointer": finding.instance_path,
                            "keyword": finding.location,
                        })),
                    ),
                ));
            }
            ProviderReference {
                provider_id: pkg.provider_id.clone(),
                version: pkg.version.clone(),
                package_digest: pkg.package_digest.clone(),
            }
        }
        // A provider built into this binary has no uploaded package: the
        // revision names it by its built-in digest, which must be the
        // driver this call targets.
        None => match crate::extensions::foundation::builtin_driver(&conn_row.provider_digest) {
            Some(driver) if impl_kind == ImplementationKind::Rust && driver == target_id => {
                ProviderReference {
                    provider_id: driver.to_string(),
                    version: crate::version::ORION_VERSION.to_string(),
                    package_digest: conn_row.provider_digest.clone(),
                }
            }
            Some(driver) => {
                return Err(forbidden(format!(
                    "CONNECTOR_CONNECTION_FORBIDDEN: connection '{connection}' names the \
                     built-in provider '{driver}', not ({impl_kind:?}, '{target_id}')"
                )));
            }
            None => {
                return Err(forbidden(format!(
                    "CONNECTOR_CONNECTION_FORBIDDEN: connection '{connection}' names provider \
                     package '{}', which is not uploaded",
                    conn_row.provider_digest
                )));
            }
        },
    };

    let effective_conn = effective_identity(&conn_row);
    let conn_ref = ConnectionReference {
        connection_id: connection.to_string(),
        revision: conn_row.revision,
        provider,
    };

    let invocation = run_ctx
        .start_invocation(conn_ref, effective_conn, operation, idempotency_key, 2)
        .map_err(connector_error)?;

    let backend = match backend {
        Some(backend) => backend,
        None => providers
            .get(impl_kind, &target_id)
            .ok_or_else(|| not_registered(impl_kind, &target_id))?,
    };
    match providers
        .dispatch_attempt(&invocation, backend.as_ref(), input_val)
        .await
    {
        ConnectorResult::Success { data, .. } => Ok(data),
        ConnectorResult::Error { error } => Err(connector_error(error)),
    }
}

/// The identity a call over `conn_row` runs under: its provider, the
/// connection and revision, and a hash of where its secrets come from —
/// never of a secret, and a revision reading another vault path is another
/// identity.
pub(crate) fn effective_identity(
    conn_row: &crate::extensions::deployment_store::ConnectionRevisionRow,
) -> EffectiveConnectionIdentity {
    EffectiveConnectionIdentity {
        provider_digest: conn_row.provider_digest.clone(),
        connection_id: conn_row.connection_id.clone(),
        revision: conn_row.revision,
        secret_hash: if conn_row.secret_locators.is_empty() {
            String::new()
        } else {
            sha256_digest(
                crate::canonical::canonical_json(&serde_json::json!(conn_row.secret_locators))
                    .as_bytes(),
            )
        },
    }
}

fn not_registered(kind: ImplementationKind, target_id: &str) -> HandlerError {
    HandlerError::new(
        ErrorClass::Backend,
        format!(
            "CONNECTOR_PROVIDER_NOT_FOUND: provider ({kind:?}, '{target_id}') is not registered"
        ),
    )
}

fn forbidden(message: String) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, message)
}

fn unknown_kind(kind: Option<&str>) -> HandlerError {
    HandlerError::new(
        ErrorClass::CallerInput,
        format!(
            "connector_call: unknown kind '{}' (one of rust, workflow, wasm)",
            kind.unwrap_or_default()
        ),
    )
}

/// The implementation kind a task names; `rust` when it names none, and
/// `None` for a name that is not one.
fn implementation_kind(kind: Option<&str>) -> Option<ImplementationKind> {
    ImplementationKind::parse(kind.unwrap_or("rust"))
}

/// A connector failure as the task error it is reported as.
pub(crate) fn connector_error(err: crate::extensions::ConnectorError) -> HandlerError {
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
pub fn register_connector_call(
    registry: &mut dataflow_rs::Registry,
    providers: ProviderRegistry,
    connectors: std::sync::Arc<crate::connector::ConnectorRegistry>,
) {
    registry.register(
        NAME,
        ConnectorCallFunction {
            providers,
            registry: connectors,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::Composition;

    fn community() -> Composition {
        crate::extensions::CompositionRoot::community()
            .build_in_memory()
            .expect("community composes")
    }
    use crate::extensions::context::{AdmittedRun, AuthorityGrant, BudgetLimits};
    use dataflow_rs::function::testing;
    use serde_json::Value;
    use std::sync::Arc;

    /// The built-in digest of the driver serving a test connection, which
    /// its revision names as its provider.
    fn digest_for(connection: &str) -> String {
        use crate::extensions::foundation::{
            DRIVER_MEMORY_CACHE, DRIVER_POSTGRESQL, DRIVER_SQLITE, builtin_provider_digest,
        };
        let driver = match connection {
            "dead-db" => DRIVER_POSTGRESQL,
            "local-db" | "bound-params-db" => DRIVER_SQLITE,
            _ => DRIVER_MEMORY_CACHE,
        };
        builtin_provider_digest(driver).expect("a built-in driver")
    }

    /// Run `call` as a served run would: inside a run context and a
    /// generation granting the harness's workflow (`test`) `grants`, each
    /// connection with a revision in `root`'s store.
    async fn served(
        root: &Composition,
        grants: &[(&str, &str)],
        call: testing::Call<ConnectorCallFunction>,
    ) -> testing::Called {
        let mut grant = AuthorityGrant::default();
        let pool = crate::storage::test_sqlite_pool().await;
        for (connection, operation) in grants {
            // A revision the test stored itself is kept: revisions are
            // immutable, and a second store of revision 1 would be refused.
            let store = root.providers().deployment_store();
            if store.get_connection(connection, 1).is_some() {
                grant
                    .allowed_operations
                    .entry((*connection).to_string())
                    .or_default()
                    .insert((*operation).to_string());
                continue;
            }
            let _ = store
                .store_connection_revision_in_db(
                    &pool,
                    connection,
                    1,
                    &digest_for(connection),
                    json!({}),
                    Default::default(),
                )
                .await;
            grant
                .allowed_operations
                .entry((*connection).to_string())
                .or_default()
                .insert((*operation).to_string());
        }
        let handle = crate::runtime::generation::test_handle();
        handle.publish_parts(crate::runtime::generation::GenerationParts {
            connector_authority: Arc::new(crate::runtime::authority::ConnectorAuthority::for_test(
                "test",
                grant.clone(),
            )),
            ..crate::runtime::generation::GenerationParts::new(
                dataflow_rs::Engine::builder()
                    .build()
                    .expect("an empty engine builds"),
                Arc::new(crate::channel::ChannelSnapshot::empty()),
                crate::engine::FunctionRegistry::builtin().clone(),
                Arc::new(crate::plugin::PluginSet::empty()),
            )
        });
        let run = AdmittedRun::admit(
            "run",
            1,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
            BudgetLimits::default(),
            grant,
        )
        .context();
        crate::runtime::generation::PINNED_GENERATION
            .scope(
                handle.load(),
                ACTIVE_RUN_CONTEXT.scope(run, std::future::IntoFuture::into_future(call)),
            )
            .await
    }

    fn cache_call(
        func: &ConnectorCallFunction,
        operation: &str,
        input: Value,
    ) -> testing::Call<ConnectorCallFunction> {
        testing::call(
            func.clone(),
            json!({
                "connection": "local-cache",
                "target_id": "org.orion.memory-cache",
                "operation": operation,
                "input": input,
                "output": "data.res"
            }),
        )
        .exprs(crate::engine::kit::test_exprs())
        .name(NAME)
    }

    /// A retryable failure counts against the (channel, connection) breaker,
    /// as every connector function's does, and an open breaker refuses the
    /// next call before it reaches the provider.
    #[tokio::test]
    async fn retryable_failures_trip_the_connection_breaker() {
        let root = community();
        let pool = crate::storage::test_sqlite_pool().await;
        root.providers()
            .deployment_store()
            .store_connection_revision_in_db(
                &pool,
                "dead-db",
                1,
                &digest_for("dead-db"),
                json!({"connection_string": "postgres://127.0.0.1:1/nothing"}),
                Default::default(),
            )
            .await
            .expect("revision stored");
        let func = ConnectorCallFunction {
            providers: root.providers().clone(),
            registry: std::sync::Arc::new(crate::connector::ConnectorRegistry::new(
                crate::connector::circuit_breaker::CircuitBreakerConfig {
                    enabled: true,
                    failure_threshold: 1,
                    recovery_timeout_secs: 60,
                    max_breakers: 16,
                },
            )),
        };
        let query = || {
            testing::call(
                func.clone(),
                json!({
                    "connection": "dead-db",
                    "target_id": "org.orion.postgresql",
                    "operation": "query",
                    "input": {"sql": "SELECT 1"},
                    "output": "data.rows"
                }),
            )
            .exprs(crate::engine::kit::test_exprs())
            .name(NAME)
        };
        let grants = &[("dead-db", "query")];
        let first = served(&root, grants, query()).await;
        let err = first.outcome.expect_err("nothing listens on port 1");
        assert!(err.is_retryable(), "{}", err.message());
        let second = served(&root, grants, query()).await;
        let err = second.outcome.expect_err("the breaker is open");
        assert!(
            err.message().contains("circuit") || format!("{err:?}").contains("circuit"),
            "{err:?}"
        );
    }

    /// Outside a served run, without a grant for its workflow, or naming a
    /// connection with no revision, a call is refused rather than run on
    /// an invented context.
    #[tokio::test]
    async fn a_call_without_governed_facts_is_refused() {
        let root = community();
        let func = ConnectorCallFunction {
            providers: root.providers().clone(),
            registry: std::sync::Arc::new(crate::connector::ConnectorRegistry::new(
                Default::default(),
            )),
        };
        let outside = cache_call(&func, "cache.get", json!({"key": "k"})).await;
        let err = outside.outcome.expect_err("refused outside a run");
        assert!(
            err.message().contains("inside a served run"),
            "{}",
            err.message()
        );

        let ungranted = served(
            &root,
            &[("local-cache", "cache.set")],
            cache_call(&func, "cache.get", json!({"key": "k"})),
        )
        .await;
        let err = ungranted.outcome.expect_err("refused without a grant");
        assert!(
            err.message().contains("does not require that connection"),
            "{}",
            err.message()
        );

        let unknown_kind = testing::call(
            func.clone(),
            json!({"connection": "c", "operation": "o", "kind": "python", "output": "data.x"}),
        )
        .exprs(crate::engine::kit::test_exprs())
        .name(NAME)
        .await;
        assert!(unknown_kind.outcome.is_err());
        assert!(
            unknown_kind
                .issues
                .iter()
                .any(|i| i.message.contains("unknown kind 'python'")),
            "{:?}",
            unknown_kind.issues
        );
    }

    /// A SQL statement built from the message is refused when the task
    /// compiles, so a caller's values reach the database only as binds.
    #[tokio::test]
    async fn a_statement_built_from_the_message_is_refused_at_load() {
        let root = community();
        for target in [
            "org.orion.sqlite",
            "org.orion.postgresql",
            "org.orion.mysql",
        ] {
            let func = ConnectorCallFunction {
                providers: root.providers().clone(),
                registry: std::sync::Arc::new(crate::connector::ConnectorRegistry::new(
                    Default::default(),
                )),
            };
            let called = testing::call(
                func,
                json!({
                    "connection": "local-db",
                    "target_id": target,
                    "operation": "query",
                    "input": { "sql": {"var": "data.q"} },
                    "output": "data.rows"
                }),
            )
            .exprs(crate::engine::kit::test_exprs())
            .name(NAME)
            .await;
            assert!(called.outcome.is_err(), "{target}: {:?}", called.outcome);
            assert!(
                called
                    .issues
                    .iter()
                    .any(|i| i.code == IssueCode::InputParse && i.message.contains("input.sql")),
                "{target}: {:?}",
                called.issues
            );
        }
    }

    /// A literal statement runs, with the message's values bound to its
    /// placeholders.
    #[tokio::test]
    async fn a_literal_statement_runs_with_the_message_bound_as_params() {
        let root = community();
        let func = ConnectorCallFunction {
            providers: root.providers().clone(),
            registry: std::sync::Arc::new(crate::connector::ConnectorRegistry::new(
                Default::default(),
            )),
        };
        let message = dataflow_rs::Message::from_parts(
            OwnedDataValue::from(&json!({})),
            OwnedDataValue::from(&json!({"n": 41, "label": "x'; DROP TABLE t; --"})),
            OwnedDataValue::Null,
        );
        let called = served(
            &root,
            &[("bound-params-db", "query")],
            testing::call(
                func,
                json!({
                    "connection": "bound-params-db",
                    "target_id": "org.orion.sqlite",
                    "operation": "query",
                    "input": {
                        "sql": "SELECT ? + 1 AS n, ? AS label",
                        "params": [{"var": "data.n"}, {"var": "data.label"}]
                    },
                    "output": "data.result"
                }),
            )
            .message(message)
            .exprs(crate::engine::kit::test_exprs())
            .name(NAME),
        )
        .await;
        assert!(called.outcome.is_ok(), "{:?}", called.outcome);
        let result = called.message.to_json()["context"]["data"]["result"].clone();
        assert_eq!(result["rows"][0]["n"], 42);
        assert_eq!(result["rows"][0]["label"], "x'; DROP TABLE t; --");
    }

    #[tokio::test]
    async fn connector_call_dispatches_memory_cache_provider_in_v4_engine() {
        let root = community();
        let func = ConnectorCallFunction {
            providers: root.providers().clone(),
            registry: std::sync::Arc::new(crate::connector::ConnectorRegistry::new(
                Default::default(),
            )),
        };

        // 1. Set a value in org.orion.memory-cache via connector_call
        let set_called = served(
            &root,
            &[("local-cache", "cache.set"), ("local-cache", "cache.get")],
            testing::call(
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
            .name(NAME),
        )
        .await;
        assert!(set_called.outcome.is_ok(), "{:?}", set_called.outcome);

        // 2. Read it back via connector_call
        let get_called = served(
            &root,
            &[("local-cache", "cache.set"), ("local-cache", "cache.get")],
            testing::call(
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
            .name(NAME),
        )
        .await;
        assert!(get_called.outcome.is_ok(), "{:?}", get_called.outcome);
        let data = Value::from(get_called.message.context().data());
        assert_eq!(data["get_res"]["hit"], true);
        assert_eq!(data["get_res"]["value"], "hello v2");

        // 3. Verify reserved operation is rejected
        let reserved_called = served(
            &root,
            &[("local-cache", "cache.set"), ("local-cache", "cache.get")],
            testing::call(
                func.clone(),
                json!({
                    "connection": "local-cache",
                    "target_id": "org.orion.memory-cache",
                    "operation": "orion.internal.poll",
                    "output": "data.err"
                }),
            )
            .exprs(crate::engine::kit::test_exprs())
            .name(NAME),
        )
        .await;
        let err = reserved_called
            .outcome
            .expect_err("reserved operation must fail");
        assert!(err.message().contains("CONNECTOR_OPERATION_FORBIDDEN"));

        // 4. TD-25: Verify revoked provider/connection is blocked by dispatch_attempt admission check!
        root.admission()
            .ledger()
            .revoke_provider("org.orion.memory-cache");
        let revoked_called = served(
            &root,
            &[("local-cache", "cache.set"), ("local-cache", "cache.get")],
            testing::call(
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
            .name(NAME),
        )
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

    /// A connection naming one built-in driver by its digest cannot be
    /// called as another: the revision says which provider serves it.
    #[tokio::test]
    async fn a_connection_is_served_only_by_the_builtin_driver_it_names() {
        let root = community();
        let func = ConnectorCallFunction {
            providers: root.providers().clone(),
            registry: std::sync::Arc::new(crate::connector::ConnectorRegistry::new(
                Default::default(),
            )),
        };
        let called = served(
            &root,
            &[("local-cache", "query")],
            testing::call(
                func,
                json!({
                    "connection": "local-cache",
                    "target_id": "org.orion.sqlite",
                    "operation": "query",
                    "input": { "sql": "SELECT 1" },
                    "output": "data.rows"
                }),
            )
            .exprs(crate::engine::kit::test_exprs())
            .name(NAME),
        )
        .await;
        let err = called.outcome.expect_err("another driver is refused");
        assert!(
            err.message().contains("CONNECTOR_CONNECTION_FORBIDDEN")
                && err
                    .message()
                    .contains("built-in provider 'org.orion.memory-cache'"),
            "{}",
            err.message()
        );
    }

    #[tokio::test]
    async fn connector_call_registered_across_serving_validation_and_offline_stubbing() {
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
        let serving_reg = crate::engine::serving_registry(offline.deps());
        assert!(serving_reg.catalogue().any(|i| i.name == NAME));

        assert!(
            crate::engine::functions::stub::stubbable_functions().any(|name| name == NAME),
            "an offline run answers connector_call from its stubs"
        );
    }
}

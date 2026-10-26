//! `connector_call` — the unified v2 connector invocation function (`V2-10.1`,
//! `V2-11.1`, `V2-14`).
//!
//! Dispatches to a registered provider backend (`rust`, `workflow`, or `wasm`)
//! through a [`ConnectorCallHost`], which the serving runtime implements over
//! its composed providers and the deployment's authority. This module owns
//! the task's input, reserved-operation denial (`orion.internal.*`) and how a
//! [`ConnectorError`] is reported.

use super::definition::{Definition, FieldOverlay, NoConnector, OrionFunction};
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::program::{AuthoredPath, IssueCode};
use dataflow_rs::{Ctx, Expr, Function, Issue, Outcome};
use serde_json::{Value, json};

use super::schema::{FieldKind, RetrySafety};
use crate::kit::{self, read};
use crate::{ErrorClass, HandlerError};
use orion_extension_api::{ConnectorError, ConnectorErrorClass, ImplementationKind, RetryAdvice};

pub const NAME: &str = "connector_call";

#[derive(Debug, FunctionConfig)]
pub struct ConnectorCallConfig {
    /// Logical connection ID or driver identifier
    pub connection: String,
    /// Public operation name (e.g. `http.request`, `sql.query`, `cache.get`)
    pub operation: String,
    /// Provider implementation kind (`rust`, `workflow`, or `wasm`; default `rust`)
    pub kind: Option<String>,
    /// Optional provider target ID (`driver_id` for `rust`, `package_digest` for
    /// `workflow`/`wasm`; default `connection`)
    pub target_id: Option<String>,
    /// Operation input object (`{"var": ...}` references are resolved from the message context)
    pub input: Option<Interpolated>,
    /// Optional idempotency key expression for retry-capable write operations
    pub idempotency_key: Option<Expr<OwnedDataValue>>,
    /// Dotted context path where the operation result is written (default `data`)
    #[output(default = "data")]
    pub output: Output,
}

/// The node a `connector_call` dispatches into: the providers it composes and
/// the authority a deployment grants. The engine knows neither; the serving
/// runtime implements this.
#[async_trait::async_trait]
pub trait ConnectorCallHost: Send + Sync {
    /// The input keys `operation` on `(kind, target)` reads as literal
    /// strings, when a provider for it is registered; empty otherwise.
    fn literal_input_keys(
        &self,
        kind: ImplementationKind,
        target: &str,
        operation: &str,
    ) -> &'static [&'static str];

    /// Run the call: authority, the connection's deployed revision, its
    /// provider, and the dispatch.
    async fn invoke(&self, call: ConnectorInvocation<'_>) -> Result<Value, HandlerError>;
}

/// One call, as the task resolved it.
pub struct ConnectorInvocation<'a> {
    /// The workflow whose task this is: the authority is granted per workflow.
    pub workflow_id: &'a str,
    /// The task's input, as authored.
    pub config: &'a ConnectorCallConfig,
    /// `input`, rendered against the message.
    pub input: Value,
    /// `idempotency_key`, evaluated.
    pub idempotency_key: Option<String>,
}

#[derive(Clone)]
pub struct ConnectorCallFunction {
    /// The providers and the deployment's authority.
    pub host: std::sync::Arc<dyn ConnectorCallHost>,
    /// Where the circuit breakers live, keyed by (channel, connection) as
    /// every connector function's are.
    pub registry: std::sync::Arc<crate::connector::ConnectorRegistry>,
}

impl OrionFunction for ConnectorCallFunction {
    type Connector = NoConnector;

    const DEFINITION: Definition = Definition {
        name: NAME,
        description: "Invoke a v2 connector provider operation under an owned RunContext.",
        category: "connector",
        retry_safety: RetrySafety::DependsOn { input: "operation" },
        fields: &[
            FieldOverlay {
                name: "input",
                kind: Some(FieldKind::Object),
                resolvable: true,
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "idempotency_key",
                kind: Some(FieldKind::String),
                ..FieldOverlay::DEFAULT
            },
        ],
        stub_target: Some("connection"),
        ..Definition::DEFAULT
    };

    fn from_deps(deps: &crate::HandlerDeps<'_>) -> Self {
        Self {
            host: deps.connector_calls.clone(),
            registry: deps.registry.clone(),
        }
    }
}

impl Function for ConnectorCallFunction {
    type Config = ConnectorCallConfig;

    fn spec(&self) -> FunctionSpec {
        super::definition::spec::<Self>()
    }

    /// The input keys the provider names as statements
    /// (`ProviderBackend::literal_input_keys`)
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
        let Some(input) = cfg.input.as_ref() else {
            return Ok(());
        };
        let issues: Vec<Issue> = self
            .host
            .literal_input_keys(kind, target, &cfg.operation)
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
        let output = Self::DEFINITION.output_path(ctx, &cfg.output)?;
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
                self.host
                    .invoke(ConnectorInvocation {
                        workflow_id,
                        config: cfg,
                        input: input_val,
                        idempotency_key,
                    })
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

pub fn not_registered(kind: ImplementationKind, target_id: &str) -> HandlerError {
    HandlerError::new(
        ErrorClass::Backend,
        format!(
            "CONNECTOR_PROVIDER_NOT_FOUND: provider ({kind:?}, '{target_id}') is not registered"
        ),
    )
}

pub fn forbidden(message: String) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, message)
}

pub fn unknown_kind(kind: Option<&str>) -> HandlerError {
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
pub fn implementation_kind(kind: Option<&str>) -> Option<ImplementationKind> {
    ImplementationKind::parse(kind.unwrap_or("rust"))
}

/// A connector failure as the task error it is reported as.
pub fn connector_error(err: ConnectorError) -> HandlerError {
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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::program::{AuthoredPath, IssueCode};
use dataflow_rs::{Ctx, Function, Issue, Outcome};
use orion_common::sync::LockExt;
use serde_json::{Value, json};

use super::super::AttemptContext;
use super::super::ProviderBackend;
use super::super::context::Invocation;
use super::super::deployment_store::{ImmutableDeploymentStore, PackageSchema};
use super::super::provider_backend::ProviderRegistry;
use super::super::{
    ConnectionReference, ConnectorError, ConnectorErrorClass, ConnectorOutput, EffectStatus,
    ErrorOrigin, ImplementationKind, codes,
};
use super::{
    PackageDependency, PackageProblem, WorkflowHost, WorkflowPackage, dependency_key, describe,
    package_registry,
};

tokio::task_local! {
    /// The package invocation a package workflow is running for: what its
    /// `connector_call` tasks derive their dependency calls from.
    static PACKAGE_INVOCATION: Invocation;
}

/// Maximum number of compiled generation engines retained concurrently per
/// workflow package so overlapping generations during drain do not thrash.
const MAX_CACHED_GENERATION_ENGINES: usize = 4;

type GenerationEngineCell = Arc<tokio::sync::OnceCell<Arc<dataflow_rs::Engine>>>;

/// A workflow package as a provider backend, registered under its digest.
pub struct WorkflowProviderBackend {
    package: WorkflowPackage,
    host: WorkflowHost,
    /// Compiled engines keyed by generation id: a package links the plugin
    /// functions that generation serves. Bounded to the most recent
    /// [`MAX_CACHED_GENERATION_ENGINES`] generations.
    engines: Mutex<BTreeMap<u64, GenerationEngineCell>>,
}

impl WorkflowProviderBackend {
    pub fn new(package: WorkflowPackage, host: WorkflowHost) -> Self {
        Self {
            package,
            host,
            engines: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn package(&self) -> &WorkflowPackage {
        &self.package
    }

    /// Compile every operation's workflow against `plugins`.
    ///
    /// # Errors
    ///
    /// Each workflow the engine refused, with its issues.
    pub fn compile(
        &self,
        plugins: &crate::plugin::PluginSet,
    ) -> Result<dataflow_rs::Engine, Vec<PackageProblem>> {
        self.compile_with(plugins, false)
    }

    /// [`Self::compile`]'s verdict for an upload or an admission, which may
    /// come before the plugins the package calls are deployed: a task naming
    /// a plugin function `plugins` does not serve is left for the build that
    /// runs it, and everything else must compile.
    ///
    /// # Errors
    ///
    /// Each problem the engine found that is not such a task.
    pub fn check(&self, plugins: &crate::plugin::PluginSet) -> Result<(), Vec<PackageProblem>> {
        self.compile_with(plugins, true).map(|_| ())
    }

    fn compile_with(
        &self,
        plugins: &crate::plugin::PluginSet,
        plugins_pending: bool,
    ) -> Result<dataflow_rs::Engine, Vec<PackageProblem>> {
        let registry =
            package_registry(&self.host, &self.package.dependencies, plugins).map_err(|e| {
                vec![PackageProblem::new(
                    "implementation",
                    orion_api::error::field_codes::INVALID,
                    e,
                )]
            })?;
        let compiler = registry.compiler(dataflow_rs::program::CompileOptions::default());
        let mut compiled = Vec::with_capacity(self.package.entries.len());
        let mut problems = Vec::new();
        for workflow in self.package.entries.values() {
            match crate::engine::compile_row(&compiler, &workflow.document) {
                Ok(c) => compiled.push(c.workflow),
                Err(issues) => {
                    let pending = |issue: &&Issue| {
                        plugins_pending
                            && issue.code == IssueCode::UnknownFunction
                            && plugin_named_by(&workflow.document, issue.step_id.as_deref())
                    };
                    for issue in issues
                        .iter()
                        .filter(|i| crate::engine::quarantines(i))
                        .filter(|i| !pending(i))
                    {
                        problems.push(PackageProblem::new(
                            format!("{}:{}", workflow.file, issue.path),
                            orion_api::error::field_codes::INVALID,
                            issue.message.clone(),
                        ));
                    }
                }
            }
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        Ok(crate::engine::serving_engine(compiler.link(compiled)))
    }

    /// The engine for `generation`, compiled once per generation outside the
    /// cache lock so concurrent calls do not serialize behind a sync mutex or
    /// thrash when two generations overlap during drain.
    async fn engine_for(
        &self,
        generation: &crate::runtime::RuntimeGeneration,
    ) -> Result<Arc<dataflow_rs::Engine>, ConnectorError> {
        let cell = {
            let mut cached = self.engines.lock_unpoisoned();
            if let Some(existing) = cached.get(&generation.id) {
                Arc::clone(existing)
            } else {
                while cached.len() >= MAX_CACHED_GENERATION_ENGINES {
                    if let Some((&oldest, _)) = cached.iter().next() {
                        cached.remove(&oldest);
                    } else {
                        break;
                    }
                }
                let cell = Arc::new(tokio::sync::OnceCell::new());
                cached.insert(generation.id, Arc::clone(&cell));
                cell
            }
        };
        let engine = cell
            .get_or_try_init(|| async {
                self.compile(&generation.plugins)
                    .map(Arc::new)
                    .map_err(|problems| {
                        ConnectorError::contract_violation(
                            "compile",
                            format!(
                                "workflow package '{}' does not compile on this generation: {}",
                                self.package.provider.provider_id,
                                describe(&problems)
                            ),
                            EffectStatus::NotStarted,
                        )
                    })
            })
            .await?;
        Ok(Arc::clone(engine))
    }
}

/// Whether the task `step_id` in `document` names a plugin-shaped function
/// (`<plugin>.<label>`).
fn plugin_named_by(document: &Value, step_id: Option<&str>) -> bool {
    let Some(step_id) = step_id else {
        return false;
    };
    crate::engine::leaf_tasks(&document["tasks"], document.get("loop"))
        .into_iter()
        .find(|task| task["id"] == step_id)
        .and_then(|task| task["function"]["name"].as_str())
        .is_some_and(|name| name.contains('.'))
}

/// A package workflow's failure, as the caller's `connector_call` reports it.
fn workflow_failed(message: String, effect: EffectStatus) -> ConnectorError {
    ConnectorError::new(
        codes::WORKFLOW_FAILED,
        ConnectorErrorClass::ProviderFailure,
        message,
        effect,
    )
    .with_origin(ErrorOrigin::provider("workflow"))
}

#[async_trait::async_trait]
impl ProviderBackend for WorkflowProviderBackend {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Workflow
    }

    fn driver_id(&self) -> &str {
        &self.package.provider.digest
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        if config.is_object() {
            Ok(())
        } else {
            Err(ConnectorError::invalid_input(
                "a workflow package's connection configuration is a JSON object",
                None,
            ))
        }
    }

    async fn invoke(
        &self,
        ctx: &AttemptContext,
        operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError> {
        let effect = ctx.effect_status();
        ctx.check_live()?;
        if !self.package.entries.contains_key(operation) {
            return Err(ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!(
                    "workflow package '{}' exports no operation '{operation}'",
                    self.package.provider.provider_id
                ),
                effect,
            ));
        }
        let generation = crate::runtime::generation::PINNED_GENERATION
            .try_with(Arc::clone)
            .map_err(|_| {
                ConnectorError::contract_violation(
                    "dispatch",
                    "a workflow package runs only inside a served run",
                    effect,
                )
            })?;
        let engine = self.engine_for(&generation).await?;
        // The package's own connector calls are dependencies of this
        // invocation: they run under its authority and share its effect
        // ledger, so the node's invocation is what the scope carries.
        let invocation = ctx.scope::<Invocation>().cloned().ok_or_else(|| {
            ConnectorError::contract_violation(
                "dispatch",
                "a workflow package runs only on an invocation this node started",
                effect,
            )
        })?;
        let remaining = ctx.deadline().saturating_duration_since(Instant::now());
        let opts = crate::engine::ExecOpts {
            cancel: Some(invocation.context().cancellation()),
            accountant: invocation
                .context()
                .accountant()
                .map(|a| a.as_ref() as &dyn dataflow_rs::run::RunAccountant),
            timeout_ms: Some(
                u64::try_from(remaining.as_millis())
                    .unwrap_or(u64::MAX)
                    .max(1),
            ),
            ..Default::default()
        };
        let execution = PACKAGE_INVOCATION
            .scope(
                invocation.clone(),
                crate::engine::execute_on(
                    &engine,
                    operation,
                    &input,
                    &json!({}),
                    opts,
                    generation.run_budget_limits,
                ),
            )
            .await;
        let effect = ctx.effect_status();
        match execution.outcome {
            crate::engine::RunOutcome::Ok => {
                let mut metadata = BTreeMap::new();
                metadata.insert(
                    "provider_digest".to_string(),
                    json!(self.package.provider.digest),
                );
                Ok(ConnectorOutput::with_metadata(execution.data(), metadata))
            }
            crate::engine::RunOutcome::Timeout(_) => Err(ConnectorError::deadline_exceeded(effect)),
            crate::engine::RunOutcome::WorkflowErrors(summary) => Err(workflow_failed(
                format!("operation '{operation}' failed: {summary}"),
                effect,
            )),
            crate::engine::RunOutcome::EngineError(e) => Err(workflow_failed(
                format!("operation '{operation}' failed: {e}"),
                effect,
            )),
        }
    }
}

/// `connector_call` inside a package workflow: a call on one of the
/// package's dependency aliases, dispatched as a dependency of the package's
/// own invocation to the connection the deployer bound it to.
#[derive(Clone)]
pub struct DependencyCall {
    pub(super) dependencies: Arc<BTreeMap<String, PackageDependency>>,
    pub(super) providers: ProviderRegistry,
}

#[derive(Debug, FunctionConfig)]
pub struct DependencyCallConfig {
    /// The dependency alias the package declares.
    pub connection: String,
    /// An operation that dependency lists.
    pub operation: String,
    /// The operation's input, rendered from the message.
    pub input: Option<Interpolated>,
    /// Where the operation's result is written (default `data`).
    #[output(default = "data")]
    pub output: Output,
}

impl Function for DependencyCall {
    type Config = DependencyCallConfig;

    fn spec(&self) -> FunctionSpec {
        crate::engine::functions::definition::spec::<
            crate::engine::functions::connector_call::ConnectorCallFunction,
        >()
    }

    /// The alias and the operation, refused here too: a document that
    /// slipped past the package check still compiles to nothing it may not
    /// call.
    fn prepare(&self, cfg: &mut DependencyCallConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        let refuse = |field: &str, message: String| {
            let mut at = AuthoredPath::root();
            at.push(field);
            Err(vec![Issue::new(IssueCode::InputParse, at, message)])
        };
        let Some(dependency) = self.dependencies.get(&cfg.connection) else {
            return refuse(
                "connection",
                format!(
                    "'{}' is not a dependency the package declares",
                    cfg.connection
                ),
            );
        };
        if !dependency.operations.contains(&cfg.operation) {
            return refuse(
                "operation",
                format!(
                    "dependency '{}' does not declare the operation '{}'",
                    cfg.connection, cfg.operation
                ),
            );
        }
        Ok(())
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &DependencyCallConfig,
    ) -> dataflow_rs::Result<Outcome> {
        let output =
            crate::engine::kit::output_path(ctx, &cfg.output, Some(&crate::engine::kit::DATA))?;
        let input = match &cfg.input {
            Some(doc) => crate::engine::kit::rendered(ctx, doc)?,
            None => json!({}),
        };
        let parent = PACKAGE_INVOCATION.try_with(Clone::clone).map_err(|_| {
            crate::engine::HandlerError::new(
                crate::engine::ErrorClass::CallerInput,
                "CONNECTOR_CONNECTION_FORBIDDEN: a package's connector_call runs only inside \
                 its package's invocation",
            )
        })?;
        let data = dispatch_dependency(&self.providers, &parent, cfg, input)
            .await
            .map_err(crate::engine::functions::connector_call::connector_error)?;
        ctx.set(&output, OwnedDataValue::from(&data))?;
        Ok(Outcome::Ok)
    }
}

/// One dependency call: the alias resolved to the connection the run's
/// authority binds it to, a dependency invocation derived from `parent`, and
/// the dispatch.
async fn dispatch_dependency(
    providers: &ProviderRegistry,
    parent: &Invocation,
    cfg: &DependencyCallConfig,
    input: Value,
) -> Result<Value, ConnectorError> {
    let effect = parent.effect_status();
    let key = dependency_key(&parent.connection().connection_id, &cfg.connection);
    let Some((bound, _)) = parent
        .context()
        .authority()
        .declared_dependencies
        .get(&key)
        .cloned()
    else {
        return Err(ConnectorError::forbidden(
            codes::DEPENDENCY_UNDECLARED,
            "dispatch",
            format!(
                "connection '{}' binds no connection to the alias '{}' in this deployment",
                parent.connection().connection_id,
                cfg.connection
            ),
            effect,
        ));
    };
    let target = dependency_target(
        providers.deployment_store(),
        &bound,
        &cfg.operation,
        &input,
        effect,
    )?;
    let child = parent.derive_dependency(
        &key,
        target.identity,
        cfg.operation.clone(),
        parent.context().deadline(),
        parent.context().budget().limits(),
        2,
    )?;
    let Some(backend) = providers.get(target.kind, &target.backend_id) else {
        return Err(ConnectorError::forbidden(
            codes::CONNECTION_FORBIDDEN,
            "dispatch",
            format!(
                "No provider backend registered for ({}, '{}')",
                target.kind, target.backend_id
            ),
            effect,
        ));
    };
    let output = providers
        .dispatch_attempt(&child, backend.as_ref(), input)
        .await?;
    if let Some(package_digest) = &target.package_digest {
        providers.deployment_store().check_operation_output(
            package_digest,
            &cfg.operation,
            &bound.connection_id,
            &output.data,
            child.effect_status(),
        )?;
    }
    Ok(output.data)
}

/// Where a dependency call dispatches: the backend and the identity it runs
/// under, and the uploaded package whose `output_schema` the answer is held
/// to (none for a built-in driver).
struct DependencyTarget {
    kind: ImplementationKind,
    backend_id: String,
    identity: super::super::EffectiveConnectionIdentity,
    package_digest: Option<String>,
}

fn dependency_target(
    store: &ImmutableDeploymentStore,
    bound: &ConnectionReference,
    operation: &str,
    input: &Value,
    effect: EffectStatus,
) -> Result<DependencyTarget, ConnectorError> {
    let Some(row) = store.get_connection(&bound.connection_id, bound.revision) else {
        return Err(ConnectorError::forbidden(
            codes::CONNECTION_FORBIDDEN,
            "dispatch",
            format!(
                "dependency connection '{}' revision {} is not stored",
                bound.connection_id, bound.revision
            ),
            effect,
        ));
    };
    let identity = row.effective_identity();
    let digest = &bound.provider.digest;
    if let Some(driver) = super::super::foundation::builtin_driver(digest) {
        return Ok(DependencyTarget {
            kind: ImplementationKind::Rust,
            backend_id: driver.to_string(),
            identity,
            package_digest: None,
        });
    }
    let Some(package) = store.get_package(digest) else {
        return Err(ConnectorError::forbidden(
            codes::CONNECTION_FORBIDDEN,
            "dispatch",
            format!("dependency provider package '{digest}' is not uploaded"),
            effect,
        ));
    };
    if store.get_admission(digest).is_none_or(|a| a.revoked) {
        return Err(ConnectorError::forbidden(
            codes::CONNECTION_FORBIDDEN,
            "dispatch",
            format!(
                "dependency provider package '{}' ({digest}) is not admitted",
                package.provider_id
            ),
            effect,
        ));
    }
    if let Some(Err(finding)) =
        store.check_package_instance(digest, PackageSchema::OperationInput(operation), input)
    {
        return Err(ConnectorError::invalid_input(
            format!(
                "'{operation}' on dependency '{}': the input does not satisfy provider '{}''s \
                 input_schema ({} refused it)",
                bound.connection_id, package.provider_id, finding.location
            ),
            Some(json!({"pointer": finding.instance_path, "keyword": finding.location})),
        ));
    }
    Ok(DependencyTarget {
        kind: package.implementation,
        backend_id: package.backend_id().to_string(),
        identity,
        package_digest: Some(digest.to_string()),
    })
}

//! Workflow provider packages: a connector whose operations are dataflow
//! workflows, run hermetically.
//!
//! A package with `implementation.kind = "workflow"` names one workflow file
//! per operation (`implementation.entries`: operation → workflow id,
//! `implementation.workflows`: the files). Once admitted, it is a
//! [`ProviderBackend`] registered under its digest, and a `connector_call`
//! against a connection over it runs the operation's workflow on Orion's
//! engine: the same parse ([`crate::engine::parse_options`]), compile
//! ([`crate::engine::compile_row`]) and run ([`crate::engine::execute`]) every
//! channel's workflow goes through, with Orion's operators. The input is the
//! message's payload and starting `data`; the result is `data` when the
//! workflow finishes.
//!
//! **Hermetic.** A package is code someone else may have written, so what its
//! workflows may reach is closed:
//!
//! - the dataflow built-ins ([`dataflow_rs::def::BUILTIN_FUNCTIONS`]), the
//!   self-contained Orion functions ([`PACKAGE_HOST_FUNCTIONS`] without
//!   `connector_call`), and plugin functions, which import nothing;
//! - `connector_call` only on an alias the manifest declares under
//!   `dependencies`, with an operation that dependency lists. The
//!   connection the alias calls is the deployer's choice, made in the
//!   connection revision's `bindings`, and the call is a dependency of the
//!   package's own invocation ([`Invocation::derive_dependency`]): it shares
//!   the caller's budget and deadline and reaches nothing else;
//! - no `{"secret": …}` node and no secret reference (`env://…`) anywhere.
//!
//! Every other Orion function — `http_call`, `db_*`, `data_*`, `cache_*`,
//! `channel_call` — reaches the deployment's own connections or channels by
//! name, and is refused. [`WorkflowPackage::from_files`] states all of it, at
//! upload and admission and again at every build that deploys a connection
//! over the package, and the package's registry ([`package_registry`]) holds
//! only the functions above, on an expression engine with no secrets, so a
//! document that slipped past the check still could not call or read
//! anything else. The message a package workflow runs on carries no
//! `metadata` (no `[vars]`).
//!
//! **No helpers.** Each operation is one entry workflow; a workflow no
//! operation enters is refused, as is a manifest declaring helpers or
//! sub-channels. Shared logic belongs in the package's authoring, as
//! compile-time fragments.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::program::{AuthoredPath, IssueCode};
use dataflow_rs::{Ctx, Function, Issue, Outcome};
use serde_json::{Value, json};

use super::context::{AttemptContext, Invocation};
use super::contract::{
    ConnectionReference, ConnectorError, ConnectorErrorClass, ConnectorOutput, EffectStatus,
    ErrorOrigin, ImplementationKind, ProviderRef, RetryAdvice, codes,
};
use super::deployment_store::{
    ConnectionRevisionRow, ImmutableDeploymentStore, PackageSchema, ProviderPackageRow,
};
use super::provider_backend::{ProviderBackend, ProviderRegistry};

/// The Orion functions a package workflow may name. `crypto`, `jwt_sign` and
/// `jwt_verify` need no connection; `connector_call` is the package's own,
/// bound to its declared dependencies. Every other Orion function is refused.
pub const PACKAGE_HOST_FUNCTIONS: [&str; 4] =
    ["crypto", "jwt_sign", "jwt_verify", "connector_call"];

/// The keys `implementation` may carry in a workflow package.
const IMPLEMENTATION_KEYS: [&str; 3] = ["kind", "entries", "workflows"];

/// One problem with a workflow package, by where it was written: the file
/// and the path inside it (`connector.json:implementation.entries.item.get`,
/// `workflows/entry.json:tasks[0].function.name`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageProblem {
    pub path: String,
    /// A field code from `orion_api::error::field_codes`.
    pub code: &'static str,
    pub message: String,
}

impl PackageProblem {
    fn new(path: impl Into<String>, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PackageProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// The problems as one line, for a log or an error message.
pub fn describe(problems: &[PackageProblem]) -> String {
    problems
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// A dependency a workflow package declares: the alias its `connector_call`
/// tasks name and the operations they may call on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDependency {
    pub alias: String,
    /// The provider id the bound connection must run on, when declared.
    pub provider_id: Option<String>,
    pub operations: BTreeSet<String>,
}

/// An operation's entry workflow: the file it came from and the document,
/// with the operation as its channel.
#[derive(Debug, Clone)]
pub struct PackageWorkflow {
    pub file: String,
    pub document: Value,
}

/// A workflow package, checked: one entry workflow per operation, every task
/// within the hermetic set.
#[derive(Debug, Clone)]
pub struct WorkflowPackage {
    pub provider: ProviderRef,
    pub entries: BTreeMap<String, PackageWorkflow>,
    pub dependencies: BTreeMap<String, PackageDependency>,
}

/// The dependencies `manifest` declares, by alias.
pub fn declared_dependencies(manifest: &Value) -> BTreeMap<String, PackageDependency> {
    manifest["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|dep| {
            let alias = dep["alias"].as_str()?.to_string();
            Some((
                alias.clone(),
                PackageDependency {
                    alias,
                    provider_id: dep["id"].as_str().map(str::to_string),
                    operations: dep["operations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                },
            ))
        })
        .collect()
}

impl WorkflowPackage {
    /// Read and check the package `manifest` describes from its `files`.
    ///
    /// # Errors
    ///
    /// Every problem found, each naming where it was written.
    pub fn from_files(
        provider: ProviderRef,
        manifest: &Value,
        files: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Self, Vec<PackageProblem>> {
        use orion_api::error::field_codes::{INVALID, REQUIRED, TYPE_MISMATCH, UNKNOWN_FIELD};

        let mut problems = Vec::new();
        let implementation = &manifest["implementation"];
        if let Some(obj) = implementation.as_object() {
            for key in obj.keys() {
                if IMPLEMENTATION_KEYS.contains(&key.as_str()) {
                    continue;
                }
                let message = if matches!(
                    key.as_str(),
                    "helpers" | "sub_channels" | "subchannels" | "binding_slots"
                ) {
                    format!(
                        "a workflow package declares no `{key}`: each operation is one entry \
                         workflow, and shared logic belongs in compile-time fragments"
                    )
                } else {
                    format!(
                        "`implementation` of a workflow package takes `kind`, `entries` and \
                         `workflows`, not `{key}`"
                    )
                };
                problems.push(PackageProblem::new(
                    format!("connector.json:implementation.{key}"),
                    UNKNOWN_FIELD,
                    message,
                ));
            }
        }

        // Every listed file, parsed and keyed by its workflow id.
        let mut by_id: BTreeMap<String, (String, Value)> = BTreeMap::new();
        let listed = implementation["workflows"].as_array();
        if listed.is_none() {
            problems.push(PackageProblem::new(
                "connector.json:implementation.workflows",
                REQUIRED,
                "a workflow package lists its workflow files under `implementation.workflows`",
            ));
        }
        for (i, entry) in listed.into_iter().flatten().enumerate() {
            let Some(file) = entry.as_str() else {
                problems.push(PackageProblem::new(
                    format!("connector.json:implementation.workflows[{i}]"),
                    TYPE_MISMATCH,
                    "a workflow file is named by its path in the package",
                ));
                continue;
            };
            let Some(bytes) = files.get(file) else {
                problems.push(PackageProblem::new(
                    format!("connector.json:implementation.workflows[{i}]"),
                    REQUIRED,
                    format!("the package has no file '{file}'"),
                ));
                continue;
            };
            let document: Value = match serde_json::from_slice(bytes) {
                Ok(doc) => doc,
                Err(e) => {
                    problems.push(PackageProblem::new(file, INVALID, format!("not JSON: {e}")));
                    continue;
                }
            };
            let Some(id) = document["id"].as_str().map(str::to_string) else {
                problems.push(PackageProblem::new(
                    format!("{file}:id"),
                    REQUIRED,
                    "a package workflow carries its `id`, which `implementation.entries` names",
                ));
                continue;
            };
            if by_id.contains_key(&id) {
                problems.push(PackageProblem::new(
                    format!("{file}:id"),
                    INVALID,
                    format!("two workflow files declare the id '{id}'"),
                ));
                continue;
            }
            by_id.insert(id, (file.to_string(), document));
        }

        // One entry workflow per operation, and no workflow that is not one.
        let mut entries = BTreeMap::new();
        let mut entered: BTreeMap<&str, &str> = BTreeMap::new();
        match implementation["entries"].as_object() {
            None => problems.push(PackageProblem::new(
                "connector.json:implementation.entries",
                REQUIRED,
                "a workflow package maps each operation to its entry workflow under \
                 `implementation.entries`",
            )),
            Some(map) => {
                for (operation, target) in map {
                    let at = format!("connector.json:implementation.entries.{operation}");
                    let Some(workflow_id) = target.as_str() else {
                        problems.push(PackageProblem::new(
                            at,
                            TYPE_MISMATCH,
                            "an entry names a workflow id",
                        ));
                        continue;
                    };
                    let Some((file, document)) = by_id.get(workflow_id) else {
                        problems.push(PackageProblem::new(
                            at,
                            INVALID,
                            format!("no listed workflow file declares the id '{workflow_id}'"),
                        ));
                        continue;
                    };
                    if let Some(other) = entered.insert(workflow_id, operation) {
                        problems.push(PackageProblem::new(
                            at,
                            INVALID,
                            format!(
                                "workflow '{workflow_id}' is already the entry of '{other}': \
                                 each operation has an entry workflow of its own"
                            ),
                        ));
                        continue;
                    }
                    entries.insert(
                        operation.clone(),
                        PackageWorkflow {
                            file: file.clone(),
                            document: document.clone(),
                        },
                    );
                }
            }
        }
        for (id, (file, _)) in &by_id {
            if !entered.contains_key(id.as_str()) {
                problems.push(PackageProblem::new(
                    file.clone(),
                    INVALID,
                    format!(
                        "workflow '{id}' is the entry of no operation: a workflow package has no \
                         helper workflows, and shared logic belongs in compile-time fragments"
                    ),
                ));
            }
        }

        let dependencies = declared_dependencies(manifest);
        for (operation, workflow) in &mut entries {
            problems.extend(check_workflow(
                &workflow.file,
                &workflow.document,
                &dependencies,
            ));
            if let Some(doc) = workflow.document.as_object_mut() {
                doc.insert("channel".to_string(), json!(operation));
            }
        }

        if problems.is_empty() {
            Ok(Self {
                provider,
                entries,
                dependencies,
            })
        } else {
            Err(problems)
        }
    }

    /// The package `row` as the store holds it: its files read back from its
    /// inventory, then checked as at upload.
    ///
    /// # Errors
    ///
    /// Every problem found; a file the inventory lists that is not stored is
    /// one.
    pub async fn from_store(
        store: &ImmutableDeploymentStore,
        pool: &crate::storage::DbPool,
        row: &ProviderPackageRow,
    ) -> Result<Self, Vec<PackageProblem>> {
        let workflows: Vec<&str> = row.manifest_json["implementation"]["workflows"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let files = store
            .package_files(pool, &row.package_digest, &workflows)
            .await
            .map_err(|e| {
                vec![PackageProblem::new(
                    "",
                    orion_api::error::field_codes::INVALID,
                    format!("the package's files could not be read: {e}"),
                )]
            })?;
        Self::from_files(row.provider_ref(), &row.manifest_json, &files)
    }
}

/// The problems with one package workflow: its own parse, its channel, and
/// every task against the hermetic set.
fn check_workflow(
    file: &str,
    document: &Value,
    dependencies: &BTreeMap<String, PackageDependency>,
) -> Vec<PackageProblem> {
    use orion_api::error::field_codes::{INVALID, UNKNOWN_FUNCTION, UNRESOLVED_SECRET_REF};

    let mut problems = Vec::new();
    let at = |path: &str| format!("{file}:{path}");

    let parsed = dataflow_rs::def::parse(document, crate::engine::parse_options());
    let mut issues = parsed.issues;
    crate::engine::name_v3_spellings(document, &mut issues);
    for issue in issues.iter().filter(|i| crate::engine::quarantines(i)) {
        problems.push(PackageProblem::new(
            at(&issue.path.to_string()),
            INVALID,
            issue.message.clone(),
        ));
    }
    if document.get("channel").is_some() {
        problems.push(PackageProblem::new(
            at("channel"),
            INVALID,
            "a package workflow is addressed by its operation and names no channel",
        ));
    }

    secrets_in(document, "", &mut |path| {
        problems.push(PackageProblem::new(
            at(path),
            UNRESOLVED_SECRET_REF,
            "a package workflow reads no secret: neither a `{\"secret\": …}` node nor a secret \
             reference. Key material reaches it through its input or a dependency",
        ));
    });

    let steps = crate::engine::walk_steps(&document["tasks"], document.get("loop"));
    for (path, task) in steps.tasks {
        let function = &task["function"];
        let Some(name) = function["name"].as_str() else {
            continue;
        };
        let name_at = at(&format!("{path}.function.name"));
        if dataflow_rs::def::BUILTIN_FUNCTIONS.contains(&name) {
            continue;
        }
        if name == crate::engine::functions::connector_call::NAME {
            problems.extend(check_dependency_call(
                &at(&format!("{path}.function.input")),
                &function["input"],
                dependencies,
            ));
            continue;
        }
        if PACKAGE_HOST_FUNCTIONS.contains(&name) {
            continue;
        }
        if crate::engine::FunctionRegistry::builtin().contains(name) {
            problems.push(PackageProblem::new(
                name_at,
                INVALID,
                format!(
                    "task '{}' calls `{name}`, which a package workflow may not: it reaches the \
                     deployment's own connections or channels by name. A package workflow calls \
                     the dataflow built-ins, `crypto`, `jwt_sign`, `jwt_verify`, plugin \
                     functions, and `connector_call` on its declared dependencies",
                    task["id"].as_str().unwrap_or_default()
                ),
            ));
            continue;
        }
        // A plugin function is named `<plugin>.<label>`; the build compiles
        // it against the plugins the generation serves.
        if !name.contains('.') {
            problems.push(PackageProblem::new(
                name_at,
                UNKNOWN_FUNCTION,
                format!(
                    "task '{}' calls `{name}`, which is neither a dataflow built-in, an Orion \
                     function a package may call, nor a plugin function",
                    task["id"].as_str().unwrap_or_default()
                ),
            ));
        }
    }
    problems
}

/// A package workflow's `connector_call`: an alias the manifest declares,
/// with an operation that dependency lists, and nothing that picks the
/// provider, which the binding decides.
fn check_dependency_call(
    at: &str,
    input: &Value,
    dependencies: &BTreeMap<String, PackageDependency>,
) -> Vec<PackageProblem> {
    use orion_api::error::field_codes::{INVALID, REQUIRED};

    let mut problems = Vec::new();
    for key in ["kind", "target_id"] {
        if input.get(key).is_some() {
            problems.push(PackageProblem::new(
                format!("{at}.{key}"),
                INVALID,
                format!(
                    "a package's `connector_call` names a dependency alias, and the \
                     connection it is bound to decides the provider: drop `{key}`"
                ),
            ));
        }
    }
    let Some(alias) = input["connection"].as_str() else {
        problems.push(PackageProblem::new(
            format!("{at}.connection"),
            REQUIRED,
            "a package's `connector_call` names one of the package's dependency aliases",
        ));
        return problems;
    };
    let Some(dependency) = dependencies.get(alias) else {
        let declared: Vec<&str> = dependencies.keys().map(String::as_str).collect();
        problems.push(PackageProblem::new(
            format!("{at}.connection"),
            INVALID,
            format!(
                "'{alias}' is not a dependency the package declares (declared: {declared:?}): \
                 a package workflow calls only what its manifest's `dependencies` name"
            ),
        ));
        return problems;
    };
    match input["operation"].as_str() {
        Some(operation) if dependency.operations.contains(operation) => {}
        Some(operation) => problems.push(PackageProblem::new(
            format!("{at}.operation"),
            INVALID,
            format!(
                "dependency '{alias}' declares the operations {:?}, not '{operation}'",
                dependency.operations
            ),
        )),
        None => problems.push(PackageProblem::new(
            format!("{at}.operation"),
            REQUIRED,
            "a `connector_call` names its operation",
        )),
    }
    problems
}

/// Report, by path, every `{"secret": …}` node and every secret reference
/// string in `value`.
fn secrets_in(value: &Value, path: &str, found: &mut dyn FnMut(&str)) {
    match value {
        Value::Object(map) => {
            if map.len() == 1 && map.contains_key("secret") {
                found(path);
                return;
            }
            for (key, child) in map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                secrets_in(child, &child_path, found);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                secrets_in(child, &format!("{path}[{i}]"), found);
            }
        }
        Value::String(s) if crate::connector::secrets::is_secret_reference(s) => found(path),
        _ => {}
    }
}

/// The key of a dependency in a run's authority: the package connection and
/// the alias, so two connections over packages with the same alias never
/// reach each other's binding.
pub fn dependency_key(connection_id: &str, alias: &str) -> String {
    format!("{connection_id}::{alias}")
}

tokio::task_local! {
    /// The package invocation a package workflow is running for: what its
    /// `connector_call` tasks derive their dependency calls from.
    static PACKAGE_INVOCATION: Invocation;
}

/// What a workflow package's backend needs from the node beside the package.
#[derive(Clone)]
pub struct WorkflowHost {
    /// The node's provider registry, which a dependency call dispatches on.
    pub providers: ProviderRegistry,
    /// The JWKS cache `jwt_verify` reads keys through.
    pub jwks: Arc<crate::jwt::jwks::JwksCache>,
    /// `engine.ops_budget`, on the package's expression engine too.
    pub ops_budget: u64,
}

/// The registry a package's workflows compile and run on: the dataflow
/// built-ins, `crypto`, `jwt_sign`, `jwt_verify`, the package's own
/// `connector_call` and `plugins`' functions, on an expression engine with no
/// secrets. Nothing else is registered, so nothing else can run.
///
/// # Errors
///
/// When the expression engine cannot be built.
pub fn package_registry(
    host: &WorkflowHost,
    dependencies: &BTreeMap<String, PackageDependency>,
    plugins: &crate::plugin::PluginSet,
) -> Result<dataflow_rs::Registry, String> {
    use crate::engine::functions::{crypto, jwt_sign, jwt_verify};
    let exprs = crate::engine::operators::orion_exprs(
        &crate::engine::ResolvedSecrets::empty(),
        host.ops_budget,
    )
    .map_err(|e| e.to_string())?;
    let mut registry = dataflow_rs::Registry::new(exprs);
    registry.register(crypto::NAME, crypto::CryptoFunction);
    registry.register(jwt_sign::NAME, jwt_sign::JwtSignFunction);
    registry.register(
        jwt_verify::NAME,
        jwt_verify::JwtVerifyFunction {
            jwks: host.jwks.clone(),
        },
    );
    registry.register(
        crate::engine::functions::connector_call::NAME,
        DependencyCall {
            dependencies: Arc::new(dependencies.clone()),
            providers: host.providers.clone(),
        },
    );
    crate::engine::register_plugin_functions(&mut registry, plugins.functions().cloned());
    Ok(registry)
}

/// A workflow package as a provider backend, registered under its digest.
pub struct WorkflowProviderBackend {
    package: WorkflowPackage,
    host: WorkflowHost,
    /// The engine compiled for the last generation it ran under: a package
    /// links the plugin functions that generation serves.
    engine: Mutex<Option<(u64, Arc<dataflow_rs::Engine>)>>,
}

impl WorkflowProviderBackend {
    pub fn new(package: WorkflowPackage, host: WorkflowHost) -> Self {
        Self {
            package,
            host,
            engine: Mutex::new(None),
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

    /// The engine for `generation`, compiled once per generation.
    fn engine_for(
        &self,
        generation: &crate::runtime::RuntimeGeneration,
    ) -> Result<Arc<dataflow_rs::Engine>, ConnectorError> {
        let mut cached = self.engine.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((id, engine)) = cached.as_ref()
            && *id == generation.id
        {
            return Ok(Arc::clone(engine));
        }
        let engine = Arc::new(self.compile(&generation.plugins).map_err(|problems| {
            ConnectorError::contract_violation(
                "compile",
                format!(
                    "workflow package '{}' does not compile on this generation: {}",
                    self.package.provider.provider_id,
                    describe(&problems)
                ),
                EffectStatus::NotStarted,
            )
        })?);
        *cached = Some((generation.id, Arc::clone(&engine)));
        Ok(engine)
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
    ConnectorError {
        code: Cow::Borrowed(codes::WORKFLOW_FAILED),
        class: ConnectorErrorClass::ProviderFailure,
        message,
        effect_status: effect,
        retry_advice: RetryAdvice::Never,
        origin: ErrorOrigin::provider("workflow"),
        retry_after_ms: None,
        details: None,
    }
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
        let effect = ctx.invocation().effect_status();
        ctx.run_context().check_live(effect)?;
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
        let engine = self.engine_for(&generation)?;
        let remaining = ctx
            .run_context()
            .deadline()
            .saturating_duration_since(Instant::now());
        let opts = crate::engine::ExecOpts {
            timeout_ms: Some(
                u64::try_from(remaining.as_millis())
                    .unwrap_or(u64::MAX)
                    .max(1),
            ),
            ..Default::default()
        };
        let execution = PACKAGE_INVOCATION
            .scope(
                ctx.invocation().clone(),
                crate::engine::execute::execute_on(
                    &engine,
                    operation,
                    &input,
                    &json!({}),
                    opts,
                    generation.run_budget_limits,
                ),
            )
            .await;
        let effect = ctx.invocation().effect_status();
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
    dependencies: Arc<BTreeMap<String, PackageDependency>>,
    providers: ProviderRegistry,
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
        crate::engine::kit::spec(crate::engine::functions::connector_call::NAME)
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
    providers
        .dispatch_attempt(&child, backend.as_ref(), input)
        .await
        .map(|output| output.data)
}

/// Where a dependency call dispatches: the backend and the identity it runs
/// under.
struct DependencyTarget {
    kind: ImplementationKind,
    backend_id: String,
    identity: super::contract::EffectiveConnectionIdentity,
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
    if let Some(driver) = super::foundation::builtin_driver(digest) {
        return Ok(DependencyTarget {
            kind: ImplementationKind::Rust,
            backend_id: driver.to_string(),
            identity,
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
    })
}

/// The problems with `row`'s `bindings` as a connection over the package
/// `provider_digest` names, for its revision create: every declared alias
/// bound, nothing else bound, and each target a stored connection over a
/// provider other than a workflow package (a dependency's own dependencies
/// are not carried, so a package cannot call another), running on the
/// provider the dependency names when it names one.
pub fn binding_problems(
    store: &ImmutableDeploymentStore,
    connection_id: &str,
    provider_digest: &str,
    bindings: &BTreeMap<String, String>,
) -> Vec<PackageProblem> {
    use orion_api::error::field_codes::{INVALID, REQUIRED, UNKNOWN_FIELD};

    let package = store
        .get_package(provider_digest)
        .filter(|p| p.implementation == ImplementationKind::Workflow);
    let Some(package) = package else {
        if bindings.is_empty() {
            return Vec::new();
        }
        return vec![PackageProblem::new(
            "bindings",
            INVALID,
            "only a connection over a workflow provider package binds dependencies",
        )];
    };
    let dependencies = declared_dependencies(&package.manifest_json);
    let mut problems = Vec::new();
    for alias in dependencies.keys() {
        if !bindings.contains_key(alias) {
            problems.push(PackageProblem::new(
                format!("bindings.{alias}"),
                REQUIRED,
                format!(
                    "provider '{}' declares the dependency '{alias}': bind it to a connection",
                    package.provider_id
                ),
            ));
        }
    }
    let connections = store.list_connections();
    for (alias, target) in bindings {
        let at = format!("bindings.{alias}");
        let Some(dependency) = dependencies.get(alias) else {
            problems.push(PackageProblem::new(
                at,
                UNKNOWN_FIELD,
                format!(
                    "provider '{}' declares no dependency '{alias}'",
                    package.provider_id
                ),
            ));
            continue;
        };
        if target == connection_id {
            problems.push(PackageProblem::new(
                at,
                INVALID,
                "a connection cannot be its own dependency",
            ));
            continue;
        }
        let Some(latest) = connections
            .iter()
            .filter(|c| &c.connection_id == target)
            .max_by_key(|c| c.revision)
        else {
            problems.push(PackageProblem::new(
                at,
                INVALID,
                format!("no connection '{target}' is stored"),
            ));
            continue;
        };
        if let Some(problem) = target_problem(store, latest, dependency) {
            problems.push(PackageProblem::new(at, INVALID, problem));
        }
    }
    problems
}

/// Why `target` cannot serve `dependency`, if it cannot.
fn target_problem(
    store: &ImmutableDeploymentStore,
    target: &ConnectionRevisionRow,
    dependency: &PackageDependency,
) -> Option<String> {
    let provider_id = match super::foundation::builtin_driver(&target.provider_digest) {
        Some(driver) => driver.to_string(),
        None => match store.get_package(&target.provider_digest) {
            Some(pkg) if pkg.implementation == ImplementationKind::Workflow => {
                return Some(format!(
                    "connection '{}' runs on the workflow package '{}': a dependency is a \
                     connection over a built-in or rust provider",
                    target.connection_id, pkg.provider_id
                ));
            }
            Some(pkg) => pkg.provider_id.to_string(),
            None => {
                return Some(format!(
                    "connection '{}' names provider '{}', which is not uploaded",
                    target.connection_id, target.provider_digest
                ));
            }
        },
    };
    match &dependency.provider_id {
        Some(wanted) if wanted != &provider_id => Some(format!(
            "dependency '{}' runs on '{wanted}', and connection '{}' on '{provider_id}'",
            dependency.alias, target.connection_id
        )),
        _ => None,
    }
}

/// What a manifest's connections over workflow packages depend on, keyed by
/// [`dependency_key`]: each binding's target at the revision the member
/// requires, with the operations the dependency declares.
///
/// # Errors
///
/// A binding whose target the member does not require, or that names a
/// connection the store does not hold at that revision.
pub fn manifest_dependencies(
    store: &ImmutableDeploymentStore,
    manifest: &super::deployment_store::CanonicalManifest,
) -> Result<BTreeMap<String, (ConnectionReference, BTreeSet<String>)>, String> {
    use super::deployment_store::EntityKind;

    let mut out = BTreeMap::new();
    for member in &manifest.members {
        if member.entity.kind != EntityKind::Connection {
            continue;
        }
        let Some(row) = store.get_connection(&member.entity.id, member.entity.revision) else {
            continue;
        };
        if row.bindings.is_empty() {
            continue;
        }
        let Some(package) = store.get_package(&row.provider_digest) else {
            return Err(format!(
                "connection '{}' binds dependencies over provider '{}', which is not uploaded",
                row.connection_id, row.provider_digest
            ));
        };
        let dependencies = declared_dependencies(&package.manifest_json);
        for (alias, target) in &row.bindings {
            let Some(required) = member
                .requires
                .iter()
                .find(|r| r.kind == EntityKind::Connection && &r.id == target)
            else {
                return Err(format!(
                    "connection '{}' binds '{alias}' to connection '{target}', which its \
                     deployment member does not require: add a requirement on connection \
                     '{target}'",
                    row.connection_id
                ));
            };
            let Some(target_row) = store.get_connection(target, required.revision) else {
                return Err(format!(
                    "connection '{}' binds '{alias}' to connection '{target}' revision {}, \
                     which is not stored",
                    row.connection_id, required.revision
                ));
            };
            let Some(provider) = store.provider_ref(&target_row.provider_digest) else {
                return Err(format!(
                    "connection '{target}' names provider '{}', which is not uploaded",
                    target_row.provider_digest
                ));
            };
            let operations = dependencies
                .get(alias)
                .map(|d| d.operations.clone())
                .unwrap_or_default();
            out.insert(
                dependency_key(&row.connection_id, alias),
                (target_row.reference(provider), operations),
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::extensions::identity::Digest;

    fn provider() -> ProviderRef {
        ProviderRef {
            publisher: "acme".to_string(),
            provider_id: "acme.crm".into(),
            version: "1.0.0".to_string(),
            digest: Digest::parse(&format!("sha256:{}", "ab".repeat(32))).unwrap(),
        }
    }

    fn manifest(entries: Value, workflows: Value) -> Value {
        json!({
            "implementation": {"kind": "workflow", "entries": entries, "workflows": workflows},
            "dependencies": [
                {"kind": "provider", "alias": "cache", "id": "org.orion.memory-cache",
                 "requires": ">=2", "operations": ["cache.get"]}
            ],
        })
    }

    fn files(docs: &[(&str, Value)]) -> BTreeMap<String, Vec<u8>> {
        docs.iter()
            .map(|(path, doc)| (path.to_string(), serde_json::to_vec(doc).unwrap()))
            .collect()
    }

    fn paths(problems: &[PackageProblem]) -> Vec<&str> {
        problems.iter().map(|p| p.path.as_str()).collect()
    }

    /// The allowlist, task by task: a host function that reaches the
    /// deployment by name is refused naming the task, a secret node or
    /// reference anywhere is refused, and `connector_call` is held to the
    /// declared dependencies.
    #[test]
    fn a_package_workflow_reaches_only_the_hermetic_set() {
        let doc = json!({"id": "entry", "name": "entry", "tasks": [
            {"id": "ok-map", "name": "m", "function": {"name": "map", "input": {"mappings": [
                {"path": "data.x", "logic": 1}]}}},
            {"id": "ok-hash", "name": "h", "function": {"name": "crypto", "input": {
                "op": "hash", "algorithm": "sha256", "data": "x", "output": "data.h"}}},
            {"id": "egress", "name": "e", "function": {"name": "http_call", "input": {
                "connector": "orders-api", "method": "GET"}}},
            {"id": "peek", "name": "p", "function": {"name": "channel_call", "input": {
                "channel": "admin"}}},
            {"id": "keyed", "name": "k", "function": {"name": "crypto", "input": {
                "op": "hmac", "algorithm": "sha256", "data": "x",
                "key": {"secret": "partner"}, "output": "data.mac"}}},
            {"id": "env", "name": "v", "function": {"name": "map", "input": {"mappings": [
                {"path": "data.k", "logic": "env://AWS_SECRET_ACCESS_KEY"}]}}},
            {"id": "dep-ok", "name": "d", "function": {"name": "connector_call", "input": {
                "connection": "cache", "operation": "cache.get", "input": {"key": "k"}}}},
            {"id": "dep-op", "name": "d", "function": {"name": "connector_call", "input": {
                "connection": "cache", "operation": "cache.set"}}},
            {"id": "dep-alias", "name": "d", "function": {"name": "connector_call", "input": {
                "connection": "orders-db", "operation": "query", "target_id": "org.orion.sqlite"}}},
        ]});
        let problems = WorkflowPackage::from_files(
            provider(),
            &manifest(
                json!({"item.get": "entry"}),
                json!(["workflows/entry.json"]),
            ),
            &files(&[("workflows/entry.json", doc)]),
        )
        .expect_err("refused");
        let at = paths(&problems);
        for refused in [
            "workflows/entry.json:tasks[2].function.name",
            "workflows/entry.json:tasks[3].function.name",
            "workflows/entry.json:tasks[4].function.input.key",
            "workflows/entry.json:tasks[5].function.input.mappings[0].logic",
            "workflows/entry.json:tasks[7].function.input.operation",
            "workflows/entry.json:tasks[8].function.input.connection",
            "workflows/entry.json:tasks[8].function.input.target_id",
        ] {
            assert!(at.contains(&refused), "{refused} refused: {problems:#?}");
        }
        assert_eq!(at.len(), 7, "nothing else is refused: {problems:#?}");
        assert!(
            problems
                .iter()
                .any(|p| p.message.contains("task 'egress' calls `http_call`")),
            "{problems:#?}"
        );
    }

    /// One entry workflow per operation: a workflow no operation enters is a
    /// helper and refused, as is a manifest declaring helpers.
    #[test]
    fn a_package_has_no_helpers() {
        let tasks = json!([{"id": "t", "name": "t", "function": {"name": "map", "input": {"mappings": [{"path": "data.x", "logic": 1}]}}}]);
        let entry = json!({"id": "entry", "name": "entry", "tasks": tasks});
        let helper = json!({"id": "helper", "name": "helper", "tasks": tasks});
        let mut m = manifest(
            json!({"item.get": "entry"}),
            json!(["workflows/entry.json", "workflows/helper.json"]),
        );
        m["implementation"]["helpers"] = json!(["helper"]);
        let problems = WorkflowPackage::from_files(
            provider(),
            &m,
            &files(&[
                ("workflows/entry.json", entry.clone()),
                ("workflows/helper.json", helper),
            ]),
        )
        .expect_err("refused");
        assert_eq!(
            paths(&problems),
            vec![
                "connector.json:implementation.helpers",
                "workflows/helper.json"
            ],
            "{problems:#?}"
        );

        let ok = WorkflowPackage::from_files(
            provider(),
            &manifest(
                json!({"item.get": "entry"}),
                json!(["workflows/entry.json"]),
            ),
            &files(&[("workflows/entry.json", entry)]),
        )
        .expect("one entry per operation is accepted");
        assert_eq!(ok.entries["item.get"].document["channel"], "item.get");
    }

    /// Defence in depth: even a document that skipped the package check
    /// cannot call a deployment-reaching function or read a secret, because
    /// the package's registry holds neither.
    #[test]
    fn the_package_registry_cannot_reach_the_deployment_or_its_secrets() {
        let host = WorkflowHost {
            providers: crate::extensions::CompositionBuilder::new(
                crate::extensions::EditionTier::Community,
            )
            .build_in_memory()
            .expect("composes")
            .providers()
            .clone(),
            jwks: Arc::new(crate::jwt::jwks::JwksCache::new(
                reqwest::Client::new(),
                false,
            )),
            ops_budget: 0,
        };
        let registry =
            package_registry(&host, &BTreeMap::new(), &crate::plugin::PluginSet::empty())
                .expect("the registry builds");
        let compiler = registry.compiler(dataflow_rs::program::CompileOptions::default());
        let compile = |task: Value| {
            crate::engine::compile_row(
                &compiler,
                &json!({"id": "w", "name": "w", "channel": "op", "tasks": [task]}),
            )
        };
        let egress = compile(
            json!({"id": "e", "name": "e", "function": {"name": "http_call",
            "input": {"connector": "orders-api", "method": "GET"}}}),
        )
        .err()
        .unwrap_or_default();
        assert!(!egress.is_empty(), "http_call is not registered");
        assert!(
            egress.iter().any(|i| i.code == IssueCode::UnknownFunction),
            "{egress:?}"
        );
        let secret = compile(
            json!({"id": "k", "name": "k", "function": {"name": "crypto",
            "input": {"op": "hmac", "algorithm": "sha256", "data": "x",
                      "key": {"secret": "partner_hmac"}, "output": "data.mac"}}}),
        );
        assert!(secret.is_err(), "no secret resolves on a package's engine");
        let pure = compile(
            json!({"id": "h", "name": "h", "function": {"name": "crypto",
            "input": {"op": "hash", "algorithm": "sha256", "data": "x", "output": "data.h"}}}),
        );
        assert!(pure.is_ok(), "{:?}", pure.err());
    }
}

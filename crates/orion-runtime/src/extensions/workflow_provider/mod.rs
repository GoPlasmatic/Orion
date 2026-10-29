//! Workflow provider packages: a connector whose operations are dataflow
//! workflows, run hermetically.
//!
//! A package with `implementation.kind = "workflow"` names one workflow file
//! per operation (`implementation.entries`: operation → workflow id,
//! `implementation.workflows`: the files). Once admitted, it is a
//! [`super::ProviderBackend`] registered under its digest, and a `connector_call`
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
//!   package's own invocation ([`super::context::Invocation::derive_dependency`]): it shares
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

mod execution;
#[cfg(test)]
mod tests;
mod validation;

pub use execution::{DependencyCall, DependencyCallConfig, WorkflowProviderBackend};
pub use validation::{binding_problems, manifest_dependencies};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value;

use super::ProviderRef;
use super::provider_backend::ProviderRegistry;

/// The Orion functions a package workflow may name. `crypto`, `jwt_sign` and
/// `jwt_verify` need no connection; `connector_call` is the package's own,
/// bound to its declared dependencies. Every other Orion function is refused.
pub const PACKAGE_HOST_FUNCTIONS: [&str; 4] =
    ["crypto", "jwt_sign", "jwt_verify", "connector_call"];

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
    pub(super) fn new(
        path: impl Into<String>,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
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

/// The key of a dependency in a run's authority: the package connection and
/// the alias, so two connections over packages with the same alias never
/// reach each other's binding.
pub fn dependency_key(connection_id: &str, alias: &str) -> String {
    format!("{connection_id}::{alias}")
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

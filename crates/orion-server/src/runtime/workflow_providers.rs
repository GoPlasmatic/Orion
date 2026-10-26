//! Which workflow provider packages this node serves.
//!
//! An uploaded package with `implementation.kind = "workflow"` becomes a
//! [`WorkflowProviderBackend`] in the admitted layer of the node's
//! [`ProviderRegistry`](crate::extensions::ProviderRegistry), registered under
//! its digest, while it is admitted and not revoked. The composed layer (what
//! the edition built in) is frozen and never touched here. [`sync`] makes the
//! registry say exactly that, from the deployment store: it runs at boot
//! after the store is read, after an admission or a revocation, whenever a
//! cluster peer's changes are read in, and at the start of every deployment
//! build — so a node that restarts, or that learns of an admission from a
//! peer, registers the package before anything deploys a connection over it,
//! and a revoked package's backend is gone before the next generation builds.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::extensions::workflow_provider::{
    WorkflowHost, WorkflowPackage, WorkflowProviderBackend, describe,
};
use crate::extensions::{ImplementationKind, ProviderBackend};
use crate::server::state::AppState;

/// What a workflow package's backend gets from this node.
pub fn host(state: &AppState) -> WorkflowHost {
    WorkflowHost {
        providers: state.composition.providers().clone(),
        jwks: state.jwks.clone(),
        ops_budget: state.config.engine.ops_budget,
    }
}

/// The backend for the stored package `digest`, checked, and compiled against
/// the serving generation's plugins (a plugin function not deployed yet is
/// left for the build that runs it).
///
/// # Errors
///
/// The package's problems, one line.
pub fn backend(state: &AppState, digest: &str) -> Result<Arc<WorkflowProviderBackend>, String> {
    let store = &state.deployments;
    let row = store
        .get_package(digest)
        .ok_or_else(|| format!("package {digest} is not uploaded"))?;
    let package = WorkflowPackage::from_store(store, &row).map_err(|p| describe(&p))?;
    let backend = WorkflowProviderBackend::new(package, host(state));
    backend
        .check(&state.runtime.load().plugins)
        .map_err(|p| describe(&p))?;
    Ok(Arc::new(backend))
}

/// Register every admitted, unrevoked workflow package not yet registered,
/// and unregister every registered one that no longer is. A package that
/// fails its checks stays unregistered, logged; a connection over it then
/// fails the build that deploys it.
pub fn sync(state: &AppState) {
    let store = &state.deployments;
    let providers = state.composition.providers();
    let serving: BTreeSet<String> = store
        .list_packages()
        .into_iter()
        .filter(|p| p.implementation == ImplementationKind::Workflow)
        .filter(|p| {
            store
                .get_admission(&p.package_digest)
                .is_some_and(|a| !a.revoked)
                && !state
                    .composition
                    .admission()
                    .ledger()
                    .is_digest_revoked(&p.package_digest)
        })
        .map(|p| p.package_digest)
        .collect();
    let registered = providers.admitted_ids(ImplementationKind::Workflow);

    for digest in registered.difference(&serving) {
        providers.withdraw(ImplementationKind::Workflow, digest);
        tracing::info!(package = %digest, "workflow provider package unregistered");
    }
    for digest in serving.difference(&registered) {
        match backend(state, digest) {
            Ok(backend) => {
                let provider = backend.package().provider.provider_id.clone();
                match providers.admit(backend as Arc<dyn ProviderBackend>) {
                    Ok(()) => tracing::info!(
                        package = %digest,
                        %provider,
                        "workflow provider package registered"
                    ),
                    // A concurrent sync registered it first.
                    Err(duplicate) => tracing::debug!(
                        package = %digest,
                        %duplicate,
                        "workflow provider package already registered"
                    ),
                }
            }
            Err(reason) => tracing::error!(
                package = %digest,
                %reason,
                "workflow provider package not registered"
            ),
        }
    }
}

/// What a workflow package's upload would be refused for: the package
/// `files` hold, read as at admission and checked against the serving
/// generation's plugins. Empty for any other kind of package, and for files
/// with no readable `connector.json` (the upload's own checks name that).
pub fn upload_problems(
    state: &AppState,
    files: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Vec<crate::extensions::PackageProblem> {
    let Some(manifest) = files
        .get("connector.json")
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
    else {
        return Vec::new();
    };
    if manifest["implementation"]["kind"] != "workflow" {
        return Vec::new();
    }
    let provider = crate::extensions::ProviderReference {
        provider_id: manifest["id"].as_str().unwrap_or_default().to_string(),
        version: manifest["version"].as_str().unwrap_or_default().to_string(),
        package_digest: String::new(),
    };
    let package = match WorkflowPackage::from_files(provider, &manifest, files) {
        Ok(package) => package,
        Err(problems) => return problems,
    };
    WorkflowProviderBackend::new(package, host(state))
        .check(&state.runtime.load().plugins)
        .err()
        .unwrap_or_default()
}

/// What admitting the stored package `digest` would be refused for, read
/// back from the store: a package stored by another node or another binary
/// is held to this node's checks too. Empty for any other kind of package.
pub fn admission_problems(
    state: &AppState,
    digest: &str,
) -> Vec<crate::extensions::PackageProblem> {
    let store = &state.deployments;
    let Some(row) = store
        .get_package(digest)
        .filter(|p| p.implementation == ImplementationKind::Workflow)
    else {
        return Vec::new();
    };
    let package = match WorkflowPackage::from_store(store, &row) {
        Ok(package) => package,
        Err(problems) => return problems,
    };
    WorkflowProviderBackend::new(package, host(state))
        .check(&state.runtime.load().plugins)
        .err()
        .unwrap_or_default()
}

/// `problems` as the refusal an admin route answers: `400 VALIDATION_ERROR`
/// naming each by file and path, in the error envelope.
pub fn refusal(
    what: &str,
    problems: Vec<crate::extensions::PackageProblem>,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    let envelope = orion_api::ErrorEnvelope {
        error: orion_api::ErrorBody {
            code: orion_api::error::codes::VALIDATION_ERROR.to_string(),
            message: format!("{what}: the workflow package is not hermetic or does not compile"),
            details: problems
                .into_iter()
                .map(|p| crate::errors::FieldError::new(p.path, p.code, p.message))
                .collect(),
            request_id: crate::request_context::request_id(),
        },
    };
    (
        axum::http::StatusCode::BAD_REQUEST,
        axum::Json(serde_json::to_value(envelope).unwrap_or_default()),
    )
}

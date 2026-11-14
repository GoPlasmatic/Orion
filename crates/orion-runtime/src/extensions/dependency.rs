//! A provider's call on a connection it declared as a dependency, by alias:
//! the one path a workflow package's `connector_call` and a Rust provider's
//! `AttemptContext::call_dependency` share.

use serde_json::{Value, json};

use super::context::Invocation;
use super::deployment_store::{ImmutableDeploymentStore, PackageSchema};
use super::provider_backend::ProviderRegistry;
use super::workflow_provider::dependency_key;
use super::{
    ConnectionReference, ConnectorError, ConnectorOutput, EffectStatus, ImplementationKind, codes,
};

/// One dependency call: the alias resolved to the connection the run's
/// authority binds it to, a dependency invocation derived from `parent`
/// (narrowed to the dependency's granted operations, one level deeper, under
/// the parent's deadline and budget, sharing its effect evidence), and the
/// dispatch, held to the target's declared input and output schemas.
///
/// The one path for a workflow package's `connector_call` on an alias and a
/// Rust provider's [`AttemptContext::call_dependency`](super::AttemptContext::call_dependency).
pub(crate) async fn dispatch_dependency(
    providers: &ProviderRegistry,
    parent: &Invocation,
    alias: &str,
    operation: &str,
    input: Value,
) -> Result<ConnectorOutput, ConnectorError> {
    let effect = parent.effect_status();
    let key = dependency_key(&parent.connection().connection_id, alias);
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
                alias
            ),
            effect,
        ));
    };
    let target = dependency_target(
        providers.deployment_store(),
        &bound,
        operation,
        &input,
        effect,
    )?;
    let child = parent.derive_dependency(
        &key,
        target.identity,
        operation.to_string(),
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
    // A Rust target's own declared schemas hold on this path as on a
    // workflow's `connector_call` (a package target's are checked by
    // `dependency_target` and below).
    if target.package_digest.is_none() {
        providers.check_described_input(
            backend.as_ref(),
            operation,
            &bound.connection_id,
            &input,
        )?;
    }
    let output = providers
        .dispatch_attempt(&child, backend.as_ref(), input)
        .await?;
    match &target.package_digest {
        Some(package_digest) => providers.deployment_store().check_operation_output(
            package_digest,
            operation,
            &bound.connection_id,
            &output.data,
            child.effect_status(),
        )?,
        None => providers.check_described_output(
            backend.as_ref(),
            operation,
            &bound.connection_id,
            &output.data,
            child.effect_status(),
        )?,
    }
    Ok(output)
}

/// Where a dependency call dispatches: the backend and the identity it runs
/// under, and the uploaded package whose `output_schema` the answer is held
/// to (none for a built-in driver).
struct DependencyTarget {
    kind: ImplementationKind,
    backend_id: String,
    identity: super::EffectiveConnectionIdentity,
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
    // A Direct Rust driver compiled into this binary, Community's or one an
    // edition composed: it has no package, and its id is the backend id.
    let compiled_in = super::foundation::builtin_driver(digest)
        .map(str::to_string)
        .or_else(|| store.composed_rust_driver(digest).map(|id| id.to_string()));
    if let Some(driver) = compiled_in {
        return Ok(DependencyTarget {
            kind: ImplementationKind::Rust,
            backend_id: driver,
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

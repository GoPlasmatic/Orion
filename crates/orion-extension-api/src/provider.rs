//! The connector provider trait: [`ProviderBackend`].
//!
//! A provider is registered under its [`BackendKey`]
//! ([`ProviderBackend::key`], [`ProviderBackend::kind`],
//! [`ProviderBackend::driver_id`]): a `rust` backend under its driver id
//! (`org.orion.http`), a `workflow` or `wasm` package under its digest. The
//! node dispatches each attempt of a call to it with an [`AttemptContext`].

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use async_trait::async_trait;
use orion_api::Digest;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::attempt::AttemptContext;
use crate::contract::{ConnectorError, ConnectorOutput, ImplementationKind};
use crate::identity::{BackendKey, EffectiveConnectionIdentity, ProviderId, Revision};

/// One operation a provider declares ([`ProviderDescription`]): the names a
/// `connector_call` may give it, and the `orion-json-schema/1` documents its
/// input and its answer must satisfy, when it declares them. The node checks
/// the input before dispatch and the answer after, the way it checks an
/// uploaded package's `operations.<name>` schemas.
///
/// # The schema profile
///
/// A declared schema must be inside `orion-json-schema/1`:
///
/// - its root declares `"$schema": "https://json-schema.org/draft/2020-12/schema"`,
///   and no nested schema declares `$schema`;
/// - it uses only these keywords: `$schema`, `$defs`, `$ref`, `title`,
///   `description`, `x-orion-sensitive`, `type`, `enum`, `const`, `minimum`,
///   `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `properties`,
///   `required`, `additionalProperties`, `minProperties`, `maxProperties`,
///   `items`, `minItems`, `maxItems`, `minLength`, `maxLength`, `allOf`,
///   `anyOf`, `oneOf` (so no `pattern` and no `format`);
/// - a `$ref` is local (`#/$defs/x`) or relative to another of the
///   provider's schemas, never a URL;
/// - `x-orion-sensitive` is a boolean.
///
/// What follows from it:
///
/// - a schema outside the profile fails the composition's build
///   (`CompositionError::InvalidProviderSchema`, naming the operation, the
///   slot and the schema pointer), so the node does not start;
/// - an input the input schema refuses is `invalid_input` (400) and the
///   provider is never called;
/// - an answer the output schema refuses is a `contract_violation`, after
///   the effect.
///
/// ```
/// use orion_extension_api::OperationDescription;
/// use serde_json::json;
///
/// let lookup = OperationDescription::new().with_input_schema(json!({
///     "$schema": "https://json-schema.org/draft/2020-12/schema",
///     "type": "object",
///     "required": ["id"],
///     "properties": { "id": { "type": "string", "minLength": 1 } },
///     "additionalProperties": false
/// }));
/// assert!(lookup.input_schema.is_some());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OperationDescription {
    /// Other names `connector_call` may call it by (`cache.get` for `get`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

impl OperationDescription {
    pub fn new() -> Self {
        Self::default()
    }

    /// Another name for the operation.
    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// The schema the call's input must satisfy.
    #[must_use]
    pub fn with_input_schema(mut self, schema: Value) -> Self {
        self.input_schema = Some(schema);
        self
    }

    /// The schema the provider's answer must satisfy.
    #[must_use]
    pub fn with_output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }
}

/// A connection a provider needs beside its own, declared under an alias
/// ([`ProviderDescription::with_dependency`]): a connection over this
/// provider names, in its `bindings`, the connection the alias is bound to,
/// and the provider calls it through [`AttemptContext::call_dependency`].
///
/// The provider reaches only the operations it declares here, on the
/// connection revision the deployment pins, under its own call's deadline and
/// budget; it never holds the target's configuration or secrets. A target
/// whose own provider declares dependencies is refused, so dependencies never
/// chain.
///
/// ```
/// use orion_extension_api::{DependencyDescription, ProviderDescription};
///
/// let description = ProviderDescription::new().with_dependency(
///     "artifact_store",
///     DependencyDescription::new()
///         .on_provider("com.orion.storage")
///         .with_operation("presign")
///         .optional(),
/// );
/// assert!(description.dependencies["artifact_store"].optional);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DependencyDescription {
    /// The provider id the bound connection must run on, when it must run
    /// on one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The operations the provider may call on it.
    #[serde(default)]
    pub operations: BTreeSet<String>,
    /// Whether a connection may leave the alias unbound.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

impl DependencyDescription {
    pub fn new() -> Self {
        Self::default()
    }

    /// Require the bound connection to run on `provider`.
    #[must_use]
    pub fn on_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Grant `operation` on the bound connection.
    #[must_use]
    pub fn with_operation(mut self, operation: impl Into<String>) -> Self {
        self.operations.insert(operation.into());
        self
    }

    /// Let a connection leave the alias unbound.
    #[must_use]
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }
}

/// What a provider declares about itself ([`ProviderBackend::describe`]):
/// its operations, by canonical name, and the connections it depends on, by
/// alias. A provider that declares one operation declares all of them: a
/// `connector_call` naming any other is refused when its workflow loads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderDescription {
    pub operations: BTreeMap<String, OperationDescription>,
    /// The connections it calls beside its own, by alias. Validated against a
    /// connection's `bindings` at upload and when a deployment is built.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, DependencyDescription>,
}

impl ProviderDescription {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare `name`.
    #[must_use]
    pub fn with_operation(
        mut self,
        name: impl Into<String>,
        operation: OperationDescription,
    ) -> Self {
        self.operations.insert(name.into(), operation);
        self
    }

    /// Declare the dependency `alias`.
    #[must_use]
    pub fn with_dependency(
        mut self,
        alias: impl Into<String>,
        dependency: DependencyDescription,
    ) -> Self {
        self.dependencies.insert(alias.into(), dependency);
        self
    }

    /// The operation `name` calls, by its canonical name or an alias,
    /// compared case-insensitively.
    pub fn operation(&self, name: &str) -> Option<(&str, &OperationDescription)> {
        self.operations
            .iter()
            .find(|(canonical, op)| {
                canonical.eq_ignore_ascii_case(name)
                    || op.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
            })
            .map(|(canonical, op)| (canonical.as_str(), op))
    }
}

/// What a provider is asked to validate: a connection's configuration and,
/// when it belongs to a stored connection, which one.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ValidateContext<'a> {
    config: &'a Value,
    connection_id: Option<&'a str>,
    revision: Option<Revision>,
    bindings: Option<&'a BTreeMap<String, String>>,
}

impl<'a> ValidateContext<'a> {
    /// A bare configuration, not yet tied to a connection.
    pub fn new(config: &'a Value) -> Self {
        Self {
            config,
            connection_id: None,
            revision: None,
            bindings: None,
        }
    }

    /// The connection's `bindings` (alias → connection id). The node has
    /// already checked them against the provider's declared dependencies; a
    /// provider reads them to judge configuration that depends on one being
    /// bound (an artifact source that names a bound store).
    #[must_use]
    pub fn with_bindings(mut self, bindings: &'a BTreeMap<String, String>) -> Self {
        self.bindings = Some(bindings);
        self
    }

    /// The connection's `bindings`, when the node gave them.
    pub fn bindings(&self) -> Option<&'a BTreeMap<String, String>> {
        self.bindings
    }

    /// Tie the configuration to a connection revision.
    #[must_use]
    pub fn for_connection(mut self, connection_id: &'a str, revision: Revision) -> Self {
        self.connection_id = Some(connection_id);
        self.revision = Some(revision);
        self
    }

    pub fn config(&self) -> &'a Value {
        self.config
    }

    pub fn connection_id(&self) -> Option<&'a str> {
        self.connection_id
    }

    pub fn revision(&self) -> Option<Revision> {
        self.revision
    }
}

/// Unified connector provider backend trait.
///
/// Providers register implementations of this trait through
/// [`Registrar::provider_backend`](crate::Registrar::provider_backend) without
/// modifying core enums or built-in handler tables. Implement it with
/// `#[async_trait::async_trait]`.
#[async_trait]
pub trait ProviderBackend: Send + Sync {
    /// Implementation kind (`Rust`, `Workflow`, or `Wasm`).
    fn kind(&self) -> ImplementationKind;

    /// The id the backend registers under: a `rust` backend's driver id (its
    /// [`ProviderId`](crate::ProviderId), e.g. `"org.orion.http"`),
    /// or a `workflow`/`wasm` backend's package digest.
    fn driver_id(&self) -> &str;

    /// Typed registration key (`BackendKey::Rust(ProviderId)`,
    /// `BackendKey::Workflow(Digest)`, or `BackendKey::Wasm(Digest)`).
    ///
    /// Only valid on a backend whose [`key_checked`](Self::key_checked) is
    /// `Ok`; composition and admission validate before ever calling this.
    /// A `workflow`/`wasm` backend registers under a `sha256:` digest, and an
    /// edition handing one an arbitrary string is a composition error, not a
    /// panic on the boot path (B-10).
    fn key(&self) -> BackendKey {
        self.key_checked()
            .expect("ProviderBackend::driver_id was not validated before key()")
    }

    /// [`key`](Self::key) as a `Result`: a `workflow`/`wasm` backend's
    /// `driver_id()` must parse as a digest. Composition and admission use
    /// this so a malformed edition id answers `CompositionError` instead of
    /// panicking.
    fn key_checked(&self) -> Result<BackendKey, String> {
        match self.kind() {
            ImplementationKind::Rust => Ok(BackendKey::Rust(ProviderId::from(self.driver_id()))),
            ImplementationKind::Workflow => Digest::parse(self.driver_id())
                .map(BackendKey::Workflow)
                .map_err(|e| format!("workflow provider id '{}': {e}", self.driver_id())),
            ImplementationKind::Wasm => Digest::parse(self.driver_id())
                .map(BackendKey::Wasm)
                .map_err(|e| format!("wasm provider id '{}': {e}", self.driver_id())),
        }
    }

    /// Validate resolved connection configuration against provider-specific invariants.
    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError>;

    /// Validate a connection's configuration with the connection around it.
    /// The host calls this one; the default defers to
    /// [`validate_config`](Self::validate_config), so a provider that needs the
    /// connection's identity or revision overrides it instead.
    fn validate_connection(&self, ctx: &ValidateContext<'_>) -> Result<(), ConnectorError> {
        self.validate_config(ctx.config())
    }

    /// Execute one attempt of `operation` with validated `input`.
    async fn invoke(
        &self,
        ctx: &AttemptContext,
        operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError>;

    /// Probe downstream connectivity or readiness under `ctx`.
    async fn health_check(&self, ctx: &AttemptContext) -> Result<(), ConnectorError> {
        let _ = ctx;
        Ok(())
    }

    /// Release what the provider holds for `identity` (a pooled client, a
    /// producer). The node calls it once the generation it publishes no longer
    /// pins that connection revision: the connection was removed, or replaced
    /// by a new revision. A call already admitted on the old generation may
    /// still hold what it acquired, so release idle resources rather than
    /// tearing down ones in use.
    async fn dispose(&self, identity: &EffectiveConnectionIdentity) {
        let _ = identity;
    }

    /// Flush and close, by `deadline`: the node is shutting down. Called once,
    /// after the channel drivers have stopped and before the trace queues
    /// drain, so a provider that buffers writes (a Kafka producer) can deliver
    /// them. Nothing is dispatched to the provider after it.
    async fn shutdown(&self, deadline: Instant) {
        let _ = deadline;
    }

    /// Input keys of `operation` that must be authored as literal strings:
    /// `connector_call` refuses a task whose `input` builds one from the
    /// message, at load. A SQL provider names its statement keys, so a
    /// caller's values reach the database only as bind parameters.
    fn literal_input_keys(&self, _operation: &str) -> Vec<String> {
        Vec::new()
    }

    /// The operations this provider serves, with their input and output
    /// schemas. `None` (the default) declares nothing: any operation name
    /// reaches [`Self::invoke`], which judges it. A description is enforced:
    /// an undeclared operation is refused when its workflow loads, and a
    /// declared schema is checked on every call. `/runtime-capabilities`
    /// serves it.
    fn describe(&self) -> Option<ProviderDescription> {
        None
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `dependencies` is on the wire only when a provider declares one, so a
    /// description that declares none serializes as it always did.
    #[test]
    fn a_description_serializes_dependencies_only_when_declared() {
        let bare = ProviderDescription::new().with_operation("get", OperationDescription::new());
        let json = serde_json::to_value(&bare).unwrap();
        assert!(json.get("dependencies").is_none(), "{json}");

        let declared = bare.with_dependency(
            "store",
            DependencyDescription::new()
                .on_provider("com.orion.storage")
                .with_operation("presign")
                .with_operation("head"),
        );
        let json = serde_json::to_value(&declared).unwrap();
        assert_eq!(
            json["dependencies"]["store"],
            serde_json::json!({
                "provider": "com.orion.storage",
                "operations": ["head", "presign"]
            }),
            "`optional` is omitted while false"
        );
        let back: ProviderDescription = serde_json::from_value(json).unwrap();
        assert_eq!(back, declared);
    }
}

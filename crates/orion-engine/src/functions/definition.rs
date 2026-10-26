//! A host function's declaration, beside its implementation.
//!
//! The config owns field names, order, descriptions, requiredness and output
//! defaults. The declaration supplies Orion's policy and the semantic detail a
//! JSON-shaped config field cannot express. The function inventory uses both
//! to build the authoring catalogue and register the serving handlers.

use std::borrow::Cow;

use dataflow_rs::expr::{FieldKind as ConfigKind, FieldSpec, Output, ValueKind};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Reference};
use dataflow_rs::{Ctx, Function, Path};
use serde_json::Value;

use super::schema::{
    ConnectorRule, FieldKind, FieldSchema, FunctionSchema, RetrySafety, StaticValidator, WriteShape,
};
use crate::connector::kind::ConnectorTarget;
use crate::kit;

/// A function compiled into Orion. Its declaration and constructor live with
/// its `Function` implementation; the inventory in the parent module supplies
/// registration and the catalogue from this one type.
pub trait OrionFunction: Function {
    /// The connector accepted by this function, or [`NoConnector`]. The same
    /// target governs authoring validation and the runtime's typed lookup.
    type Connector: ConnectorTarget;

    /// The policy and semantic metadata not represented by `Config::fields()`.
    const DEFINITION: Definition;

    /// Construct this function from the node's shared dependencies.
    fn from_deps(deps: &crate::HandlerDeps<'_>) -> Self;
}

/// A function with no `connector` reference.
pub struct NoConnector;

impl ConnectorTarget for NoConnector {
    type Config = ();
    const TYPES: &'static [crate::connector::ConnectorType] = &[];

    fn extract(_config: &crate::connector::ConnectorConfig) -> Option<&Self::Config> {
        None
    }

    fn noun() -> &'static str {
        "no connector"
    }
}

/// What an output expression evaluating to `null` means. This differs from
/// an omitted optional output, which writes nothing regardless of this policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullOutput {
    Data,
    Reject,
}

impl NullOutput {
    pub(crate) fn path(self) -> Option<&'static Path> {
        match self {
            Self::Data => Some(&kit::DATA),
            Self::Reject => None,
        }
    }
}

/// Semantic metadata for a field whose config type cannot state it all. For
/// example `Expr<OwnedDataValue>` may be a string key, while an interpolated
/// document may be an object of SQL parameters. Names, order, descriptions and
/// requiredness always come from the config.
#[derive(Debug, Clone, Copy)]
pub struct FieldOverlay {
    pub name: &'static str,
    pub kind: Option<FieldKind>,
    pub resolvable: bool,
    pub secret_at: &'static [&'static str],
    pub template_at: Option<&'static [&'static str]>,
}

impl FieldOverlay {
    pub const DEFAULT: Self = Self {
        name: "",
        kind: None,
        resolvable: false,
        secret_at: &[],
        template_at: None,
    };
}

/// A function's declaration. Output presence and defaults are derived from the
/// config; a handler states only its policy for a computed `null` destination.
#[derive(Debug)]
pub struct Definition {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub retry_safety: RetrySafety,
    pub fields: &'static [FieldOverlay],
    pub deny_unknown: bool,
    pub validate_static: Option<StaticValidator>,
    pub null_output: NullOutput,
    /// `None` executes for real offline. A field name selects a stub target;
    /// an empty name needs no target and accepts the wildcard stub.
    pub stub_target: Option<&'static str>,
    /// A harmless offline result when this operation needs no backend at all.
    pub stub_default: Option<fn(&Value) -> Value>,
}

impl Definition {
    pub const DEFAULT: Self = Self {
        name: "",
        description: "",
        category: "",
        retry_safety: RetrySafety::Pure,
        fields: &[],
        deny_unknown: true,
        validate_static: None,
        null_output: NullOutput::Data,
        stub_target: None,
        stub_default: None,
    };

    /// The execution engine's spec, without looking up the function by name.
    fn base_spec(&self) -> FunctionSpec {
        FunctionSpec::default()
            .with_description(self.description)
            .with_category(self.category)
            .with_retry(kit::retry(self.retry_safety))
    }

    /// Resolve a write destination using this function's declared null policy.
    pub fn output_path<'o>(
        &self,
        ctx: &Ctx<'_>,
        output: &'o Output,
    ) -> dataflow_rs::Result<Cow<'o, Path>> {
        kit::output_path(ctx, output, self.null_output.path())
    }
}

/// A handler's authoring schema, from its declaration and compiled config.
pub(super) fn schema<F: OrionFunction>() -> FunctionSchema {
    let definition = &F::DEFINITION;
    let fields = F::Config::fields();
    let connector = ConnectorRule::of(F::Connector::TYPES);
    let writes = match fields.iter().find(|field| field.primary) {
        Some(output) => WriteShape::OutputPath {
            default_root: output
                .default
                .as_deref()
                .and_then(|json| serde_json::from_str::<&str>(json).ok()),
        },
        None => WriteShape::Nothing,
    };
    FunctionSchema {
        name: definition.name,
        description: definition.description,
        category: definition.category,
        input_fields: fields
            .iter()
            .map(|field| field_schema(field, definition.fields))
            .collect(),
        writes,
        retry_safety: definition.retry_safety,
        connector,
        deny_unknown: definition.deny_unknown,
        validate_static: definition.validate_static,
        definition,
    }
}

/// The engine reference is built from the same associated connector target as
/// the authoring rule and typed runtime lookup.
pub fn spec<F: OrionFunction>() -> FunctionSpec {
    let mut spec = F::DEFINITION.base_spec();
    if !F::Connector::TYPES.is_empty() {
        spec.references = vec![Reference::new("connector", F::DEFINITION.name)];
    }
    spec
}

fn field_schema(field: &'static FieldSpec, overlays: &[FieldOverlay]) -> FieldSchema {
    let (kind, template_at) = field_shape(field.kind);
    let mut schema = FieldSchema {
        name: &field.name,
        description: field.description.as_deref().unwrap_or_default(),
        kind,
        required: field.required,
        template_at,
        ..FieldSchema::DEFAULT
    };
    if let Some(overlay) = overlays.iter().find(|overlay| overlay.name == field.name) {
        schema.kind = overlay.kind.unwrap_or(schema.kind);
        schema.resolvable = overlay.resolvable;
        schema.secret_at = overlay.secret_at;
        schema.template_at = overlay.template_at.unwrap_or(schema.template_at);
    }
    schema
}

fn field_shape(kind: ConfigKind) -> (FieldKind, &'static [&'static str]) {
    match kind {
        ConfigKind::Literal(kind) => (value_kind(kind), &[]),
        ConfigKind::Expr(kind) => (value_kind(kind), &[""]),
        ConfigKind::Output => (FieldKind::String, &[""]),
        ConfigKind::List(_) => (FieldKind::Array, &[]),
        ConfigKind::Map(value) => (
            FieldKind::Object,
            if matches!(value(), ConfigKind::Expr(_)) {
                &["*"]
            } else {
                &[]
            },
        ),
        ConfigKind::Nested(_) => (FieldKind::Object, &[]),
        _ => (FieldKind::Any, &[]),
    }
}

fn value_kind(kind: ValueKind) -> FieldKind {
    match kind {
        ValueKind::String | ValueKind::Path => FieldKind::String,
        ValueKind::Number => FieldKind::Number,
        ValueKind::Bool => FieldKind::Bool,
        ValueKind::Object => FieldKind::Object,
        ValueKind::Array => FieldKind::Array,
        _ => FieldKind::Any,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_overlay_addresses_a_declared_field_once() {
        for schema in super::super::schema::registry() {
            let mut names = HashSet::new();
            for overlay in schema.definition.fields {
                assert!(
                    names.insert(overlay.name),
                    "{}.{} repeated",
                    schema.name,
                    overlay.name
                );
                assert!(
                    schema
                        .input_fields
                        .iter()
                        .any(|field| field.name == overlay.name),
                    "{}.{} has metadata but no config field",
                    schema.name,
                    overlay.name
                );
            }
            if let Some(target) = schema
                .definition
                .stub_target
                .filter(|target| !target.is_empty())
            {
                assert!(
                    schema.input_fields.iter().any(|field| field.name == target),
                    "{} stubs a target absent from its config: {target}",
                    schema.name
                );
            }
        }
    }

    #[test]
    fn fields_keep_their_literal_and_secret_boundaries() {
        let functions = crate::FunctionRegistry::builtin();
        let http = functions.get("http_call").expect("HTTP function");
        let connector = http
            .input_fields
            .as_ref()
            .expect("fields")
            .iter()
            .find(|field| field.name == "connector")
            .expect("connector");
        assert!(connector.required);
        assert!(
            connector.template_at.is_empty(),
            "a connector name is a literal"
        );

        let jwt = functions.get("jwt_verify").expect("JWT function");
        let keys = jwt
            .input_fields
            .as_ref()
            .expect("fields")
            .iter()
            .find(|field| field.name == "keys")
            .expect("keys");
        assert_eq!(keys.kind, FieldKind::Array);
        assert_eq!(keys.secret_at, &["[].key"]);
        assert!(!keys.required, "a JWKS URL can supply the keys instead");
    }
}

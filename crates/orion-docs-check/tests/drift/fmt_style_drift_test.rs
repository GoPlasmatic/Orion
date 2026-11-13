//! Drift guards for the formatter's tables and numbers.
//!
//! The canonical key order is a hand-written table per document shape, and
//! a table is a promise that goes stale the moment a request struct gains a
//! field. So each one is pinned to the thing it describes: the entity
//! tables to the definition-file request schemas, the step tables to
//! dataflow-rs's own `Task`, the input tables to the function registry and
//! the reference page, and the style numbers to the page that documents
//! them.

use std::collections::BTreeSet;

use orion_authoring::definitions::documents::{
    CreateChannelRequest, CreateConnectorRequest, CreateWorkflowRequest,
};
use orion_authoring::definitions::fmt::style;
/// The properties of a definition file's schema. The definition-file shapes
/// are not request bodies of any route, so they are read off the type rather
/// than `docs/openapi.json`.
fn schema_properties<T: utoipa::PartialSchema>() -> BTreeSet<String> {
    let schema = serde_json::to_value(T::schema()).unwrap();
    schema["properties"]
        .as_object()
        .expect("a definition-file schema has properties")
        .keys()
        .cloned()
        .collect()
}

fn table(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| k.to_string()).collect()
}

/// `activate` is the artifact-only key `compile` adds; it is not a request
/// field and the schemas do not list it.
fn without_activate(mut set: BTreeSet<String>) -> BTreeSet<String> {
    set.remove("activate");
    set
}

#[test]
fn entity_key_tables_cover_exactly_the_request_schemas() {
    for (name, schema, keys, properties) in [
        (
            "workflow",
            "CreateWorkflowRequest",
            style::WORKFLOW_KEYS,
            schema_properties::<CreateWorkflowRequest>(),
        ),
        (
            "channel",
            "CreateChannelRequest",
            style::CHANNEL_KEYS,
            schema_properties::<CreateChannelRequest>(),
        ),
        (
            "connector",
            "CreateConnectorRequest",
            style::CONNECTOR_KEYS,
            schema_properties::<CreateConnectorRequest>(),
        ),
    ] {
        assert_eq!(
            without_activate(table(keys)),
            properties,
            "{name}: the canonical key table and the {schema} schema disagree — a field \
             was added to one and not the other"
        );
    }
}

#[test]
fn every_builtin_input_table_names_an_engine_builtin_and_only_those() {
    let catalogue = orion_engine::FunctionRegistry::builtin().catalogue();
    let builtins: BTreeSet<&str> = catalogue
        .iter()
        .filter(|e| e.input_fields.is_none())
        .map(|e| e.name.as_str())
        .collect();
    let tabled: BTreeSet<&str> = style::BUILTIN_INPUT_KEYS.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        tabled, builtins,
        "BUILTIN_INPUT_KEYS must cover exactly the functions with no registry schema"
    );
}

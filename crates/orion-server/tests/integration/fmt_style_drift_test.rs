//! Drift guards for the formatter's tables and numbers.
//!
//! The canonical key order is a hand-written table per document shape, and
//! a table is a promise that goes stale the moment a request struct gains a
//! field. So each one is pinned to the thing it describes: the entity
//! tables to the request schemas in `docs/openapi.json`, the step tables to
//! dataflow-rs's own `Task`, the input tables to the function registry and
//! the reference page, and the style numbers to the page that documents
//! them.

use std::collections::BTreeSet;

use orion::definitions::fmt::style::{self, STYLE};

const OPENAPI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/openapi.json");
const FMT_MD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/src/reference/fmt.md"
);
const WORKFLOWS_MD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/src/reference/workflows.md"
);
const FUNCTIONS_MD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/src/reference/functions.md"
);

fn schema_properties(name: &str) -> BTreeSet<String> {
    let spec: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(OPENAPI).unwrap()).unwrap();
    spec["components"]["schemas"][name]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("schema {name} has no properties"))
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
    for (name, schema, keys) in [
        ("workflow", "CreateWorkflowRequest", style::WORKFLOW_KEYS),
        ("channel", "CreateChannelRequest", style::CHANNEL_KEYS),
        ("connector", "CreateConnectorRequest", style::CONNECTOR_KEYS),
    ] {
        assert_eq!(
            without_activate(table(keys)),
            schema_properties(schema),
            "{name}: the canonical key table and the {schema} schema disagree — a field \
             was added to one and not the other"
        );
    }
}

/// The step tables against the workflow reference's own field tables.
/// `dataflow_rs::Task` is `#[non_exhaustive]` and not `Serialize`, so the
/// page — which `docs_routes_drift_test`'s siblings already keep honest —
/// is the closest pin available. A task's `description` is accepted by the
/// engine but not documented, and is allowed as the one extra.
#[test]
fn step_tables_match_the_workflow_reference() {
    let page = std::fs::read_to_string(WORKFLOWS_MD).unwrap();
    let section = |heading: &str| -> BTreeSet<String> {
        let body = page
            .split(heading)
            .nth(1)
            .unwrap_or_else(|| panic!("workflows.md lacks `{heading}`"));
        let body = body.split("\n##").next().unwrap();
        body.lines()
            .filter(|l| l.starts_with("| `"))
            .map(|l| {
                l.trim_start_matches("| `")
                    .split('`')
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect()
    };
    // The `## Tasks` section documents the task table and then the
    // `function` object's table; only the first is the task's own keys.
    let tasks_section = page.split("\n## Tasks").nth(1).unwrap();
    let task_keys: BTreeSet<String> = tasks_section
        .split("\n| `name` | string | **yes** | — | One of")
        .next()
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("| `"))
        .map(|l| {
            l.trim_start_matches("| `")
                .split('`')
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    let mut table_minus_description = table(style::TASK_KEYS);
    table_minus_description.remove("description");
    assert_eq!(
        table_minus_description, task_keys,
        "TASK_KEYS vs the Tasks table"
    );
    assert_eq!(
        table(style::GROUP_KEYS),
        section("\n### Task groups"),
        "GROUP_KEYS vs the Task groups table"
    );
}

#[test]
fn every_builtin_input_table_names_an_engine_builtin_and_only_those() {
    let catalogue = orion::engine::FunctionRegistry::builtin().catalogue();
    let builtins: BTreeSet<&str> = catalogue
        .iter()
        .filter(|e| e.input_fields.is_none())
        .flat_map(|e| std::iter::once(e.name.as_str()).chain(e.aliases.iter().map(String::as_str)))
        .collect();
    let tabled: BTreeSet<&str> = style::BUILTIN_INPUT_KEYS.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        tabled, builtins,
        "BUILTIN_INPUT_KEYS must cover exactly the functions with no registry schema"
    );
}

/// The reference page lists each function's fields in a table; the
/// formatter orders an input by the registry (Orion functions) or
/// `BUILTIN_INPUT_KEYS` (engine built-ins). Both are canonical order, so
/// they must agree — a reader who learned the fields from the page should
/// see them in that order in every formatted file.
#[test]
fn documented_field_order_is_the_formatters() {
    let doc = std::fs::read_to_string(FUNCTIONS_MD).unwrap();
    let mut checked = 0;
    let mut mismatches = Vec::new();
    for section in doc.split("\n### `").skip(1) {
        let name = section.split('`').next().unwrap();
        // The section's *first* table: `data_write` follows its field table
        // with one for the `write` envelope's own members.
        // A row may document several fields at once (`cc` / `bcc`); every
        // backticked name in its first cell counts, in order.
        let documented: Vec<String> = section
            .lines()
            .skip_while(|l| !l.starts_with("| `"))
            .take_while(|l| l.starts_with("| `"))
            .flat_map(|l| {
                let cell = l.trim_start_matches("| ").split(" | ").next().unwrap_or("");
                cell.split('`')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            // Top-level fields only: `mappings[].path` describes a member.
            .filter(|f| !f.contains('[') && !f.contains('.'))
            .collect();
        let Some(formatter) = orion::definitions::fmt::roles::input_key_order(Some(name)) else {
            continue;
        };
        if documented.is_empty() {
            continue;
        }
        let formatter: Vec<String> = formatter.iter().map(|s| s.to_string()).collect();
        if documented != formatter {
            mismatches.push(format!(
                "`{name}`: documented {documented:?}, formatter (registry) {formatter:?}"
            ));
        }
        checked += 1;
    }
    assert!(
        mismatches.is_empty(),
        "the reference page lists these functions' fields in a different order from the \
         registry, and the formatter follows the registry:\n{}",
        mismatches.join("\n")
    );
    assert!(
        checked >= 20,
        "only {checked} function sections were compared"
    );
}

#[test]
fn the_style_page_quotes_the_numbers_the_code_uses() {
    let page = std::fs::read_to_string(FMT_MD).expect("docs/src/reference/fmt.md");
    let quoted = |label: &str| -> usize {
        let row = page
            .lines()
            .find(|l| l.starts_with(&format!("| {label} |")))
            .unwrap_or_else(|| panic!("fmt.md has no `| {label} |` row"));
        row.split('|')
            .nth(2)
            .unwrap()
            .trim()
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap_or_else(|_| panic!("`{label}` row does not start with a number"))
    };
    assert_eq!(quoted("Line width"), STYLE.width);
    assert_eq!(quoted("Indent"), STYLE.indent);
    assert_eq!(quoted("Scalar array inline cap"), STYLE.max_scalar_inline);
}

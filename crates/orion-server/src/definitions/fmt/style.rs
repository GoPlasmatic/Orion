//! The house style: every number the formatter uses, and the canonical key
//! order of every object shape it recognises.
//!
//! There is no runtime configuration, by design — one style everywhere is
//! the formatter's whole value, the way it is `gofmt`'s. Changing a value here
//! is a code change that reformats `examples/` in the same commit
//! (`fmt_examples_test` fails otherwise), which is the right amount of
//! friction for a style change.

/// The numbers.
pub struct Style {
    /// A node prints on one line when it fits within this column.
    pub width: usize,
    /// Spaces per nesting level.
    pub indent: usize,
    /// An array of scalars longer than this breaks one element per line even
    /// when it would fit.
    pub max_scalar_inline: usize,
}

pub const STYLE: Style = Style {
    width: 100,
    indent: 2,
    max_scalar_inline: 8,
};

/// The key `SharedDefinitions::splice` merges from: it is the base the rest
/// of the object overrides, so it reads first in every object it appears in,
/// whatever the object's role.
pub const FROM_KEY: &str = "$from";

/// Canonical order of the keys a workflow document carries — the
/// `CreateWorkflowRequest` fields, in reading order, plus `activate`, the
/// artifact-only key `compile` adds.
pub const WORKFLOW_KEYS: &[&str] = &[
    "workflow_id",
    "name",
    "description",
    "tags",
    "priority",
    "condition",
    "loop",
    "continue_on_error",
    "activate",
    "tasks",
];

/// A leaf task: dataflow-rs's `Task` fields plus Orion's step-level
/// `terminal`.
///
/// `halt_on` sits beside `terminal` because they are the two halves of one
/// question — position and outcome — and an author reading a task wants to see
/// them together rather than with the error-handling key between them.
pub const TASK_KEYS: &[&str] = &[
    "id",
    "name",
    "description",
    "condition",
    "terminal",
    "halt_on",
    "continue_on_error",
    "function",
];

/// A task group: the step keys that apply to a group, then its members.
pub const GROUP_KEYS: &[&str] = &[
    "id",
    "name",
    "description",
    "condition",
    "terminal",
    "tasks",
];

/// A fragment call site in source form.
pub const USE_STEP_KEYS: &[&str] = &["id", "use", "with"];

pub const FUNCTION_KEYS: &[&str] = &["name", "input"];

pub const MAPPING_KEYS: &[&str] = &["path", "logic"];

pub const VALIDATION_RULE_KEYS: &[&str] = &["logic", "message"];

/// The `loop` object, in the order the reference documents it.
pub const LOOP_KEYS: &[&str] = &["counter", "init", "max", "increment"];

/// `CreateChannelRequest`, in reading order, plus `activate`.
pub const CHANNEL_KEYS: &[&str] = &[
    "channel_id",
    "name",
    "description",
    "tags",
    "channel_type",
    "protocol",
    "methods",
    "route_pattern",
    "topic",
    "consumer_group",
    "priority",
    "workflow_id",
    "activate",
    "transport_config",
    "config",
];

/// `CreateConnectorRequest`, in reading order.
pub const CONNECTOR_KEYS: &[&str] = &["id", "name", "connector_type", "enabled", "tags", "config"];

/// A shared-definitions document. Any other namespace an author adds follows
/// these in author order.
pub const SHARED_DOC_KEYS: &[&str] = &["constants", "errors", "fragments"];

pub const FRAGMENT_KEYS: &[&str] = &["params", "tasks"];

/// A `*.case.json` file — the `TestCase` fields in the order `orion-server
/// test` documents them.
pub const CASE_KEYS: &[&str] = &[
    "name",
    "workflow",
    "input",
    "metadata",
    "secrets",
    "stubs",
    "stubs_file",
    "expect",
    "expect_errors",
    "expect_calls",
    "expect_tasks",
];

/// Input key order for the dataflow-rs built-ins, which declare no schema in
/// the function registry. Orion's own functions take theirs from the
/// registry's field tables, so this covers exactly the names
/// `schema::catalogue` lists with `source: Engine`. Order follows the
/// reference page, which is where a reader learned the fields.
pub const BUILTIN_INPUT_KEYS: &[(&str, &[&str])] = &[
    ("parse_json", &["source", "target"]),
    ("parse_xml", &["source", "target"]),
    ("map", &["mappings"]),
    ("filter", &["condition", "on_reject"]),
    ("validation", &["rules"]),
    ("validate", &["rules"]),
    ("log", &["message", "level", "fields"]),
    ("publish_json", &["source", "target", "pretty"]),
    ("publish_xml", &["source", "target", "root_element"]),
];

/// A promotion artifact's envelope.
pub const ARTIFACT_KEYS: &[&str] = &["package", "requires", "connectors", "workflows", "channels"];

/// The `package` block inside an artifact.
pub const ARTIFACT_META_KEYS: &[&str] = &[
    "name",
    "version",
    "orion",
    "content_hash",
    "exported_from",
    "exported_at",
];

/// Every table, for the drift tests that pin them to the request structs.
pub fn all_tables() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("workflow", WORKFLOW_KEYS),
        ("task", TASK_KEYS),
        ("group", GROUP_KEYS),
        ("use_step", USE_STEP_KEYS),
        ("function", FUNCTION_KEYS),
        ("mapping", MAPPING_KEYS),
        ("validation_rule", VALIDATION_RULE_KEYS),
        ("loop", LOOP_KEYS),
        ("channel", CHANNEL_KEYS),
        ("connector", CONNECTOR_KEYS),
        ("shared_doc", SHARED_DOC_KEYS),
        ("fragment", FRAGMENT_KEYS),
        ("case", CASE_KEYS),
        ("artifact", ARTIFACT_KEYS),
        ("artifact_meta", ARTIFACT_META_KEYS),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_table_repeats_a_key() {
        for (name, table) in all_tables() {
            let mut sorted = table.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), table.len(), "{name} repeats a key");
        }
    }
}

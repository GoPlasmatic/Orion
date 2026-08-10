//! Input-schema registry for engine functions.
//!
//! Each entry in the registry describes the JSON `function.input` object a
//! workflow author must provide for a given function name. The schemas are
//! consumed in two places:
//!
//!   1. Workflow create/update validation — `validate_input()` walks the
//!      schema and emits structured `FieldError` items (via A3) so authors
//!      see exactly which input key is missing or has the wrong type before
//!      the workflow is ever activated.
//!   2. `GET /api/v1/admin/functions` — surfaces the registry so external
//!      tools (CLIs, IDEs, generated docs) know the shape of each function.
//!
//! Schemas are intentionally hand-rolled rather than derived: the dataflow-rs
//! input structs use deserialize-time defaults that don't show up in derived
//! schemas, and we want to keep the validator dependency-free.

use serde::Serialize;
use serde_json::Value;

use crate::errors::FieldError;

/// Coarse type tag for a function input field. Mirrors the JSON value kinds
/// the validator can check without bringing in a full JSON-Schema engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    String,
    Number,
    Bool,
    Object,
    Array,
    /// Accept any JSON value. Used for free-form payloads like the `value`
    /// passed to `cache_write` or `data` passed to `channel_call`.
    Any,
}

impl FieldKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FieldKind::String => "string",
            FieldKind::Number => "number",
            FieldKind::Bool => "bool",
            FieldKind::Object => "object",
            FieldKind::Array => "array",
            FieldKind::Any => "any",
        }
    }

    fn matches(self, v: &Value) -> bool {
        match self {
            FieldKind::String => v.is_string(),
            FieldKind::Number => v.is_number(),
            FieldKind::Bool => v.is_boolean(),
            FieldKind::Object => v.is_object(),
            FieldKind::Array => v.is_array(),
            FieldKind::Any => true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldSchema {
    pub name: &'static str,
    pub description: &'static str,
    pub kind: FieldKind,
    pub required: bool,
    /// Whether the handler folds `{"var": ..}` nodes in this field against the
    /// message context before use (see
    /// `connector_helpers::resolve_value`). Resolvable fields accept a
    /// `{"var": ..}` node in place of a literal of their declared `kind`;
    /// everything else — connector names, SQL text, output paths — stays
    /// literal by design.
    pub resolvable: bool,
    /// A second accepted spelling for this field, or `None`.
    ///
    /// Two fields have one, both spelled `response_path` (the pre-1.0 name of
    /// `output`): `http_call.output`, via a serde alias on dataflow-rs's
    /// `HttpCallConfig`, and `channel_call.output`, via an alias on Orion's
    /// own struct. A serde alias cannot express precedence, so supplying
    /// **both** spellings is a duplicate-field parse error rather than an
    /// "`output` wins" rule. `check_fields` reports that here instead of
    /// letting the workflow load and quarantine its channel.
    pub alias: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSchema {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub input_fields: &'static [FieldSchema],
    /// Whether a key outside `input_fields` is an error rather than ignored.
    ///
    /// True for the functions dataflow-rs owns the config struct for
    /// (`http_call`, `publish_kafka`), whose structs are `deny_unknown_fields`
    /// as of 3.1: a misspelled key there fails `Workflow::from_json`, which for
    /// Orion means the channel is quarantined at load. Catching it at authoring
    /// time turns that into a 400 naming the field. Orion's own handlers take
    /// freeform `serde_json::Value` inputs and keep ignoring extra keys.
    pub deny_unknown: bool,
}

// F53: each function's field table lives in the module implementing it, so a
// handler and the schema describing it are edited in one place. Every
// schema/handler divergence this audit found — F23's `channel_call` input, the
// `method` casing, the Mongo `database` rule — was a table that drifted because
// it was in a different file from the code it described.
use super::cache_read::CACHE_READ_FIELDS;
use super::cache_write::CACHE_WRITE_FIELDS;
use super::channel_call::CHANNEL_CALL_FIELDS;
use super::data_query::DATA_QUERY_FIELDS;
use super::data_write::{DATA_WRITE_ENVELOPE_FIELDS, DATA_WRITE_FIELDS};
use super::db_read::DB_READ_FIELDS;
use super::db_write::DB_WRITE_FIELDS;
use super::http_call::HTTP_CALL_FIELDS;
use super::mongo_read::MONGO_READ_FIELDS;
use super::publish_kafka::PUBLISH_KAFKA_FIELDS;

const REGISTRY: &[FunctionSchema] = &[
    FunctionSchema {
        name: "cache_read",
        description: "Read a value from a cache connector (Redis or in-memory).",
        category: "connector",
        input_fields: CACHE_READ_FIELDS,
        deny_unknown: false,
    },
    FunctionSchema {
        name: "cache_write",
        description: "Write a value to a cache connector.",
        category: "connector",
        input_fields: CACHE_WRITE_FIELDS,
        deny_unknown: false,
    },
    FunctionSchema {
        name: "db_read",
        description: "Execute a SELECT against a SQL connector.",
        category: "connector",
        input_fields: DB_READ_FIELDS,
        deny_unknown: false,
    },
    FunctionSchema {
        name: "db_write",
        description: "Execute INSERT/UPDATE/DELETE against a SQL connector.",
        category: "connector",
        input_fields: DB_WRITE_FIELDS,
        deny_unknown: false,
    },
    FunctionSchema {
        name: "data_query",
        description: "Run a backend-neutral query (filter + envelope) against a SQL, MongoDB, or Elasticsearch connector.",
        category: "connector",
        input_fields: DATA_QUERY_FIELDS,
        deny_unknown: false,
    },
    FunctionSchema {
        name: "data_write",
        description: "Run a backend-neutral mutation (insert/update/delete/upsert) against a SQL, MongoDB, or Elasticsearch connector.",
        category: "connector",
        input_fields: DATA_WRITE_FIELDS,
        deny_unknown: false,
    },
    FunctionSchema {
        name: "mongo_read",
        description: "Run find() against a MongoDB connector.",
        category: "connector",
        input_fields: MONGO_READ_FIELDS,
        deny_unknown: false,
    },
    FunctionSchema {
        name: "channel_call",
        description: "Invoke another channel's workflow in-process (no HTTP hop).",
        category: "control",
        input_fields: CHANNEL_CALL_FIELDS,
        deny_unknown: false,
    },
    FunctionSchema {
        name: "http_call",
        description: "HTTP request to an HTTP connector with retry + circuit breaker.",
        category: "connector",
        input_fields: HTTP_CALL_FIELDS,
        deny_unknown: true,
    },
    FunctionSchema {
        name: "publish_kafka",
        description: "Publish a message to a Kafka topic via a Kafka connector.",
        category: "connector",
        input_fields: PUBLISH_KAFKA_FIELDS,
        deny_unknown: true,
    },
];

/// Every function that has an input schema. Accepted function names
/// without an entry here (e.g. `map`, `log`, `filter`) are still accepted
/// by workflows — they just won't get input-schema checking.
pub fn registry() -> &'static [FunctionSchema] {
    REGISTRY
}

fn find(name: &str) -> Option<&'static FunctionSchema> {
    REGISTRY.iter().find(|s| s.name == name)
}

/// A `{"var": ..}` node — the one shape a `resolvable` field may carry in
/// place of a literal of its declared kind. Nodes nested deeper are not checked
/// here: the declared kind still describes the field's own shape, and the
/// resolver folds `{"var": ..}` at any depth inside it.
fn is_var_node(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == 1 && o.contains_key("var"))
}

/// Check one field list against one JSON object, reporting paths under
/// `path_prefix`. Shared by the top-level input check and `data_write`'s
/// nested `write` envelope.
fn check_fields(
    fields: &[FieldSchema],
    input: &Value,
    path_prefix: &str,
    function_name: &str,
) -> Vec<FieldError> {
    let mut errors = Vec::new();
    let Some(obj) = input.as_object() else {
        return errors;
    };
    for field in fields {
        // An aliased field may be supplied under either name — but not both.
        // Upstream's alias makes that a `duplicate field` parse error, so
        // there is no precedence to fall back on.
        let alias_value = field.alias.and_then(|alias| obj.get(alias));
        if let Some(alias) = field.alias
            && obj.contains_key(field.name)
            && alias_value.is_some()
        {
            errors.push(FieldError::new(
                format!("{path_prefix}.{}", field.name),
                "DUPLICATE_FIELD",
                format!(
                    "'{}' and its alias '{alias}' are both set; supply exactly one",
                    field.name
                ),
            ));
            continue;
        }
        match (obj.get(field.name).or(alias_value), field.required) {
            (None, true) => errors.push(FieldError::new(
                format!("{path_prefix}.{}", field.name),
                "REQUIRED",
                format!(
                    "function '{function_name}' requires '{}' ({})",
                    field.name,
                    field.kind.as_str()
                ),
            )),
            (Some(v), _) if !field.kind.matches(v) && !(field.resolvable && is_var_node(v)) => {
                errors.push(
                    FieldError::new(
                        format!("{path_prefix}.{}", field.name),
                        "TYPE_MISMATCH",
                        format!("expected {} for '{}'", field.kind.as_str(), field.name),
                    )
                    .with_expected(Value::String(field.kind.as_str().to_string()))
                    .with_got(v.clone()),
                );
            }
            _ => {}
        }
    }
    errors
}

/// Report every key in `input` that the schema does not declare.
///
/// Only called for functions whose upstream config struct is
/// `deny_unknown_fields` — see [`FunctionSchema::deny_unknown`]. Without this
/// a typo like `outputs` passes create, activates, and then fails
/// `Workflow::from_json` at engine build, taking its whole channel into
/// quarantine with a message about a field the author cannot see from the API.
fn check_unknown_fields(
    fields: &[FieldSchema],
    input: &Value,
    path_prefix: &str,
    function_name: &str,
) -> Vec<FieldError> {
    let Some(obj) = input.as_object() else {
        return Vec::new();
    };
    obj.keys()
        .filter(|key| {
            !fields
                .iter()
                .any(|f| f.name == key.as_str() || f.alias == Some(key.as_str()))
        })
        .map(|key| {
            FieldError::new(
                format!("{path_prefix}.{key}"),
                "UNKNOWN_FIELD",
                format!(
                    "function '{function_name}' has no input field '{key}' — \
                     it would be rejected when the workflow is loaded"
                ),
            )
        })
        .collect()
}

/// Validate a function's `input` JSON against the registered schema for
/// `function_name`. `task_path` is the dotted prefix used to build field
/// paths (e.g. `"tasks[2]"`). Returns an empty `Vec` when the function
/// has no registered schema or all checks pass.
///
/// At least one of `channel` / `channel_logic` is required for
/// `channel_call`; that cross-field rule is enforced here in addition
/// to the per-field schema checks.
pub fn validate_input(function_name: &str, input: &Value, task_path: &str) -> Vec<FieldError> {
    let Some(schema) = find(function_name) else {
        return Vec::new();
    };

    let mut errors = Vec::new();
    let obj = match input.as_object() {
        Some(o) => o,
        None => {
            errors.push(FieldError::new(
                format!("{task_path}.function.input"),
                "TYPE_MISMATCH",
                format!("function '{function_name}' input must be a JSON object"),
            ));
            return errors;
        }
    };

    let input_path = format!("{task_path}.function.input");
    errors.extend(check_fields(
        schema.input_fields,
        input,
        &input_path,
        function_name,
    ));
    if schema.deny_unknown {
        errors.extend(check_unknown_fields(
            schema.input_fields,
            input,
            &input_path,
            function_name,
        ));
    }

    // Cross-field: data_write's mutation envelope. Nested under `write` since
    // W7; the pre-1.0 flat form is still accepted, and whichever shape the
    // task uses is checked against the same field list.
    if function_name == "data_write" {
        match obj.get("write") {
            // A non-object `write` is already reported by the field loop above.
            Some(w) if w.is_object() => errors.extend(check_fields(
                DATA_WRITE_ENVELOPE_FIELDS,
                w,
                &format!("{input_path}.write"),
                function_name,
            )),
            Some(_) => {}
            // Legacy flat form: envelope keys sit alongside the handler keys.
            None if obj.contains_key("op") => errors.extend(check_fields(
                DATA_WRITE_ENVELOPE_FIELDS,
                input,
                &input_path,
                function_name,
            )),
            None => errors.push(FieldError::new(
                format!("{input_path}.write"),
                "REQUIRED",
                "function 'data_write' requires 'write' (object): the mutation \
                 envelope { op, target, … }",
            )),
        }
    }

    // Cross-field: channel_call requires either `channel` or `channel_logic`.
    if function_name == "channel_call"
        && obj.get("channel").is_none()
        && obj.get("channel_logic").is_none()
    {
        errors.push(FieldError::new(
            format!("{task_path}.function.input"),
            "REQUIRED",
            "channel_call requires either 'channel' (static) or 'channel_logic' (dynamic)",
        ));
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_function_returns_no_errors() {
        // Functions without registered schemas pass through — keeps the door
        // open for ad-hoc dataflow-rs functions that haven't been catalogued.
        let errs = validate_input("nope", &json!({}), "tasks[0]");
        assert!(errs.is_empty());
    }

    #[test]
    fn cache_read_missing_connector_is_required_error() {
        let errs = validate_input("cache_read", &json!({"key": "k"}), "tasks[0]");
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].path, "tasks[0].function.input.connector");
        assert_eq!(errs[0].code, "REQUIRED");
    }

    #[test]
    fn cache_read_full_input_validates() {
        let errs = validate_input(
            "cache_read",
            &json!({"connector": "c", "key": "k", "output": "data.out"}),
            "tasks[0]",
        );
        assert!(errs.is_empty(), "{:?}", errs);
    }

    #[test]
    fn type_mismatch_reports_expected_and_got() {
        let errs = validate_input(
            "cache_read",
            &json!({"connector": 42, "key": "k"}),
            "tasks[1]",
        );
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, "TYPE_MISMATCH");
        assert_eq!(errs[0].path, "tasks[1].function.input.connector");
        assert_eq!(errs[0].expected.as_ref().expect("test"), &json!("string"));
        assert_eq!(errs[0].got.as_ref().expect("test"), &json!(42));
    }

    #[test]
    fn non_object_input_emits_single_type_error() {
        let errs = validate_input("cache_read", &json!("not an object"), "tasks[0]");
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].path, "tasks[0].function.input");
        assert_eq!(errs[0].code, "TYPE_MISMATCH");
    }

    #[test]
    fn mongo_read_collects_all_missing_required_at_once() {
        let errs = validate_input("mongo_read", &json!({"connector": "c"}), "tasks[0]");
        let paths: Vec<&str> = errs.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"tasks[0].function.input.database"));
        assert!(paths.contains(&"tasks[0].function.input.collection"));
    }

    #[test]
    fn channel_call_needs_channel_or_logic() {
        let errs = validate_input("channel_call", &json!({}), "tasks[0]");
        assert!(errs.iter().any(|e| e.code == "REQUIRED"
            && e.path == "tasks[0].function.input"
            && e.message.contains("channel_call")));
    }

    #[test]
    fn channel_call_with_static_channel_is_ok() {
        let errs = validate_input(
            "channel_call",
            &json!({"channel": "downstream"}),
            "tasks[0]",
        );
        assert!(errs.is_empty(), "{:?}", errs);
    }

    #[test]
    fn channel_call_with_dynamic_logic_is_ok() {
        let errs = validate_input(
            "channel_call",
            &json!({"channel_logic": {"var": "data.target"}}),
            "tasks[0]",
        );
        assert!(errs.is_empty(), "{:?}", errs);
    }

    #[test]
    fn registry_is_non_empty_and_contains_all_known_connector_functions() {
        let names: Vec<&str> = registry().iter().map(|s| s.name).collect();
        assert!(names.contains(&"cache_read"));
        assert!(names.contains(&"cache_write"));
        assert!(names.contains(&"db_read"));
        assert!(names.contains(&"db_write"));
        assert!(names.contains(&"mongo_read"));
        assert!(names.contains(&"channel_call"));
        assert!(names.contains(&"http_call"));
        assert!(names.contains(&"publish_kafka"));
    }
}

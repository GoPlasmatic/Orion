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
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSchema {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub input_fields: &'static [FieldSchema],
}

const CACHE_READ_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the cache connector to read from.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "key",
        description: "Cache key to look up.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where the result is stored. Defaults to \"data\".",
        kind: FieldKind::String,
        required: false,
    },
];

const CACHE_WRITE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the cache connector to write to.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "key",
        description: "Cache key to set.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "value",
        description: "Value to store. May be any JSON value.",
        kind: FieldKind::Any,
        required: true,
    },
    FieldSchema {
        name: "ttl_secs",
        description: "Time-to-live in seconds. Omit for no expiry.",
        kind: FieldKind::Number,
        required: false,
    },
];

const DB_READ_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the SQL connector to query.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "query",
        description: "SQL query. Use $1, $2, ... placeholders bound from `params`.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "params",
        description: "Array of values to bind to query placeholders.",
        kind: FieldKind::Array,
        required: false,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where rows are written. Defaults to \"data\".",
        kind: FieldKind::String,
        required: false,
    },
];

const DB_WRITE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the SQL connector to execute against.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "query",
        description: "INSERT/UPDATE/DELETE statement.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "params",
        description: "Array of values to bind to query placeholders.",
        kind: FieldKind::Array,
        required: false,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the rows-affected count is written.",
        kind: FieldKind::String,
        required: false,
    },
];

const DATA_QUERY_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the db (SQL/MongoDB) or es (Elasticsearch) connector to query.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "query",
        description: "Backend-neutral query envelope: source/filter/fields/sort/limit/skip/include.",
        kind: FieldKind::Object,
        required: true,
    },
    FieldSchema {
        name: "database",
        description: "MongoDB database name (required for MongoDB connectors).",
        kind: FieldKind::String,
        required: false,
    },
    FieldSchema {
        name: "schema",
        description: "Optional inline entity schema (renames, type hints, allowlist, relations) \
                      enabling some/all/none and typed coercion.",
        kind: FieldKind::Object,
        required: false,
    },
    FieldSchema {
        name: "params",
        description: "Object of named values folded into the filter's {\"param\": ..} nodes. \
                      A value of {\"var\": \"path\"} is read from the message context.",
        kind: FieldKind::Object,
        required: false,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where rows are written. Defaults to \"data\".",
        kind: FieldKind::String,
        required: false,
    },
];

const DATA_WRITE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the db (SQL/MongoDB) or es (Elasticsearch) connector to write to.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "op",
        description: "Mutation kind: \"insert\", \"update\", \"delete\", or \"upsert\".",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "target",
        description: "Logical entity to write to (schema-resolved to a table/collection).",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "values",
        description: "Row object, or array of row objects (bulk), for insert/upsert.",
        kind: FieldKind::Any,
        required: false,
    },
    FieldSchema {
        name: "set",
        description: "Object of column → value assignments for update/upsert.",
        kind: FieldKind::Object,
        required: false,
    },
    FieldSchema {
        name: "filter",
        description: "Query-dialect filter selecting rows for update/delete. An \
                      update/delete without it is rejected unless \"all\": true is set.",
        kind: FieldKind::Object,
        required: false,
    },
    FieldSchema {
        name: "on_conflict",
        description: "Upsert conflict clause: { \"target\": [cols], \"action\": \"update\"|\"nothing\" }.",
        kind: FieldKind::Object,
        required: false,
    },
    FieldSchema {
        name: "returning",
        description: "Column names to return from mutated rows (Postgres/SQLite only).",
        kind: FieldKind::Array,
        required: false,
    },
    FieldSchema {
        name: "all",
        description: "Acknowledge an intentionally unfiltered update/delete (affects every row).",
        kind: FieldKind::Bool,
        required: false,
    },
    FieldSchema {
        name: "database",
        description: "MongoDB database name (required for MongoDB connectors).",
        kind: FieldKind::String,
        required: false,
    },
    FieldSchema {
        name: "schema",
        description: "Optional inline entity schema (renames, allowlist, writable flag).",
        kind: FieldKind::Object,
        required: false,
    },
    FieldSchema {
        name: "params",
        description: "Object of named values folded into {\"param\": ..} nodes in values/set/filter. \
                      A value of {\"var\": \"path\"} is read from the message context.",
        kind: FieldKind::Object,
        required: false,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where the write result is written. Defaults to \"data\".",
        kind: FieldKind::String,
        required: false,
    },
];

const MONGO_READ_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the MongoDB connector.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "database",
        description: "Mongo database name.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "collection",
        description: "Mongo collection name.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "filter",
        description: "Mongo find() filter document. Defaults to {}.",
        kind: FieldKind::Object,
        required: false,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where matched documents are written.",
        kind: FieldKind::String,
        required: false,
    },
];

const CHANNEL_CALL_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "channel",
        description: "Target channel name to invoke. Mutually exclusive with channel_logic.",
        kind: FieldKind::String,
        required: false,
    },
    FieldSchema {
        name: "channel_logic",
        description: "JSONLogic expression evaluating to the target channel name.",
        kind: FieldKind::Any,
        required: false,
    },
    FieldSchema {
        name: "data",
        description: "Static payload to pass to the target channel.",
        kind: FieldKind::Any,
        required: false,
    },
    FieldSchema {
        name: "data_logic",
        description: "JSONLogic expression evaluating to the payload to pass.",
        kind: FieldKind::Any,
        required: false,
    },
    FieldSchema {
        name: "response_path",
        description: "Dotted path where the called channel's response is stored.",
        kind: FieldKind::String,
        required: false,
    },
    FieldSchema {
        name: "timeout_ms",
        description: "Per-call timeout in milliseconds.",
        kind: FieldKind::Number,
        required: false,
    },
];

const HTTP_CALL_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the HTTP connector to call.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "method",
        description: "HTTP method (GET, POST, PUT, DELETE, PATCH). Defaults to GET.",
        kind: FieldKind::String,
        required: false,
    },
    FieldSchema {
        name: "path",
        description: "Static path appended to the connector's base URL.",
        kind: FieldKind::String,
        required: false,
    },
    FieldSchema {
        name: "path_logic",
        description: "JSONLogic expression evaluated to derive the request path.",
        kind: FieldKind::Any,
        required: false,
    },
    FieldSchema {
        name: "headers",
        description: "Additional request headers.",
        kind: FieldKind::Object,
        required: false,
    },
    FieldSchema {
        name: "body",
        description: "Static request body (any JSON value).",
        kind: FieldKind::Any,
        required: false,
    },
    FieldSchema {
        name: "body_logic",
        description: "JSONLogic expression evaluated to derive the request body.",
        kind: FieldKind::Any,
        required: false,
    },
    FieldSchema {
        name: "response_path",
        description: "Dotted path where the response body is written. Omit to discard it.",
        kind: FieldKind::String,
        required: false,
    },
    FieldSchema {
        name: "timeout_ms",
        description: "Request timeout in milliseconds. Defaults to 30000.",
        kind: FieldKind::Number,
        required: false,
    },
];

const PUBLISH_KAFKA_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the Kafka connector to publish through.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "topic",
        description: "Target topic name.",
        kind: FieldKind::String,
        required: true,
    },
    FieldSchema {
        name: "key_logic",
        description: "JSONLogic expression to derive the message key.",
        kind: FieldKind::Any,
        required: false,
    },
    FieldSchema {
        name: "value_logic",
        description: "JSONLogic expression to derive the message value.",
        kind: FieldKind::Any,
        required: false,
    },
];

const REGISTRY: &[FunctionSchema] = &[
    FunctionSchema {
        name: "cache_read",
        description: "Read a value from a cache connector (Redis or in-memory).",
        category: "connector",
        input_fields: CACHE_READ_FIELDS,
    },
    FunctionSchema {
        name: "cache_write",
        description: "Write a value to a cache connector.",
        category: "connector",
        input_fields: CACHE_WRITE_FIELDS,
    },
    FunctionSchema {
        name: "db_read",
        description: "Execute a SELECT against a SQL connector.",
        category: "connector",
        input_fields: DB_READ_FIELDS,
    },
    FunctionSchema {
        name: "db_write",
        description: "Execute INSERT/UPDATE/DELETE against a SQL connector.",
        category: "connector",
        input_fields: DB_WRITE_FIELDS,
    },
    FunctionSchema {
        name: "data_query",
        description: "Run a backend-neutral query (filter + envelope) against a SQL, MongoDB, or Elasticsearch connector.",
        category: "connector",
        input_fields: DATA_QUERY_FIELDS,
    },
    FunctionSchema {
        name: "data_write",
        description: "Run a backend-neutral mutation (insert/update/delete/upsert) against a SQL, MongoDB, or Elasticsearch connector.",
        category: "connector",
        input_fields: DATA_WRITE_FIELDS,
    },
    FunctionSchema {
        name: "mongo_read",
        description: "Run find() against a MongoDB connector.",
        category: "connector",
        input_fields: MONGO_READ_FIELDS,
    },
    FunctionSchema {
        name: "channel_call",
        description: "Invoke another channel's workflow in-process (no HTTP hop).",
        category: "control",
        input_fields: CHANNEL_CALL_FIELDS,
    },
    FunctionSchema {
        name: "http_call",
        description: "HTTP request to an HTTP connector with retry + circuit breaker.",
        category: "connector",
        input_fields: HTTP_CALL_FIELDS,
    },
    FunctionSchema {
        name: "publish_kafka",
        description: "Publish a message to a Kafka topic via a Kafka connector.",
        category: "connector",
        input_fields: PUBLISH_KAFKA_FIELDS,
    },
];

/// All function schemas known to v0.2. Functions in `engine::KNOWN_FUNCTIONS`
/// without an entry here (e.g. `map`, `log`, `filter`) are still accepted
/// by workflows — they just won't get input-schema checking.
pub fn registry() -> &'static [FunctionSchema] {
    REGISTRY
}

pub fn find(name: &str) -> Option<&'static FunctionSchema> {
    REGISTRY.iter().find(|s| s.name == name)
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

    for field in schema.input_fields {
        let value = obj.get(field.name);
        match (value, field.required) {
            (None, true) => errors.push(FieldError::new(
                format!("{task_path}.function.input.{}", field.name),
                "REQUIRED",
                format!(
                    "function '{function_name}' requires '{}' ({})",
                    field.name,
                    field.kind.as_str()
                ),
            )),
            (Some(v), _) if !field.kind.matches(v) => {
                errors.push(
                    FieldError::new(
                        format!("{task_path}.function.input.{}", field.name),
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

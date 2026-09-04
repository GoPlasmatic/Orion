//! What the analysis knows about individual operators and functions: which
//! operators rebind `var` for their later arguments, and where in a task's
//! input the engine evaluates JSONLogic.
//!
//! Both tables are closed and pinned by tests — a new operator or a new
//! logic-bearing field fails a test until someone classifies it. That is
//! the discipline that lets a rule say "this read is certainly of the
//! context" rather than "probably".

use serde_json::Value;

use crate::engine::FunctionRegistry;

/// Operators whose arguments after the first are evaluated **per element**
/// (or per error, for `try`), with `var` rebound to that element. A read
/// inside one of those arguments is not a read of the message context, so
/// every rule that reasons about context paths treats it as unknown.
///
/// `switch`/`match` are here defensively: their pattern arguments are
/// evaluated against the matched value in some forms, and "unknown" is the
/// safe classification.
pub const SCOPING: &[&str] = &[
    "map", "filter", "reduce", "all", "some", "none", "group_by", "distinct", "sort", "try",
    "switch", "match",
];

/// Operators whose every argument is evaluated in the enclosing scope.
/// Together with [`SCOPING`] this must cover the whole vocabulary
/// (`analysis::tests::every_operator_is_classified`).
pub const NON_SCOPING: &[&str] = &[
    "var",
    "val",
    "==",
    "!=",
    "===",
    "!==",
    ">",
    ">=",
    "<",
    "<=",
    "and",
    "or",
    "!",
    "!!",
    "if",
    "?:",
    "+",
    "-",
    "*",
    "/",
    "%",
    "max",
    "min",
    "cat",
    "substr",
    "in",
    "merge",
    "missing",
    "missing_some",
    "now",
    "datetime",
    "parse_date",
    "format_date",
    "date_diff",
    "timestamp",
    "length",
    "upper",
    "lower",
    "trim",
    "split",
    "starts_with",
    "ends_with",
    "slice",
    "abs",
    "ceil",
    "floor",
    "keys",
    "values",
    "entries",
    "??",
    "type",
    "exists",
    "throw",
    "base64_encode",
    "base64_decode",
    "base64url_encode",
    "base64url_decode",
    "hex_encode",
    "hex_decode",
    "random",
    "url_encode",
    "url_decode",
    "join",
    "secret",
];

/// Operators whose result is not a function of the context alone. An
/// expression containing one cannot be evaluated offline on the engine's
/// behalf, so rules that evaluate stay silent on it. `secret` is here
/// because its value comes from the engine's store, not the message.
pub const NONDETERMINISTIC: &[&str] = &["now", "random", "secret"];

pub fn is_scoping(op: &str) -> bool {
    SCOPING.contains(&op)
}

/// The JSONLogic expressions the engine evaluates in a task's `input`, as
/// `(path relative to the input, expression)`. Covers the built-ins' logic
/// fields, every registry field whose `template_at` says dataflow-rs evaluates
/// it, and every field marked `resolvable` (a `{"var": …}` fold is a read too).
///
/// `template_at` is why an `http_call` path or header value counts. Before
/// dataflow-rs 3.9 those were static strings, so a `{"var": "payload.x"}` in
/// one was inert text and reporting it as a read would have been wrong; now it
/// is evaluated, and a rule that could not see it was under-reporting.
pub fn input_expressions<'a>(
    function: &str,
    input: &'a Value,
    functions: &FunctionRegistry,
) -> Vec<(String, &'a Value)> {
    let mut out = Vec::new();
    let Some(map) = input.as_object() else {
        return out;
    };
    let each = |key: &str, member: &str, out: &mut Vec<(String, &'a Value)>| {
        if let Some(items) = map.get(key).and_then(Value::as_array) {
            for (i, item) in items.iter().enumerate() {
                if let Some(expr) = item.get(member) {
                    out.push((format!("{key}[{i}].{member}"), expr));
                }
            }
        }
    };
    let one = |key: &str, out: &mut Vec<(String, &'a Value)>| {
        if let Some(expr) = map.get(key) {
            out.push((key.to_string(), expr));
        }
    };
    match function {
        "map" => each("mappings", "logic", &mut out),
        "filter" => one("condition", &mut out),
        "validation" | "validate" => each("rules", "logic", &mut out),
        "log" => {
            one("message", &mut out);
            if let Some(fields) = map.get("fields").and_then(Value::as_object) {
                for (name, expr) in fields {
                    out.push((format!("fields.{name}"), expr));
                }
            }
        }
        // `channel_call`'s `channel` and `data` are not listed here: they are
        // ordinary `template_at` fields now, found by the sweep below along
        // with every other one, and their pre-1.0 `*_logic` spellings resolve
        // through the same registry aliases.
        _ => {}
    }
    for (field, value) in map {
        if out.iter().any(|(p, _)| p == field) {
            continue;
        }
        let template_at = functions.template_paths(function, field);
        // `""` — the field's own value is the expression. `"*"` — the field is
        // a map whose *values* are, so the map itself is not one.
        if template_at.contains(&"") {
            out.push((field.clone(), value));
        } else if template_at.contains(&"*") {
            if let Some(members) = value.as_object() {
                for (name, expr) in members {
                    out.push((format!("{field}.{name}"), expr));
                }
            }
        } else if functions.is_resolvable_field(function, field) {
            out.push((field.clone(), value));
        }
    }
    out
}

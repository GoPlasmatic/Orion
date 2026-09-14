//! The `model_infer` input schema: what a task hands the handler in
//! [`crate::model::handler`], and the authoring-time rules over it.
//!
//! The handler lives in `model/` because an inference is the model
//! subsystem's — the runtime, the session cache, the permits are all there
//! — and this file is the registry's view of it, beside the other tables
//! so the create-time validator, the catalogue, the formatter and the
//! authoring analysis read one declaration. The static check below and
//! the handler's own parsing agree by construction: both read
//! [`crate::model::runtimes::NAMES`] for the runtime, and both refuse a
//! non-positive `timeout_ms` — the static one only where it was written as
//! a literal, because a deadline computed per message is not a number until
//! one arrives.

use serde_json::Value;

use super::schema::{FieldKind, FieldSchema};
use crate::errors::FieldError;

pub(super) const MODEL_INFER_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "model",
        description: "The model id; the active version resolves. JSONLogic here is what lets one \
                      workflow route to any model.",
        kind: FieldKind::String,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "input",
        description: "The JSON root every input adapter of the manifest sees. `{\"var\": \"\"}` \
                      hands the adapters the whole message context.",
        kind: FieldKind::Any,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "runtime",
        description: "Which runtime runs the graph: one of the compiled-in names (`tract`); \
                      default `[models.default_runtime]` for the model's format.",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted result path, default `temp_data.inference`.",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "raw",
        description: "Skip `result`; write `{name: tensor}` in wire form for chaining.",
        kind: FieldKind::Bool,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "timeout_ms",
        description: "Per-call deadline (JSONLogic), capped by `models.max_timeout_ms`; a cold \
                      load on first use is charged to it.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "stats_output",
        description: "Path for `{id, version, digest, runtime, device, parameters, \
                      artifact_bytes, queued_ms, inference_ms, cold_load}`; absent means not \
                      written.",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
];

/// The one spelling of the `runtime` refusal: the field error a create,
/// validate or lint of a workflow naming a runtime this build lacks carries.
/// The static validator above returns it in the registry's tuple form; the
/// path and the code are the same either way.
pub(crate) fn unknown_runtime(path: impl Into<String>, name: &str) -> FieldError {
    FieldError::new(
        path,
        "MODEL_RUNTIME_UNKNOWN",
        format!(
            "runtime '{name}' is not one this build knows: {}",
            crate::model::runtimes::NAMES.join(", ")
        ),
    )
}

/// The cross-field rules the per-field table cannot say: a `runtime` this
/// build does not know is `MODEL_RUNTIME_UNKNOWN`, naming what it does know;
/// a `timeout_ms` that is not a positive integer is `INVALID`. The ceiling
/// on `timeout_ms` is the node's config, which an authoring-time check
/// cannot see, so only the sign is judged here.
pub(super) fn validate_static_input(
    obj: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    let mut errors: Vec<(&'static str, &'static str, String)> = Vec::new();
    // A non-string `runtime` is the field loop's TYPE_MISMATCH.
    if let Some(name) = obj.get("runtime").and_then(Value::as_str)
        && crate::model::runtimes::intern(name).is_none()
    {
        let refusal = unknown_runtime("runtime", name);
        errors.push((
            "runtime",
            orion_api::error::field_codes::MODEL_RUNTIME_UNKNOWN,
            refusal.message,
        ));
    }
    // A scalar is unambiguously itself in JSONLogic, so it is judged here;
    // an object or array may be an operator call, and what it computes per
    // message is the handler's to check.
    if let Some(timeout) = obj
        .get("timeout_ms")
        .filter(|v| !v.is_null() && !v.is_object() && !v.is_array())
        && timeout.as_u64().is_none_or(|ms| ms == 0)
    {
        errors.push((
            "timeout_ms",
            "INVALID",
            "timeout_ms must be a positive integer (milliseconds)".to_string(),
        ));
    }
    if let Some(Value::String(id)) = obj.get("model")
        && id.trim().is_empty()
    {
        errors.push(("model", "INVALID", "model must name a model".to_string()));
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::FunctionRegistry;
    use serde_json::json;

    fn errors_for(input: Value) -> Vec<(String, String)> {
        FunctionRegistry::builtin()
            .validate_input("model_infer", &input, "tasks[0]")
            .into_iter()
            .map(|e| (e.path, e.code))
            .collect()
    }

    /// The registry carries the table in the order the handler documents,
    /// and a well-formed input passes.
    #[test]
    fn the_table_is_registered_in_order_and_a_good_input_passes() {
        let entry = FunctionRegistry::builtin()
            .get("model_infer")
            .expect("registered");
        let names: Vec<&str> = entry
            .input_fields
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "model",
                "input",
                "runtime",
                "output",
                "raw",
                "timeout_ms",
                "stats_output"
            ]
        );
        assert_eq!(entry.category, "compute");
        assert!(
            errors_for(json!({
                "model": "ada.c4-tiny",
                "input": {"var": ""},
                "runtime": "tract",
                "output": "data.policy",
                "raw": false,
                "timeout_ms": 250,
                "stats_output": "temp_data.stats"
            }))
            .is_empty()
        );
        // A computed model is JSONLogic, not a type error.
        assert!(
            errors_for(json!({"model": {"var": "data.model"}, "input": {"var": "data"}}))
                .is_empty()
        );
    }

    /// Each rule, by code and path.
    #[test]
    fn an_unknown_runtime_and_a_bad_timeout_are_refused_by_code() {
        let errors = errors_for(json!({
            "model": "ada.c4-tiny",
            "input": {},
            "runtime": "nope",
            "timeout_ms": 0
        }));
        assert!(
            errors.contains(&(
                "tasks[0].function.input.runtime".to_string(),
                "MODEL_RUNTIME_UNKNOWN".to_string()
            )),
            "{errors:?}"
        );
        assert!(
            errors.contains(&(
                "tasks[0].function.input.timeout_ms".to_string(),
                "INVALID".to_string()
            )),
            "{errors:?}"
        );
        let errors = errors_for(json!({"model": " ", "input": {}, "timeout_ms": -5}));
        assert!(
            errors.contains(&(
                "tasks[0].function.input.model".to_string(),
                "INVALID".to_string()
            )),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|(p, _)| p.ends_with(".timeout_ms")),
            "{errors:?}"
        );
        // Strict: a key outside the table is refused.
        let errors = errors_for(json!({"model": "m", "input": {}, "stats": "x"}));
        assert!(
            errors
                .iter()
                .any(|(p, c)| p.ends_with(".stats") && c == "UNKNOWN_FIELD"),
            "{errors:?}"
        );
        // Missing requireds.
        let errors = errors_for(json!({}));
        assert!(
            errors.contains(&(
                "tasks[0].function.input.model".to_string(),
                "REQUIRED".to_string()
            )),
            "{errors:?}"
        );
        assert!(
            errors.contains(&(
                "tasks[0].function.input.input".to_string(),
                "REQUIRED".to_string()
            )),
            "{errors:?}"
        );
    }

    /// A deadline computed per message is an expression, not a type error
    /// (#327). `channel_call` and `http_call` — the two sibling functions
    /// that also take a per-call deadline — have always accepted one, and a
    /// workflow dividing a shared wall-clock budget between several
    /// inferences has to compute each share. What it evaluates to is still
    /// judged, by the handler, at the only moment it is a number.
    #[test]
    fn a_computed_timeout_is_an_expression_and_a_literal_one_is_still_judged() {
        let timeout = FunctionRegistry::builtin()
            .get("model_infer")
            .and_then(|e| {
                e.input_fields
                    .as_deref()?
                    .iter()
                    .find(|f| f.name == "timeout_ms")
                    .map(|f| f.template_at)
            })
            .expect("timeout_ms is registered");
        assert!(timeout.contains(&""), "the field itself is the expression");

        for computed in [json!({"var": "temp_data.ms"}), json!({"+": [40, 10]})] {
            let errors = errors_for(json!({
                "model": "ada.c4-tiny",
                "input": {"var": ""},
                "timeout_ms": computed
            }));
            assert!(errors.is_empty(), "{errors:?}");
        }
        // A literal keeps both checks it always had: the kind, and the sign.
        for literal in [json!(0), json!(-5), json!("250")] {
            let errors = errors_for(json!({
                "model": "ada.c4-tiny",
                "input": {},
                "timeout_ms": literal
            }));
            assert!(
                errors.iter().any(|(p, _)| p.ends_with(".timeout_ms")),
                "{errors:?}"
            );
        }
    }

    /// The runtime check reads the same table the handler selects from, and
    /// the refusal is one field error however it is reached.
    #[test]
    fn every_known_runtime_passes_the_static_check() {
        for name in crate::model::runtimes::NAMES {
            let obj = json!({"model": "m", "input": {}, "runtime": name});
            assert!(
                validate_static_input(obj.as_object().expect("object")).is_empty(),
                "{name}"
            );
        }
        let refusal = unknown_runtime("tasks[0].function.input.runtime", "ort");
        assert_eq!(
            refusal.code,
            orion_api::error::field_codes::MODEL_RUNTIME_UNKNOWN
        );
        assert_eq!(refusal.path, "tasks[0].function.input.runtime");
        assert!(refusal.message.contains("'ort'") && refusal.message.contains("tract"));
    }
}

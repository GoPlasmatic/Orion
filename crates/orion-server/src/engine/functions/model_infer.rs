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
//!
//! [`ModelInferConfig`] is the same input on dataflow-rs 4, its `check` the
//! static rules above, so a task 1.12 refused at authoring is refused at
//! load.

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Output};
use dataflow_rs::function::FunctionConfig;
use dataflow_rs::program::IssueCode;
use dataflow_rs::{Expr, Issue};
use serde_json::{Map, Value};

use super::schema::{FieldKind, FieldSchema};
use crate::engine::kit::{self, DetachedExpr};
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
        name: "select",
        description: "JSONLogic in place of the manifest's `result`, evaluated natively over \
                      the outputs by name and the call's `input` root as `input`, so a tensor \
                      is reduced (`reshape`, `gather`, `argmax`, …) before anything is written. \
                      Exclusive with `raw`.",
        kind: FieldKind::Any,
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
                      artifact_bytes, ops, peak_ops, queued_ms, inference_ms, cold_load}`; \
                      absent means not written.",
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
    if obj.get("raw").and_then(Value::as_bool) == Some(true)
        && obj.get("select").is_some_and(|v| !v.is_null())
    {
        errors.push((
            "select",
            "INVALID",
            "'raw' and 'select' are exclusive: 'raw' writes the outputs whole, 'select' reduces \
             them first"
                .to_string(),
        ));
    }
    if let Some(Value::String(id)) = obj.get("model")
        && id.trim().is_empty()
    {
        errors.push(("model", "INVALID", "model must name a model".to_string()));
    }
    errors
}

// -- dataflow-rs 4 --

/// I185's Advisory for a literal root: what `model_infer` writes is the
/// task's `select`, or the manifest's `result`, which may be an object and
/// may not, so a context root, which takes only an object (break 21), fails
/// each call whose result is not one, where 1.12 replaced the root with it.
/// `raw` writes `{name: tensor}`, an object, so it draws none.
const ROOT_OUTPUT: &str = "`model_infer` writes what `select` or the model's `result` gives \
    at `output`, and a context root takes only an object (break 21): a call whose result is \
    not an object fails; set `output` to a path below it, such as `temp_data.inference`";

/// `model_infer`'s input on dataflow-rs 4, in its field table's order
/// (I179), each field read as 1.12 read it (I183). `model`, `input` and
/// `timeout_ms` are expressions over the message, evaluated per call and
/// judged by the call as 1.12 judged them; `model` and `input` are
/// required, at load (I183). `runtime` and `raw` are literals, as 1.12 read
/// them. `select` is detached: compiled on the engine's expressions and
/// evaluated by the call over the model's outputs, never the message.
/// `output`, the primary output, and `stats_output` are literal dotted
/// paths, as 1.12 read them; a computed one, which 1.12 refused at
/// authoring and on every call, is refused at load. Its `check` is 1.12's
/// authoring-time rules.
#[derive(Debug, FunctionConfig)]
#[config(check = check)]
pub struct ModelInferConfig {
    /// The model id; the active version resolves.
    pub(crate) model: Expr<OwnedDataValue>,
    /// The JSON root every input adapter of the manifest sees.
    pub(crate) input: Expr<OwnedDataValue>,
    /// Which runtime runs the graph; the format's default when unset.
    pub(crate) runtime: Option<String>,
    /// Where the result is written.
    #[output(default = "temp_data.inference", primary)]
    pub(crate) output: Output,
    /// Whether the outputs are written whole, as `{name: tensor}`.
    pub(crate) raw: Option<bool>,
    /// An expression over the outputs, in place of the manifest's `result`.
    pub(crate) select: Option<DetachedExpr>,
    /// The call's deadline, in milliseconds, capped by the model's.
    pub(crate) timeout_ms: Option<Expr<OwnedDataValue>>,
    /// Where the call's stats are written; unset writes none.
    pub(crate) stats_output: Option<Output>,
}

impl ModelInferConfig {
    /// The task's input as 1.12's authoring check judged it: the fields
    /// [`validate_static_input`] reads, as written.
    fn authored(&self) -> Map<String, Value> {
        let mut input = Map::new();
        input.insert("model".to_string(), self.model.raw().clone());
        input.insert("input".to_string(), self.input.raw().clone());
        if let Some(runtime) = &self.runtime {
            input.insert("runtime".to_string(), Value::String(runtime.clone()));
        }
        if let Some(raw) = self.raw {
            input.insert("raw".to_string(), Value::Bool(raw));
        }
        if let Some(select) = &self.select {
            input.insert("select".to_string(), select.raw().clone());
        }
        if let Some(timeout) = &self.timeout_ms {
            input.insert("timeout_ms".to_string(), timeout.raw().clone());
        }
        input
    }

    /// I185, from `prepare`: an Advisory at `output` when it is a literal
    /// context root and the call may write a non-object there.
    pub(crate) fn root_output(&self) -> Result<(), Vec<Issue>> {
        if self.raw == Some(true) {
            return Ok(());
        }
        kit::root_output_advisory(&self.output, ROOT_OUTPUT)
    }
}

/// 1.12's authoring-time rules for `model_infer` ([`validate_static_input`],
/// by which create, update, import, validate and `lint` refused a task), on
/// the config at load: a runtime this build does not know, a literal
/// `timeout_ms` that is not a positive integer, `raw` beside `select`, and a
/// blank literal `model`, each refused at the field 1.12 named, in its words.
/// And an `output` or `stats_output` that is not written as a string, which
/// 1.12's field loop refused (`TYPE_MISMATCH`) and its handler refused on
/// every call, in the handler's words: the authored JSON decides, so an
/// expression that folds to a path is refused too, as 1.12 refused it.
fn check(cfg: &ModelInferConfig, cx: &mut CompileCx<'_>) {
    super::mongo_common::report_static(validate_static_input(&cfg.authored()), cx);
    for (field, out) in [
        ("output", Some(&cfg.output)),
        ("stats_output", cfg.stats_output.as_ref()),
    ] {
        if let Some(out) = out
            && !out.path().raw().is_string()
        {
            cx.at(field, |cx| {
                cx.issue(
                    IssueCode::InputParse,
                    format!("'{field}' must be a dotted path (string)"),
                );
            });
        }
    }
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
                "select",
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
        // `select` alone passes; beside `raw: true` it cannot mean anything.
        assert!(
            errors_for(json!({
                "model": "ada.c4-tiny",
                "input": {"var": ""},
                "select": {"argmax": [{"var": "policy"}, 1]}
            }))
            .is_empty()
        );
        assert_eq!(
            errors_for(json!({
                "model": "ada.c4-tiny",
                "input": {"var": ""},
                "raw": true,
                "select": {"var": "policy"}
            })),
            [(
                "tasks[0].function.input.select".to_string(),
                "INVALID".to_string()
            )]
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

    // -- dataflow-rs 4: the config, twinned against the table above --

    use dataflow_rs::expr::{FieldKind as V4Kind, ValueKind};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::Severity;

    fn compile(input: Value) -> Result<ModelInferConfig, Vec<Issue>> {
        testing::config_with::<ModelInferConfig>(&crate::engine::kit::test_exprs(), input)
    }

    /// The Rejected issues of a config that does not load, as
    /// `(path, code)`.
    fn rejected(input: Value) -> Vec<(String, IssueCode)> {
        compile(input)
            .expect_err("refused")
            .into_iter()
            .filter(|i| i.severity == Severity::Rejected)
            .map(|i| (i.path.to_string(), i.code))
            .collect()
    }

    /// Declaration order is the field table's (I179); `model` and `input`
    /// are required expressions (I183); `select` is detached, a literal to
    /// the engine, its `var` reads the outputs' and never walked as the
    /// message's; `output` is the primary of the two outputs, defaulting to
    /// `temp_data.inference`, and `stats_output` an optional output beside it.
    #[test]
    fn the_config_is_the_field_table_with_output_primary() {
        let fields = ModelInferConfig::fields();
        let names: Vec<&str> = fields.iter().map(|f| &*f.name).collect();
        let table: Vec<&str> = MODEL_INFER_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(names, table);
        let field = |name: &str| fields.iter().find(|f| f.name == name).expect(name);
        assert!(field("model").required && field("input").required);
        assert!(matches!(field("model").kind, V4Kind::Expr(ValueKind::Any)));
        assert!(matches!(
            field("select").kind,
            V4Kind::Literal(ValueKind::Any)
        ));
        assert!(!field("select").required);
        assert!(matches!(field("output").kind, V4Kind::Output));
        assert!(field("output").primary);
        assert_eq!(
            field("output").default.as_deref(),
            Some("\"temp_data.inference\"")
        );
        assert!(matches!(field("stats_output").kind, V4Kind::Output));
        assert!(!field("stats_output").primary);

        let cfg = compile(json!({
            "model": {"var": "data.model"},
            "input": {"var": ""},
            "select": {"argmax": [{"var": "policy"}, 1]},
            "stats_output": "temp_data.stats"
        }))
        .expect("loads");
        let primary = dataflow_rs::function::FunctionConfig::primary_output(&cfg)
            .expect("`output` is the primary");
        assert_eq!(
            primary.constant().map(ToString::to_string).as_deref(),
            Some("temp_data.inference")
        );
        assert_eq!(
            cfg.stats_output
                .as_ref()
                .and_then(|o| o.constant())
                .map(ToString::to_string)
                .as_deref(),
            Some("temp_data.stats")
        );
        let select = cfg.select.as_ref().expect("set");
        assert_eq!(select.raw(), &json!({"argmax": [{"var": "policy"}, 1]}));

        // `select` reads its own root, so nothing in it is the message's.
        struct Seen(Vec<Value>);
        impl dataflow_rs::expr::ExprVisitor for Seen {
            fn expr(
                &mut self,
                _at: &dataflow_rs::program::AuthoredPath,
                raw: &Value,
                _records: bool,
            ) {
                self.0.push(raw.clone());
            }
        }
        let mut seen = Seen(Vec::new());
        dataflow_rs::function::FunctionConfig::visit(&cfg, &mut seen);
        assert!(
            !seen.0.contains(&json!({"argmax": [{"var": "policy"}, 1]})),
            "{:?}",
            seen.0
        );
    }

    /// `a_good_input_passes` and each authoring rule of
    /// `an_unknown_runtime_and_a_bad_timeout_are_refused_by_code`, at load:
    /// 1.12 refused each at authoring and on every call, so dataflow-rs 4
    /// refuses the task before it runs.
    #[test]
    fn the_authoring_rules_are_refused_at_load() {
        compile(json!({
            "model": "ada.c4-tiny",
            "input": {"var": ""},
            "runtime": "tract",
            "output": "data.policy",
            "raw": false,
            "timeout_ms": 250,
            "stats_output": "temp_data.stats"
        }))
        .expect("a good input loads");
        compile(json!({
            "model": "ada.c4-tiny",
            "input": {"var": ""},
            "select": {"argmax": [{"var": "policy"}, 1]}
        }))
        .expect("`select` alone loads");
        compile(json!({"model": {"var": "data.model"}, "input": {"var": "data"}}))
            .expect("a computed model is an expression");

        assert_eq!(
            rejected(json!({
                "model": "ada.c4-tiny", "input": {"var": ""}, "raw": true,
                "select": {"var": "policy"}
            })),
            [("select".to_string(), IssueCode::InputParse)]
        );
        let refused = rejected(json!({
            "model": "ada.c4-tiny", "input": {}, "runtime": "nope", "timeout_ms": 0
        }));
        assert!(
            refused.contains(&("runtime".to_string(), IssueCode::InputParse)),
            "{refused:?}"
        );
        assert!(
            refused.contains(&("timeout_ms".to_string(), IssueCode::InputParse)),
            "{refused:?}"
        );
        let refused = rejected(json!({"model": " ", "input": {}, "timeout_ms": -5}));
        assert!(
            refused.contains(&("model".to_string(), IssueCode::InputParse)),
            "{refused:?}"
        );
        assert!(
            refused.iter().any(|(p, _)| p == "timeout_ms"),
            "{refused:?}"
        );
        // The texts are the authoring check's.
        let issues =
            compile(json!({"model": "m", "input": {}, "runtime": "ort"})).expect_err("refused");
        assert_eq!(
            issues[0].message,
            unknown_runtime("runtime", "ort").message,
            "{issues:?}"
        );
        // An unknown key is refused (break 2a), and so is a missing required
        // field (I183).
        let refused = rejected(json!({"model": "m", "input": {}, "stats": "x"}));
        assert!(refused.iter().any(|(p, _)| p == "stats"), "{refused:?}");
        let refused = rejected(json!({}));
        assert!(refused.iter().any(|(p, _)| p == "model"), "{refused:?}");
        assert!(refused.iter().any(|(p, _)| p == "input"), "{refused:?}");
    }

    /// `a_computed_timeout_is_an_expression_and_a_literal_one_is_still_judged`
    /// on v4: a computed deadline loads and is judged per call; a literal is
    /// judged at load, for its kind and its sign.
    #[test]
    fn a_computed_timeout_loads_and_a_literal_one_is_judged_at_load() {
        for computed in [json!({"var": "temp_data.ms"}), json!({"+": [40, 10]})] {
            compile(json!({"model": "m", "input": {"var": ""}, "timeout_ms": computed}))
                .expect("a computed deadline loads");
        }
        for literal in [json!(0), json!(-5), json!("250")] {
            let refused = rejected(json!({"model": "m", "input": {}, "timeout_ms": literal}));
            assert_eq!(
                refused,
                [("timeout_ms".to_string(), IssueCode::InputParse)],
                "{literal}"
            );
        }
    }

    /// `every_known_runtime_passes_the_static_check` on v4.
    #[test]
    fn every_known_runtime_loads() {
        for name in crate::model::runtimes::NAMES {
            compile(json!({"model": "m", "input": {}, "runtime": name})).expect(name);
        }
        assert_eq!(
            rejected(json!({"model": "m", "input": {}, "runtime": 7})),
            [("runtime".to_string(), IssueCode::InputParse)]
        );
    }

    /// `output` and `stats_output` are literal dotted paths, as 1.12 read
    /// them: anything else, an expression that folds to a path included,
    /// 1.12 refused at authoring (`TYPE_MISMATCH`) and on every call, and
    /// dataflow-rs 4 refuses at load. `null` is the default (none for
    /// `stats_output`), as 1.12 read it.
    #[test]
    fn an_output_is_a_literal_path() {
        for field in ["output", "stats_output"] {
            for computed in [
                json!({"var": "data.where"}),
                json!({"cat": ["temp_data", ".x"]}),
            ] {
                let refused = compile(json!({"model": "m", "input": {}, field: computed}))
                    .expect_err("refused");
                let messages: Vec<(String, &str)> = refused
                    .iter()
                    .map(|i| (i.path.to_string(), i.message.as_str()))
                    .collect();
                assert_eq!(
                    messages,
                    [(
                        field.to_string(),
                        format!("'{field}' must be a dotted path (string)").as_str()
                    )],
                    "{field}: {computed}"
                );
            }
            let refused = rejected(json!({"model": "m", "input": {}, field: 5}));
            assert_eq!(refused.len(), 1, "{field}: {refused:?}");
            assert_eq!(refused[0].0, field);
        }
        let cfg = compile(json!({"model": "m", "input": {}, "output": null, "stats_output": null}))
            .expect("null is the default");
        assert_eq!(
            cfg.output.constant().map(ToString::to_string).as_deref(),
            Some("temp_data.inference")
        );
        assert!(cfg.stats_output.is_none());
    }

    /// A `select` that reads a secret is refused at load: its result is
    /// written to the message. 1.12 let it through authoring (the field is
    /// read literally there) and its handler wrote the secret's value.
    #[test]
    fn a_select_reading_a_secret_is_refused_at_load() {
        let issues = compile(json!({
            "model": "m", "input": {}, "select": {"cat": [{"secret": "api-key"}, "x"]}
        }))
        .expect_err("refused");
        let codes: Vec<(String, IssueCode)> = issues
            .iter()
            .map(|i| (i.path.to_string(), i.code))
            .collect();
        assert_eq!(
            codes,
            [("select".to_string(), IssueCode::SecretInMessageWrite)]
        );
    }
}

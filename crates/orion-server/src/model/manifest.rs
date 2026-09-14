//! The model manifest: what a model takes, what it gives back, and how a
//! message is marshalled into and out of it.
//!
//! A manifest is JSON (a plugin's is TOML; a model's travels inside the
//! model row and the package artifact, where JSON is what everything else
//! already is). It declares the ABI it was written against, the model's
//! name, its inputs and outputs — each a name, a dtype and a fixed shape —
//! and two kinds of JSONLogic: one **adapter** per input, evaluated against
//! the message to produce that input's tensor, and one **result**
//! expression, evaluated against the outputs to produce what the task
//! writes. Both default to the obvious thing (`{"tensor": [{"var": name},
//! dtype]}` and a list per output), so a manifest for a model whose caller
//! already speaks tensors is inputs and outputs and nothing more.
//!
//! Validation reports every problem with a path in one pass, as the plugin
//! manifest's does. Two rules are Orion's rather than the format's. Every
//! expression must **compile** on the same operator vocabulary, in the same
//! templating mode and with the same key escape as the serving engine, so
//! what the engine refuses outright is refused at upload rather than at the
//! first inference. That is a narrow gate by design of the engine: in
//! templating mode a multi-key object is an output template and an unknown
//! key is data, and an operator given arguments of the wrong shape compiles
//! to a marker that fails at evaluation — so a typo'd operator name or a
//! wrong arity is not a compile error here and cannot be. And every
//! expression is **screened**: an adapter may not read the secret store
//! (`{"secret": …}` — a manifest is authored by whoever owns the model,
//! which is not necessarily whoever owns the secrets), and may not call
//! `now` or `random`, because a replay of a traced inference must reproduce
//! the same tensors. The screen is structural — any single-key object under
//! one of those keys — for the reason above: `secret` is registered on the
//! serving engine alone, and to a bare engine an unknown key is data.
//!
//! Compilation is checked here and not kept: the compiled adapters belong to
//! the generation that runs them, which is built elsewhere with the engine
//! that will evaluate them.

use std::sync::OnceLock;

use dataflow_rs::datalogic_rs as datalogic;
use dataflow_rs::datavalue::{DType, OwnedDataTensor};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::errors::FieldError;

/// The one manifest version this binary speaks.
pub const ABI: &str = "orion:model@1.0.0";

/// The prefix every model manifest's `abi` starts with, at any version —
/// what tells a model manifest from a plugin's or a workflow's before the
/// version is checked.
const ABI_FAMILY: &str = "orion:model@";

/// The format a manifest declares when it says nothing — the one every
/// build serves. What a manifest *may* declare is
/// [`runtimes::KNOWN_FORMATS`](super::runtimes::KNOWN_FORMATS): the union of
/// what the runtimes this build knows load.
const FORMAT_ONNX: &str = "onnx";

/// The operator keys no adapter may use, each with the reason.
const FORBIDDEN_OPERATORS: [(&str, &str); 3] = [
    (
        "secret",
        "an adapter may not read the secret store: a manifest is authored by the model's \
         owner, and the secrets are the deployment's",
    ),
    (
        "now",
        "an adapter may not read the clock: a replay of a traced inference must reproduce the \
         same tensors",
    ),
    (
        "random",
        "an adapter may not draw randomness: a replay of a traced inference must reproduce the \
         same tensors",
    ),
];

/// Whether `value` is a model manifest at all — an object whose `abi` is in
/// the `orion:model@` family. The version is checked by [`Manifest::validated`];
/// this only answers which validator to hand a document to.
pub fn is_model_manifest(value: &Value) -> bool {
    value
        .get("abi")
        .and_then(Value::as_str)
        .is_some_and(|abi| abi.starts_with(ABI_FAMILY))
}

/// A parsed, validated manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Must equal [`ABI`].
    pub abi: String,
    /// The model id: lowercase labels joined by `.`. `orion.*` is reserved.
    pub name: String,
    /// Informational. Orion assigns the entity version.
    pub version: String,
    /// The artifact's format: one of the formats a runtime this build
    /// knows serves (`onnx` today, the default when absent). Which runtime
    /// loads it is the node's `[models.default_runtime]` row for the format.
    #[serde(default = "default_format")]
    pub format: String,
    /// Path of the artifact relative to the manifest, read by offline
    /// tooling and the CLI only: where the bytes are **on disk**, for
    /// `lint` to read the graph's stats, `dry-run` and `test` to run the
    /// model for real, and `compile` to hash. A served row names its
    /// artifact by connector, key and digest and ignores this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// Where a pipeline put the bytes for a serving instance: the
    /// `storage` connector and object key `compile` writes into the
    /// package's `models[]` entry, beside the digest of the file `artifact`
    /// names. The deployable counterpart of `artifact` — one is a path on
    /// the authoring machine, the other an object the target fetches at
    /// admission. Neither is required: a manifest registered by hand carries
    /// its reference on the request, and `compile` refuses a manifest
    /// missing either, naming what to add.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<ArtifactReference>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub inputs: Vec<InputDecl>,
    #[serde(default)]
    pub outputs: Vec<OutputDecl>,
    /// JSONLogic over the outputs (each by name) producing what the task
    /// writes. Absent means [`Manifest::default_result`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

fn default_format() -> String {
    FORMAT_ONNX.to_string()
}

/// Where a serving instance finds the bytes: a `storage` connector by name
/// and the object key within its bucket. The digest is not here — it is
/// computed from the file `artifact` names, never declared, so a manifest
/// cannot claim one thing and ship another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReference {
    pub connector: String,
    pub key: String,
}

/// One input tensor and how the message becomes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputDecl {
    /// The graph's input name — also the context key the default adapter
    /// reads.
    pub name: String,
    /// A datavalue dtype wire name (`f32`, `i64`, `bool`, …), lowercase.
    pub dtype: String,
    /// The fixed shape, every dimension positive.
    pub shape: Vec<usize>,
    /// JSONLogic over the message producing this input's tensor. Absent
    /// means [`Manifest::default_adapter`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<Value>,
}

impl InputDecl {
    /// The declared dtype. `None` only for a manifest that did not validate.
    pub fn dtype(&self) -> Option<DType> {
        parse_dtype(&self.dtype).ok()
    }

    /// Elements in one tensor of this shape, or `None` when the product
    /// overflows.
    pub fn element_count(&self) -> Option<usize> {
        element_count(&self.shape)
    }
}

/// One output tensor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputDecl {
    /// The graph's output name — also the key the result expression reads
    /// it under.
    pub name: String,
    /// A datavalue dtype wire name, lowercase.
    pub dtype: String,
    /// The fixed shape, every dimension positive.
    pub shape: Vec<usize>,
}

impl OutputDecl {
    /// The declared dtype. `None` only for a manifest that did not validate.
    pub fn dtype(&self) -> Option<DType> {
        parse_dtype(&self.dtype).ok()
    }

    /// Elements in one tensor of this shape, or `None` when the product
    /// overflows.
    pub fn element_count(&self) -> Option<usize> {
        element_count(&self.shape)
    }
}

impl Manifest {
    /// A manifest from its JSON value (an upload body, a stored row, an
    /// import item), checked against every rule the type cannot express.
    /// Every problem is reported, with a path into the document, so an
    /// author fixes a manifest in one round.
    pub fn validated(value: &Value) -> Result<Self, Vec<FieldError>> {
        let manifest: Manifest = serde_path_to_error::deserialize(value).map_err(|e| {
            let path = e.path().to_string();
            vec![FieldError::new(
                if path == "." {
                    "manifest"
                } else {
                    path.as_str()
                },
                "INVALID",
                e.inner().to_string(),
            )]
        })?;
        let problems = manifest.problems();
        if problems.is_empty() {
            Ok(manifest)
        } else {
            Err(problems)
        }
    }

    /// [`Self::validated`] over the JSON text of a manifest file.
    pub fn parse(text: &str) -> Result<Self, Vec<FieldError>> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| vec![FieldError::new("manifest", "INVALID", e.to_string())])?;
        Self::validated(&value)
    }

    /// Every rule the type cannot express. Empty for a valid manifest.
    pub fn problems(&self) -> Vec<FieldError> {
        let mut out = Vec::new();
        if self.abi != ABI {
            out.push(FieldError::new(
                "abi",
                "INVALID",
                format!("unsupported abi '{}': this server speaks '{ABI}'", self.abi),
            ));
        }
        if let Err(reason) = check_model_name(&self.name) {
            out.push(FieldError::new("name", "INVALID", reason));
        }
        if self.version.trim().is_empty() {
            out.push(FieldError::new(
                "version",
                "REQUIRED",
                "version must not be empty",
            ));
        }
        if !super::runtimes::KNOWN_FORMATS.contains(&self.format.as_str()) {
            out.push(FieldError::new(
                "format",
                "INVALID",
                format!(
                    "unsupported format '{}': this server loads {}",
                    self.format,
                    super::runtimes::KNOWN_FORMATS
                        .iter()
                        .map(|f| format!("'{f}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        if let Some(artifact) = &self.artifact
            && let Err(reason) = check_artifact_path(artifact)
        {
            out.push(FieldError::new("artifact", "INVALID", reason));
        }
        if let Some(reference) = &self.reference {
            for (field, value) in [("connector", &reference.connector), ("key", &reference.key)] {
                if value.trim().is_empty() {
                    out.push(FieldError::new(
                        format!("reference.{field}"),
                        "REQUIRED",
                        format!("reference.{field} must not be empty"),
                    ));
                }
            }
        }
        if self.inputs.is_empty() {
            out.push(FieldError::new(
                "inputs",
                "REQUIRED",
                "a model must declare at least one input",
            ));
        }

        let mut seen: Vec<&str> = Vec::new();
        for (i, input) in self.inputs.iter().enumerate() {
            let path = format!("inputs[{i}]");
            check_tensor_decl(
                &path,
                &input.name,
                &input.dtype,
                &input.shape,
                &mut seen,
                &mut out,
            );
            if let Some(adapter) = &input.adapter {
                check_expression(&format!("{path}.adapter"), adapter, &mut out);
            }
        }
        let mut seen: Vec<&str> = Vec::new();
        for (i, output) in self.outputs.iter().enumerate() {
            let path = format!("outputs[{i}]");
            check_tensor_decl(
                &path,
                &output.name,
                &output.dtype,
                &output.shape,
                &mut seen,
                &mut out,
            );
        }
        if let Some(result) = &self.result {
            check_expression("result", result, &mut out);
        }
        out
    }

    /// Every input name, in graph order.
    pub fn input_names(&self) -> impl Iterator<Item = &str> {
        self.inputs.iter().map(|i| i.name.as_str())
    }

    /// Every output name, in graph order.
    pub fn output_names(&self) -> impl Iterator<Item = &str> {
        self.outputs.iter().map(|o| o.name.as_str())
    }

    /// One zero-filled tensor per input, of the declared dtype and shape —
    /// what the admission probe feeds a model. `Err` only for a manifest
    /// that did not validate (an unknown dtype, a shape that overflows).
    pub fn zero_inputs(&self) -> Result<Vec<OwnedDataTensor>, String> {
        self.inputs
            .iter()
            .map(|input| {
                let dtype = input.dtype().ok_or_else(|| {
                    format!(
                        "input '{}': dtype '{}' is not one this server decodes",
                        input.name, input.dtype
                    )
                })?;
                let len = input
                    .element_count()
                    .and_then(|count| dtype.byte_len(count))
                    .ok_or_else(|| format!("input '{}': the shape overflows", input.name))?;
                OwnedDataTensor::from_bytes(dtype, input.shape.clone(), &vec![0u8; len])
                    .map_err(|e| format!("input '{}': {e}", input.name))
            })
            .collect()
    }

    /// The adapter that produces `input`'s tensor: the declared one, or
    /// [`Self::default_adapter`].
    pub fn adapter_for(&self, input: &InputDecl) -> Value {
        input
            .adapter
            .clone()
            .unwrap_or_else(|| Self::default_adapter(&input.name, &input.dtype))
    }

    /// The expression that produces the task's result: the declared one, or
    /// [`Self::default_result`].
    pub fn result_logic(&self) -> Value {
        self.result
            .clone()
            .unwrap_or_else(|| Self::default_result(&self.outputs))
    }

    /// `{"tensor": [{"var": name}, dtype]}` — the message carries the input
    /// as a nested list under its own name.
    pub fn default_adapter(name: &str, dtype: &str) -> Value {
        json!({ "tensor": [{ "var": name }, dtype] })
    }

    /// An object with one member per output, each the output as a nested
    /// list. An output whose name is a live operator (`shape`, `cast`, …) is
    /// written with the engine's `$` key escape, so the template emits the
    /// name as a literal key rather than calling the operator.
    pub fn default_result(outputs: &[OutputDecl]) -> Value {
        let mut object = serde_json::Map::new();
        for output in outputs {
            let key = if crate::engine::operators::is_operator(&output.name) {
                format!("{TEMPLATE_KEY_ESCAPE}{}", output.name)
            } else {
                output.name.clone()
            };
            object.insert(key, json!({ "to_list": [{ "var": output.name }] }));
        }
        Value::Object(object)
    }
}

/// The key escape the serving engine sets on every datalogic engine it
/// builds, mirrored on the compile-check engine here so the two agree about
/// what `{"$shape": …}` means. Asserted against dataflow-rs by the tests.
const TEMPLATE_KEY_ESCAPE: char = '$';

/// The engine every adapter and result must compile on: Orion's operator
/// vocabulary, in the templating mode every serving engine runs, with the
/// serving engine's key escape. Built once; it holds no state.
fn compile_engine() -> &'static datalogic::Engine {
    static ENGINE: OnceLock<datalogic::Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        crate::engine::operators::add_to_datalogic(
            datalogic::Engine::builder()
                .with_templating(true)
                .with_template_key_escape(TEMPLATE_KEY_ESCAPE),
        )
        .build()
    })
}

/// Compile `logic` and screen it, reporting under `path`.
fn check_expression(path: &str, logic: &Value, out: &mut Vec<FieldError>) {
    if let Err(e) = compile_engine().compile(logic) {
        out.push(FieldError::new(
            path,
            "INVALID",
            format!("does not compile: {e}"),
        ));
    }
    screen(path, logic, out);
}

/// Refuse every single-key object under a forbidden operator, wherever it
/// sits in `logic`. Structural on purpose — see the module docs.
fn screen(path: &str, logic: &Value, out: &mut Vec<FieldError>) {
    match logic {
        Value::Object(map) => {
            if map.len() == 1 {
                let key = map.keys().next().expect("one member");
                if let Some((_, reason)) = FORBIDDEN_OPERATORS.iter().find(|(op, _)| op == key) {
                    out.push(FieldError::new(
                        path,
                        "INVALID",
                        format!("uses {{\"{key}\": …}}: {reason}"),
                    ));
                }
            }
            for (key, value) in map {
                screen(&format!("{path}.{key}"), value, out);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                screen(&format!("{path}[{i}]"), item, out);
            }
        }
        _ => {}
    }
}

/// The rules an input and an output declaration share: a non-empty name
/// unique within its list, a supported dtype, a positive shape.
fn check_tensor_decl<'a>(
    path: &str,
    name: &'a str,
    dtype: &str,
    shape: &[usize],
    seen: &mut Vec<&'a str>,
    out: &mut Vec<FieldError>,
) {
    if name.is_empty() {
        out.push(FieldError::new(
            format!("{path}.name"),
            "REQUIRED",
            "name must not be empty",
        ));
    } else if seen.contains(&name) {
        out.push(FieldError::new(
            format!("{path}.name"),
            "DUPLICATE_FIELD",
            format!("'{name}' is declared twice"),
        ));
    }
    seen.push(name);
    if let Err(reason) = parse_dtype(dtype) {
        out.push(FieldError::new(format!("{path}.dtype"), "INVALID", reason));
    }
    if shape.is_empty() {
        out.push(FieldError::new(
            format!("{path}.shape"),
            "REQUIRED",
            "shape must list at least one dimension",
        ));
    } else if let Some(i) = shape.iter().position(|d| *d == 0) {
        out.push(FieldError::new(
            format!("{path}.shape[{i}]"),
            "INVALID",
            "every dimension must be positive: a model declares fixed shapes, and a zero \
             dimension is a tensor with nothing in it",
        ));
    } else if element_count(shape).is_none() {
        out.push(FieldError::new(
            format!("{path}.shape"),
            "INVALID",
            "the product of the dimensions does not fit in memory on this host",
        ));
    }
}

/// The dtype a manifest may declare: a datavalue wire name, spelled in
/// lowercase, that an adapter can decode.
pub(super) fn parse_dtype(name: &str) -> Result<DType, String> {
    let Some(dtype) = DType::from_name(name) else {
        let known: Vec<&str> = DType::ALL
            .iter()
            .filter(|d| d.has_native_element())
            .map(|d| d.name())
            .collect();
        return Err(format!(
            "unknown dtype '{name}'; one of {}",
            known.join(", ")
        ));
    };
    if dtype.name() != name {
        return Err(format!(
            "dtype '{name}' must be spelled '{}': the wire name is lowercase",
            dtype.name()
        ));
    }
    if !dtype.has_native_element() {
        return Err(format!(
            "dtype '{name}' is not supported in 1.x: an adapter cannot decode half-precision \
             tensors, so declare the model's f32 boundary and let the graph cast"
        ));
    }
    Ok(dtype)
}

fn element_count(shape: &[usize]) -> Option<usize> {
    shape.iter().try_fold(1usize, |n, d| n.checked_mul(*d))
}

/// `label(.label)*`, each label `[a-z][a-z0-9-]*`, and not under `orion`.
fn check_model_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("model name must not be empty".to_string());
    }
    for label in name.split('.') {
        if !is_label(label) {
            return Err(format!(
                "model name '{name}': label '{label}' must be lowercase, start with a letter and \
                 contain only [a-z0-9-]"
            ));
        }
    }
    if name == "orion" || name.starts_with("orion.") {
        return Err(format!(
            "model name '{name}': the 'orion' namespace is reserved"
        ));
    }
    Ok(())
}

fn is_label(label: &str) -> bool {
    let mut chars = label.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A relative path beneath the manifest's directory, with no way out of it.
fn check_artifact_path(path: &str) -> Result<(), String> {
    use std::path::{Component, Path};
    if path.is_empty() {
        return Err("artifact path must not be empty".to_string());
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(format!(
            "artifact path '{path}' must be relative to the manifest"
        ));
    }
    for component in p.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(format!(
                    "artifact path '{path}' may not leave the manifest's directory"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Value {
        json!({
            "abi": "orion:model@1.0.0",
            "name": "ada.c4-tiny",
            "version": "0.3.0",
            "format": "onnx",
            "artifact": "c4-tiny.onnx",
            "description": "A tiny Connect-4 policy network",
            "inputs": [
                {
                    "name": "board",
                    "dtype": "f32",
                    "shape": [1, 2, 6, 7],
                    "adapter": { "tensor": [{ "var": "data.board" }, "f32"] }
                }
            ],
            "outputs": [
                { "name": "policy", "dtype": "f32", "shape": [1, 7] },
                { "name": "value", "dtype": "f32", "shape": [1, 1] }
            ],
            "result": {
                "policy": { "to_list": [{ "var": "policy" }] },
                "value": { "to_list": [{ "var": "value" }] }
            }
        })
    }

    fn at(errors: &[FieldError]) -> Vec<(String, String)> {
        errors
            .iter()
            .map(|e| (e.path.clone(), e.code.clone()))
            .collect()
    }

    fn refused(mut doc: Value, edit: impl FnOnce(&mut Value)) -> Vec<FieldError> {
        edit(&mut doc);
        Manifest::validated(&doc).expect_err("must be refused")
    }

    #[test]
    fn the_design_example_validates() {
        let manifest = Manifest::validated(&good()).expect("valid");
        assert_eq!(manifest.name, "ada.c4-tiny");
        assert_eq!(manifest.format, "onnx");
        assert_eq!(manifest.artifact.as_deref(), Some("c4-tiny.onnx"));
        assert_eq!(manifest.input_names().collect::<Vec<_>>(), ["board"]);
        assert_eq!(
            manifest.output_names().collect::<Vec<_>>(),
            ["policy", "value"]
        );
        assert_eq!(manifest.inputs[0].dtype(), Some(DType::F32));
        assert_eq!(manifest.inputs[0].element_count(), Some(84));
        assert_eq!(manifest.outputs[0].element_count(), Some(7));
        assert!(is_model_manifest(&good()));
        assert!(!is_model_manifest(&json!({"abi": "orion:plugin@1.0.0"})));
        assert!(!is_model_manifest(&json!({"id": "wf", "tasks": []})));
        // The text form goes through the same door.
        Manifest::parse(&good().to_string()).expect("parses");
        assert_eq!(
            Manifest::parse("{").expect_err("not json")[0].path,
            "manifest"
        );
    }

    #[test]
    fn format_adapter_and_result_default() {
        let mut doc = good();
        let object = doc.as_object_mut().expect("object");
        object.remove("format");
        object.remove("result");
        object["inputs"][0]
            .as_object_mut()
            .expect("input")
            .remove("adapter");
        let manifest = Manifest::validated(&doc).expect("defaults fill in");
        assert_eq!(manifest.format, "onnx");
        assert_eq!(
            manifest.adapter_for(&manifest.inputs[0]),
            json!({ "tensor": [{ "var": "board" }, "f32"] })
        );
        assert_eq!(
            manifest.result_logic(),
            json!({
                "policy": { "to_list": [{ "var": "policy" }] },
                "value": { "to_list": [{ "var": "value" }] }
            })
        );
        // Both defaults pass the same compile and screen a declared
        // expression does.
        let mut out = Vec::new();
        check_expression(
            "adapter",
            &manifest.adapter_for(&manifest.inputs[0]),
            &mut out,
        );
        check_expression("result", &manifest.result_logic(), &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    /// An output named after a live operator would turn the default result
    /// into a call; the escape the serving engine honours keeps it a key.
    #[test]
    fn a_default_result_escapes_an_output_named_like_an_operator() {
        assert_eq!(
            TEMPLATE_KEY_ESCAPE,
            dataflow_rs::Engine::builder()
                .build()
                .expect("engine")
                .template_key_escape(),
            "the compile-check escape must be the serving engine's"
        );
        let outputs = vec![
            OutputDecl {
                name: "shape".to_string(),
                dtype: "i64".to_string(),
                shape: vec![2],
            },
            OutputDecl {
                name: "logits".to_string(),
                dtype: "f32".to_string(),
                shape: vec![1, 3],
            },
        ];
        let result = Manifest::default_result(&outputs);
        assert_eq!(
            result,
            json!({
                "$shape": { "to_list": [{ "var": "shape" }] },
                "logits": { "to_list": [{ "var": "logits" }] }
            })
        );
        let mut out = Vec::new();
        check_expression("result", &result, &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn the_abi_version_name_and_format_are_checked() {
        let err = refused(good(), |d| d["abi"] = json!("orion:model@2.0.0"));
        assert_eq!(at(&err), [("abi".to_string(), "INVALID".to_string())]);
        assert!(err[0].message.contains(ABI));

        for (name, needle) in [
            ("orion.c4", "reserved"),
            ("orion", "reserved"),
            ("Ada.c4", "lowercase"),
            ("ada.c4_tiny", "[a-z0-9-]"),
            ("ada..c4", "label ''"),
            ("", "must not be empty"),
        ] {
            let err = refused(good(), |d| d["name"] = json!(name));
            assert!(
                err.iter()
                    .any(|e| e.path == "name" && e.message.contains(needle)),
                "{name:?}: {err:?}"
            );
        }
        // One label is enough for a model, unlike a plugin.
        let mut doc = good();
        doc["name"] = json!("c4");
        Manifest::validated(&doc).expect("a single label is a valid name");

        let err = refused(good(), |d| d["version"] = json!("  "));
        assert_eq!(at(&err), [("version".to_string(), "REQUIRED".to_string())]);

        let err = refused(good(), |d| d["format"] = json!("safetensors"));
        assert_eq!(at(&err), [("format".to_string(), "INVALID".to_string())]);
        assert!(err[0].message.contains("'safetensors'"), "{err:?}");
        assert!(err[0].message.contains("'onnx'"), "{err:?}");
        // Every format the runtime table knows is accepted by the manifest.
        for format in crate::model::runtimes::KNOWN_FORMATS {
            let mut doc = good();
            doc["format"] = json!(format);
            assert!(
                Manifest::validated(&doc).is_ok(),
                "{format} is a known format"
            );
        }
    }

    #[test]
    fn the_artifact_path_stays_beneath_the_manifest() {
        for bad in ["../x.onnx", "/etc/x.onnx", ""] {
            let err = refused(good(), |d| d["artifact"] = json!(bad));
            assert_eq!(
                at(&err),
                [("artifact".to_string(), "INVALID".to_string())],
                "{bad}"
            );
        }
        let mut doc = good();
        doc["artifact"] = json!("build/x.onnx");
        Manifest::validated(&doc).expect("a nested relative path is fine");
        doc.as_object_mut().expect("object").remove("artifact");
        Manifest::validated(&doc).expect("absent is fine: a served row never reads it");
    }

    /// The deployable reference travels beside the local path: both optional,
    /// and a reference that is present names a connector and a key.
    #[test]
    fn a_reference_names_a_connector_and_a_key() {
        let mut doc = good();
        doc["reference"] = json!({ "connector": "models", "key": "c4/0.3.0.onnx" });
        let manifest = Manifest::validated(&doc).expect("a full reference is fine");
        assert_eq!(
            manifest.reference,
            Some(ArtifactReference {
                connector: "models".to_string(),
                key: "c4/0.3.0.onnx".to_string(),
            })
        );
        // Round-trips, and is absent from the serialised form when unset.
        let back = serde_json::to_value(&manifest).expect("serialises");
        assert_eq!(back["reference"]["key"], "c4/0.3.0.onnx");
        assert!(
            serde_json::to_value(Manifest::validated(&good()).expect("valid"))
                .expect("serialises")
                .get("reference")
                .is_none()
        );

        let err = refused(good(), |d| {
            d["reference"] = json!({ "connector": "", "key": " " })
        });
        assert_eq!(
            at(&err),
            [
                ("reference.connector".to_string(), "REQUIRED".to_string()),
                ("reference.key".to_string(), "REQUIRED".to_string()),
            ]
        );
        // The digest is never declared: it is computed from the file.
        let err =
            refused(
                good(),
                |d| {
                    d["reference"] =
                        json!({ "connector": "models", "key": "k", "digest": "sha256:x" })
                },
            );
        assert_eq!(err[0].path, "reference.digest");
        assert!(
            err[0].message.contains("unknown field"),
            "{}",
            err[0].message
        );
    }

    #[test]
    fn inputs_are_required_and_names_are_unique_per_list() {
        let err = refused(good(), |d| d["inputs"] = json!([]));
        assert_eq!(at(&err), [("inputs".to_string(), "REQUIRED".to_string())]);
        let err = refused(good(), |d| {
            d.as_object_mut().expect("object").remove("inputs");
        });
        assert_eq!(at(&err), [("inputs".to_string(), "REQUIRED".to_string())]);

        let err = refused(good(), |d| d["outputs"][1]["name"] = json!("policy"));
        assert_eq!(
            at(&err),
            [("outputs[1].name".to_string(), "DUPLICATE_FIELD".to_string())]
        );
        // The same name on an input and an output is two different tensors.
        let mut doc = good();
        doc["outputs"][0]["name"] = json!("board");
        doc["result"] = json!({ "board": { "to_list": [{ "var": "board" }] } });
        Manifest::validated(&doc).expect("input and output namespaces are separate");

        let err = refused(good(), |d| d["inputs"][0]["name"] = json!(""));
        assert_eq!(
            at(&err),
            [("inputs[0].name".to_string(), "REQUIRED".to_string())]
        );
    }

    #[test]
    fn dtypes_are_lowercase_wire_names_and_half_precision_is_refused() {
        let err = refused(good(), |d| d["inputs"][0]["dtype"] = json!("float32"));
        assert_eq!(
            at(&err),
            [("inputs[0].dtype".to_string(), "INVALID".to_string())]
        );
        assert!(err[0].message.contains("f32"), "{}", err[0].message);
        assert!(
            !err[0].message.contains("f16"),
            "the list offers only decodable dtypes"
        );

        let err = refused(good(), |d| d["outputs"][0]["dtype"] = json!("F32"));
        assert_eq!(
            at(&err),
            [("outputs[0].dtype".to_string(), "INVALID".to_string())]
        );
        assert!(err[0].message.contains("lowercase"), "{}", err[0].message);

        for half in ["f16", "bf16"] {
            let err = refused(good(), |d| d["inputs"][0]["dtype"] = json!(half));
            assert_eq!(err[0].path, "inputs[0].dtype");
            assert!(
                err[0].message.contains("half-precision"),
                "{half}: {}",
                err[0].message
            );
        }
        for ok in [
            "bool", "i8", "u8", "i16", "u16", "i32", "u32", "i64", "u64", "f32", "f64",
        ] {
            let mut doc = good();
            doc["inputs"][0]["dtype"] = json!(ok);
            doc["inputs"][0]["adapter"] = json!({ "tensor": [{ "var": "data.board" }, ok] });
            Manifest::validated(&doc).unwrap_or_else(|e| unreachable!("{ok}: {e:?}"));
        }
    }

    #[test]
    fn shapes_are_non_empty_positive_and_bounded() {
        let err = refused(good(), |d| d["inputs"][0]["shape"] = json!([]));
        assert_eq!(
            at(&err),
            [("inputs[0].shape".to_string(), "REQUIRED".to_string())]
        );
        let err = refused(good(), |d| d["inputs"][0]["shape"] = json!([1, 0, 6]));
        assert_eq!(
            at(&err),
            [("inputs[0].shape[1]".to_string(), "INVALID".to_string())]
        );
        // A negative dimension never reaches the rules: the type refuses it,
        // and the path still points at the element.
        let err = refused(good(), |d| d["outputs"][0]["shape"] = json!([1, -7]));
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].path, "outputs[0].shape[1]");
        assert_eq!(err[0].code, "INVALID");
        let huge = usize::MAX / 2 + 1;
        let err = refused(good(), |d| d["outputs"][0]["shape"] = json!([huge, 2]));
        assert_eq!(
            at(&err),
            [("outputs[0].shape".to_string(), "INVALID".to_string())]
        );
        assert!(
            err[0].message.contains("does not fit"),
            "{}",
            err[0].message
        );
    }

    /// The screen is structural: the key alone is the offence, wherever it
    /// sits, and the message says why.
    #[test]
    fn the_screen_refuses_secret_now_and_random_wherever_they_sit() {
        let err = refused(good(), |d| {
            d["inputs"][0]["adapter"] = json!({
                "tensor": [
                    { "if": [{ "var": "data.flag" }, { "secret": "api-key" }, { "var": "data.board" }] },
                    "f32"
                ]
            });
            d["result"] = json!({
                "policy": { "to_list": [{ "var": "policy" }] },
                "value": { "now": [] },
                "seed": { "cat": [{ "random": [0, 1] }, "x"] }
            });
        });
        let paths: Vec<&str> = err.iter().map(|e| e.path.as_str()).collect();
        assert!(
            paths.contains(&"inputs[0].adapter.tensor[0].if[1]"),
            "{paths:?}"
        );
        assert!(paths.contains(&"result.value"), "{paths:?}");
        assert!(paths.contains(&"result.seed.cat[0]"), "{paths:?}");
        for e in &err {
            assert_eq!(e.code, "INVALID");
        }
        let messages: Vec<&str> = err.iter().map(|e| e.message.as_str()).collect();
        assert!(
            messages.iter().any(|m| m.contains("secret store")),
            "{messages:?}"
        );
        assert!(messages.iter().any(|m| m.contains("clock")), "{messages:?}");
        assert!(
            messages.iter().any(|m| m.contains("randomness")),
            "{messages:?}"
        );

        // A multi-key object carrying the name as a field is data, not a
        // call: the serving engine reads `{"secret": x, "other": y}` as a
        // template.
        let mut doc = good();
        doc["result"] = json!({ "secret": { "var": "policy" }, "now": { "var": "value" } });
        Manifest::validated(&doc).expect("two keys is a template, not an operator");
    }

    /// One pass reports every problem, so an author fixes a manifest in one
    /// round rather than one field at a time.
    #[test]
    fn every_problem_is_reported_at_once() {
        let err = refused(good(), |d| {
            d["abi"] = json!("orion:model@0.9.0");
            d["name"] = json!("orion.c4");
            d["version"] = json!("");
            d["inputs"][0]["dtype"] = json!("f16");
            d["outputs"][0]["shape"] = json!([]);
        });
        let paths: Vec<&str> = err.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "abi",
                "name",
                "version",
                "inputs[0].dtype",
                "outputs[0].shape"
            ]
        );
    }

    #[test]
    fn unknown_keys_and_an_adapter_on_an_output_are_refused_by_the_type() {
        // The type's refusals carry the path of the offending member.
        let err = refused(good(), |d| d["license"] = json!("MIT"));
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].path, "license");
        assert_eq!(err[0].code, "INVALID");
        assert!(
            err[0].message.contains("unknown field"),
            "{}",
            err[0].message
        );
        let err = refused(good(), |d| {
            d["outputs"][0]["adapter"] = json!({ "var": "policy" });
        });
        assert_eq!(err[0].path, "outputs[0].adapter");
        assert!(
            err[0].message.contains("unknown field"),
            "{}",
            err[0].message
        );
        let err = refused(good(), |d| d["inputs"][0]["shape"] = json!("1x7"));
        assert_eq!(err[0].path, "inputs[0].shape");
        // A document that is not an object at all is reported at the root.
        let err = Manifest::validated(&json!([])).expect_err("not an object");
        assert_eq!(err[0].path, "manifest");
    }
}

//! The plugin manifest: host-owned metadata submitted with a component.
//!
//! A manifest says what a component exports and what each export accepts —
//! the plugin's name, the WIT version it was built against, and one entry per
//! function in the vocabulary of [`FieldSchema`](crate::engine::functions::schema::FieldSchema)
//! and nothing more. JSON-Schema-shaped syntax would promise a validation
//! `validate_input` does not do, so the manifest declares exactly the facts the
//! registry can act on: a field's name, kind, whether it is required, and
//! whether the engine evaluates it as an expression (`template_at`) or the
//! handler folds `{"var": …}` in it (`resolvable`).
//!
//! Parsing is strict on purpose. An unknown key, an unsupported `abi`, an
//! invalid kind or a reserved name rejects the upload — a manifest is a
//! contract the author reads back from the catalogue, and a key that was
//! silently ignored is a promise the runtime never made.

use serde::{Deserialize, Serialize};

use crate::engine::functions::schema::{FieldKind, RetrySafety, Source, WriteShape};
use crate::engine::{FieldSpec, FunctionEntry, PluginBinding};
use crate::errors::FieldError;

/// The one WIT package version this binary speaks. A manifest names it as
/// `abi`, so a component built against a later world is refused at upload
/// rather than failing its self-test with a message about a missing export.
pub const ABI: &str = "orion:plugin@1.0.0";

/// The name every plugin task may set and no manifest may declare: where the
/// result is written. Implicit on every function because every function
/// writes exactly one value — the `OutputPath` contract.
pub const OUTPUT_FIELD: &str = "output";

/// The input field every plugin function carries without declaring it.
fn output_field() -> FieldSpec {
    FieldSpec {
        name: OUTPUT_FIELD.to_string(),
        description: "Dotted context path the result is written at (defaults to the \
                      function's output_default_root)."
            .to_string(),
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        secret_at: &[],
        template_at: &[],
        alias: None,
    }
}

/// A parsed, validated manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Must equal [`ABI`].
    pub abi: String,
    /// The plugin id: lowercase reverse-domain, at least two labels. `orion.*`
    /// and unqualified names are reserved.
    pub name: String,
    /// Informational. Orion assigns the entity version.
    pub version: String,
    /// Path of the component relative to the manifest, read by offline tooling
    /// and the CLI upload only. The server receives the bytes in the request
    /// and identifies them by digest; no node ever holds a path.
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default)]
    pub functions: Vec<FunctionDecl>,
}

/// One exported function.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionDecl {
    /// Must be `<plugin name>.<label>`: a plugin's functions live in its own
    /// namespace, which is what keeps them from ever colliding with a built-in
    /// or with another plugin's.
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_category")]
    pub category: String,
    /// Where the result lands when the task names no `output`. `None` means a
    /// task must name one.
    #[serde(default)]
    pub output_default_root: Option<OutputRoot>,
    #[serde(default)]
    pub input_fields: Vec<FieldDecl>,
}

fn default_category() -> String {
    "transform".to_string()
}

/// The context roots a default output may name — a closed set, so the
/// registry's `WriteShape` keeps its `'static` spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputRoot {
    Data,
    TempData,
    Metadata,
}

impl OutputRoot {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::TempData => "temp_data",
            Self::Metadata => "metadata",
        }
    }
}

/// One input field.
///
/// `template_at` marks the field's own value as a JSONLogic expression: the
/// engine compiles it once when the workflow loads and evaluates it per
/// message, exactly as a built-in's `template_at: [""]` field. `resolvable`
/// is the narrower `{"var": …}` fold the handler does itself. A field is one
/// or the other — a template already evaluates `var` — and `secret_at` does
/// not exist here: a plugin never sees key material.
///
/// The template form needs the engine to compile against *this* function's
/// table, and one handler type serves every function a manifest declares; it
/// became possible when dataflow-rs 3.12 gave the compile hook a receiver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDecl {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub kind: FieldKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub resolvable: bool,
    #[serde(default)]
    pub template_at: bool,
}

impl<'de> Deserialize<'de> for FieldKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        match raw.as_str() {
            "string" => Ok(FieldKind::String),
            "number" => Ok(FieldKind::Number),
            "bool" => Ok(FieldKind::Bool),
            "object" => Ok(FieldKind::Object),
            "array" => Ok(FieldKind::Array),
            "any" => Ok(FieldKind::Any),
            other => Err(serde::de::Error::custom(format!(
                "unknown kind '{other}' — one of string, number, bool, object, array, any"
            ))),
        }
    }
}

impl Manifest {
    /// Parse and validate a TOML manifest. Every problem is reported, with a
    /// path into the document, so an author fixes a manifest in one round.
    pub fn parse(text: &str) -> Result<Self, Vec<FieldError>> {
        let manifest: Manifest = toml::from_str(text)
            .map_err(|e| vec![FieldError::new("manifest", "INVALID", e.to_string())])?;
        let problems = manifest.problems();
        if problems.is_empty() {
            Ok(manifest)
        } else {
            Err(problems)
        }
    }

    /// A manifest deserialized from JSON (a stored row, an import item),
    /// checked against every rule the parse cannot express through the type.
    pub fn validated(self) -> Result<Self, Vec<FieldError>> {
        let problems = self.problems();
        if problems.is_empty() {
            Ok(self)
        } else {
            Err(problems)
        }
    }

    /// Every rule the parse cannot express through the type.
    fn problems(&self) -> Vec<FieldError> {
        let mut out = Vec::new();
        if self.abi != ABI {
            out.push(FieldError::new(
                "abi",
                "INVALID",
                format!("unsupported abi '{}': this server speaks '{ABI}'", self.abi),
            ));
        }
        if let Err(reason) = check_plugin_name(&self.name) {
            out.push(FieldError::new("name", "INVALID", reason));
        }
        if self.version.trim().is_empty() {
            out.push(FieldError::new(
                "version",
                "REQUIRED",
                "version must not be empty",
            ));
        }
        if let Some(component) = &self.component
            && let Err(reason) = check_component_path(component)
        {
            out.push(FieldError::new("component", "INVALID", reason));
        }
        if self.functions.is_empty() {
            out.push(FieldError::new(
                "functions",
                "REQUIRED",
                "a plugin must export at least one function",
            ));
        }
        let mut seen: Vec<&str> = Vec::new();
        for (i, function) in self.functions.iter().enumerate() {
            let path = format!("functions[{i}]");
            if let Err(reason) = check_function_name(&self.name, &function.name) {
                out.push(FieldError::new(format!("{path}.name"), "INVALID", reason));
            } else if seen.contains(&function.name.as_str()) {
                out.push(FieldError::new(
                    format!("{path}.name"),
                    "DUPLICATE_FIELD",
                    format!("function '{}' is declared twice", function.name),
                ));
            }
            seen.push(&function.name);
            if function.category.trim().is_empty() {
                out.push(FieldError::new(
                    format!("{path}.category"),
                    "REQUIRED",
                    "category must not be empty",
                ));
            }
            let mut fields: Vec<&str> = Vec::new();
            for (j, field) in function.input_fields.iter().enumerate() {
                let field_path = format!("{path}.input_fields[{j}].name");
                if field.name == OUTPUT_FIELD {
                    out.push(FieldError::new(
                        field_path,
                        "INVALID",
                        format!(
                            "'{OUTPUT_FIELD}' is implicit on every function and may not be declared"
                        ),
                    ));
                    continue;
                }
                if let Err(reason) = check_field_name(&field.name) {
                    out.push(FieldError::new(field_path, "INVALID", reason));
                    continue;
                }
                if fields.contains(&field.name.as_str()) {
                    out.push(FieldError::new(
                        field_path,
                        "DUPLICATE_FIELD",
                        format!("field '{}' is declared twice", field.name),
                    ));
                }
                if field.template_at && field.resolvable {
                    out.push(FieldError::new(
                        format!("{path}.input_fields[{j}].template_at"),
                        "INVALID",
                        format!(
                            "field '{}' is both template_at and resolvable — a template already \
                             evaluates {{\"var\": …}}, so declare one of the two",
                            field.name
                        ),
                    ));
                }
                fields.push(&field.name);
            }
        }
        out
    }

    /// The registry entries this manifest declares, bound to the plugin row
    /// and digest they came from.
    ///
    /// Every entry is `Source::Plugin`, writes one value at `output` and is
    /// `Pure` by construction — a world with no imports cannot have an
    /// external effect. The implicit `output` field is appended here, because
    /// the create-time gate validates a plugin task with the entry's own table
    /// and `deny_unknown` set.
    pub fn entries(&self, binding: &PluginBinding) -> Vec<FunctionEntry> {
        self.functions
            .iter()
            .map(|f| FunctionEntry {
                name: f.name.clone(),
                description: f.description.clone(),
                category: f.category.clone(),
                source: Source::Plugin,
                aliases: Vec::new(),
                input_fields: Some(
                    f.input_fields
                        .iter()
                        .map(|d| FieldSpec {
                            name: d.name.clone(),
                            description: d.description.clone(),
                            kind: d.kind,
                            required: d.required,
                            resolvable: d.resolvable,
                            secret_at: &[],
                            // The field's own value is the expression — the
                            // one position a plugin field can be evaluated at,
                            // since the guest receives the resolved value.
                            template_at: if d.template_at { &[""] } else { &[] },
                            alias: None,
                        })
                        .chain(std::iter::once(output_field()))
                        .collect(),
                ),
                writes: WriteShape::OutputPath {
                    default_root: f.output_default_root.map(OutputRoot::as_str),
                },
                retry_safety: RetrySafety::Pure,
                deny_unknown: true,
                validate_static: None,
                connector: None,
                plugin: Some(binding.clone()),
            })
            .collect()
    }

    /// Every function name this manifest declares.
    pub fn function_names(&self) -> impl Iterator<Item = &str> {
        self.functions.iter().map(|f| f.name.as_str())
    }
}

/// `label(.label)+`, each label `[a-z][a-z0-9-]*`, and not under `orion`.
fn check_plugin_name(name: &str) -> Result<(), String> {
    let labels: Vec<&str> = name.split('.').collect();
    if labels.len() < 2 {
        return Err(format!(
            "plugin name '{name}' must be reverse-domain with at least two labels \
             (e.g. 'acme.iso8583'); unqualified names are reserved"
        ));
    }
    for label in &labels {
        if !is_label(label) {
            return Err(format!(
                "plugin name '{name}': label '{label}' must be lowercase, start with a letter \
                 and contain only [a-z0-9-]"
            ));
        }
    }
    if labels[0] == "orion" {
        return Err(format!(
            "plugin name '{name}': the 'orion' namespace is reserved"
        ));
    }
    Ok(())
}

/// `<plugin>.<label>` — one label beyond the plugin's own name.
fn check_function_name(plugin: &str, function: &str) -> Result<(), String> {
    let Some(rest) = function
        .strip_prefix(plugin)
        .and_then(|r| r.strip_prefix('.'))
    else {
        return Err(format!(
            "function '{function}' must be namespaced under the plugin: '{plugin}.<name>'"
        ));
    };
    if rest.is_empty() || !rest.split('.').all(is_label) {
        return Err(format!(
            "function '{function}': the part after '{plugin}.' must be lowercase labels \
             [a-z][a-z0-9-]* joined by '.'"
        ));
    }
    Ok(())
}

/// A field name is an identifier: `[a-z][a-z0-9_]*`.
fn check_field_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "field name '{name}' must be lowercase, start with a letter and contain only [a-z0-9_]"
        ))
    }
}

fn is_label(label: &str) -> bool {
    let mut chars = label.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A relative path beneath the manifest's directory, with no way out of it.
fn check_component_path(path: &str) -> Result<(), String> {
    use std::path::{Component, Path};
    if path.is_empty() {
        return Err("component path must not be empty".to_string());
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(format!(
            "component path '{path}' must be relative to the manifest"
        ));
    }
    for component in p.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "component path '{path}' may not leave the manifest's directory"
                ));
            }
        }
    }
    Ok(())
}

/// Whether a guest error code is the stable identifier the ABI requires:
/// `^[A-Z][A-Z0-9_]{0,63}$`.
pub fn is_valid_error_code(code: &str) -> bool {
    let mut chars = code.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
        && code.len() <= 64
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
abi = "orion:plugin@1.0.0"
name = "acme.iso8583"
version = "1.2.0"
component = "component.wasm"

[[functions]]
name = "acme.iso8583.parse"
description = "Parse an ISO 8583 message into field-numbered JSON"
category = "transform"
output_default_root = "data"

[[functions.input_fields]]
name = "message"
kind = "string"
required = true
resolvable = true

[[functions.input_fields]]
name = "spec"
kind = "string"
required = true
"#;

    fn binding() -> PluginBinding {
        PluginBinding {
            id: "acme.iso8583".to_string(),
            version: 1,
            digest: "sha256:00".to_string(),
            abi: ABI.to_string(),
        }
    }

    fn codes_at(errors: &[FieldError]) -> Vec<(String, String)> {
        errors
            .iter()
            .map(|e| (e.path.clone(), e.code.clone()))
            .collect()
    }

    #[test]
    fn the_design_example_parses_into_one_pure_entry_with_an_implicit_output() {
        let manifest = Manifest::parse(GOOD).expect("parses");
        let entries = manifest.entries(&binding());
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.name, "acme.iso8583.parse");
        assert_eq!(e.source, Source::Plugin);
        assert_eq!(e.retry_safety, RetrySafety::Pure);
        assert!(e.deny_unknown);
        assert!(e.connector.is_none());
        assert_eq!(
            e.writes,
            WriteShape::OutputPath {
                default_root: Some("data")
            }
        );
        let names: Vec<&str> = e
            .input_fields
            .as_deref()
            .expect("fields")
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(names, ["message", "spec", "output"]);
        assert_eq!(e.plugin.as_ref().expect("bound").digest, "sha256:00");
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let err = Manifest::parse(&GOOD.replace(
            "version = \"1.2.0\"",
            "version = \"1.2.0\"\nlicense = \"MIT\"",
        ))
        .expect_err("unknown key");
        assert_eq!(err[0].code, "INVALID");
        assert!(err[0].message.contains("license"), "{}", err[0].message);
    }

    #[test]
    fn an_unsupported_abi_is_refused() {
        let err = Manifest::parse(&GOOD.replace("orion:plugin@1.0.0", "orion:plugin@2.0.0"))
            .expect_err("abi");
        assert_eq!(codes_at(&err), [("abi".to_string(), "INVALID".to_string())]);
        assert!(err[0].message.contains("orion:plugin@1.0.0"));
    }

    #[test]
    fn reserved_and_unqualified_names_are_refused() {
        for (name, needle) in [
            ("orion.codec", "reserved"),
            ("acme", "at least two labels"),
            ("Acme.codec", "lowercase"),
            ("acme.codec_x", "[a-z0-9-]"),
        ] {
            let text = GOOD
                .replace("name = \"acme.iso8583\"", &format!("name = \"{name}\""))
                .replace("acme.iso8583.parse", &format!("{name}.parse"));
            let err = Manifest::parse(&text).expect_err(name);
            assert!(
                err.iter()
                    .any(|e| e.path == "name" && e.message.contains(needle)),
                "{name}: {err:?}"
            );
        }
    }

    #[test]
    fn a_function_outside_the_plugins_namespace_is_refused() {
        let err = Manifest::parse(&GOOD.replace("acme.iso8583.parse", "parse")).expect_err("ns");
        assert_eq!(err[0].path, "functions[0].name");
        assert!(err[0].message.contains("acme.iso8583.<name>"));
        let err = Manifest::parse(&GOOD.replace("acme.iso8583.parse", "acme.other.parse"))
            .expect_err("other ns");
        assert_eq!(err[0].path, "functions[0].name");
    }

    #[test]
    fn duplicates_and_a_declared_output_are_refused() {
        let dup = GOOD.replace(
            "[[functions.input_fields]]\nname = \"spec\"",
            "[[functions.input_fields]]\nname = \"message\"",
        );
        let err = Manifest::parse(&dup).expect_err("dup");
        assert_eq!(
            codes_at(&err),
            [(
                "functions[0].input_fields[1].name".to_string(),
                "DUPLICATE_FIELD".to_string()
            )]
        );
        let out = GOOD.replace(
            "[[functions.input_fields]]\nname = \"spec\"",
            "[[functions.input_fields]]\nname = \"output\"",
        );
        let err = Manifest::parse(&out).expect_err("output");
        assert_eq!(err[0].path, "functions[0].input_fields[1].name");
        assert!(err[0].message.contains("implicit"));
    }

    #[test]
    fn an_unknown_kind_is_refused() {
        let err = Manifest::parse(&GOOD.replace(
            "kind = \"string\"\nrequired = true\n",
            "kind = \"text\"\nrequired = true\n",
        ))
        .expect_err("kind");
        assert!(
            err[0].message.contains("unknown kind 'text'"),
            "{}",
            err[0].message
        );
    }

    /// `template_at = true` is the field's own value as an expression — the
    /// registry spelling a built-in uses — and it excludes `resolvable`,
    /// because a template already evaluates `{"var": …}`.
    #[test]
    fn a_template_field_is_an_expression_and_not_also_resolvable() {
        let manifest = Manifest::parse(&GOOD.replace("resolvable = true", "template_at = true"))
            .expect("template_at parses");
        let entries = manifest.entries(&binding());
        let fields = entries[0].input_fields.as_deref().expect("fields");
        assert_eq!(fields[0].name, "message");
        assert_eq!(fields[0].template_at, [""]);
        assert!(!fields[0].resolvable);
        assert_eq!(fields[1].template_at, [] as [&str; 0], "spec stays literal");

        let err = Manifest::parse(
            &GOOD.replace("resolvable = true", "resolvable = true\ntemplate_at = true"),
        )
        .expect_err("both");
        assert_eq!(
            codes_at(&err),
            [(
                "functions[0].input_fields[0].template_at".to_string(),
                "INVALID".to_string()
            )]
        );
        assert!(err[0].message.contains("declare one"), "{}", err[0].message);
    }

    #[test]
    fn the_component_path_stays_beneath_the_manifest() {
        for bad in ["../x.wasm", "/etc/x.wasm", ""] {
            let err = Manifest::parse(&GOOD.replace("component.wasm", bad)).expect_err(bad);
            assert!(err.iter().any(|e| e.path == "component"), "{bad}: {err:?}");
        }
        assert!(Manifest::parse(&GOOD.replace("component.wasm", "build/component.wasm")).is_ok());
    }

    #[test]
    fn a_manifest_without_functions_is_refused() {
        let text = GOOD.split("[[functions]]").next().expect("head");
        let err = Manifest::parse(text).expect_err("no functions");
        assert_eq!(err[0].path, "functions");
        assert_eq!(err[0].code, "REQUIRED");
    }

    #[test]
    fn error_codes_follow_the_abi_grammar() {
        for good in ["E", "BAD_INPUT", "X1_Y2", &"A".repeat(64)] {
            assert!(is_valid_error_code(good), "{good}");
        }
        for bad in ["", "bad", "1A", "A-B", "A B", &"A".repeat(65)] {
            assert!(!is_valid_error_code(bad), "{bad:?}");
        }
    }
}

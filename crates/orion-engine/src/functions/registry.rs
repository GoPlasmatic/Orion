//! The one registry of every function a workflow may name.
//!
//! It answers "may a workflow name this, and what does it accept?" for every
//! reader. A set of static tables could not: it is closed at compile time,
//! and a plugin function is an entry that exists only once a component has
//! been loaded — so the lookup has to be a *value*: built once
//! per `orion_server::runtime::RuntimeGeneration` from the static tables plus
//! whatever that generation loaded, and handed to every reader. The static
//! declarations live with the handlers; one inventory in `functions/mod.rs`
//! drives their registration and derives their schemas from the typed configs.
//! [`FunctionRegistry::builtin`] converts those schemas into owned entries once and is
//! what every offline surface and the boot generation read.
//!
//! What the registry deliberately does **not** own is the functions. A
//! generation builds more than one engine (the serving one, the
//! `POST /definitions/test` one), so an entry describes a function and
//! whoever builds an engine's `dataflow_rs::Registry` supplies the function
//! behind it — `engine::register_functions` for an Orion entry, a loaded
//! plugin for a plugin entry. The `functions_docs_drift_test` and `function_schema_test`
//! guards pin the two halves to each other against a live engine.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, OnceLock};

use serde::Serialize;
use serde_json::Value;

use super::schema::{
    self, ConnectorRule, FieldKind, FieldSchema, FunctionSchema, RetrySafety, Source,
    StaticValidator, WriteShape,
};
use crate::errors::FieldError;

/// One input field of a registered function — [`FieldSchema`], owned.
///
/// Serialises to exactly the JSON the static row does, member for member, so
/// `GET /admin/functions` reads the same whether an entry came from a table or
/// a manifest. `secret_at` and `template_at` stay `'static`: the spellings are
/// a closed vocabulary (`""`, `"*"`, `"[].key"`), and a plugin field is only
/// ever `&[]` or `&[""]`.
#[derive(Debug, Clone, Serialize)]
pub struct FieldSpec {
    pub name: String,
    pub description: String,
    pub kind: FieldKind,
    pub required: bool,
    pub resolvable: bool,
    pub secret_at: &'static [&'static str],
    pub template_at: &'static [&'static str],
}

impl FieldSpec {
    /// Whether `key` names this field.
    pub fn answers_to(&self, key: &str) -> bool {
        self.name == key
    }

    /// Whether the field's own value is a `Template`, so its authored JSON may
    /// be an expression rather than a literal of its declared kind.
    fn is_template(&self) -> bool {
        self.template_at.contains(&"")
    }

    /// Whether the authored value `v` passes this field's kind check, the
    /// one [`FunctionEntry::validate_input`] makes of every field it is
    /// handed: a value of the declared kind; an object or an array in a
    /// `Template` position, which may be an operator call; a `{"var": ..}`
    /// node in a `resolvable` field; a `{"secret": ..}` node where the field
    /// reads key material at its root. A plugin function's dataflow-rs 4
    /// config asks it of each `resolvable` field at load, so the two cannot
    /// disagree.
    pub fn admits(&self, v: &Value) -> bool {
        self.kind.matches(v)
            // A `Template` field's kind describes the *resolved* value. An
            // object or array there may be an operator call, so only a
            // scalar — unambiguously itself in JSONLogic — is still checked
            // against the kind directly.
            || (self.is_template() && (v.is_object() || v.is_array()))
            || (self.resolvable && is_var_node(v))
            || takes_secret_node(self, v)
    }
}

impl From<&FieldSchema> for FieldSpec {
    fn from(f: &FieldSchema) -> Self {
        Self {
            name: f.name.to_string(),
            description: f.description.to_string(),
            kind: f.kind,
            required: f.required,
            resolvable: f.resolvable,
            secret_at: f.secret_at,
            template_at: f.template_at,
        }
    }
}

/// Which plugin a [`Source::Plugin`] entry came from — what the catalogue,
/// a trace and a package all name it by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginBinding {
    pub id: String,
    pub version: i64,
    /// `sha256:…` of the component bytes, computed by the server on upload.
    pub digest: String,
    /// The WIT package version the component was built against.
    pub abi: String,
}

/// One function a workflow may name: everything the readers ask about it.
#[derive(Debug, Clone)]
pub struct FunctionEntry {
    pub name: String,
    pub description: String,
    pub category: String,
    pub source: Source,
    /// `None` for an engine built-in, which declares no schema and is not
    /// input-validated at create time. A consumer branches on presence.
    pub input_fields: Option<Vec<FieldSpec>>,
    pub writes: WriteShape,
    pub retry_safety: RetrySafety,
    pub deny_unknown: bool,
    pub validate_static: Option<StaticValidator>,
    pub connector: Option<ConnectorRule>,
    pub plugin: Option<PluginBinding>,
}

impl FunctionEntry {
    /// An Orion handler's entry, from its static row.
    pub fn orion(schema: &FunctionSchema) -> Self {
        Self {
            name: schema.name.to_string(),
            description: schema.description.to_string(),
            category: schema.category.to_string(),
            source: Source::Orion,
            input_fields: Some(schema.input_fields.iter().map(FieldSpec::from).collect()),
            writes: schema.writes,
            retry_safety: schema.retry_safety,
            deny_unknown: schema.deny_unknown,
            validate_static: schema.validate_static,
            connector: schema.connector,
            plugin: None,
        }
    }

    /// An engine built-in's entry, from its [`schema::ENGINE_BUILTINS`] row.
    fn engine(
        name: &str,
        description: &str,
        writes: WriteShape,
        retry_safety: RetrySafety,
    ) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            category: "data".to_string(),
            source: Source::Engine,
            input_fields: None,
            writes,
            retry_safety,
            deny_unknown: false,
            validate_static: None,
            connector: None,
            plugin: None,
        }
    }

    /// Whether this function's `connector` field must name a connector.
    pub fn takes_connector(&self) -> bool {
        self.connector.is_some()
    }

    fn field(&self, key: &str) -> Option<&FieldSpec> {
        self.input_fields
            .as_deref()?
            .iter()
            .find(|f| f.answers_to(key))
    }
}

/// One entry of `GET /api/v2/admin/functions`.
///
/// Every name a workflow may use, not only the functions Orion input-schema
/// validates: those omit `map`, `filter`, `parse_json` and the rest, which are
/// the ones people actually type — in the deployment that reported it
/// such names were 425 of 631 tasks, `map` alone 310. A completion
/// source offering the connector functions and none of those is not an
/// incomplete catalogue, it is the wrong one.
///
/// A projection of [`FunctionEntry`] rather than the entry itself, because the
/// wire shape is pinned by the OpenAPI document in GoPlasmatic/Orion-docs and
/// the two are allowed to grow at different rates: `writes` and the connector
/// rule are the analysis's business and are not served.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct CatalogueEntry {
    pub name: String,
    pub description: String,
    /// `connector`, `control`, `data`, `compute`, or `utility`.
    pub category: String,
    /// `orion` for a handler Orion implements and input-schema validates,
    /// `engine` for a dataflow-rs built-in the engine executes itself,
    /// `plugin` for a WebAssembly plugin's function.
    #[schema(value_type = String)]
    pub source: Source,
    /// **Absent**, not null, when the function declares no input schema —
    /// which is the honest JSON encoding of "there is nothing here", and what
    /// a consumer branches on.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<Object>>)]
    pub input_fields: Option<Vec<FieldSpec>>,
    /// What a second run of this function does — see [`RetrySafety`]. Served
    /// for every entry, built-ins included: "is it safe to retry this task?"
    /// is a question about every function a workflow can name, not only the
    /// ones Orion declares an input schema for. `{"kind": …}`: `pure`,
    /// `read`, `idempotent_write`, `unsafe_write`, or `depends_on` with the
    /// `input` that decides it.
    #[schema(value_type = Object)]
    pub retry_safety: RetrySafety,
    /// Present only on a `source: "plugin"` entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub plugin: Option<PluginBinding>,
}

/// Every function a workflow may name, and what each one declares.
///
/// Immutable once built. A generation holds one in an `Arc`; the offline
/// surfaces hold [`Self::builtin`].
#[derive(Debug)]
pub struct FunctionRegistry {
    /// Sorted by canonical name — a catalogue is browsed, and the static
    /// tables' own order groups by implementation concern.
    entries: Vec<Arc<FunctionEntry>>,
    /// Name → index into `entries`.
    index: HashMap<String, usize>,
}

impl FunctionRegistry {
    /// The registry of everything this binary ships: Orion's handlers and the
    /// engine's built-ins, converted from the static tables once per process.
    ///
    /// What every offline surface (`lint`, `preflight`, `dry-run`, `test`, the
    /// stub table) reads, and what the boot generation publishes until a
    /// loaded plugin set extends it.
    pub fn builtin() -> &'static Arc<FunctionRegistry> {
        static BUILTIN: OnceLock<Arc<FunctionRegistry>> = OnceLock::new();
        BUILTIN.get_or_init(|| {
            let entries = schema::registry()
                .iter()
                .map(FunctionEntry::orion)
                .chain(schema::ENGINE_BUILTINS.iter().map(
                    |&(name, description, writes, retry_safety)| {
                        FunctionEntry::engine(name, description, writes, retry_safety)
                    },
                ))
                .collect();
            Arc::new(
                FunctionRegistry::from_entries(entries)
                    .expect("the static tables declare each function once"),
            )
        })
    }

    /// Build a registry over `entries`, refusing a name two entries claim.
    ///
    /// Refused rather than last-wins because every reader assumes a name means
    /// one function: validation would check one schema and the engine would
    /// dispatch the other.
    pub fn from_entries(entries: Vec<FunctionEntry>) -> Result<Self, String> {
        let mut entries: Vec<Arc<FunctionEntry>> = entries.into_iter().map(Arc::new).collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let mut index = HashMap::new();
        for (i, entry) in entries.iter().enumerate() {
            let name = &entry.name;
            if let Some(prior) = index.insert(name.clone(), i) {
                let prior: &FunctionEntry = &entries[prior];
                return Err(format!(
                    "function name '{name}' is claimed twice: by '{}' ({}) and '{}' ({})",
                    prior.name,
                    prior.source.as_str(),
                    entry.name,
                    entry.source.as_str()
                ));
            }
            intern(name);
        }
        Ok(Self { entries, index })
    }

    /// This registry plus `extra` — the serving generation's registry, built
    /// from the built-in one and whatever the generation loaded.
    pub fn with_entries(&self, extra: Vec<FunctionEntry>) -> Result<Self, String> {
        let entries = self
            .entries
            .iter()
            .map(|e| (**e).clone())
            .chain(extra)
            .collect();
        Self::from_entries(entries)
    }

    /// The entry named `name`.
    pub fn get(&self, name: &str) -> Option<&FunctionEntry> {
        self.index.get(name).map(|&i| &*self.entries[i])
    }

    /// Whether a workflow may name `name`.
    ///
    /// This gates workflow **creation**, so a name this rejects is not a
    /// warning — the workflow is refused outright. The rule is "would the
    /// engine Orion actually builds be able to run this task?", and the answer
    /// is membership: an engine built-in is here only if it runs with no
    /// registration (`enrich` needs a handler Orion never registers, so it is
    /// absent), and a handler-backed name is here only if `register_functions`
    /// registers it.
    /// `the_create_time_gate_agrees_with_the_running_engine` pins that to a
    /// live engine's `can_dispatch` in both directions.
    pub fn contains(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    /// Every canonical name, sorted. Aliases are not yielded — a
    /// [`CatalogueEntry`] carries its own.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    /// Every entry, sorted by name.
    pub fn entries(&self) -> impl Iterator<Item = &FunctionEntry> {
        self.entries.iter().map(|e| &**e)
    }

    /// The registered name nearest to `name`, when one is close enough that a
    /// typo is the likely explanation — the suggestion the `UNKNOWN_FUNCTION`
    /// validation error appends.
    ///
    /// Function names are short (4–15 characters), so the fixed distance-3
    /// window `config/unknown_env.rs` gives env-var overrides would surface
    /// suggestions for inputs that are clearly unrelated ("x" is 3 edits from
    /// "map"). The window here scales with the shorter name — a third of its
    /// length, clamped to 1–3 edits — which covers the realistic typo shapes
    /// (a transposed pair, a doubled letter, a missing suffix) and nothing
    /// else.
    pub fn suggest(&self, name: &str) -> Option<&str> {
        let needle: Vec<char> = name.chars().collect();
        self.names()
            .map(|candidate| {
                let candidate_chars: Vec<char> = candidate.chars().collect();
                (
                    crate::text::edit_distance_chars(&needle, &candidate_chars),
                    candidate,
                )
            })
            .filter(|(distance, candidate)| {
                let window = (name.len().min(candidate.len()) / 3).clamp(1, 3);
                *distance <= window
            })
            .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)))
            .map(|(_, candidate)| candidate)
    }

    /// What `GET /api/v2/admin/functions` serves: every entry, sorted by name.
    pub fn catalogue(&self) -> Vec<CatalogueEntry> {
        self.entries
            .iter()
            .map(|e| CatalogueEntry {
                name: e.name.clone(),
                description: e.description.clone(),
                category: e.category.clone(),
                source: e.source,
                input_fields: e.input_fields.clone(),
                retry_safety: e.retry_safety,
                plugin: e.plugin.clone(),
            })
            .collect()
    }

    /// Where `function` writes its output, for any function a workflow may
    /// name — Orion's handlers, the engine's built-ins and a plugin's alike.
    ///
    /// `None` for a name the registry does not know, which is a function that
    /// does not exist: the analysis then reports no writes rather than
    /// guessing a shape for it: a catch-all applying the `output` rule to
    /// *any* unrecognised name would let a typoed function silently contribute
    /// a write.
    pub fn write_shape(&self, function: &str) -> Option<WriteShape> {
        self.get(function).map(|e| e.writes)
    }

    /// Whether `function` takes a connector — see [`ConnectorRule`].
    pub fn takes_connector(&self, function: &str) -> bool {
        self.get(function)
            .is_some_and(FunctionEntry::takes_connector)
    }

    /// Whether `field` is one this function folds `{"var": ..}` nodes in
    /// before use — i.e. whether the value the handler acts on differs from
    /// the value the author wrote.
    ///
    /// The offline call recorder reads this to resolve a task's payload the
    /// same way the real handler will, so a recorded call shows what *would be
    /// sent* rather than what was typed. Driving it off the registry rather
    /// than a per-function list is what makes a new connector function's calls
    /// recordable as soon as it fills in the field table it already has to
    /// fill in.
    pub fn is_resolvable_field(&self, function: &str, field: &str) -> bool {
        self.get(function)
            .and_then(|e| e.field(field))
            .is_some_and(|f| f.resolvable)
    }

    /// Where inside `field` this function reads key material — the only paths
    /// where `env://NAME` or `vault://…` means anything other than itself.
    ///
    /// Driven off the registry rather than a per-function list for the same
    /// reason [`Self::is_resolvable_field`] is: a function that starts
    /// resolving references in a new field declares it in the field table it
    /// already maintains, and the authoring-time check follows automatically.
    ///
    /// A function with no declared schema (an engine built-in) answers `&[]`:
    /// none of them resolves a reference, and treating an unknown function as
    /// permissive would make the check silently vacuous for the one case it
    /// cannot see into.
    pub fn secret_paths(&self, function: &str, field: &str) -> &'static [&'static str] {
        self.get(function)
            .and_then(|e| e.field(field))
            .map(|f| f.secret_at)
            .unwrap_or(&[])
    }

    /// Where inside `field` dataflow-rs evaluates JSONLogic — see
    /// [`FieldSchema::template_at`]. Driven off the registry for the same
    /// reason [`Self::secret_paths`] is: the handler's own field table is the
    /// one declaration.
    pub fn template_paths(&self, function: &str, field: &str) -> &'static [&'static str] {
        self.get(function)
            .and_then(|e| e.field(field))
            .map(|f| f.template_at)
            .unwrap_or(&[])
    }

    /// Validate a function's `input` JSON against its declared schema.
    /// `task_path` is the dotted prefix used to build field paths (e.g.
    /// `"tasks[2]"`). Returns an empty `Vec` when the function declares no
    /// schema or all checks pass.
    ///
    /// At least one of `channel` / `channel_logic` is required for
    /// `channel_call`; that cross-field rule is enforced here in addition to
    /// the per-field schema checks.
    pub fn validate_input(
        &self,
        function_name: &str,
        input: &Value,
        task_path: &str,
    ) -> Vec<FieldError> {
        self.get(function_name)
            .map(|entry| entry.validate_input(input, task_path))
            .unwrap_or_default()
    }
}

impl FunctionEntry {
    /// Validate a task's `input` against this entry's declared schema — the
    /// per-entry half of [`FunctionRegistry::validate_input`], also run by a
    /// plugin handler at execution time over the resolved input.
    pub fn validate_input(&self, input: &Value, task_path: &str) -> Vec<FieldError> {
        let Some(fields) = self.input_fields.as_deref() else {
            return Vec::new();
        };
        let entry = self;
        let function_name = self.name.as_str();

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
        errors.extend(check_fields(fields, input, &input_path, function_name));
        if entry.deny_unknown {
            errors.extend(check_unknown_fields(
                fields,
                input,
                &input_path,
                function_name,
            ));
        }

        // Cross-field: data_write's mutation envelope. It is nested under
        // `write`, or flat with its keys beside the handler keys; whichever
        // shape the task uses is checked against the same field list.
        if function_name == "data_write" {
            match obj.get("write") {
                // A non-object `write` is already reported by the field loop above.
                Some(w) if w.is_object() => errors.extend(check_fields(
                    &DATA_WRITE_ENVELOPE,
                    w,
                    &format!("{input_path}.write"),
                    function_name,
                )),
                Some(_) => {}
                // Flat form: envelope keys sit alongside the handler keys.
                None if obj.contains_key("op") => errors.extend(check_fields(
                    &DATA_WRITE_ENVELOPE,
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

        // Connector targets are literals: dependency checks and runtime lookup
        // must identify them before evaluating a message. Keep the specific
        // explanation for a computed name, replacing the generic kind error
        // so this remains one refusal even when the config declares a literal.
        if entry.takes_connector()
            && let Some(value) = obj.get("connector")
            && (value.is_object() || value.is_array())
        {
            let path = format!("{input_path}.connector");
            errors.retain(|error| error.path != path);
            errors.push(
                FieldError::new(
                    path,
                    "TYPE_MISMATCH",
                    format!(
                        "function '{function_name}' needs a literal connector name — the \
                         connector is resolved before the message is read, and the same name \
                         is what the dependency list, the activation gate and the connector \
                         rename guard are built from"
                    ),
                )
                .with_expected(Value::String("string".to_string()))
                .with_got(value.clone()),
            );
        }

        // Cross-field rules registered on the entry — each lives next to its
        // handler as `validate_static_input` and shares the execution path's
        // tables, so the authoring-time rules and the
        // runtime cannot drift, and a new function's rules are one registry
        // field.
        if let Some(validate) = entry.validate_static {
            for (suffix, code, message) in validate(obj) {
                let path = if suffix.is_empty() {
                    input_path.clone()
                } else {
                    format!("{input_path}.{suffix}")
                };
                errors.push(FieldError::new(path, code, message));
            }
        }

        // Cross-field: http_call's format axes. dataflow-rs carries
        // `body_format` and `response_format` as uninterpreted strings, so the
        // value table is enforced here — an unknown value is an authoring-time
        // error, never a request-time surprise. A *static* `body` is
        // shape-checked against the format too, by the same `encode_body` the
        // request path runs, so the two layers cannot drift; a `body_logic`
        // body only exists per message and gets that check at request time.
        if function_name == "http_call" {
            use super::http_common::{BodyFormat, ResponseFormat, encode_body};

            // A non-string value is already a TYPE_MISMATCH from the field loop.
            let body_format =
                match BodyFormat::parse(obj.get("body_format").and_then(Value::as_str)) {
                    Ok(f) => Some(f),
                    Err(msg) => {
                        errors.push(FieldError::new(
                            format!("{input_path}.body_format"),
                            "INVALID",
                            msg,
                        ));
                        None
                    }
                };
            if let Err(msg) =
                ResponseFormat::parse(obj.get("response_format").and_then(Value::as_str))
            {
                errors.push(FieldError::new(
                    format!("{input_path}.response_format"),
                    "INVALID",
                    msg,
                ));
            }
            if let (Some(format), Some(body)) = (body_format, obj.get("body"))
                && format != BodyFormat::Json
                && let Err(e) = encode_body(body, format)
            {
                // `encode_body` refuses only as the caller's to fix, so the
                // guidance is the message without the class label its error
                // record carries.
                let msg = crate::HandlerError::from(e).msg;
                errors.push(FieldError::new(
                    format!("{input_path}.body"),
                    "INVALID",
                    msg,
                ));
            }
        }

        errors
    }
}

/// `data_write`'s mutation envelope, in the registry's owned form. A static
/// table like every other, converted once.
static DATA_WRITE_ENVELOPE: LazyLock<Vec<FieldSpec>> = LazyLock::new(|| {
    super::data_write::DATA_WRITE_ENVELOPE_FIELDS
        .iter()
        .map(FieldSpec::from)
        .collect()
});

/// A `{"var": ..}` node — the one shape a `resolvable` field may carry in
/// place of a literal of its declared kind. Nodes nested deeper are not checked
/// here: the declared kind still describes the field's own shape, and the
/// resolver folds `{"var": ..}` at any depth inside it.
fn is_var_node(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == 1 && o.contains_key("var"))
}

/// Whether this field's own value may be a `{"secret": ..}` node in place of a
/// literal of its declared kind — true only when `secret_at` lists the field
/// root, for the same reason and with the same depth rule as [`is_var_node`].
/// The handler reads it through [`super::secret_ref`]; the declared kind still
/// describes what the *resolved* value must be.
///
/// `jwt_verify.keys` reads key material two levels down rather than at the
/// root, so it does **not** qualify: a bare `{"secret": …}` there is an object
/// where an array belongs, and the handler would read no static key from it at
/// all.
fn takes_secret_node(field: &FieldSpec, v: &Value) -> bool {
    field.secret_at.contains(&"") && super::secret_ref::secret_name(v).is_some()
}

/// Check one field list against one JSON object, reporting paths under
/// `path_prefix`. Shared by the top-level input check and `data_write`'s
/// nested `write` envelope.
fn check_fields(
    fields: &[FieldSpec],
    input: &Value,
    path_prefix: &str,
    function_name: &str,
) -> Vec<FieldError> {
    let mut errors = Vec::new();
    let Some(obj) = input.as_object() else {
        return errors;
    };
    for field in fields {
        match (obj.get(field.name.as_str()), field.required) {
            (None, true) => errors.push(FieldError::new(
                format!("{path_prefix}.{}", field.name),
                "REQUIRED",
                format!(
                    "function '{function_name}' requires '{}' ({})",
                    field.name,
                    field.kind.as_str()
                ),
            )),
            (Some(v), _) if !field.admits(v) => {
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
    fields: &[FieldSpec],
    input: &Value,
    path_prefix: &str,
    function_name: &str,
) -> Vec<FieldError> {
    let Some(obj) = input.as_object() else {
        return Vec::new();
    };
    obj.keys()
        .filter(|key| !fields.iter().any(|f| f.answers_to(key)))
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

// ============================================================
// Metric labels
// ============================================================

/// Every name any registry has ever held, as `&'static str`.
///
/// `metrics` labels want a `&'static str` or an owned `String`; borrowing from
/// this set avoids allocating per task, and it doubles as the cardinality
/// bound the observer relies on — a name no registry registered cannot mint a
/// label value. Bounded by the distinct names registered over the process's
/// life: the static tables once, plus each distinct plugin function name once
/// however many generations load it.
///
/// `ArcSwap`, not a lock: this is read on every task for its metric label, and
/// a shared read lock put every task on one cache line (P-16). Writes are
/// rare and copy-on-write.
fn labels() -> &'static arc_swap::ArcSwap<HashSet<&'static str>> {
    static LABELS: OnceLock<arc_swap::ArcSwap<HashSet<&'static str>>> = OnceLock::new();
    LABELS.get_or_init(|| arc_swap::ArcSwap::from_pointee(HashSet::new()))
}

/// Maximum distinct function names interned as static metric labels over the
/// process's lifetime. Above this cap, additional dynamic names collapse to
/// `"plugin"` so repeated uploads cannot grow the label set without bound.
const MAX_INTERNED_LABELS: usize = 1024;

/// Record `name` as a label value, leaking it once while below [`MAX_INTERNED_LABELS`].
fn intern(name: &str) {
    if labels().load().contains(name) {
        return;
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    labels().rcu(|set| {
        let mut next = (**set).clone();
        if next.len() < MAX_INTERNED_LABELS {
            next.insert(leaked);
        }
        std::sync::Arc::new(next)
    });
}

/// The `&'static str` a registered name interns to, or `None` for a name no
/// registry has held — the caller collapses that to one label value.
pub fn interned(name: &str) -> Option<&'static str> {
    labels().load().get(name).copied()
}

impl FunctionEntry {
    /// The `&'static str` this entry's name interns to — a metric label.
    ///
    /// Interned on first use for an entry no registry has held yet, so a
    /// handler built straight from a manifest (a test, a dry run) labels
    /// itself the same way one a generation carries does. Bounded by
    /// `MAX_INTERNED_LABELS`, falling back to `"plugin"` if the cap is full.
    pub fn label(&self) -> &'static str {
        if let Some(label) = interned(&self.name) {
            return label;
        }
        intern(&self.name);
        interned(&self.name).unwrap_or("plugin")
    }
}

// ============================================================
// Tests
// ============================================================

/// The moved schema tests below read the built-in registry through the free
/// functions they were written against.
#[cfg(test)]
fn validate_input(function: &str, input: &Value, task_path: &str) -> Vec<FieldError> {
    FunctionRegistry::builtin().validate_input(function, input, task_path)
}

#[cfg(test)]
fn is_resolvable_field(function: &str, field: &str) -> bool {
    FunctionRegistry::builtin().is_resolvable_field(function, field)
}

#[cfg(test)]
fn write_shape(function: &str) -> Option<WriteShape> {
    FunctionRegistry::builtin().write_shape(function)
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;
#[cfg(test)]
#[path = "write_shape_tests.rs"]
mod write_shape_tests;

#[cfg(test)]
#[path = "resolvable_contract_tests.rs"]
mod resolvable_contract_tests;

#[cfg(test)]
#[path = "validate_input_tests.rs"]
mod validate_input_tests;

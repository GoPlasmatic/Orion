//! The one registry of every function a workflow may name.
//!
//! Three static tables used to answer "may a workflow name this, and what does
//! it accept?": `schema::REGISTRY` (the Orion handlers' field tables),
//! `ENGINE_BUILTINS` (the engine's own eight) and `CUSTOM_HANDLER_FUNCTIONS`
//! (the names `build_custom_functions` registers) — plus three more keyed by
//! name in `handlers.rs` for the connector rules. Nine readers consulted them
//! through nine free functions, each a linear scan over `&'static` rows, and
//! the set was closed at compile time by construction: nothing that was not a
//! `const` could be a function.
//!
//! A plugin function is exactly that — an entry that exists only once a
//! component has been loaded — so the lookup has to be a *value*: built once
//! per [`crate::runtime::RuntimeGeneration`] from the static tables plus
//! whatever that generation loaded, and handed to every reader. The static
//! tables stay where they are, next to the handlers they describe (F53);
//! [`FunctionRegistry::builtin`] converts them into owned entries once and is
//! what every offline surface and the boot generation read.
//!
//! What the registry deliberately does **not** own is the handlers. An
//! [`EngineBuilder::with_handlers`] takes boxed handlers by value and a
//! generation builds more than one engine (the serving one, the
//! `POST /workflows/{id}/test` one), so an entry describes a function and
//! whoever builds an engine supplies the handler behind it — from
//! `build_custom_functions` for an Orion entry, from a loaded plugin for a
//! plugin entry. The `functions_docs_drift_test` and `function_schema_test`
//! guards pin the two halves to each other against a live engine.
//!
//! [`EngineBuilder::with_handlers`]: dataflow_rs::engine::EngineBuilder::with_handlers

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, OnceLock, RwLock};

use dataflow_rs::engine::error::DataflowError;
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
    pub alias: Option<String>,
}

impl FieldSpec {
    /// Whether `key` names this field, under either spelling.
    pub fn answers_to(&self, key: &str) -> bool {
        self.name == key || self.alias.as_deref() == Some(key)
    }

    /// Whether the field's own value is a `Template`, so its authored JSON may
    /// be an expression rather than a literal of its declared kind.
    fn is_template(&self) -> bool {
        self.template_at.contains(&"")
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
            alias: f.alias.map(str::to_string),
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
    /// Other accepted spellings of the name. An alias resolves to this entry
    /// and is never catalogued as an entry of its own.
    pub aliases: Vec<String>,
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
            aliases: Vec::new(),
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
        aliases: &[&str],
        writes: WriteShape,
        retry_safety: RetrySafety,
    ) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            category: "data".to_string(),
            source: Source::Engine,
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
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

/// One entry of `GET /api/v1/admin/functions`.
///
/// The endpoint used to serve the schema registry directly, which meant it
/// listed only the functions Orion input-schema validates — 18 of the 27 valid
/// names, omitting `map`, `filter`, `parse_json` and the rest. Those are the
/// ones people actually type: in the deployment that reported it (#288) the
/// nine omitted names were 425 of 631 tasks, `map` alone 310. A completion
/// source offering the connector functions and none of those is not an
/// incomplete catalogue, it is the wrong one.
///
/// A projection of [`FunctionEntry`] rather than the entry itself, because the
/// wire shape is pinned by `docs/openapi.json` and the two are allowed to grow
/// at different rates: `writes` and the connector rule are the analysis's
/// business and are not served.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueEntry {
    pub name: String,
    pub description: String,
    pub category: String,
    pub source: Source,
    /// Other accepted spellings of this name. Serving an alias as its own
    /// entry would tell a completion tool there are two functions.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// **Absent**, not null, when the function declares no input schema —
    /// which is the honest JSON encoding of "there is nothing here", and what
    /// a consumer branches on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_fields: Option<Vec<FieldSpec>>,
    /// What a second run of this function does — see [`RetrySafety`]. Served
    /// for every entry, built-ins included: "is it safe to retry this task?"
    /// is a question about every function a workflow can name, not only the
    /// ones Orion declares an input schema for.
    pub retry_safety: RetrySafety,
    /// Present only on a `source: "plugin"` entry.
    #[serde(skip_serializing_if = "Option::is_none")]
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
    /// Canonical name *and* every alias → index into `entries`.
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
                    |&(name, description, aliases, writes, retry_safety)| {
                        FunctionEntry::engine(name, description, aliases, writes, retry_safety)
                    },
                ))
                .collect();
            Arc::new(
                FunctionRegistry::from_entries(entries)
                    .expect("the static tables declare each function once"),
            )
        })
    }

    /// Build a registry over `entries`, refusing a name or alias that two
    /// entries claim.
    ///
    /// Refused rather than last-wins because every reader assumes a name means
    /// one function: validation would check one schema and the engine would
    /// dispatch the other.
    pub fn from_entries(entries: Vec<FunctionEntry>) -> Result<Self, String> {
        let mut entries: Vec<Arc<FunctionEntry>> = entries.into_iter().map(Arc::new).collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let mut index = HashMap::new();
        for (i, entry) in entries.iter().enumerate() {
            for name in std::iter::once(&entry.name).chain(&entry.aliases) {
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

    /// The entry `name` resolves to, under its canonical name or an alias.
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
    /// absent — F54, the case where a hand-copied list accepted it and every
    /// `enrich` workflow failed its every request), and a handler-backed name
    /// is here only if `build_custom_functions` registers it.
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

    /// Every spelling [`Self::contains`] accepts: the canonical names and
    /// their aliases. The vocabulary the create-time gate admits, for the
    /// tests that hold it against a live engine's `dispatchable_functions`.
    pub fn accepted_names(&self) -> impl Iterator<Item = &str> {
        self.index.keys().map(String::as_str)
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

    /// What `GET /api/v1/admin/functions` serves: every entry, sorted by name.
    pub fn catalogue(&self) -> Vec<CatalogueEntry> {
        self.entries
            .iter()
            .map(|e| CatalogueEntry {
                name: e.name.clone(),
                description: e.description.clone(),
                category: e.category.clone(),
                source: e.source,
                aliases: e.aliases.clone(),
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
    /// guessing a shape for it. That is a deliberate change from the previous
    /// hand-written `match`, whose catch-all arm applied the
    /// `output`/`response_path` rule to *any* unrecognised name — so a typoed
    /// function silently contributed a write.
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

        // Cross-field: data_write's mutation envelope. Nested under `write`
        // since W7; the pre-1.0 flat form is still accepted, and whichever
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
                // Legacy flat form: envelope keys sit alongside the handler keys.
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

        // A connector must be named, not computed.
        //
        // dataflow-rs 3.9 made `http_call`/`publish_kafka`'s `connector` a
        // `Template` like every other parameter, so the kind check above no
        // longer refuses an object there — a template field's kind describes
        // what it evaluates to. Orion needs this one to fold to a name it can
        // read *without* a message, and not because the handler is lazy: the
        // connector is looked up before the message is consulted (F58), and
        // the same static name is what `GET /workflows/{id}/dependencies`
        // reports, what the activation gate checks exists, what refuses a
        // rename or delete of a connector still in use, and what a package's
        // `requires` list is built from. A computed name is invisible to all
        // five, so admitting one means teaching all five, not relaxing one
        // check.
        //
        // Driven off the schema rather than a function list, so a connector
        // handler added later inherits the rule with the field table it
        // already fills in. A string is the test upstream's own
        // `ConnectorName` uses to answer `Static` vs `Computed`, so the two
        // agree by construction.
        //
        // Exactly the complement of what `check_fields` still checks: it
        // reports a *scalar* of the wrong type itself, so this fires only for
        // the object and array it now waves through, and one wrong connector
        // is one error.
        if let Some(field) = fields.iter().find(|f| f.name == "connector")
            && field.is_template()
            && let Some(value) = obj.get(field.name.as_str())
            && (value.is_object() || value.is_array())
        {
            errors.push(
                FieldError::new(
                    format!("{input_path}.connector"),
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
        // tables (#263 and friends), so the authoring-time rules and the
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
                let msg = match e {
                    DataflowError::Validation(m) => m,
                    other => other.to_string(),
                };
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
        // An aliased field may be supplied under either name — but not both.
        // Upstream's alias makes that a `duplicate field` parse error, so
        // there is no precedence to fall back on.
        let alias_value = field.alias.as_deref().and_then(|alias| obj.get(alias));
        if let Some(alias) = field.alias.as_deref()
            && obj.contains_key(field.name.as_str())
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
        match (obj.get(field.name.as_str()).or(alias_value), field.required) {
            (None, true) => errors.push(FieldError::new(
                format!("{path_prefix}.{}", field.name),
                "REQUIRED",
                format!(
                    "function '{function_name}' requires '{}' ({})",
                    field.name,
                    field.kind.as_str()
                ),
            )),
            (Some(v), _)
                if !field.kind.matches(v)
                    // A `Template` field's kind describes the *resolved* value.
                    // An object or array there may be an operator call, so only
                    // a scalar — unambiguously itself in JSONLogic — is still
                    // checked against the kind directly.
                    && !(field.is_template() && (v.is_object() || v.is_array()))
                    && !(field.resolvable && is_var_node(v))
                    && !takes_secret_node(field, v) =>
            {
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
fn labels() -> &'static RwLock<HashSet<&'static str>> {
    static LABELS: OnceLock<RwLock<HashSet<&'static str>>> = OnceLock::new();
    LABELS.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Record `name` as a label value, leaking it once.
fn intern(name: &str) {
    let labels = labels();
    if labels
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(name)
    {
        return;
    }
    let mut write = labels.write().unwrap_or_else(|e| e.into_inner());
    if !write.contains(name) {
        write.insert(Box::leak(name.to_string().into_boxed_str()));
    }
}

/// The `&'static str` a registered name interns to, or `None` for a name no
/// registry has held — the caller collapses that to one label value.
pub fn interned(name: &str) -> Option<&'static str> {
    labels()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .copied()
}

impl FunctionEntry {
    /// The `&'static str` this entry's name interns to — a metric label.
    ///
    /// Interned on first use for an entry no registry has held yet, so a
    /// handler built straight from a manifest (a test, a dry run) labels
    /// itself the same way one a generation carries does. Still bounded: one
    /// leak per distinct registered name.
    pub fn label(&self) -> &'static str {
        if let Some(label) = interned(&self.name) {
            return label;
        }
        intern(&self.name);
        interned(&self.name).expect("interned just above")
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
mod registry_tests {
    use super::*;
    use dataflow_rs::BuiltinKind;

    fn plugin_entry(name: &str) -> FunctionEntry {
        FunctionEntry {
            name: name.to_string(),
            description: "a plugin function".to_string(),
            category: "transform".to_string(),
            source: Source::Plugin,
            aliases: Vec::new(),
            input_fields: Some(vec![FieldSpec {
                name: "message".to_string(),
                description: "the message".to_string(),
                kind: FieldKind::String,
                required: true,
                resolvable: true,
                secret_at: &[],
                template_at: &[],
                alias: None,
            }]),
            writes: WriteShape::OutputPath {
                default_root: Some("data"),
            },
            retry_safety: RetrySafety::Pure,
            deny_unknown: true,
            validate_static: None,
            connector: None,
            plugin: Some(PluginBinding {
                id: "acme.codec".to_string(),
                version: 1,
                digest: "sha256:00".to_string(),
                abi: "orion:plugin@1.0.0".to_string(),
            }),
        }
    }

    /// Every self-contained dataflow-rs built-in is accepted.
    ///
    /// This used to build a probe engine, let it fail, and **string-parse the
    /// built-in list out of the `FunctionNotFound` Display impl** — because
    /// the crate kept `BUILTIN_FUNCTION_NAMES` `pub(crate)` and the error
    /// message was the only public surface that enumerated it. 3.1 publishes
    /// the const and a classifier, and documents that message as explicitly
    /// unpinned.
    #[test]
    fn every_self_contained_builtin_is_accepted() {
        let registry = FunctionRegistry::builtin();
        let mut checked = 0;
        for name in dataflow_rs::BUILTIN_FUNCTION_NAMES {
            if dataflow_rs::builtin_function_kind(name) == Some(BuiltinKind::SelfContained) {
                assert!(
                    registry.contains(name),
                    "'{name}' runs with no registration, so rejecting it at create \
                     refuses a workflow the engine would happily execute"
                );
                checked += 1;
            }
        }
        assert!(checked >= 8, "implausibly few self-contained built-ins");
    }

    /// A built-in that needs a handler is accepted only if Orion registered
    /// one — and `enrich` is the case that proves it matters.
    ///
    /// `enrich` deserializes into a typed built-in variant, so `Engine::new`
    /// accepts it and `check_custom_inputs` skips it by construction: it never
    /// becomes `FunctionConfig::Custom`. It was added to the old hand-copied
    /// name list to stop create rejecting it, which meant every `enrich`
    /// workflow activated cleanly and then failed *every* request with
    /// `FunctionNotFound`. Nothing registers a handler for it.
    #[test]
    fn a_builtin_needing_a_handler_is_accepted_only_when_one_is_registered() {
        let registry = FunctionRegistry::builtin();
        for name in dataflow_rs::BUILTIN_FUNCTION_NAMES {
            if dataflow_rs::builtin_function_kind(name) != Some(BuiltinKind::RequiresHandler) {
                continue;
            }
            assert_eq!(
                registry.contains(name),
                schema::registry().iter().any(|s| s.name == *name),
                "'{name}' needs a registered handler; accepting it without one \
                 green-lights a workflow that 500s on every request"
            );
        }
        assert!(registry.contains("http_call"));
        assert!(registry.contains("publish_kafka"));
        assert!(
            !registry.contains("enrich"),
            "Orion registers no `enrich` handler, so the name must be refused \
             at create rather than at every request"
        );
    }

    /// Every static row is an entry, with its schema, and an unknown name is
    /// not.
    #[test]
    fn every_orion_handler_is_an_entry_with_its_schema() {
        let registry = FunctionRegistry::builtin();
        for schema in schema::registry() {
            let entry = registry.get(schema.name);
            assert!(entry.is_some(), "'{}' has no entry", schema.name);
            let entry = entry.expect("asserted above");
            assert_eq!(entry.source, Source::Orion);
            assert_eq!(
                entry.input_fields.as_ref().map(Vec::len),
                Some(schema.input_fields.len()),
                "'{}' lost fields in conversion",
                schema.name
            );
            assert_eq!(entry.writes, schema.writes);
            assert_eq!(entry.retry_safety, schema.retry_safety);
            assert_eq!(entry.connector, schema.connector);
        }
        assert!(!registry.contains("__not_a_function__"));
        assert!(registry.get("__not_a_function__").is_none());
    }

    /// An alias resolves to its function and is not an entry of its own.
    #[test]
    fn an_alias_resolves_to_its_function() {
        let registry = FunctionRegistry::builtin();
        assert_eq!(
            registry.get("validate").map(|e| e.name.as_str()),
            Some("validation")
        );
        assert!(registry.names().all(|n| n != "validate"));
        assert!(registry.catalogue().iter().all(|e| e.name != "validate"));
    }

    #[test]
    fn the_catalogue_is_sorted_by_name() {
        let names: Vec<String> = FunctionRegistry::builtin()
            .catalogue()
            .into_iter()
            .map(|e| e.name)
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    /// The typos the `UNKNOWN_FUNCTION` message exists for: the suggestions
    /// must be the real, registered names they misspell.
    #[test]
    fn suggestion_recovers_common_typos() {
        let registry = FunctionRegistry::builtin();
        for (typo, expected) in [
            ("mongo_writes", "mongo_write"),
            ("jwt_verifiy", "jwt_verify"),
            ("cache_readd", "cache_read"),
        ] {
            assert_eq!(
                registry.suggest(typo),
                Some(expected),
                "'{typo}' should point at '{expected}'"
            );
        }
    }

    /// The window scales with name length, so garbage that is far from every
    /// registered name gets no suggestion rather than a wrong one —
    /// `http_request` is a plausible typo but seven edits from `http_call`.
    #[test]
    fn suggestion_is_silent_when_nothing_is_close() {
        let registry = FunctionRegistry::builtin();
        assert_eq!(registry.suggest("http_request"), None);
        assert_eq!(registry.suggest("no_such_function_xyz"), None);
        assert_eq!(registry.suggest("totally_unrelated"), None);
        assert_eq!(registry.suggest("x"), None);
    }

    /// Suggestions only ever name something the engine can actually run.
    #[test]
    fn suggestion_never_names_an_unknown_function() {
        let registry = FunctionRegistry::builtin();
        for typo in ["mongo_writes", "jwt_verifiy", "cache_readd"] {
            if let Some(candidate) = registry.suggest(typo) {
                assert!(
                    registry.contains(candidate),
                    "'{candidate}' is suggested for '{typo}' but is not itself registered"
                );
            }
        }
    }

    /// F52: which functions take a connector is one fact on the function's
    /// row now, so "declares a connector but no type" cannot be written. What
    /// can still go wrong is the row itself being dropped, so the set is
    /// pinned by name.
    #[test]
    fn the_connector_bearing_functions_are_exactly_these() {
        let registry = FunctionRegistry::builtin();
        let mut takes: Vec<&str> = registry
            .entries()
            .filter(|e| e.takes_connector())
            .map(|e| e.name.as_str())
            .collect();
        takes.sort_unstable();
        assert_eq!(
            takes,
            [
                "cache_read",
                "cache_write",
                "data_query",
                "data_write",
                "db_read",
                "db_write",
                "http_call",
                "mongo_aggregate",
                "mongo_read",
                "mongo_write",
                "publish_kafka",
                "send_email",
                "storage_head",
                "storage_presign",
            ]
        );
        for entry in registry.entries() {
            if let Some(rule) = entry.connector {
                assert!(
                    !rule.types.is_empty(),
                    "'{}' takes a connector but names no connector type, so activation \
                     cannot check it",
                    entry.name
                );
                assert!(
                    entry
                        .input_fields
                        .as_deref()
                        .is_some_and(|fields| { fields.iter().any(|f| f.name == "connector") }),
                    "'{}' declares a connector rule but no `connector` field",
                    entry.name
                );
            }
        }
        let mongo: Vec<&str> = registry
            .entries()
            .filter(|e| e.connector.is_some_and(|r| r.requires_mongo_database))
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(
            mongo,
            [
                "data_query",
                "data_write",
                "mongo_aggregate",
                "mongo_read",
                "mongo_write"
            ]
        );
    }

    /// An engine built-in declares nothing, so nothing folds and nothing is a
    /// secret path — treating it as permissive would make the authoring checks
    /// vacuous exactly where they cannot see.
    #[test]
    fn an_engine_builtin_declares_no_field_facts() {
        let registry = FunctionRegistry::builtin();
        assert!(!registry.is_resolvable_field("map", "mappings"));
        assert!(registry.secret_paths("map", "mappings").is_empty());
        assert!(registry.template_paths("map", "mappings").is_empty());
        assert!(
            registry
                .validate_input("map", &serde_json::json!(7), "tasks[0]")
                .is_empty()
        );
    }

    /// A name two entries claim is refused — every reader assumes a name means
    /// one function, and last-wins would have validation check one schema
    /// while the engine dispatches another.
    #[test]
    fn a_name_claimed_twice_is_refused() {
        let registry = FunctionRegistry::builtin();
        let err = registry
            .with_entries(vec![plugin_entry("crypto")])
            .expect_err("a plugin may not shadow an Orion handler");
        assert!(err.contains("'crypto'"), "{err}");
        assert!(err.contains("orion") && err.contains("plugin"), "{err}");

        let err = registry
            .with_entries(vec![plugin_entry("validate")])
            .expect_err("a plugin may not shadow an alias either");
        assert!(err.contains("'validate'"), "{err}");

        let err = registry
            .with_entries(vec![plugin_entry("acme.a"), plugin_entry("acme.a")])
            .expect_err("two plugin entries of one name");
        assert!(err.contains("'acme.a'"), "{err}");
    }

    /// The generation's registry: the built-ins plus what was loaded, with
    /// the plugin entry answering every question a built-in does.
    #[test]
    fn a_plugin_entry_is_an_ordinary_entry() {
        let registry = FunctionRegistry::builtin()
            .with_entries(vec![plugin_entry("acme.codec.parse")])
            .expect("extends");
        assert!(registry.contains("acme.codec.parse"));
        assert!(registry.contains("map") && registry.contains("crypto"));
        assert_eq!(
            registry.write_shape("acme.codec.parse"),
            Some(WriteShape::OutputPath {
                default_root: Some("data")
            })
        );
        assert!(registry.is_resolvable_field("acme.codec.parse", "message"));
        assert!(!registry.takes_connector("acme.codec.parse"));

        let errors = registry.validate_input(
            "acme.codec.parse",
            &serde_json::json!({"messag": "x"}),
            "tasks[0]",
        );
        let codes: Vec<&str> = errors.iter().map(|e| e.code.as_str()).collect();
        assert!(codes.contains(&"REQUIRED"), "{errors:?}");
        assert!(codes.contains(&"UNKNOWN_FIELD"), "{errors:?}");

        let catalogue = registry.catalogue();
        let entry = catalogue
            .iter()
            .find(|e| e.name == "acme.codec.parse")
            .expect("catalogued");
        assert_eq!(entry.source, Source::Plugin);
        let json = serde_json::to_value(entry).expect("serialises");
        assert_eq!(json["source"], "plugin");
        assert_eq!(json["plugin"]["id"], "acme.codec");
        assert_eq!(json["plugin"]["digest"], "sha256:00");
        assert_eq!(json["retry_safety"]["kind"], "pure");

        // And a built-in's wire shape is untouched: no `plugin` key appears.
        let map = catalogue.iter().find(|e| e.name == "map").expect("map");
        let json = serde_json::to_value(map).expect("serialises");
        assert!(json.get("plugin").is_none(), "{json}");
        assert!(json.get("input_fields").is_none(), "{json}");
    }

    /// The owned field serialises exactly as the static row does — the wire
    /// shape `docs/openapi.json` pins must not depend on where an entry came
    /// from.
    #[test]
    fn an_owned_field_serialises_like_the_static_row() {
        for schema in schema::registry() {
            for field in schema.input_fields {
                let from_row = serde_json::to_value(field).expect("row");
                let from_spec = serde_json::to_value(FieldSpec::from(field)).expect("spec");
                assert_eq!(from_row, from_spec, "{}.{}", schema.name, field.name);
            }
        }
    }

    /// Names are interned once, aliases included, and an unregistered name
    /// interns to nothing — the observer's cardinality bound.
    #[test]
    fn registered_names_intern_and_unregistered_ones_do_not() {
        let registry = FunctionRegistry::builtin();
        for name in registry.names() {
            assert_eq!(interned(name), Some(name), "'{name}' not interned");
        }
        assert_eq!(interned("validate"), Some("validate"));
        assert_eq!(interned("__never_registered__"), None);
        assert_eq!(registry.get("map").expect("map").label(), "map");
    }
}
#[cfg(test)]
mod write_shape_tests {
    use super::*;

    /// The guard this whole field exists for: a 19th handler cannot reach the
    /// authoring analysis with its output semantics unknown.
    ///
    /// Both tables are checked, because `task_writes` reads both — a built-in
    /// added upstream and mirrored here without a shape would be just as silent
    /// as a new Orion handler without one.
    #[test]
    fn every_function_declares_where_it_writes() {
        for schema in schema::registry() {
            assert!(
                write_shape(schema.name).is_some(),
                "function '{}' has no WriteShape",
                schema.name
            );
        }
        for (name, _, aliases, _, _) in schema::ENGINE_BUILTINS {
            assert!(
                write_shape(name).is_some(),
                "built-in '{name}' has no WriteShape"
            );
            for alias in *aliases {
                assert!(
                    write_shape(alias).is_some(),
                    "built-in alias '{alias}' has no WriteShape"
                );
            }
        }
    }

    /// A name neither table knows contributes no writes, rather than being run
    /// through the generic `output` rule. See `task_writes`.
    #[test]
    fn an_unknown_function_has_no_write_shape() {
        assert!(write_shape("no_such_function").is_none());
    }

    /// The three shapes the analysis distinguishes, pinned to the functions
    /// that motivated them.
    #[test]
    fn the_declared_shapes_match_the_handlers_they_describe() {
        assert_eq!(write_shape("map"), Some(WriteShape::Mappings));
        assert_eq!(write_shape("parse_json"), Some(WriteShape::Target));
        assert_eq!(write_shape("filter"), Some(WriteShape::Nothing));
        assert_eq!(
            write_shape("data_query"),
            Some(WriteShape::OutputPath {
                default_root: Some("data")
            }),
            "data_query defaults its output to the data root"
        );
        assert_eq!(
            write_shape("db_read"),
            Some(WriteShape::OutputPath { default_root: None })
        );
    }
}

#[cfg(test)]
mod resolvable_contract_tests {
    use super::*;

    /// §3.3: `FieldSchema::resolvable` is now the *only* declaration of which
    /// input fields fold `{"var": ..}` against the message.
    ///
    /// Four surfaces read it, and before this they could each be right about a
    /// different answer: the connector handlers decided per call site by
    /// calling a resolve helper or not,
    /// `validation::unresolvable_logic_warnings` warns about an expression in
    /// a field it believes literal, `stub.rs` folds the declared set when
    /// `dry-run` executes offline, and `analysis::operators` decides what a
    /// clippy rule can see through. Every resolve helper in
    /// `connector_helpers` now gates on this table, so the handler cannot be
    /// the one that disagrees.
    #[test]
    fn the_table_is_what_decides_whether_a_field_folds() {
        // Two fields of the same function, differing only in this flag.
        assert!(
            is_resolvable_field("db_read", "params"),
            "bind parameters are the request-controlled half of a statement"
        );
        assert!(
            !is_resolvable_field("db_read", "query"),
            "the SQL text is literal by design — it is what makes `params` the \
             *only* request-controlled part of the statement"
        );
        assert!(
            !is_resolvable_field("db_read", "connector"),
            "a connector name must not be chosen by the message"
        );

        // An unknown function declares nothing, so nothing folds — treating it
        // as permissive would make the gate vacuous exactly where it cannot
        // see.
        assert!(!is_resolvable_field("no_such_function", "params"));
    }

    /// The same non-resolvable string field is refused at authoring time, so
    /// the runtime gate is defence in depth rather than the only guard: a
    /// `{"var": ..}` node is an object, and `query` is declared a `String`.
    #[test]
    fn an_expression_in_a_literal_field_is_refused_at_create_time() {
        let errors = validate_input(
            "db_read",
            &serde_json::json!({
                "connector": "orders",
                "query": {"var": "data.req.sql"},
            }),
            "tasks[0]",
        );
        assert!(
            errors.iter().any(|e| e.path.contains("query")),
            "a message-derived `query` must be refused at authoring time: {errors:?}"
        );
    }

    /// And the resolvable twin is accepted in the same position, so the test
    /// above is about the flag and not about objects being refused generally.
    #[test]
    fn an_expression_in_a_resolvable_field_is_accepted_at_create_time() {
        let errors = validate_input(
            "db_read",
            &serde_json::json!({
                "connector": "orders",
                "query": "SELECT 1 WHERE id = $1",
                "params": [{"var": "data.req.id"}],
            }),
            "tasks[0]",
        );
        assert!(errors.is_empty(), "{errors:?}");
    }
}

#[cfg(test)]
mod validate_input_tests {
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

    /// A computed connector is refused, and refused *once*: the ordinary kind
    /// check no longer sees it (a template field's kind describes what it
    /// evaluates to), so the rule that needs it literal is the only reporter.
    #[test]
    fn a_computed_connector_is_refused_with_the_reason() {
        let errs = validate_input(
            "http_call",
            &json!({"connector": {"var": "data.which"}}),
            "tasks[0]",
        );
        let connector: Vec<_> = errs
            .iter()
            .filter(|e| e.path == "tasks[0].function.input.connector")
            .collect();
        assert_eq!(connector.len(), 1, "{errs:?}");
        assert_eq!(connector[0].code, "TYPE_MISMATCH");
        assert!(connector[0].message.contains("literal connector name"));
    }

    /// The other parameters of the same function stay computable — the limit is
    /// the connector, not the config.
    #[test]
    fn the_other_http_call_parameters_stay_computable() {
        let errs = validate_input(
            "http_call",
            &json!({
                "connector": "api",
                "path": {"cat": ["/o/", {"var": "data.id"}]},
                "timeout_ms": {"var": "data.t"},
                "headers": {"X": {"var": "data.h"}}
            }),
            "tasks[0]",
        );
        assert!(errs.is_empty(), "{errs:?}");
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

    /// One field carries both the literal and the computed spelling, so "name a
    /// target" is the field's own `required` rather than a cross-field rule
    /// over a pair — and the error points at the field instead of the input.
    #[test]
    fn channel_call_needs_a_channel() {
        let errs = validate_input("channel_call", &json!({}), "tasks[0]");
        assert!(errs.iter().any(|e| e.code == "REQUIRED"
            && e.path == "tasks[0].function.input.channel"
            && e.message.contains("channel_call")));
    }

    /// The pre-1.0 spelling is an alias of that field, so it satisfies it.
    #[test]
    fn the_pre_1_0_channel_logic_spelling_still_names_a_target() {
        let errs = validate_input(
            "channel_call",
            &json!({"channel_logic": {"var": "data.target"}}),
            "tasks[0]",
        );
        assert!(errs.is_empty(), "{errs:?}");
    }

    /// A computed channel is an expression, so its kind describes what it must
    /// evaluate to and the authored object is not checked against it.
    #[test]
    fn a_computed_channel_is_not_type_checked_against_string() {
        let errs = validate_input(
            "channel_call",
            &json!({"channel": {"cat": ["orders-", {"var": "data.region"}]}}),
            "tasks[0]",
        );
        assert!(errs.is_empty(), "{errs:?}");
    }

    /// A non-string scalar still is: it is unambiguously itself in JSONLogic,
    /// so it is a channel name that is not a string.
    #[test]
    fn a_scalar_channel_of_the_wrong_type_is_still_caught() {
        let errs = validate_input("channel_call", &json!({"channel": 7}), "tasks[0]");
        assert!(
            errs.iter()
                .any(|e| e.code == "TYPE_MISMATCH" && e.path.ends_with(".channel")),
            "{errs:?}"
        );
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
    fn http_call_unknown_format_values_are_authoring_time_errors() {
        let errs = validate_input(
            "http_call",
            &json!({"connector": "c", "body_format": "multipart", "response_format": "base64"}),
            "tasks[0]",
        );
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert_eq!(errs[0].path, "tasks[0].function.input.body_format");
        assert_eq!(errs[0].code, "INVALID");
        assert_eq!(errs[1].path, "tasks[0].function.input.response_format");
        assert_eq!(errs[1].code, "INVALID");
    }

    #[test]
    fn http_call_known_format_values_validate() {
        let errs = validate_input(
            "http_call",
            &json!({
                "connector": "c",
                "method": "POST",
                "body_format": "form",
                // Scalars, an array of scalars, a null, and a bracket-path
                // key — the full supported form surface.
                "body": {
                    "grant_type": "refresh_token",
                    "retries": 3,
                    "to": ["+15551111111", "+15552222222"],
                    "optional": null,
                    "metadata[order_id]": "6735",
                },
                "response_format": "text",
                "output": "temp_data.token",
            }),
            "tasks[0]",
        );
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn http_call_static_body_is_shape_checked_against_the_format() {
        // A nested value under 'form' is caught at authoring time by the same
        // encoder the request path runs.
        let errs = validate_input(
            "http_call",
            &json!({"connector": "c", "body_format": "form", "body": {"bad": {"nested": 1}}}),
            "tasks[0]",
        );
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].path, "tasks[0].function.input.body");
        assert_eq!(errs[0].code, "INVALID");
        assert!(errs[0].message.contains("'bad'"), "{}", errs[0].message);

        // 'text' requires a string body.
        let errs = validate_input(
            "http_call",
            &json!({"connector": "c", "body_format": "text", "body": {"a": 1}}),
            "tasks[0]",
        );
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].code, "INVALID");

        // A body_logic body only exists per message — nothing to check here.
        let errs = validate_input(
            "http_call",
            &json!({"connector": "c", "body_format": "form", "body_logic": {"var": "data.form"}}),
            "tasks[0]",
        );
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn registry_is_non_empty_and_contains_all_known_connector_functions() {
        let names: Vec<&str> = schema::registry().iter().map(|s| s.name).collect();
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

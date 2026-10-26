//! Authoring schemas derived from Orion's compiled function configs.
//!
//! Each handler owns an `OrionFunction::DEFINITION` beside its typed config.
//! The inventory in `functions/mod.rs` uses that declaration for both serving
//! registration and this catalogue. Field names, order, descriptions,
//! requiredness and output defaults come from `FunctionConfig::fields()`;
//! sparse overlays supply Orion's resolution, secret and semantic type rules.
//!
//! The formatter reads these schemas for field order. Other authoring tools,
//! validation and runtime discovery read `FunctionRegistry`, which combines
//! these schemas with the engine's built-ins and a generation's plugins.

use serde::Serialize;
use serde_json::Value;

use crate::connector::ConnectorType;

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

    pub(super) fn matches(self, v: &Value) -> bool {
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
    /// message context before use (see `connector_helpers::resolve_value`).
    /// Resolvable fields accept a `{"var": ..}` node in place of a literal of
    /// their declared `kind`; everything else — connector names, SQL text,
    /// output paths — stays literal by design.
    ///
    /// **The weaker of the two.** [`Self::template_at`] is the field that says
    /// dataflow-rs *evaluates* this position, which is what a scalar field
    /// carries now. What is left here is the document-shaped fields — a dialect
    /// `params`, JWT
    /// `claims`, a cached `value` — where evaluating would be a breaking
    /// change rather than a feature: one `$` comes off every key in a template
    /// position, so `{"$set": …}` would emit `{"set": …}` and every stored
    /// definition would need its prefixes doubled. A `{"var": ..}` fold has no
    /// such effect, so those fields keep it.
    ///
    /// The two are mutually exclusive on any one field: a field is either
    /// evaluated or folded, never both.
    pub resolvable: bool,
    /// Where **inside** this field the handler reads key material — a
    /// `{"secret": "name"}` node against the engine store, or an `env://` /
    /// `vault://` reference resolved at execution — as paths relative to the
    /// field itself.
    ///
    /// `&[]` (the default) means nowhere. `&[""]` means the field's own value
    /// is key material: `crypto.key`, `jwt_sign.key`, and `jwt_verify`'s
    /// `issuer` and `audience`. `&["[].key"]` means each array element's `key`
    /// member is, and nothing else in the field is: `jwt_verify.keys`.
    ///
    /// A path list rather than a `bool`, because the two questions a `bool`
    /// tried to answer at once have different answers for `keys`. May this
    /// field carry a `{"secret": …}` node in place of its declared kind?
    /// Only if `""` is listed — `keys` is an `Array` and must stay one, or
    /// the handler's `as_array()` reads `None` and silently verifies against
    /// no static key at all. And where must `UNRESOLVED_SECRET_REF` hold its
    /// fire? Only at these paths — `keys[].kid` and `keys[].key_encoding` are
    /// read verbatim, so an `env://` there is as stray as anywhere else.
    ///
    /// Everywhere unlisted, a `scheme://` string is a literal the handler
    /// sends on as-is — a URL spelled `env://API_BASE`, not the variable's
    /// value — which is why `validation::secret_reference_errors` refuses one
    /// outside these paths rather than letting it reach the backend.
    pub secret_at: &'static [&'static str],
    /// Where **inside** this field dataflow-rs evaluates JSONLogic — as paths
    /// relative to the field itself, the same spelling [`Self::secret_at`]
    /// uses and for the same reason a bool would not do.
    ///
    /// `&[]` (the default) means nowhere: the field is read as the literal it
    /// was authored as. `&[""]` means the field's *own* value is a `Template`
    /// — `http_call.path`. `&["*"]` means each member's
    /// value is one and the field itself is not: `http_call.headers` is a map
    /// of templates, so the map must still be an object while any value in it
    /// may be an expression.
    ///
    /// Distinct from [`Self::resolvable`], which is Orion's own `{"var": …}`
    /// folding in a *custom* handler's freeform input. This one is the engine's,
    /// on the typed configs dataflow-rs owns, where since 3.9 every parameter
    /// is JSONLogic.
    ///
    /// Two surfaces read it. `check_fields` stops treating [`Self::kind`] as a
    /// claim about the *authored* JSON here — the kind describes what the field
    /// must **evaluate to**, and an object or array may be an operator call, so
    /// only a scalar is still checked directly (a scalar is unambiguously
    /// itself in JSONLogic, which is the same line dataflow-rs's own
    /// `Template::uncompiled_literal` draws). And `analysis::operators::
    /// input_expressions` reports these positions to clippy, so a
    /// `{"var": "payload.x"}` in a request path counts as the read it is.
    pub template_at: &'static [&'static str],
}

impl FieldSchema {
    /// The neutral row every field table builds on: not required, not
    /// resolvable, not secret, not templated.
    ///
    /// Used when converting config fields and by the nested mutation-envelope
    /// schema. Function-specific metadata lives in `FieldOverlay` instead.
    pub const DEFAULT: Self = FieldSchema {
        name: "",
        description: "",
        kind: FieldKind::Any,
        required: false,
        resolvable: false,
        secret_at: &[],
        template_at: &[],
    };
}

/// A function's cross-field authoring-time validator: `(path-suffix, code,
/// message)` triples over a static input object; an empty suffix addresses
/// the input object itself. Each one lives next to its handler (conventionally
/// named `validate_static_input`) so the rules it applies are the execution
/// path's own tables.
pub type StaticValidator =
    fn(&serde_json::Map<String, Value>) -> Vec<(&'static str, &'static str, String)>;

/// Where a task's output lands in the message context.
///
/// `definitions::analysis::dataflow::task_writes` used to answer this with a
/// hand-written `match` over function names — a mirror of every handler's
/// output semantics, kept in a different file from the handlers, pinned by no
/// test. That is the exact drift class the rest of this repo turns into build
/// failures, so the answer moves next to the function that owns it and the
/// analysis reads it from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum WriteShape {
    /// The dotted path in `output` (or its pre-1.0 spelling `response_path`).
    /// `default_root` comes from the primary output's config default. `None`
    /// means an omitted optional output writes nothing. A function with no
    /// output field is [`Self::Nothing`].
    OutputPath { default_root: Option<&'static str> },
    /// `data.{target}` — the engine's parse/publish built-ins, whose `target`
    /// is a key under `data` rather than a full path.
    Target,
    /// Each `mappings[].path`, which are already full paths.
    Mappings,
    /// Writes nothing into the context.
    Nothing,
}

/// Whether running a task twice can do its work twice.
///
/// Not the same question as [`ErrorClass::is_retryable`], which asks whether an
/// *error* was transient. This asks what a retry costs when the answer is yes:
/// a `db_read` re-run is free, a `db_write` insert re-run is a second row. Orion
/// retries tasks in more places than an author necessarily has in mind — the
/// DLQ retry loop, a cron occurrence's retry, `http_call`'s own transport
/// retry — and nothing told them which functions those retries
/// are safe over.
///
/// The connector layer already knows this shape for one case:
/// `HttpConnectorConfig::retry_non_idempotent` exists because *"a timed-out
/// POST may already have been applied"*. This is that fact, per function, where
/// an author and their tooling can read it.
///
/// [`ErrorClass::is_retryable`]: crate::ErrorClass::is_retryable
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetrySafety {
    /// No external effect at all: the answer is a function of the input, and
    /// nothing outside the message changes. Free to retry.
    Pure,
    /// Reads state without changing it. A retry costs another round trip and
    /// may observe a newer value, but repeats no work.
    Read,
    /// Writes, but a second run lands the same end state as the first.
    IdempotentWrite,
    /// Writes, and a second run duplicates the effect. Retrying sends the
    /// second email, publishes the second record.
    UnsafeWrite,
    /// The answer is the task's own, and this is the input that decides it.
    ///
    /// The honest variant. A third of the set genuinely has no fixed answer —
    /// `data_write` with `op: "upsert"` is idempotent and with `op: "insert"`
    /// is not; `http_call` is a `GET` or a `POST`. Collapsing those to a
    /// boolean would state something false about half of every deployment's
    /// workflows, so the field names the input to go and look at instead.
    DependsOn { input: &'static str },
}

impl RetrySafety {
    /// The value `/admin/functions` and the reference table both spell.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::Read => "read",
            Self::IdempotentWrite => "idempotent_write",
            Self::UnsafeWrite => "unsafe_write",
            Self::DependsOn { .. } => "depends_on",
        }
    }
}

/// What a connector-bearing function requires of the connector it names.
///
/// Two questions used to be answered by two tables in `handlers.rs` —
/// `CONNECTOR_FUNCTIONS` (does it take one?) and `required_connector_types`
/// (of which types?) — each keyed by function name in a file away from the
/// schema that declares the `connector` field. A function in one table and not
/// another was a check that silently skipped it (F52). One value on the
/// function's own row cannot be half-filled.
///
/// `types` is non-empty and ordered as it should read in an error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectorRule {
    pub types: &'static [ConnectorType],
}

impl ConnectorRule {
    pub(crate) const fn of(types: &'static [ConnectorType]) -> Option<Self> {
        if types.is_empty() {
            None
        } else {
            Some(Self { types })
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSchema {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub input_fields: Vec<FieldSchema>,
    /// Where this function's output lands. Read by the authoring analysis, so
    /// a new handler cannot reach the clippy rules with its writes unknown —
    /// `every_function_declares_where_it_writes` refuses one that tries.
    pub writes: WriteShape,
    /// What a second run of this task does — see [`RetrySafety`]. Declared
    /// per function rather than derived, because nothing in the input schema
    /// implies it: `db_read` and `db_write` have the same shape and opposite
    /// answers.
    pub retry_safety: RetrySafety,
    /// The connector this function's `connector` field must name, or `None`
    /// for a function that takes no connector — see [`ConnectorRule`].
    #[serde(skip)]
    pub connector: Option<ConnectorRule>,
    /// Whether a key outside `input_fields` is an error rather than ignored.
    ///
    /// This is the authoring validator's policy. The compiled configs also
    /// refuse unknown fields through dataflow-rs, including likely typos.
    pub deny_unknown: bool,
    /// Cross-field rules beyond the per-field table (op × algorithm tables,
    /// key-source rules, stage allowlists, …), registered here so
    /// `validate_input` dispatches them from the same table that declares the
    /// function — a new function's rules are one field, never another
    /// hand-copied block.
    #[serde(skip)]
    pub validate_static: Option<StaticValidator>,
    #[serde(skip)]
    pub(crate) definition: &'static super::definition::Definition,
}

/// Every Orion function, generated from the same inventory that registers it.
pub fn registry() -> &'static [FunctionSchema] {
    static SCHEMAS: std::sync::LazyLock<Vec<FunctionSchema>> =
        std::sync::LazyLock::new(super::schemas);
    &SCHEMAS
}

// ============================================================
// The engine's built-ins, and who provides what
// ============================================================

/// Who provides a function's behaviour.
///
/// The discriminator that tells a consumer *why* an entry has no
/// `input_fields`: dataflow-rs contributes the function and executes it
/// itself, so Orion has no schema to declare for it and does not
/// input-validate it at create time.
///
/// It correlates exactly with schema presence today. It is kept separate
/// because it need not: nothing stops Orion declaring a schema for `map`
/// later, and a consumer branching on "is this validated" should read
/// `input_fields`, while one branching on "whose function is this" should
/// read `source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A dataflow-rs built-in, executed by the engine.
    Engine,
    /// An Orion handler, with a declared input schema.
    Orion,
    /// A function loaded from a plugin: a declared input schema, executed in
    /// the plugin sandbox. Absent from the static tables by construction — it
    /// enters the registry from a loaded plugin set, never from this file.
    Plugin,
}

impl Source {
    /// The wire spelling, for messages that name where an entry came from.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Orion => "orion",
            Self::Plugin => "plugin",
        }
    }
}

/// The dataflow-rs built-ins: valid in a workflow, executed by the engine,
/// with no Orion-declared input schema.
///
/// `(name, description, writes, retry_safety)` — the four things that vary.
/// Every
/// entry is `category: "data"` (the fourth wire value, matching the grouping
/// `reference/functions/index.md` already gives these in its summary table),
/// `source: Engine`, and no input schema, so the registry supplies those
/// rather than each row restating them.
/// Descriptions are the code's, and `functions_docs_drift_test` checks the
/// page against them rather than the reverse.
///
/// `writes` is here for the same reason it is on [`FunctionSchema`]: the
/// authoring analysis has to know where a built-in puts its output, and the
/// only defensible place to say so is beside the row that declares the
/// built-in. `retry_safety` is here on the same argument, and stated per row
/// rather than supplied wholesale by the registry even though all eight are
/// currently [`RetrySafety::Pure`] — a built-in that one day reaches outside
/// the message must be made to answer, not inherit a default that was true of
/// its neighbours. (`log` writes only to this node's own observability output.
/// A retry repeating a log line is not a duplicated effect in the sense this
/// field is about.)
pub(super) const ENGINE_BUILTINS: &[(&str, &str, WriteShape, RetrySafety)] = &[
    (
        "parse_json",
        "Parse the raw payload into the data context.",
        WriteShape::Target,
        RetrySafety::Pure,
    ),
    (
        "parse_xml",
        "Parse an XML payload into the data context.",
        WriteShape::Target,
        RetrySafety::Pure,
    ),
    (
        "map",
        "Transform and reshape data with JSONLogic mappings.",
        WriteShape::Mappings,
        RetrySafety::Pure,
    ),
    (
        "filter",
        "Gate the pipeline on a JSONLogic condition.",
        WriteShape::Nothing,
        RetrySafety::Pure,
    ),
    (
        "validation",
        "Collect validation errors from JSONLogic rules.",
        WriteShape::Nothing,
        RetrySafety::Pure,
    ),
    (
        "log",
        "Emit a structured log line.",
        WriteShape::Nothing,
        RetrySafety::Pure,
    ),
    (
        "publish_json",
        "Serialize a context field to a JSON string.",
        WriteShape::Target,
        RetrySafety::Pure,
    ),
    (
        "publish_xml",
        "Serialize a context field to an XML string.",
        WriteShape::Target,
        RetrySafety::Pure,
    ),
];

/// The `&'static str` spelling of `key` in `fields` — for the
/// `validate_static_input` tuples, whose path suffixes must be static.
/// `fallback` covers keys outside the table (unreachable for real inputs,
/// merely safe for arbitrary ones).
pub(super) fn static_field_name(
    fields: &'static [dataflow_rs::expr::FieldSpec],
    key: &str,
    fallback: &'static str,
) -> &'static str {
    fields
        .iter()
        .map(|f| f.name.as_ref())
        .find(|n| *n == key)
        .unwrap_or(fallback)
}

/// A field kind as a plugin manifest spells it (`string`, `number`, `bool`,
/// `object`, `array`, `any`).
impl<'de> serde::Deserialize<'de> for FieldKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <String as serde::Deserialize>::deserialize(d)?;
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

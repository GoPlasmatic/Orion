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

use dataflow_rs::engine::error::DataflowError;
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
    /// message context before use (see `connector_helpers::resolve_value`).
    /// Resolvable fields accept a `{"var": ..}` node in place of a literal of
    /// their declared `kind`; everything else — connector names, SQL text,
    /// output paths — stays literal by design.
    ///
    /// **The weaker of the two.** [`Self::template_at`] is the field that says
    /// dataflow-rs *evaluates* this position, which is what a scalar field
    /// carries now. What is left here is the document-shaped fields — a MongoDB
    /// `filter`/`update`/`document`/`pipeline`, a dialect `params`, JWT
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
    /// — `http_call.path`, `publish_kafka.topic`. `&["*"]` means each member's
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

impl FieldSchema {
    /// The neutral row every field table builds on: not required, not
    /// resolvable, not secret, not templated, no alias.
    ///
    /// The tables are `const` slices, so without this every field on this
    /// struct has to be spelled at all ~137 sites — and adding one costs a
    /// mechanical diff long enough to hide the handful of rows where the new
    /// value is not the default. With it, a row states only what is true of
    /// it: `FieldSchema { name: "key", …, secret_at: &[""], ..FieldSchema::DEFAULT }`.
    ///
    /// `name`, `description` and `kind` have no meaningful default; every row
    /// spells them.
    pub const DEFAULT: Self = FieldSchema {
        name: "",
        description: "",
        kind: FieldKind::Any,
        required: false,
        resolvable: false,
        secret_at: &[],
        template_at: &[],
        alias: None,
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
    /// `default_root` is what the write lands on when neither is given — `None`
    /// for a function that writes nothing without being told where.
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
/// a `db_read` re-run is free, a `send_email` re-run is a second email. Orion
/// retries tasks in more places than an author necessarily has in mind — the
/// DLQ retry loop, a Kafka redelivery of an uncommitted offset, `http_call`'s
/// own transport retry — and nothing told them which functions those retries
/// are safe over.
///
/// The connector layer already knows this shape for one case:
/// `HttpConnectorConfig::retry_non_idempotent` exists because *"a timed-out
/// POST may already have been applied"*. This is that fact, per function, where
/// an author and their tooling can read it.
///
/// [`ErrorClass::is_retryable`]: crate::engine::ErrorClass::is_retryable
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

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSchema {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub input_fields: &'static [FieldSchema],
    /// Where this function's output lands. Read by the authoring analysis, so
    /// a new handler cannot reach the clippy rules with its writes unknown —
    /// `every_function_declares_where_it_writes` refuses one that tries.
    pub writes: WriteShape,
    /// What a second run of this task does — see [`RetrySafety`]. Declared
    /// per function rather than derived, because nothing in the input schema
    /// implies it: `db_read` and `db_write` have the same shape and opposite
    /// answers.
    pub retry_safety: RetrySafety,
    /// Whether a key outside `input_fields` is an error rather than ignored.
    ///
    /// True for the functions dataflow-rs owns the config struct for
    /// (`http_call`, `publish_kafka`), whose structs are `deny_unknown_fields`
    /// as of 3.1: a misspelled key there fails `Workflow::from_json`, which for
    /// Orion means the channel is quarantined at load. Catching it at authoring
    /// time turns that into a 400 naming the field. Orion's own handlers take
    /// freeform `serde_json::Value` inputs and keep ignoring extra keys.
    pub deny_unknown: bool,
    /// Cross-field rules beyond the per-field table (op × algorithm tables,
    /// key-source rules, stage allowlists, …), registered here so
    /// `validate_input` dispatches them from the same table that declares the
    /// function — a new function's rules are one field, never another
    /// hand-copied block.
    #[serde(skip)]
    pub validate_static: Option<StaticValidator>,
}

// F53: each function's field table lives in the module implementing it, so a
// handler and the schema describing it are edited in one place. Every
// schema/handler divergence this audit found — F23's `channel_call` input, the
// `method` casing, the Mongo `database` rule — was a table that drifted because
// it was in a different file from the code it described.
use super::cache_read::CACHE_READ_FIELDS;
use super::cache_write::CACHE_WRITE_FIELDS;
use super::channel_call::CHANNEL_CALL_FIELDS;
use super::crypto::CRYPTO_FIELDS;
use super::data_query::DATA_QUERY_FIELDS;
use super::data_write::{DATA_WRITE_ENVELOPE_FIELDS, DATA_WRITE_FIELDS};
use super::db_read::DB_READ_FIELDS;
use super::db_write::DB_WRITE_FIELDS;
use super::http_call::HTTP_CALL_FIELDS;
use super::jwt_sign::JWT_SIGN_FIELDS;
use super::jwt_verify::JWT_VERIFY_FIELDS;
use super::mongo_aggregate::MONGO_AGGREGATE_FIELDS;
use super::mongo_read::MONGO_READ_FIELDS;
use super::mongo_write::MONGO_WRITE_FIELDS;
use super::publish_kafka::PUBLISH_KAFKA_FIELDS;
use super::send_email::SEND_EMAIL_FIELDS;
use super::storage_head::STORAGE_HEAD_FIELDS;
use super::storage_presign::STORAGE_PRESIGN_FIELDS;

const REGISTRY: &[FunctionSchema] = &[
    FunctionSchema {
        name: "cache_read",
        description: "Read a value from a cache connector (Redis or in-memory).",
        category: "connector",
        input_fields: CACHE_READ_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::Read,
        deny_unknown: false,
        validate_static: None,
    },
    FunctionSchema {
        name: "cache_write",
        description: "Write a value to a cache connector.",
        category: "connector",
        input_fields: CACHE_WRITE_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::IdempotentWrite,
        deny_unknown: false,
        validate_static: None,
    },
    FunctionSchema {
        name: "db_read",
        description: "Execute a SELECT against a SQL connector.",
        category: "connector",
        input_fields: DB_READ_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::Read,
        deny_unknown: false,
        validate_static: None,
    },
    FunctionSchema {
        name: "db_write",
        description: "Execute INSERT/UPDATE/DELETE against a SQL connector.",
        category: "connector",
        input_fields: DB_WRITE_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::DependsOn { input: "sql" },
        deny_unknown: false,
        validate_static: None,
    },
    FunctionSchema {
        name: "data_query",
        description: "Run a backend-neutral query (filter + envelope) against a SQL, MongoDB, or Elasticsearch connector.",
        category: "connector",
        input_fields: DATA_QUERY_FIELDS,
        writes: WriteShape::OutputPath {
            default_root: Some("data"),
        },
        retry_safety: RetrySafety::Read,
        deny_unknown: false,
        validate_static: None,
    },
    FunctionSchema {
        name: "data_write",
        description: "Run a backend-neutral mutation (insert/update/delete/upsert) against a SQL, MongoDB, or Elasticsearch connector.",
        category: "connector",
        input_fields: DATA_WRITE_FIELDS,
        writes: WriteShape::OutputPath {
            default_root: Some("data"),
        },
        retry_safety: RetrySafety::DependsOn { input: "op" },
        deny_unknown: false,
        validate_static: None,
    },
    FunctionSchema {
        name: "mongo_read",
        description: "Run find() against a MongoDB connector, with optional projection/sort/limit/skip.",
        category: "connector",
        input_fields: MONGO_READ_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::Read,
        deny_unknown: false,
        validate_static: None,
    },
    FunctionSchema {
        name: "mongo_write",
        description: "Write documents to a MongoDB connector: insert/update/replace/delete, nested documents as extended JSON.",
        category: "connector",
        // Strict: a typoed `upsert` or `ordered` silently changes what a
        // write does — the crypto/send_email rationale exactly.
        input_fields: MONGO_WRITE_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::DependsOn { input: "op" },
        deny_unknown: true,
        validate_static: Some(super::mongo_write::validate_static_input),
    },
    FunctionSchema {
        name: "mongo_aggregate",
        description: "Run an aggregation pipeline against a MongoDB connector (stage-allowlisted; $out/$merge behind a connector opt-in).",
        category: "connector",
        input_fields: MONGO_AGGREGATE_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::Read,
        deny_unknown: true,
        validate_static: Some(super::mongo_aggregate::validate_static_input),
    },
    FunctionSchema {
        name: "channel_call",
        description: "Invoke another channel's workflow in-process (no HTTP hop).",
        category: "control",
        input_fields: CHANNEL_CALL_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::DependsOn { input: "channel" },
        deny_unknown: false,
        validate_static: None,
    },
    FunctionSchema {
        name: "crypto",
        description: "Digests, HMAC compute/verify, and password hashing — a self-contained operation envelope.",
        category: "utility",
        // The handler itself tolerates extra keys, but strictness matters
        // more here than anywhere: a typoed field on a crypto op would
        // silently mean "use the default".
        input_fields: CRYPTO_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::Pure,
        deny_unknown: true,
        validate_static: Some(super::crypto::validate_static_input),
    },
    FunctionSchema {
        name: "jwt_sign",
        description: "Mint a signed JWT (login, refresh, client assertions).",
        category: "utility",
        input_fields: JWT_SIGN_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::Pure,
        deny_unknown: true,
        validate_static: Some(super::jwt_sign::validate_static_input),
    },
    FunctionSchema {
        name: "jwt_verify",
        description: "Verify a JWT mid-workflow (provider id_tokens, refresh tokens) against static keys or a JWKS.",
        category: "utility",
        input_fields: JWT_VERIFY_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::Read,
        deny_unknown: true,
        validate_static: Some(super::jwt_verify::validate_static_input),
    },
    FunctionSchema {
        name: "http_call",
        description: "HTTP request to an HTTP connector with retry + circuit breaker.",
        category: "connector",
        input_fields: HTTP_CALL_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::DependsOn { input: "method" },
        deny_unknown: true,
        validate_static: None,
    },
    FunctionSchema {
        name: "send_email",
        description: "Send an email through an SMTP connector.",
        category: "connector",
        // Same rationale as crypto: a typoed field on an email (a lost `bcc`,
        // a misspelled `reply_to`) silently changes who gets what.
        input_fields: SEND_EMAIL_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::UnsafeWrite,
        deny_unknown: true,
        validate_static: Some(super::send_email::validate_static_input),
    },
    FunctionSchema {
        name: "storage_presign",
        description: "Compute a time-limited presigned URL for one object — no data path.",
        category: "connector",
        input_fields: STORAGE_PRESIGN_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::Pure,
        deny_unknown: true,
        validate_static: Some(super::storage_presign::validate_static_input),
    },
    FunctionSchema {
        name: "storage_head",
        description: "Object metadata (exists/size/etag) from a storage connector.",
        category: "connector",
        input_fields: STORAGE_HEAD_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::Read,
        deny_unknown: true,
        validate_static: None,
    },
    FunctionSchema {
        name: "publish_kafka",
        description: "Publish a message to a Kafka topic via a Kafka connector.",
        category: "connector",
        input_fields: PUBLISH_KAFKA_FIELDS,
        writes: WriteShape::OutputPath { default_root: None },
        retry_safety: RetrySafety::UnsafeWrite,
        deny_unknown: true,
        validate_static: None,
    },
];

/// Every function that has an input schema. Accepted function names
/// without an entry here (e.g. `map`, `log`, `filter`) are still accepted
/// by workflows — they just won't get input-schema checking.
pub fn registry() -> &'static [FunctionSchema] {
    REGISTRY
}

// ============================================================
// The catalogue: every name a workflow may use
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
}

/// One entry of `GET /api/v1/admin/functions`.
///
/// The endpoint used to serve the `REGISTRY` directly, which meant it listed only
/// the functions Orion input-schema validates — 18 of the 27 valid names,
/// omitting `map`, `filter`, `parse_json` and the rest. Those are the ones
/// people actually type: in the deployment that reported it (#288) the nine
/// omitted names were 425 of 631 tasks, `map` alone 310. A completion source
/// offering the connector functions and none of those is not an incomplete
/// catalogue, it is the wrong one.
///
/// So the catalogue is the union, and the schema registry stays what it was.
/// Two lists rather than one overloaded list: `validate_input` and
/// `is_resolvable_field` ask "what does this function declare", which is still
/// the `REGISTRY`, and only the endpoint and the docs guard ask "what may a
/// workflow name".
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub source: Source,
    /// Other accepted spellings of this name. Serving an alias as its own
    /// entry would tell a completion tool there are two functions.
    #[serde(skip_serializing_if = "<[&str]>::is_empty")]
    pub aliases: &'static [&'static str],
    /// **Absent**, not null, when the function declares no input schema —
    /// which is the honest JSON encoding of "there is nothing here", and what
    /// a consumer branches on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_fields: Option<&'static [FieldSchema]>,
    /// What a second run of this function does — see [`RetrySafety`]. Served
    /// for every entry, built-ins included: "is it safe to retry this task?"
    /// is a question about every function a workflow can name, not only the
    /// ones Orion declares an input schema for.
    pub retry_safety: RetrySafety,
}

/// The dataflow-rs built-ins: valid in a workflow, executed by the engine,
/// with no Orion-declared input schema.
///
/// `(name, description, aliases, writes, retry_safety)` — the five things that
/// vary. Every
/// entry is `category: "data"` (the fourth wire value, matching the grouping
/// `reference/functions.md` already gives these in its summary table),
/// `source: Engine`, and no input schema, so [`catalogue`] supplies those
/// rather than each row restating them.
/// Descriptions are the code's, and `functions_docs_drift_test` checks the
/// page against them rather than the reverse.
///
/// `writes` is here for the same reason it is on [`FunctionSchema`]: the
/// authoring analysis has to know where a built-in puts its output, and the
/// only defensible place to say so is beside the row that declares the
/// built-in. `retry_safety` is here on the same argument, and stated per row
/// rather than supplied wholesale by [`catalogue`] even though all eight are
/// currently [`RetrySafety::Pure`] — a built-in that one day reaches outside
/// the message must be made to answer, not inherit a default that was true of
/// its neighbours. (`log` writes only to this node's own observability output.
/// A retry repeating a log line is not a duplicated effect in the sense this
/// field is about.)
const ENGINE_BUILTINS: &[(&str, &str, &[&str], WriteShape, RetrySafety)] = &[
    (
        "parse_json",
        "Parse the raw payload into the data context.",
        &[],
        WriteShape::Target,
        RetrySafety::Pure,
    ),
    (
        "parse_xml",
        "Parse an XML payload into the data context.",
        &[],
        WriteShape::Target,
        RetrySafety::Pure,
    ),
    (
        "map",
        "Transform and reshape data with JSONLogic mappings.",
        &[],
        WriteShape::Mappings,
        RetrySafety::Pure,
    ),
    (
        "filter",
        "Gate the pipeline on a JSONLogic condition.",
        &[],
        WriteShape::Nothing,
        RetrySafety::Pure,
    ),
    (
        "validation",
        "Collect validation errors from JSONLogic rules.",
        // Upstream accepts both spellings; they are one function.
        &["validate"],
        WriteShape::Nothing,
        RetrySafety::Pure,
    ),
    (
        "log",
        "Emit a structured log line.",
        &[],
        WriteShape::Nothing,
        RetrySafety::Pure,
    ),
    (
        "publish_json",
        "Serialize a context field to a JSON string.",
        &[],
        WriteShape::Target,
        RetrySafety::Pure,
    ),
    (
        "publish_xml",
        "Serialize a context field to an XML string.",
        &[],
        WriteShape::Target,
        RetrySafety::Pure,
    ),
];

/// Where `function` writes its output, for any function a workflow may name —
/// Orion's handlers and the engine's built-ins alike.
///
/// `None` for a name neither table knows, which is a function that does not
/// exist: the analysis then reports no writes rather than guessing a shape for
/// it. That is a deliberate change from the previous hand-written `match`,
/// whose catch-all arm applied the `output`/`response_path` rule to *any*
/// unrecognised name — so a typoed function silently contributed a write.
pub fn write_shape(function: &str) -> Option<WriteShape> {
    if let Some(schema) = REGISTRY.iter().find(|s| s.name == function) {
        return Some(schema.writes);
    }
    ENGINE_BUILTINS
        .iter()
        .find(|(name, _, aliases, _, _)| *name == function || aliases.contains(&function))
        .map(|(_, _, _, writes, _)| *writes)
}

/// Every function a workflow may name, sorted by name.
///
/// Sorted because a catalogue is browsed: the registry's own order groups by
/// implementation concern, which is not what a reader or a completion list
/// wants.
pub fn catalogue() -> Vec<CatalogueEntry> {
    let mut out: Vec<CatalogueEntry> = REGISTRY
        .iter()
        .map(|schema| CatalogueEntry {
            name: schema.name,
            description: schema.description,
            category: schema.category,
            source: Source::Orion,
            aliases: &[],
            input_fields: Some(schema.input_fields),
            retry_safety: schema.retry_safety,
        })
        .chain(ENGINE_BUILTINS.iter().map(
            |&(name, description, aliases, _writes, retry_safety)| CatalogueEntry {
                name,
                description,
                category: "data",
                source: Source::Engine,
                aliases,
                input_fields: None,
                retry_safety,
            },
        ))
        .collect();
    out.sort_by_key(|e| e.name);
    out
}

fn find(name: &str) -> Option<&'static FunctionSchema> {
    REGISTRY.iter().find(|s| s.name == name)
}

/// Whether `field` is one this function folds `{"var": ..}` nodes in before
/// use — i.e. whether the value the handler acts on differs from the value the
/// author wrote.
///
/// The offline call recorder reads this to resolve a task's payload the same
/// way the real handler will, so a recorded call shows what *would be sent*
/// rather than what was typed. Driving it off the registry rather than a
/// per-function list is what makes a new connector function's calls recordable
/// as soon as it fills in the field table it already has to fill in.
pub fn is_resolvable_field(function_name: &str, field: &str) -> bool {
    find(function_name).is_some_and(|schema| {
        schema
            .input_fields
            .iter()
            .any(|f| f.resolvable && (f.name == field || f.alias == Some(field)))
    })
}

/// Where inside `field` this function reads key material — the only paths
/// where `env://NAME` or `vault://…` means anything other than itself.
///
/// Driven off the registry rather than a per-function list for the same reason
/// [`is_resolvable_field`] is: a function that starts resolving references in a
/// new field declares it in the field table it already maintains, and the
/// authoring-time check follows automatically.
///
/// A function with no declared schema (an engine built-in) answers `&[]`: none
/// of them resolves a reference, and treating an unknown function as permissive
/// would make the check silently vacuous for the one case it cannot see into.
pub fn secret_paths(function_name: &str, field: &str) -> &'static [&'static str] {
    find(function_name)
        .and_then(|schema| {
            schema
                .input_fields
                .iter()
                .find(|f| f.name == field || f.alias == Some(field))
        })
        .map(|f| f.secret_at)
        .unwrap_or(&[])
}

/// Where inside `field` dataflow-rs evaluates JSONLogic — see
/// [`FieldSchema::template_at`]. Driven off the registry for the same reason
/// [`secret_paths`] is: the handler's own field table is the one declaration.
pub fn template_paths(function_name: &str, field: &str) -> &'static [&'static str] {
    find(function_name)
        .and_then(|schema| {
            schema
                .input_fields
                .iter()
                .find(|f| f.name == field || f.alias == Some(field))
        })
        .map(|f| f.template_at)
        .unwrap_or(&[])
}

/// Whether the field's own value is a `Template`, so its authored JSON may be
/// an expression rather than a literal of its declared kind.
fn is_template_field(field: &FieldSchema) -> bool {
    field.template_at.contains(&"")
}

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
fn takes_secret_node(field: &FieldSchema, v: &Value) -> bool {
    field.secret_at.contains(&"") && super::secret_ref::secret_name(v).is_some()
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
            (Some(v), _)
                if !field.kind.matches(v)
                    // A `Template` field's kind describes the *resolved* value.
                    // An object or array there may be an operator call, so only
                    // a scalar — unambiguously itself in JSONLogic — is still
                    // checked against the kind directly.
                    && !(is_template_field(field) && (v.is_object() || v.is_array()))
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

    // A connector must be named, not computed.
    //
    // dataflow-rs 3.9 made `http_call`/`publish_kafka`'s `connector` a
    // `Template` like every other parameter, so the kind check above no longer
    // refuses an object there — a template field's kind describes what it
    // evaluates to. Orion needs this one to fold to a name it can read *without*
    // a message, and not because the handler is lazy: the connector is looked up
    // before the message is consulted (F58), and the same static name is what
    // `GET /workflows/{id}/dependencies` reports, what the activation gate
    // checks exists, what refuses a rename or delete of a connector still in
    // use, and what a package's `requires` list is built from. A computed name
    // is invisible to all five, so admitting one means teaching all five, not
    // relaxing one check.
    //
    // Driven off the schema rather than a function list, so a connector handler
    // added later inherits the rule with the field table it already fills in.
    // A string is the test upstream's own `ConnectorName` uses to answer
    // `Static` vs `Computed`, so the two agree by construction.
    //
    // Exactly the complement of what `check_fields` still checks: it reports a
    // *scalar* of the wrong type itself, so this fires only for the object and
    // array it now waves through, and one wrong connector is one error.
    if let Some(field) = schema.input_fields.iter().find(|f| f.name == "connector")
        && is_template_field(field)
        && let Some(value) = obj.get(field.name)
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

    // Cross-field rules registered on the schema entry — each lives next to
    // its handler as `validate_static_input` and shares the execution path's
    // tables (#263 and friends), so the authoring-time rules and the runtime
    // cannot drift, and a new function's rules are one registry field.
    if let Some(validate) = schema.validate_static {
        for (suffix, code, message) in validate(obj) {
            let path = if suffix.is_empty() {
                input_path.clone()
            } else {
                format!("{input_path}.{suffix}")
            };
            errors.push(FieldError::new(path, code, message));
        }
    }

    // Cross-field: http_call's format axes. dataflow-rs carries `body_format`
    // and `response_format` as uninterpreted strings, so the value table is
    // enforced here — an unknown value is an authoring-time error, never a
    // request-time surprise. A *static* `body` is shape-checked against the
    // format too, by the same `encode_body` the request path runs, so the two
    // layers cannot drift; a `body_logic` body only exists per message and
    // gets that check at request time.
    if function_name == "http_call" {
        use super::http_common::{BodyFormat, ResponseFormat, encode_body};

        // A non-string value is already a TYPE_MISMATCH from the field loop.
        let body_format = match BodyFormat::parse(obj.get("body_format").and_then(Value::as_str)) {
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
        if let Err(msg) = ResponseFormat::parse(obj.get("response_format").and_then(Value::as_str))
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

/// The `&'static str` spelling of `key` in `fields` — for the
/// `validate_static_input` tuples, whose path suffixes must be static.
/// `fallback` covers keys outside the table (unreachable for real inputs,
/// merely safe for arbitrary ones).
pub(super) fn static_field_name(
    fields: &[FieldSchema],
    key: &str,
    fallback: &'static str,
) -> &'static str {
    fields
        .iter()
        .map(|f| f.name)
        .find(|n| *n == key)
        .unwrap_or(fallback)
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
        for schema in REGISTRY {
            assert!(
                write_shape(schema.name).is_some(),
                "function '{}' has no WriteShape",
                schema.name
            );
        }
        for (name, _, aliases, _, _) in ENGINE_BUILTINS {
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

//! The static tables: what Orion's own handlers and the engine's built-ins
//! declare about themselves.
//!
//! Each [`FunctionSchema`] describes the JSON `function.input` object a
//! workflow author must provide for one Orion handler — its field table, where
//! it writes, whether a retry repeats work, what connector it needs. The eight
//! engine built-ins are declared beside them in `ENGINE_BUILTINS`.
//!
//! **Nothing reads these tables directly except the formatter and the
//! handlers' own runtime helpers.** Every other reader — create-time
//! validation, the catalogue, the authoring analysis, the offline tools — goes
//! through [`super::registry::FunctionRegistry`], which converts this file's
//! `'static` rows into owned entries at construction and can hold entries these
//! tables never will (a plugin's). `fmt` keeps reading the static form on
//! purpose: one style everywhere is its whole value, so a plugin function's
//! input keeps author order.
//!
//! Schemas are intentionally hand-rolled rather than derived: the dataflow-rs
//! input structs use deserialize-time defaults that don't show up in derived
//! schemas, and we want to keep the validator dependency-free.

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

/// What a connector-bearing function requires of the connector it names.
///
/// Three questions used to be answered by three tables in `handlers.rs` —
/// `CONNECTOR_FUNCTIONS` (does it take one?), `required_connector_types` (of
/// which types?) and `requires_mongo_database` (must a MongoDB one be given a
/// `database`?) — each keyed by function name in a file away from the schema
/// that declares the `connector` field. A function in one table and not
/// another was a check that silently skipped it (F52). One value on the
/// function's own row cannot be half-filled.
///
/// `types` is non-empty and ordered as it should read in an error message.
/// `requires_mongo_database` is true only for the functions that speak to
/// MongoDB, which has no default database in its connection string: the
/// `mongo_*` trio declares `database` required outright, and the portable
/// dialect pair cannot — the same task shape is valid against SQL and
/// Elasticsearch — so for those it is conditional on the connector actually
/// being Mongo, checked at activation rather than at first request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectorRule {
    pub types: &'static [ConnectorType],
    pub requires_mongo_database: bool,
}

impl ConnectorRule {
    const fn of(types: &'static [ConnectorType]) -> Option<Self> {
        Some(Self {
            types,
            requires_mongo_database: false,
        })
    }

    const fn mongo(types: &'static [ConnectorType]) -> Option<Self> {
        Some(Self {
            types,
            requires_mongo_database: true,
        })
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
    /// The connector this function's `connector` field must name, or `None`
    /// for a function that takes no connector — see [`ConnectorRule`].
    #[serde(skip)]
    pub connector: Option<ConnectorRule>,
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
use super::data_write::DATA_WRITE_FIELDS;
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
        connector: ConnectorRule::of(&[ConnectorType::Cache]),
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
        connector: ConnectorRule::of(&[ConnectorType::Cache]),
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
        connector: ConnectorRule::of(&[ConnectorType::Db]),
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
        connector: ConnectorRule::of(&[ConnectorType::Db]),
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
        connector: ConnectorRule::mongo(&[ConnectorType::Db, ConnectorType::Es]),
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
        connector: ConnectorRule::mongo(&[ConnectorType::Db, ConnectorType::Es]),
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
        connector: ConnectorRule::mongo(&[ConnectorType::Db]),
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
        connector: ConnectorRule::mongo(&[ConnectorType::Db]),
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
        connector: ConnectorRule::mongo(&[ConnectorType::Db]),
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
        connector: None,
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
        connector: None,
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
        connector: None,
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
        connector: None,
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
        connector: ConnectorRule::of(&[ConnectorType::Http]),
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
        connector: ConnectorRule::of(&[ConnectorType::Smtp]),
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
        connector: ConnectorRule::of(&[ConnectorType::Storage]),
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
        connector: ConnectorRule::of(&[ConnectorType::Storage]),
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
        connector: ConnectorRule::of(&[ConnectorType::Kafka]),
        deny_unknown: true,
        validate_static: None,
    },
];

/// Every Orion handler's declaration, in implementation order.
///
/// The static half of [`super::registry::FunctionRegistry::builtin`], which is
/// what every reader but the formatter consults. Accepted function names
/// without an entry here (e.g. `map`, `log`, `filter`) are the engine's own,
/// declared in `ENGINE_BUILTINS`.
pub fn registry() -> &'static [FunctionSchema] {
    REGISTRY
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
/// `(name, description, aliases, writes, retry_safety)` — the five things that
/// vary. Every
/// entry is `category: "data"` (the fourth wire value, matching the grouping
/// `reference/functions.md` already gives these in its summary table),
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
pub(super) const ENGINE_BUILTINS: &[(&str, &str, &[&str], WriteShape, RetrySafety)] = &[
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

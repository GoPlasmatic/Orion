use serde::{Deserialize, Serialize};

/// Operator-owned bounds on what the portable dialect (`data_query` /
/// `data_write`) may reach through this connector.
///
/// [`OperationGates`] answers *which verbs*; this answers *which tables*. The
/// two are separate because a read-only connector still needs bounding: `read`
/// alone does not stop a workflow author from selecting the whole database.
///
/// Both default to off, because the 1.0 flip of `unmapped` to `reject` already
/// makes the safe mode the default one. These exist for the case that flip
/// cannot cover: a task may still write `"unmapped": "identity"` itself, and
/// only the connector's owner can say that is not allowed here.
/// Unknown keys are rejected (unlike the connector config around it, which
/// must keep loading 0.3.x rows carrying since-removed fields). Nothing
/// pre-1.0 can carry a `dialect` block, and a misspelled key here — the
/// `requireSchema` / `allowed_entites` class of typo — is privileged security
/// configuration silently not applying, the same rationale
/// `query::schema` denies unknown fields for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DialectGuards {
    /// Refuse any dialect call that did not declare a real schema — no
    /// entities, or an explicit `"unmapped": "identity"`. Closes the per-task
    /// opt-out so one forgotten `schema` key cannot reopen the connector.
    pub require_schema: bool,
    /// Physical table/collection/index names this connector may touch. Empty
    /// (the default) means unrestricted. Matched against the name *after*
    /// schema renames apply, so a rename cannot step around it, and it covers
    /// relation targets and many-to-many junction tables as well as the
    /// envelope's own `source`/`target`.
    pub allowed_entities: Vec<String>,
}

impl DialectGuards {
    /// Whether `require_schema` is satisfied by the registry a task supplied.
    ///
    /// "A real schema" means both halves: at least one declared entity, and
    /// allowlist mode. Either alone is empty — an entity list under
    /// `"unmapped": "identity"` bounds nothing, and allowlist mode with no
    /// entities can reach nothing anyway.
    pub fn schema_is_sufficient(&self, declared_entities: bool, identity_mode: bool) -> bool {
        !self.require_schema || (declared_entities && !identity_mode)
    }
}

/// Per-connector operation gates. Everything defaults to allowed; disabling an
/// operation turns the corresponding handler call into a located validation
/// error, so a connector can be made read-only (or insert-only, …) in its
/// config without touching workflows.
///
/// - `read` gates `data_query` and `db_read`.
/// - `insert` / `update` / `delete` / `upsert` gate the matching `data_write` op.
/// - `raw_write` gates the raw-SQL `db_write` escape hatch, which cannot be
///   classified per-op (hand-written SQL may contain any statement).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OperationGates {
    pub read: bool,
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
    pub upsert: bool,
    pub raw_write: bool,
}

impl Default for OperationGates {
    fn default() -> Self {
        Self {
            read: true,
            insert: true,
            update: true,
            delete: true,
            upsert: true,
            raw_write: true,
        }
    }
}

impl OperationGates {
    /// Whether the named operation is enabled. Unknown names are denied
    /// (defensive — callers pass the fixed set above).
    pub fn allows(&self, op: &str) -> bool {
        match op {
            "read" => self.read,
            "insert" => self.insert,
            "update" => self.update,
            "delete" => self.delete,
            "upsert" => self.upsert,
            "raw_write" => self.raw_write,
            _ => false,
        }
    }
}

/// Cache-connector operation gates. Everything defaults to allowed;
/// disabling an operation turns the corresponding handler call into a located
/// validation error, so a cache connector can be made read-only in its config
/// without touching workflows.
///
/// - `read` gates `cache_read`.
/// - `write` gates `cache_write`.
///
/// There is deliberately no `delete`: the `CacheBackend` trait exposes
/// get / set / set_ex only, no workflow function deletes a key, and a gate
/// over an operation that cannot be performed would be exactly the kind of
/// accepted-but-never-read field F22 removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheOperationGates {
    pub read: bool,
    pub write: bool,
}

impl Default for CacheOperationGates {
    fn default() -> Self {
        Self {
            read: true,
            write: true,
        }
    }
}

/// HTTP-connector method allow-list.
///
/// The db gates are booleans because their operation set is fixed; an HTTP
/// connector's is its method, so the gate is a list. Empty — the default —
/// means every method `http_call` can issue is allowed. Naming even one
/// method makes the list exhaustive: a connector pointed at a read-only
/// upstream can be `["GET"]` and no workflow can POST through it.
///
/// Matching is case-insensitive; entries are validated against the methods
/// `http_call` supports when the connector is created or updated, so a typo is
/// a 400 rather than a connector that refuses every call at request time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpOperationGates {
    pub methods: Vec<String>,
}

impl HttpOperationGates {
    /// Whether `method` is allowed. An empty list allows everything.
    pub fn allows_method(&self, method: &str) -> bool {
        self.methods.is_empty()
            || self
                .methods
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(method))
    }
}

/// The methods `http_call` can issue — the vocabulary an
/// [`HttpOperationGates`] allow-list is validated against. Anything else in
/// the list would silently never match, because `HttpCallConfig` cannot
/// produce it.
pub const VALID_HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE"];

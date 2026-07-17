//! The write envelope and its resolution into a backend-neutral mutation.
//!
//! `data_write` is the write counterpart of `data_query`: one backend-neutral
//! envelope (`op` / `target` / `values` / `set` / `filter` / `on_conflict` /
//! `returning`) that renders to a native SQL `INSERT`/`UPDATE`/`DELETE`/upsert or
//! a MongoDB write. The `filter` of an `update`/`delete` is the *query dialect's*
//! filter — lowered by [`crate::query::lower`] into the same [`Cond`] IR the read
//! path uses, so relation predicates (`some`/`all`/`none`) work unchanged.
//!
//! [`resolve_write`] does the whole backend-neutral transformation once: parse the
//! envelope, fold `{"param": ..}` value nodes into literals, resolve logical column
//! names to physical (honouring the schema allowlist and `writable` flag), coerce
//! values into the IR, and lower the filter. The per-backend renderers
//! (`backend::sql`, `backend::mongo`) consume the [`ResolvedWrite`].
//!
//! See `proposals/data-write-dialect.md` for the full design.

use serde_json::{Map, Value as Json};

use crate::query::error::QueryError;
use crate::query::ir::{self, Cond};
use crate::query::lower::{Params, lower_with};
use crate::query::schema::EntityRegistry;

/// The mutation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOp {
    Insert,
    Update,
    Delete,
    Upsert,
}

impl WriteOp {
    pub fn as_str(self) -> &'static str {
        match self {
            WriteOp::Insert => "insert",
            WriteOp::Update => "update",
            WriteOp::Delete => "delete",
            WriteOp::Upsert => "upsert",
        }
    }
}

/// What an `upsert` does when the conflict target already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    /// `DO UPDATE` — overwrite the conflicting row with the incoming values/`set`.
    Update,
    /// `DO NOTHING` — leave the existing row untouched.
    Nothing,
}

/// A resolved upsert conflict clause (physical conflict-target columns + action).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedConflict {
    pub targets: Vec<String>,
    pub action: ConflictAction,
}

/// A fully resolved, backend-neutral mutation: physical names, IR values, and a
/// lowered filter. Produced by [`resolve_write`] and consumed by the renderers.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedWrite {
    pub op: WriteOp,
    /// Physical table / collection.
    pub table: String,
    /// Physical column names for `insert`/`upsert` (aligned to each row in `rows`).
    pub columns: Vec<String>,
    /// One value tuple per inserted row (aligned to `columns`).
    pub rows: Vec<Vec<ir::Value>>,
    /// Physical column → value assignments for `update`/`upsert`.
    pub set: Vec<(String, ir::Value)>,
    /// Lowered `filter` for `update`/`delete` (`None` when no `filter` was given).
    pub cond: Option<Cond>,
    /// Whether a (non-null) `filter` key was present — drives the unfiltered guard.
    pub filter_present: bool,
    pub conflict: Option<ResolvedConflict>,
    /// Physical column names to return from mutated rows.
    pub returning: Vec<String>,
    /// Explicit acknowledgement that an unfiltered `update`/`delete` is intended.
    pub all: bool,
}

/// A located write-translation error. Filter errors reuse [`QueryError`] verbatim
/// (the filter is lowered by the query dialect). All variants map to
/// `DataflowError::Validation` at the handler edge.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteError {
    /// The envelope is malformed (bad/missing `op`, `target`, `values`, …).
    InvalidEnvelope(String),
    /// A field required for this `op` is missing (e.g. `set` for update).
    MissingField { field: String, op: String },
    /// An `update`/`delete` has no `filter` and no `"all": true` acknowledgement.
    UnfilteredMutation { op: String },
    /// Unfiltered writes are disabled by config (`write.allow_unfiltered`).
    UnfilteredNotAllowed { op: String },
    /// A bulk insert asked for more rows than `write.max_rows`.
    TooManyRows { requested: usize, max: u64 },
    /// A column value is not a bindable scalar (array/object).
    NotRepresentable { what: String, at: String },
    /// The chosen backend cannot express a requested feature (e.g. RETURNING on MySQL).
    FeatureUnsupportedByTarget { feature: String, target: String },
    /// A filter / field / relation error from the shared query lowering.
    Query(QueryError),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::InvalidEnvelope(m) => write!(f, "invalid write envelope: {m}"),
            WriteError::MissingField { field, op } => {
                write!(f, "'{op}' requires a '{field}' field")
            }
            WriteError::UnfilteredMutation { op } => write!(
                f,
                "'{op}' has no filter; set \"all\": true to intentionally affect every row"
            ),
            WriteError::UnfilteredNotAllowed { op } => write!(
                f,
                "unfiltered '{op}' is disabled (enable write.allow_unfiltered to permit it)"
            ),
            WriteError::TooManyRows { requested, max } => write!(
                f,
                "insert of {requested} rows exceeds the configured maximum {max}"
            ),
            WriteError::NotRepresentable { what, at } => {
                write!(f, "{what} cannot be written as a bound value (at {at})")
            }
            WriteError::FeatureUnsupportedByTarget { feature, target } => {
                write!(f, "{feature} is not supported by the {target} backend")
            }
            WriteError::Query(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WriteError {}

impl From<QueryError> for WriteError {
    fn from(e: QueryError) -> Self {
        WriteError::Query(e)
    }
}

impl From<WriteError> for dataflow_rs::engine::error::DataflowError {
    fn from(e: WriteError) -> Self {
        dataflow_rs::engine::error::DataflowError::Validation(e.to_string())
    }
}

/// Parse and resolve the whole `data_write` input into a [`ResolvedWrite`].
///
/// `params` are the already-message-resolved named values (`{"param": name}` in
/// `values`/`set`/`filter` fold to these); `reg` is the optional inline schema.
pub fn resolve_write(
    input: &Json,
    params: &Params,
    reg: &EntityRegistry,
) -> Result<ResolvedWrite, WriteError> {
    let op = match input.get("op").and_then(|v| v.as_str()) {
        Some("insert") => WriteOp::Insert,
        Some("update") => WriteOp::Update,
        Some("delete") => WriteOp::Delete,
        Some("upsert") => WriteOp::Upsert,
        Some(other) => {
            return Err(WriteError::InvalidEnvelope(format!(
                "unknown op '{other}' (expected insert/update/delete/upsert)"
            )));
        }
        None => {
            return Err(WriteError::InvalidEnvelope(
                "missing required string field 'op'".to_string(),
            ));
        }
    };

    let target = input
        .get("target")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            WriteError::InvalidEnvelope("missing required string field 'target'".to_string())
        })?
        .to_string();
    let table = reg.physical_table(&target);

    // Rows (insert / upsert).
    let (columns, rows) = parse_rows(input.get("values"), params, reg, &target)?;

    // Set assignments (update / upsert).
    let set = parse_set(input.get("set"), params, reg, &target)?;

    // Filter (update / delete) — the query dialect's filter, lowered to `Cond`.
    let filter_node = input.get("filter").filter(|v| !v.is_null());
    let filter_present = filter_node.is_some();
    let cond = match filter_node {
        Some(f) => Some(lower_with(f, params, reg, &target)?),
        None => None,
    };

    let conflict = parse_conflict(input.get("on_conflict"), reg, &target)?;
    let returning = parse_returning(input.get("returning"), reg, &target)?;
    let all = input.get("all").and_then(|v| v.as_bool()).unwrap_or(false);

    // Per-op required-field checks (the unfiltered guard is enforced by the
    // handler, which also holds the `allow_unfiltered` config).
    match op {
        WriteOp::Insert => {
            if rows.is_empty() {
                return Err(WriteError::MissingField {
                    field: "values".to_string(),
                    op: "insert".to_string(),
                });
            }
        }
        WriteOp::Update => {
            if set.is_empty() {
                return Err(WriteError::MissingField {
                    field: "set".to_string(),
                    op: "update".to_string(),
                });
            }
        }
        WriteOp::Delete => {}
        WriteOp::Upsert => {
            if rows.is_empty() {
                return Err(WriteError::MissingField {
                    field: "values".to_string(),
                    op: "upsert".to_string(),
                });
            }
            if conflict.is_none() {
                return Err(WriteError::MissingField {
                    field: "on_conflict".to_string(),
                    op: "upsert".to_string(),
                });
            }
        }
    }

    Ok(ResolvedWrite {
        op,
        table,
        columns,
        rows,
        set,
        cond,
        filter_present,
        conflict,
        returning,
        all,
    })
}

/// Parse `values` (a single row object or an array of them) into a shared column
/// list plus one IR-value tuple per row. Every row must have the same columns.
fn parse_rows(
    node: Option<&Json>,
    params: &Params,
    reg: &EntityRegistry,
    entity: &str,
) -> Result<(Vec<String>, Vec<Vec<ir::Value>>), WriteError> {
    let raw_rows: Vec<&Map<String, Json>> = match node {
        None | Some(Json::Null) => return Ok((Vec::new(), Vec::new())),
        Some(Json::Object(m)) => vec![m],
        Some(Json::Array(a)) => {
            let mut out = Vec::with_capacity(a.len());
            for (i, r) in a.iter().enumerate() {
                out.push(r.as_object().ok_or_else(|| {
                    WriteError::InvalidEnvelope(format!("values[{i}] must be an object"))
                })?);
            }
            out
        }
        Some(_) => {
            return Err(WriteError::InvalidEnvelope(
                "'values' must be an object or an array of objects".to_string(),
            ));
        }
    };

    if raw_rows.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let logical: Vec<String> = raw_rows[0].keys().cloned().collect();
    if logical.is_empty() {
        return Err(WriteError::InvalidEnvelope(
            "'values' rows must have at least one column".to_string(),
        ));
    }
    let columns: Vec<String> = logical
        .iter()
        .map(|c| reg.resolve_write_column(entity, c, "values"))
        .collect::<Result<_, _>>()?;

    let mut rows = Vec::with_capacity(raw_rows.len());
    for (i, r) in raw_rows.iter().enumerate() {
        if r.len() != logical.len() || logical.iter().any(|k| !r.contains_key(k)) {
            return Err(WriteError::InvalidEnvelope(format!(
                "values[{i}] must have the same columns as the first row"
            )));
        }
        let mut vals = Vec::with_capacity(logical.len());
        for c in &logical {
            vals.push(resolve_value_node(&r[c], params, "values")?);
        }
        rows.push(vals);
    }
    Ok((columns, rows))
}

/// Parse `set` (column → value/param) into physical column + IR value pairs.
fn parse_set(
    node: Option<&Json>,
    params: &Params,
    reg: &EntityRegistry,
    entity: &str,
) -> Result<Vec<(String, ir::Value)>, WriteError> {
    let map = match node {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Object(m)) => m,
        Some(_) => {
            return Err(WriteError::InvalidEnvelope(
                "'set' must be an object of column → value".to_string(),
            ));
        }
    };
    let mut out = Vec::with_capacity(map.len());
    for (col, v) in map {
        let phys = reg.resolve_write_column(entity, col, "set")?;
        out.push((phys, resolve_value_node(v, params, "set")?));
    }
    Ok(out)
}

fn parse_conflict(
    node: Option<&Json>,
    reg: &EntityRegistry,
    entity: &str,
) -> Result<Option<ResolvedConflict>, WriteError> {
    let map = match node {
        None | Some(Json::Null) => return Ok(None),
        Some(Json::Object(m)) => m,
        Some(_) => {
            return Err(WriteError::InvalidEnvelope(
                "'on_conflict' must be an object".to_string(),
            ));
        }
    };
    let targets_raw = map
        .get("target")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            WriteError::InvalidEnvelope(
                "on_conflict.target must be an array of columns".to_string(),
            )
        })?;
    if targets_raw.is_empty() {
        return Err(WriteError::InvalidEnvelope(
            "on_conflict.target must name at least one column".to_string(),
        ));
    }
    let mut targets = Vec::with_capacity(targets_raw.len());
    for t in targets_raw {
        let name = t.as_str().ok_or_else(|| {
            WriteError::InvalidEnvelope("on_conflict.target entries must be strings".to_string())
        })?;
        targets.push(reg.resolve_write_column(entity, name, "on_conflict.target")?);
    }
    let action = match map.get("action").and_then(|v| v.as_str()) {
        None | Some("update") => ConflictAction::Update,
        Some("nothing") => ConflictAction::Nothing,
        Some(other) => {
            return Err(WriteError::InvalidEnvelope(format!(
                "on_conflict.action '{other}' must be \"update\" or \"nothing\""
            )));
        }
    };
    Ok(Some(ResolvedConflict { targets, action }))
}

/// Resolve `returning` column names to physical names. These are output columns,
/// so they are *not* subject to the `writable` check (a generated `id` is common).
fn parse_returning(
    node: Option<&Json>,
    reg: &EntityRegistry,
    entity: &str,
) -> Result<Vec<String>, WriteError> {
    let arr = match node {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(a)) => a,
        Some(_) => {
            return Err(WriteError::InvalidEnvelope(
                "'returning' must be an array of column names".to_string(),
            ));
        }
    };
    let mut out = Vec::with_capacity(arr.len());
    for (i, c) in arr.iter().enumerate() {
        let name = c.as_str().ok_or_else(|| {
            WriteError::InvalidEnvelope(format!("returning[{i}] must be a string"))
        })?;
        out.push(physical_column(reg, entity, name));
    }
    Ok(out)
}

/// Physical name for a declared column, ignoring the read/write allowlist flags
/// (used for `returning`, which reads back possibly non-writable columns).
fn physical_column(reg: &EntityRegistry, entity: &str, name: &str) -> String {
    reg.entities
        .get(entity)
        .and_then(|e| e.columns.get(name))
        .and_then(|c| c.name.clone())
        .unwrap_or_else(|| name.to_string())
}

/// Resolve one `values`/`set` value node: a `{"param": name}` folds to the named
/// param; anything else is taken as a literal. Both must be bindable scalars.
fn resolve_value_node(node: &Json, params: &Params, at: &str) -> Result<ir::Value, WriteError> {
    if let Json::Object(m) = node
        && m.len() == 1
        && let Some(p) = m.get("param")
    {
        let name = p.as_str().ok_or_else(|| {
            WriteError::InvalidEnvelope(format!("{at}: param name must be a string"))
        })?;
        let resolved = params.get(name).ok_or_else(|| {
            WriteError::Query(QueryError::MissingParam {
                name: name.to_string(),
                at: at.to_string(),
            })
        })?;
        return json_to_value(resolved, at);
    }
    json_to_value(node, at)
}

/// Convert a JSON scalar to the IR value, restricted to the `AnyPool`-safe set
/// (Null/Bool/Int/Float/Str). Arrays and objects are not bindable column values.
fn json_to_value(j: &Json, at: &str) -> Result<ir::Value, WriteError> {
    Ok(match j {
        Json::Null => ir::Value::Null,
        Json::Bool(b) => ir::Value::Bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                ir::Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                ir::Value::Float(f)
            } else {
                ir::Value::Str(n.to_string())
            }
        }
        Json::String(s) => ir::Value::Str(s.clone()),
        Json::Array(_) | Json::Object(_) => {
            return Err(WriteError::NotRepresentable {
                what: "an array/object column value".to_string(),
                at: at.to_string(),
            });
        }
    })
}

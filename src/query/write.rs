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
//! values into the IR, lower the filter, and enforce the write-safety guards
//! (the [`WriteConfig`] bulk-row cap and the unfiltered-mutation double opt-in) —
//! so no caller can obtain a [`ResolvedWrite`] that violates them (W15). The
//! per-backend renderers (`backend::sql`, `backend::mongo`) consume the
//! [`ResolvedWrite`].
//!
//! See `proposals/data-write-dialect.md` for the full design.

use serde_json::{Map, Value as Json};

use crate::config::WriteConfig;
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
    /// Whether the lowered filter actually restricts the affected rows — this,
    /// not the mere presence of a `filter` key, drives the unfiltered guard.
    ///
    /// A filter that is satisfied by every row (`{"and": []}`, `{"!": {"or": []}}`,
    /// `{"and": [{"and": []}]}`, …) restricts nothing: the statement affects the
    /// whole table whether the renderer omits the `WHERE` clause or emits a
    /// tautology. Keying the guard on key *presence* let any of these skip both
    /// the `"all": true` acknowledgement and `write.allow_unfiltered`.
    /// See [`Cond::is_always_true`].
    pub effective_filter: bool,
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

/// The complete key set of the write envelope. Anything else is a typo, and a
/// typo here is a filter or `returning` silently not applying (W6).
const ENVELOPE_KEYS: [&str; 8] = [
    "op",
    "target",
    "values",
    "set",
    "filter",
    "on_conflict",
    "returning",
    "all",
];

/// Parse and resolve the whole `data_write` input into a [`ResolvedWrite`],
/// enforcing the write-safety guards from `cfg` (W15).
///
/// `params` are the already-message-resolved named values (`{"param": name}` in
/// `values`/`set`/`filter` fold to these); `reg` is the optional inline schema.
pub fn resolve_write(
    input: &Json,
    params: &Params,
    reg: &EntityRegistry,
    cfg: &WriteConfig,
) -> Result<ResolvedWrite, WriteError> {
    // W6: unknown keys were silently ignored — `"retuning"` meant no
    // returning, a misspelled `filter` key meant an unfiltered mutation.
    let obj = input.as_object().ok_or_else(|| {
        WriteError::InvalidEnvelope("write envelope must be a JSON object".to_string())
    })?;
    if let Some(unknown) = obj.keys().find(|k| !ENVELOPE_KEYS.contains(&k.as_str())) {
        return Err(WriteError::InvalidEnvelope(format!(
            "unknown key '{unknown}' in write envelope (expected \
             op/target/values/set/filter/on_conflict/returning/all)"
        )));
    }

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
    let table = reg.physical_table(&target)?;

    // Rows (insert / upsert).
    let (columns, rows) = parse_rows(input.get("values"), params, reg, &target)?;

    // Set assignments (update / upsert).
    let set = parse_set(input.get("set"), params, reg, &target)?;

    // Filter (update / delete) — the query dialect's filter, lowered to `Cond`.
    // The guard is computed from the *lowered* condition: a filter that folds to
    // `Cond::True` restricts nothing and must be treated as unfiltered.
    let filter_node = input.get("filter").filter(|v| !v.is_null());
    let cond = match filter_node {
        Some(f) => Some(lower_with(f, params, reg, &target)?),
        None => None,
    };
    let effective_filter = cond.as_ref().is_some_and(|c| !c.is_always_true());

    let conflict = parse_conflict(input.get("on_conflict"), reg, &target)?;
    let returning = parse_returning(input.get("returning"), reg, &target)?;
    let all = input.get("all").and_then(|v| v.as_bool()).unwrap_or(false);

    // Per-op required-field checks.
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

    // W15: the write-safety guards live in the resolution itself, so no caller
    // can obtain a `ResolvedWrite` that violates them. They used to be
    // enforced only by the `data_write` handler, making this function —
    // documented as the whole backend-neutral transformation — unsafe to call
    // alone.
    if matches!(op, WriteOp::Insert | WriteOp::Upsert) && rows.len() as u64 > cfg.max_rows {
        return Err(WriteError::TooManyRows {
            requested: rows.len(),
            max: cfg.max_rows,
        });
    }
    if matches!(op, WriteOp::Update | WriteOp::Delete) && !effective_filter {
        if !all {
            return Err(WriteError::UnfilteredMutation {
                op: op.as_str().to_string(),
            });
        }
        if !cfg.allow_unfiltered {
            return Err(WriteError::UnfilteredNotAllowed {
                op: op.as_str().to_string(),
            });
        }
    }

    Ok(ResolvedWrite {
        op,
        table,
        columns,
        rows,
        set,
        cond,
        effective_filter,
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
            // Location includes row and column so a wide multi-row insert
            // pinpoints the offending value, not just "values".
            vals.push(resolve_value_node(
                &r[c],
                params,
                &format!("values[{i}].{c}"),
            )?);
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
        out.push((phys, resolve_value_node(v, params, &format!("set.{col}"))?));
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
    if let Some(unknown) = map
        .keys()
        .find(|k| !matches!(k.as_str(), "target" | "action"))
    {
        return Err(WriteError::InvalidEnvelope(format!(
            "unknown key '{unknown}' in on_conflict (expected target/action)"
        )));
    }
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
        // W3: `returning` reads columns back, so it is subject to the read
        // allowlist and to `unmapped: "reject"` exactly like `fields` is.
        // It previously fell through to the raw name regardless of policy, so
        // `"returning": ["secret"]` read any column the DB user could see —
        // the doc comment justified skipping the *writable* check and silently
        // skipped the allowlist too. `queryable` is the right gate here:
        // reading back a non-writable column is legitimate.
        let field = reg.resolve_field(entity, name, &format!("returning[{i}]"))?;
        out.push(field.physical);
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A config that lets every envelope shape through, so envelope tests are
    /// not entangled with the guards (which have their own tests below).
    fn permissive() -> WriteConfig {
        WriteConfig {
            max_rows: 1000,
            allow_unfiltered: true,
        }
    }

    fn resolve(input: Json) -> Result<ResolvedWrite, WriteError> {
        resolve_write(
            &input,
            &Params::new(),
            &EntityRegistry::default(),
            &permissive(),
        )
    }

    // -- envelope errors ---------------------------------------------------

    #[test]
    fn missing_op_is_invalid_envelope() {
        let err = resolve(json!({ "target": "orders" })).expect_err("no op");
        assert!(matches!(err, WriteError::InvalidEnvelope(_)));
        assert!(err.to_string().contains("op"), "{err}");
    }

    #[test]
    fn unknown_op_is_invalid_envelope_naming_the_op() {
        let err = resolve(json!({ "op": "truncate", "target": "orders" })).expect_err("bad op");
        let msg = err.to_string();
        assert!(msg.contains("truncate"), "{msg}");
        assert!(msg.contains("insert/update/delete/upsert"), "{msg}");
    }

    #[test]
    fn missing_target_is_invalid_envelope() {
        let err = resolve(json!({ "op": "insert", "values": {"a": 1} })).expect_err("no target");
        assert!(err.to_string().contains("target"), "{err}");
    }

    #[test]
    fn empty_target_is_invalid_envelope() {
        let err = resolve(json!({ "op": "insert", "target": "", "values": {"a": 1} }))
            .expect_err("empty target");
        assert!(err.to_string().contains("target"), "{err}");
    }

    // -- unknown keys (W6) -------------------------------------------------

    /// `"retuning"` used to mean no returning and a misspelled `filter` key
    /// meant an unfiltered mutation — the intent was silently discarded.
    #[test]
    fn unknown_envelope_keys_are_rejected_naming_the_key() {
        for bad in ["retuning", "vaules", "flter"] {
            let mut input = json!({ "op": "insert", "target": "orders", "values": {"a": 1} });
            input[bad] = json!(["id"]);
            let err = resolve(input).expect_err("unknown key must be rejected");
            assert!(matches!(err, WriteError::InvalidEnvelope(_)), "{err}");
            assert!(err.to_string().contains(bad), "{err}");
        }
    }

    #[test]
    fn unknown_on_conflict_keys_are_rejected() {
        let err = resolve(json!({
            "op": "upsert", "target": "users",
            "values": { "email": "a@x.io" },
            "on_conflict": { "target": ["email"], "action": "update", "do": "nothing" }
        }))
        .expect_err("unknown on_conflict key must be rejected");
        assert!(err.to_string().contains("'do'"), "{err}");
        assert!(err.to_string().contains("on_conflict"), "{err}");
    }

    // -- per-op required fields --------------------------------------------

    #[test]
    fn insert_without_values_is_missing_field() {
        let err = resolve(json!({ "op": "insert", "target": "orders" })).expect_err("no values");
        assert!(
            matches!(&err, WriteError::MissingField { field, op } if field == "values" && op == "insert"),
            "{err}"
        );
    }

    #[test]
    fn update_without_set_is_missing_field() {
        let err = resolve(json!({ "op": "update", "target": "orders" })).expect_err("no set");
        assert!(
            matches!(&err, WriteError::MissingField { field, op } if field == "set" && op == "update"),
            "{err}"
        );
    }

    #[test]
    fn upsert_without_on_conflict_is_missing_field() {
        let err = resolve(json!({ "op": "upsert", "target": "orders", "values": {"id": 1} }))
            .expect_err("no on_conflict");
        assert!(
            matches!(&err, WriteError::MissingField { field, op } if field == "on_conflict" && op == "upsert"),
            "{err}"
        );
    }

    // -- value representability --------------------------------------------

    #[test]
    fn nested_object_column_value_is_not_representable() {
        let err = resolve(json!({
            "op": "insert",
            "target": "orders",
            "values": { "meta": { "nested": true } }
        }))
        .expect_err("object value");
        assert!(matches!(err, WriteError::NotRepresentable { .. }), "{err}");
        // The error must name where, so a multi-column insert is debuggable.
        assert!(err.to_string().contains("meta"), "{err}");
    }

    #[test]
    fn array_column_value_is_not_representable() {
        let err = resolve(json!({
            "op": "insert",
            "target": "orders",
            "values": { "tags": ["a", "b"] }
        }))
        .expect_err("array value");
        assert!(matches!(err, WriteError::NotRepresentable { .. }), "{err}");
    }

    // -- params ------------------------------------------------------------

    #[test]
    fn unknown_param_reference_errors_with_the_name() {
        let err = resolve(json!({
            "op": "insert",
            "target": "orders",
            "values": { "total": { "param": "missing_param" } }
        }))
        .expect_err("unknown param");
        assert!(err.to_string().contains("missing_param"), "{err}");
    }

    #[test]
    fn param_reference_resolves_to_the_named_value() {
        let mut params = Params::new();
        params.insert("amount".to_string(), json!(42));
        let resolved = resolve_write(
            &json!({
                "op": "insert",
                "target": "orders",
                "values": { "total": { "param": "amount" } }
            }),
            &params,
            &EntityRegistry::default(),
            &permissive(),
        )
        .expect("resolves");
        assert_eq!(resolved.rows.len(), 1);
        assert!(matches!(resolved.rows[0][0], ir::Value::Int(42)));
    }

    // -- write-safety guards (W15: enforced by resolve_write itself) -------

    /// The guards used to live only in the `data_write` handler, so
    /// `resolve_write` alone produced an unguarded unfiltered DELETE.
    #[test]
    fn unfiltered_delete_without_all_is_rejected_by_resolve_write() {
        let err = resolve(json!({ "op": "delete", "target": "orders" }))
            .expect_err("no filter, no acknowledgement");
        assert!(
            matches!(&err, WriteError::UnfilteredMutation { op } if op == "delete"),
            "{err}"
        );
    }

    #[test]
    fn unfiltered_delete_with_all_still_needs_the_config_opt_in() {
        let err = resolve_write(
            &json!({ "op": "delete", "target": "orders", "all": true }),
            &Params::new(),
            &EntityRegistry::default(),
            &WriteConfig {
                max_rows: 1000,
                allow_unfiltered: false,
            },
        )
        .expect_err("config forbids unfiltered mutations");
        assert!(
            matches!(&err, WriteError::UnfilteredNotAllowed { op } if op == "delete"),
            "{err}"
        );
    }

    #[test]
    fn unfiltered_delete_with_both_opt_ins_resolves() {
        let resolved =
            resolve(json!({ "op": "delete", "target": "orders", "all": true })).expect("resolves");
        assert!(resolved.all, "the 'all' acknowledgement must survive");
        assert!(!resolved.effective_filter);
    }

    #[test]
    fn a_bulk_insert_over_max_rows_is_rejected_by_resolve_write() {
        let err = resolve_write(
            &json!({
                "op": "insert",
                "target": "orders",
                "values": [ {"a": 1}, {"a": 2}, {"a": 3} ]
            }),
            &Params::new(),
            &EntityRegistry::default(),
            &WriteConfig {
                max_rows: 2,
                allow_unfiltered: false,
            },
        )
        .expect_err("3 rows over a cap of 2");
        assert!(
            matches!(
                err,
                WriteError::TooManyRows {
                    requested: 3,
                    max: 2
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_filter_that_folds_to_true_does_not_count_as_filtered() {
        // Every renderer omits the WHERE clause for `Cond::True`, so a filter
        // that lowers to it affects the whole table. Keying the guard on the
        // presence of the `filter` key let these skip both `"all": true` and
        // `write.allow_unfiltered` — an unbounded DELETE from one token.
        for vacuous in [
            json!({ "and": [] }),
            json!({ "!": { "or": [] } }),
            json!({ "and": [{ "and": [] }] }),
        ] {
            let err = resolve(json!({
                "op": "delete",
                "target": "orders",
                "filter": vacuous.clone(),
            }))
            .expect_err("a vacuous filter restricts nothing");
            assert!(
                matches!(err, WriteError::UnfilteredMutation { .. }),
                "filter {vacuous} must not satisfy the guard"
            );
        }
    }

    #[test]
    fn a_real_filter_counts_as_filtered() {
        let resolved = resolve(json!({
            "op": "delete",
            "target": "orders",
            "filter": { "==": [{ "field": "status" }, "cancelled"] },
        }))
        .expect("resolves");
        assert!(resolved.effective_filter);
    }

    #[test]
    fn an_unsatisfiable_filter_still_counts_as_filtered() {
        // `Cond::False` restricts everything, which is the opposite problem —
        // it must not be mistaken for "no filter" and trip the guard.
        let resolved = resolve(json!({
            "op": "delete",
            "target": "orders",
            "filter": { "or": [] },
        }))
        .expect("resolves");
        assert!(resolved.effective_filter);
    }
}

//! The write twin of `mongo_read` (#263), completing the raw-native pair
//! grammar: `db_read`/`db_write` speak SQL, `mongo_read`/this speak MongoDB.
//!
//! `op` is an **open value set** — `replace_one` and friends are values, not
//! functions, so future capabilities (`find_one_and_update`, `bulk_write`) are
//! one match arm plus validation rows, never a new registration surface.
//! Documents are extended JSON through the same `bson` bridge the read path
//! uses, so nested arrays/objects, `$oid`, `$date`, … all pass through with no
//! Orion-specific code.
//!
//! Safety rails are the estate's existing ones, not new inventions: the
//! connector's per-operation gates (an `upsert: true` call is gated as
//! `upsert`, matching `data_write`); the W15 unfiltered-mutation double
//! opt-in — on *every* filtered op, `_one` included, where an empty filter
//! means "the first document in natural order"; `write.max_rows` capping the
//! bulk insert; and the F28 partial-write classification for an `insert_many`
//! that half-landed.

use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use mongodb::bson::Document;
use serde_json::{Map, Value, json};

use super::connector_helpers::{
    ConnectorCall, apply_output, timed_query, to_connect_error, to_exec_error,
};
use super::data_write::{bulk_result, mongo_write_errors};
use super::mongo_common::{
    require_document, require_mongo_connector, resolve_document, resolve_document_array,
};
use super::schema::{FieldKind, FieldSchema};
use crate::config::WriteConfig;
use crate::connector::ConnectorRegistry;
use crate::connector::mongo_pool::MongoPoolCache;
use crate::query::backend::mongo::insert_outcome;
use crate::query::bulk::{BulkOutcome, ItemOutcome};
use crate::query::write::WriteError;

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "mongo_write";

/// The open op value set. Growth rule: a new capability is a new *value* here
/// (plus its validation rows), not a new function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MongoOp {
    InsertOne,
    InsertMany,
    UpdateOne,
    UpdateMany,
    ReplaceOne,
    DeleteOne,
    DeleteMany,
}

impl MongoOp {
    pub(super) const VALUES: &'static str =
        "insert_one/insert_many/update_one/update_many/replace_one/delete_one/delete_many";

    pub(super) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "insert_one" => MongoOp::InsertOne,
            "insert_many" => MongoOp::InsertMany,
            "update_one" => MongoOp::UpdateOne,
            "update_many" => MongoOp::UpdateMany,
            "replace_one" => MongoOp::ReplaceOne,
            "delete_one" => MongoOp::DeleteOne,
            "delete_many" => MongoOp::DeleteMany,
            _ => return None,
        })
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            MongoOp::InsertOne => "insert_one",
            MongoOp::InsertMany => "insert_many",
            MongoOp::UpdateOne => "update_one",
            MongoOp::UpdateMany => "update_many",
            MongoOp::ReplaceOne => "replace_one",
            MongoOp::DeleteOne => "delete_one",
            MongoOp::DeleteMany => "delete_many",
        }
    }

    /// The connector operation gate this op consults. `upsert: true` switches
    /// an update/replace to the `upsert` gate, matching `data_write`'s
    /// gate-per-effective-op rule.
    pub(super) fn gate(self, upsert: bool) -> &'static str {
        match self {
            MongoOp::InsertOne | MongoOp::InsertMany => "insert",
            MongoOp::UpdateOne | MongoOp::UpdateMany | MongoOp::ReplaceOne => {
                if upsert {
                    "upsert"
                } else {
                    "update"
                }
            }
            MongoOp::DeleteOne | MongoOp::DeleteMany => "delete",
        }
    }

    /// Whether this op selects existing documents with a `filter`.
    pub(super) fn takes_filter(self) -> bool {
        !matches!(self, MongoOp::InsertOne | MongoOp::InsertMany)
    }

    /// The op-specific fields this op accepts (used by authoring-time
    /// validation to refuse a field that would be silently ignored).
    /// Every op, so the authoring-time check can derive its field set from
    /// [`Self::allowed_fields`] instead of restating it.
    pub(super) const ALL: [MongoOp; 7] = [
        MongoOp::InsertOne,
        MongoOp::InsertMany,
        MongoOp::UpdateOne,
        MongoOp::UpdateMany,
        MongoOp::ReplaceOne,
        MongoOp::DeleteOne,
        MongoOp::DeleteMany,
    ];

    pub(super) fn allowed_fields(self) -> &'static [&'static str] {
        match self {
            MongoOp::InsertOne => &["document"],
            MongoOp::InsertMany => &["documents", "ordered"],
            MongoOp::UpdateOne | MongoOp::UpdateMany => {
                &["filter", "update", "upsert", "all", "array_filters"]
            }
            MongoOp::ReplaceOne => &["filter", "document", "upsert", "all"],
            MongoOp::DeleteOne | MongoOp::DeleteMany => &["filter", "all"],
        }
    }
}

/// Workflow function handler for writing documents to MongoDB.
pub struct MongoWriteHandler {
    pub pool_cache: Arc<MongoPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// The `[write]` safety bounds: `max_rows` caps the `insert_many` batch,
    /// `allow_unfiltered` is half of the W15 double opt-in.
    pub write_config: WriteConfig,
}

#[async_trait]
impl AsyncFunctionHandler for MongoWriteHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // F48/F58: the literal prologue first.
        let call = ConnectorCall::begin(NAME, input, ctx)?;
        let database = call.require_str(input, "database")?;
        let collection = call.require_str(input, "collection")?;
        let op = MongoOp::parse(call.require_str(input, "op")?).ok_or_else(|| {
            DataflowError::Validation(format!("{NAME} 'op' must be one of {}", MongoOp::VALUES))
        })?;
        let upsert = literal_bool(input, "upsert");
        let ordered = input
            .get("ordered")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        // Resolve the op's documents against the message, then apply the
        // shared write guards before anything touches the network.
        let write = prepare(op, input, upsert, ordered, ctx)?;
        guard_unfiltered(op, &write, literal_bool(input, "all"), &self.write_config)?;
        if let Prepared::InsertMany { docs, .. } = &write
            && docs.len() as u64 > self.write_config.max_rows
        {
            return Err(WriteError::TooManyRows {
                requested: docs.len(),
                max: self.write_config.max_rows,
            }
            .into());
        }

        call.run(&self.registry, async {
            let connector_config = call.resolve(&self.registry, Some(op.gate(upsert))).await?;
            let db_config = require_mongo_connector(&connector_config, NAME, call.connector)?;

            let client = self
                .pool_cache
                .get_client(call.connector, db_config)
                .await
                .map_err(to_connect_error)?;
            let coll = client.database(database).collection::<Document>(collection);

            // F11: one wall-clock bound over connect + write.
            let (result, outcome) = timed_query(db_config.query_timeout_ms, call.name, async {
                execute_write(&coll, write).await.map_err(|e| e.to_string())
            })
            .await?;

            apply_output(ctx, call.output, result);
            Ok(outcome)
        })
        .await
    }
}

/// A fully resolved write, ready for the driver call.
enum Prepared {
    InsertOne {
        doc: Document,
    },
    InsertMany {
        docs: Vec<Document>,
        ordered: bool,
    },
    Update {
        filter: Document,
        update: Document,
        upsert: bool,
        many: bool,
        array_filters: Option<Vec<Document>>,
    },
    Replace {
        filter: Document,
        doc: Document,
        upsert: bool,
    },
    Delete {
        filter: Document,
        many: bool,
    },
}

/// Resolve the op-conditional fields (each an extended-JSON document folded
/// for `{"var": ..}` nodes) and enforce their shape rules.
fn prepare(
    op: MongoOp,
    input: &Value,
    upsert: bool,
    ordered: bool,
    ctx: &TaskContext<'_>,
) -> Result<Prepared, DataflowError> {
    Ok(match op {
        MongoOp::InsertOne => Prepared::InsertOne {
            doc: require_document(input, "document", NAME, ctx)?,
        },
        MongoOp::InsertMany => Prepared::InsertMany {
            docs: resolve_documents_array(input, ctx)?,
            ordered,
        },
        MongoOp::UpdateOne | MongoOp::UpdateMany => {
            let update = require_document(input, "update", NAME, ctx)?;
            require_update_operators(&update)?;
            let array_filters = resolve_document_array(input, "array_filters", NAME, ctx)?;
            if let Some(ref filters) = array_filters {
                require_usable_array_filters(filters, &update)?;
            }
            Prepared::Update {
                filter: resolve_filter(op, input, ctx)?,
                update,
                upsert,
                many: op == MongoOp::UpdateMany,
                array_filters,
            }
        }
        MongoOp::ReplaceOne => {
            let doc = require_document(input, "document", NAME, ctx)?;
            if let Some(key) = doc.keys().find(|k| k.starts_with('$')) {
                return Err(DataflowError::Validation(format!(
                    "{NAME} {}",
                    replace_plain_message(key)
                )));
            }
            Prepared::Replace {
                filter: resolve_filter(op, input, ctx)?,
                doc,
                upsert,
            }
        }
        MongoOp::DeleteOne | MongoOp::DeleteMany => Prepared::Delete {
            filter: resolve_filter(op, input, ctx)?,
            many: op == MongoOp::DeleteMany,
        },
    })
}

/// The W15 unfiltered-mutation double opt-in, on every filtered op. `_one`
/// included deliberately: an empty filter there means "the first document in
/// natural order", which deserves the acknowledgement no less.
fn guard_unfiltered(
    op: MongoOp,
    write: &Prepared,
    all: bool,
    cfg: &WriteConfig,
) -> Result<(), DataflowError> {
    let unfiltered = match write {
        Prepared::Update { filter, .. }
        | Prepared::Replace { filter, .. }
        | Prepared::Delete { filter, .. } => filter.is_empty(),
        _ => false,
    };
    if op.takes_filter() && unfiltered {
        if !all {
            return Err(WriteError::UnfilteredMutation {
                op: op.as_str().to_string(),
            }
            .into());
        }
        if !cfg.allow_unfiltered {
            return Err(WriteError::UnfilteredNotAllowed {
                op: op.as_str().to_string(),
            }
            .into());
        }
    }
    Ok(())
}

/// One wording per shape rule, shared verbatim by the runtime refusal and
/// authoring-time validation ([`validate_static_input`]) — the two surfaces
/// must state the same rule.
fn update_operators_message(plain_key: &str) -> String {
    format!(
        "'update' must use atomic operators ($set, $inc, $push, …), but has \
         plain key '{plain_key}' — use op 'replace_one' to overwrite the \
         whole document"
    )
}

/// Collect the `$[identifier]` names an update document references.
///
/// `$[]` — every element, unconditionally — carries no identifier and needs no
/// filter, so it is deliberately not collected. Walks nested documents and
/// arrays, since a path can appear at any depth inside `$set`/`$push`/….
fn referenced_identifiers(update: &Document) -> Vec<String> {
    fn from_key(key: &str, out: &mut Vec<String>) {
        let mut rest = key;
        while let Some(start) = rest.find("$[") {
            let after = &rest[start + 2..];
            let Some(end) = after.find(']') else { return };
            let ident = &after[..end];
            // `$[]` has no identifier.
            if !ident.is_empty() && !out.iter().any(|o| o == ident) {
                out.push(ident.to_string());
            }
            rest = &after[end + 1..];
        }
    }
    fn walk(doc: &Document, out: &mut Vec<String>) {
        for (key, value) in doc {
            from_key(key, out);
            match value {
                mongodb::bson::Bson::Document(d) => walk(d, out),
                mongodb::bson::Bson::Array(items) => {
                    for item in items {
                        if let mongodb::bson::Bson::Document(d) = item {
                            walk(d, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(update, &mut out);
    out
}

/// The identifier a single array-filter document constrains.
///
/// MongoDB's rule is one top-level identifier per filter document: every
/// top-level key is `<ident>` or `<ident>.<path>`, except the logical
/// combinators, whose identifier comes from their branches.
fn array_filter_identifier(filter: &Document) -> Result<String, String> {
    fn ident_of(key: &str) -> Option<&str> {
        let head = key.split('.').next()?;
        let valid = !head.is_empty()
            && head.starts_with(|c: char| c.is_ascii_alphabetic())
            && head.chars().all(|c| c.is_ascii_alphanumeric());
        valid.then_some(head)
    }
    fn collect(doc: &Document, out: &mut Vec<String>) {
        for (key, value) in doc {
            if matches!(key.as_str(), "$and" | "$or" | "$nor") {
                if let mongodb::bson::Bson::Array(items) = value {
                    for item in items {
                        if let mongodb::bson::Bson::Document(d) = item {
                            collect(d, out);
                        }
                    }
                }
                continue;
            }
            if let Some(ident) = ident_of(key)
                && !out.iter().any(|o| o == ident)
            {
                out.push(ident.to_string());
            }
        }
    }
    let mut idents = Vec::new();
    collect(filter, &mut idents);
    match idents.len() {
        1 => Ok(idents.remove(0)),
        0 => Err("names no identifier — each entry constrains one \
                  `$[identifier]`, e.g. {\"s.active\": true}"
            .to_string()),
        _ => Err(format!(
            "names more than one identifier ({}) — MongoDB allows one \
             top-level identifier per filter document, so split it into \
             separate entries",
            idents.join(", ")
        )),
    }
}

/// Cross-check `array_filters` against the update's `$[identifier]` paths.
///
/// This is where most of this feature's value is. A server-side rejection —
/// *"No array filter found for identifier 's' in path 'sessions.$[s].active'"*
/// — reaches the author as an opaque **500 `ENGINE_ERROR` with the text
/// discarded**, because a driver error becomes `function_execution` and the
/// catch-all arm replaces the message. Anything caught before the driver call
/// raises `Validation`, which is a **400 with the text preserved verbatim** —
/// and, moved into `validate_static_input`, additionally fires at workflow
/// create/update/import and in `orion-server lint`.
fn cross_check_array_filters(filters: &[Document], update: &Document) -> Result<(), String> {
    if filters.is_empty() {
        return Err("'array_filters' must not be empty — omit the field instead".to_string());
    }
    let mut declared = Vec::with_capacity(filters.len());
    for (i, filter) in filters.iter().enumerate() {
        if filter.is_empty() {
            return Err(format!("array_filters[{i}] must be a non-empty object"));
        }
        let ident =
            array_filter_identifier(filter).map_err(|e| format!("array_filters[{i}] {e}"))?;
        declared.push(ident);
    }

    let referenced = referenced_identifiers(update);
    if referenced.is_empty() {
        return Err(
            "'array_filters' is set but 'update' references no `$[identifier]` path — \
             the filters would never be used. (`$` updates the first match and `$[]` \
             every element; neither takes a filter.)"
                .to_string(),
        );
    }
    if let Some(missing) = referenced.iter().find(|r| !declared.contains(r)) {
        return Err(format!(
            "'update' references `$[{missing}]` but 'array_filters' declares no filter \
             for it — MongoDB refuses the update"
        ));
    }
    if let Some(unused) = declared.iter().find(|d| !referenced.contains(d)) {
        return Err(format!(
            "'array_filters' declares '{unused}' but 'update' never uses `$[{unused}]` — \
             MongoDB refuses an unused array filter"
        ));
    }
    Ok(())
}

/// The runtime half of [`cross_check_array_filters`], for values that only
/// exist per message.
fn require_usable_array_filters(
    filters: &[Document],
    update: &Document,
) -> Result<(), DataflowError> {
    cross_check_array_filters(filters, update)
        .map_err(|e| DataflowError::Validation(format!("{NAME} {e}")))
}

fn replace_plain_message(operator_key: &str) -> String {
    format!(
        "replacement 'document' must be a plain document, but has operator \
         key '{operator_key}' — use op 'update_one' for operator updates"
    )
}

/// An update document must be operator-shaped (`$set`, `$inc`, …): the driver
/// refuses a plain document anyway, but with a message that does not say what
/// to do instead.
fn require_update_operators(update: &Document) -> Result<(), DataflowError> {
    if update.is_empty() {
        return Err(DataflowError::Validation(format!(
            "{NAME} 'update' must not be empty"
        )));
    }
    if let Some(key) = update.keys().find(|k| !k.starts_with('$')) {
        return Err(DataflowError::Validation(format!(
            "{NAME} {}",
            update_operators_message(key)
        )));
    }
    Ok(())
}

/// The selection filter of an update/replace/delete. Missing folds to `{}` —
/// the unfiltered guard, not this, decides whether that is acceptable.
fn resolve_filter(
    op: MongoOp,
    input: &Value,
    ctx: &TaskContext<'_>,
) -> Result<Document, DataflowError> {
    debug_assert!(op.takes_filter());
    Ok(resolve_document(input, "filter", NAME, ctx)?.unwrap_or_default())
}

/// The `documents` array for `insert_many`: each element an extended-JSON
/// document, `{"var": ..}` folded at any depth (so the whole array can come
/// from the message).
fn resolve_documents_array(
    input: &Value,
    ctx: &TaskContext<'_>,
) -> Result<Vec<Document>, DataflowError> {
    resolve_document_array(input, "documents", NAME, ctx)?
        .ok_or_else(|| DataflowError::Validation(format!("{NAME} requires 'documents' field")))
}

fn literal_bool(input: &Value, field: &str) -> bool {
    input.get(field).and_then(Value::as_bool).unwrap_or(false)
}

/// Run the prepared write and normalise its result to the same envelopes
/// `data_write`'s Mongo branch produces — inserts report the bulk shape (F28:
/// a partial ordered batch is 207 with applied/failed/never-attempted named),
/// updates report matched/modified, deletes report the count.
async fn execute_write(
    coll: &mongodb::Collection<Document>,
    write: Prepared,
) -> Result<(Value, TaskOutcome), DataflowError> {
    match write {
        Prepared::InsertOne { doc } => match coll.insert_one(doc).await {
            Ok(res) => {
                let id = serde_json::to_value(&res.inserted_id).ok();
                bulk_result(BulkOutcome::all_ok(vec![id]), "MongoDB insert")
            }
            Err(e) => Err(to_exec_error(e)),
        },
        Prepared::InsertMany { docs, ordered } => {
            if docs.is_empty() {
                return Ok((
                    json!({ "status": "ok", "inserted": 0, "ids": [] }),
                    TaskOutcome::Success,
                ));
            }
            let sent = docs.len();
            match coll.insert_many(docs).ordered(ordered).await {
                Ok(res) => {
                    let mut pairs: Vec<(usize, mongodb::bson::Bson)> =
                        res.inserted_ids.into_iter().collect();
                    pairs.sort_by_key(|(i, _)| *i);
                    let ids: Vec<Option<Value>> = pairs
                        .into_iter()
                        .map(|(_, b)| serde_json::to_value(b).ok())
                        .collect();
                    bulk_result(BulkOutcome::all_ok(ids), "MongoDB insert")
                }
                Err(e) => match mongo_write_errors(&e) {
                    // Ordered stops at the first failure (later documents were
                    // never attempted); unordered attempts every document, so
                    // non-reported indices all landed.
                    Some(failed) if ordered => {
                        bulk_result(insert_outcome(sent, &failed), "MongoDB insert")
                    }
                    Some(failed) => {
                        bulk_result(unordered_insert_outcome(sent, &failed), "MongoDB insert")
                    }
                    None => Err(to_exec_error(e)),
                },
            }
        }
        Prepared::Update {
            filter,
            update,
            upsert,
            many,
            array_filters,
        } => {
            let res = if many {
                let mut call = coll.update_many(filter, update).upsert(upsert);
                if let Some(f) = array_filters {
                    call = call.array_filters(f);
                }
                call.await
            } else {
                let mut call = coll.update_one(filter, update).upsert(upsert);
                if let Some(f) = array_filters {
                    call = call.array_filters(f);
                }
                call.await
            };
            update_envelope(res)
        }
        Prepared::Replace {
            filter,
            doc,
            upsert,
        } => update_envelope(coll.replace_one(filter, doc).upsert(upsert).await),
        Prepared::Delete { filter, many } => {
            let res = if many {
                coll.delete_many(filter).await
            } else {
                coll.delete_one(filter).await
            };
            match res {
                Ok(r) => Ok((
                    json!({ "status": "ok", "deleted": r.deleted_count }),
                    TaskOutcome::Success,
                )),
                Err(e) => Err(to_exec_error(e)),
            }
        }
    }
}

/// The shared update/replace result envelope.
fn update_envelope(
    res: mongodb::error::Result<mongodb::results::UpdateResult>,
) -> Result<(Value, TaskOutcome), DataflowError> {
    match res {
        Ok(r) => {
            let mut out = json!({
                "status": "ok",
                "matched": r.matched_count,
                "modified": r.modified_count,
            });
            if let Some(id) = r.upserted_id {
                out["upserted_id"] = serde_json::to_value(id).unwrap_or(Value::Null);
            }
            Ok((out, TaskOutcome::Success))
        }
        Err(e) => Err(to_exec_error(e)),
    }
}

/// Per-item outcomes for an **unordered** `insert_many` failure: the server
/// attempted every document, so anything not in the reported failures landed —
/// there is no never-attempted tail, unlike the ordered case (F28).
fn unordered_insert_outcome(sent: usize, failed: &[(usize, Value)]) -> BulkOutcome {
    let items = (0..sent)
        .map(|i| match failed.iter().find(|(idx, _)| *idx == i) {
            Some((_, detail)) => ItemOutcome::error(i, detail.clone()),
            None => ItemOutcome::ok(i, None),
        })
        .collect();
    BulkOutcome { items }
}

// -- Authoring-time validation (F53) --

/// Structural checks shared between execution and the authoring-time
/// validator (`schema.rs::validate_input`): create, update, import,
/// `POST /admin/workflows/validate` and `orion-server lint` all refuse a task
/// these rules refuse, so a bad op shape never waits for its first request.
/// Only literal values are judged — a `{"var": ..}` payload is a runtime
/// matter.
pub(super) fn validate_static_input(
    input: &Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    let mut errs = Vec::new();
    let Some(op_raw) = input.get("op").and_then(Value::as_str) else {
        // Presence/type of `op` itself is the generic field schema's job.
        return errs;
    };
    let Some(op) = MongoOp::parse(op_raw) else {
        errs.push((
            "op",
            "unknown_op",
            format!("unknown op '{op_raw}' (expected {})", MongoOp::VALUES),
        ));
        return errs;
    };

    // Required op-conditional fields.
    let required: &[&str] = match op {
        MongoOp::InsertOne => &["document"],
        MongoOp::InsertMany => &["documents"],
        MongoOp::UpdateOne | MongoOp::UpdateMany => &["update"],
        MongoOp::ReplaceOne => &["document"],
        MongoOp::DeleteOne | MongoOp::DeleteMany => &[],
    };
    for field in required {
        if !input.contains_key(*field) {
            errs.push((
                *field,
                "missing_required",
                format!("op '{}' requires '{field}'", op.as_str()),
            ));
        }
    }
    // A filtered op needs a filter, or the explicit `"all": true`
    // acknowledgement (the deployment half of the opt-in is runtime config).
    if op.takes_filter()
        && !input.contains_key("filter")
        && input.get("all").and_then(Value::as_bool) != Some(true)
    {
        errs.push((
            "filter",
            "missing_required",
            format!(
                "op '{}' requires 'filter' (or \"all\": true to intentionally \
                 affect every document)",
                op.as_str()
            ),
        ));
    }

    // A field the op would silently ignore is an authoring mistake.
    //
    // Derived from `allowed_fields` rather than restated: a hand-maintained
    // copy would silently stop checking any field added there but forgotten
    // here — the exact bug class `field_not_applicable` exists to catch.
    // Deduped: several fields (`filter`, `upsert`, `all`) appear in more than
    // one op's list, and a repeat would report the same mistake twice.
    let mut checked: Vec<&'static str> = Vec::new();
    for field in MongoOp::ALL
        .iter()
        .flat_map(|o| o.allowed_fields())
        .copied()
    {
        if checked.contains(&field) {
            continue;
        }
        checked.push(field);
        if input.contains_key(field) && !op.allowed_fields().contains(&field) {
            errs.push((
                field,
                "field_not_applicable",
                format!("'{field}' does not apply to op '{}'", op.as_str()),
            ));
        }
    }

    // Shape rules on literal documents (a `{"var": ..}` payload defers to
    // runtime, where the same rules run against the resolved value).
    if let Some(update) = literal_object(input.get("update"))
        && matches!(op, MongoOp::UpdateOne | MongoOp::UpdateMany)
    {
        if update.is_empty() {
            errs.push((
                "update",
                "empty_update",
                "'update' must not be empty".to_string(),
            ));
        } else if let Some(key) = update.keys().find(|k| !k.starts_with('$')) {
            errs.push((
                "update",
                "update_requires_operators",
                update_operators_message(key),
            ));
        }
    }
    // The `array_filters` cross-check, only when BOTH the update and the
    // filters are literal — a `{"var": ..}` payload defers to runtime, where
    // the same function runs against the resolved value.
    if matches!(op, MongoOp::UpdateOne | MongoOp::UpdateMany)
        && let Some(update) = literal_object(input.get("update"))
        && let Some(filters) = input.get("array_filters").and_then(Value::as_array)
        && let Ok(update_doc) = mongodb::bson::to_document(&Value::Object(update.clone()))
        && let Some(filter_docs) = filters
            .iter()
            .map(|f| {
                f.as_object()
                    .and_then(|o| mongodb::bson::to_document(&Value::Object(o.clone())).ok())
            })
            .collect::<Option<Vec<_>>>()
        && let Err(e) = cross_check_array_filters(&filter_docs, &update_doc)
    {
        errs.push(("array_filters", "unusable_array_filters", e));
    }
    if op == MongoOp::ReplaceOne
        && let Some(doc) = literal_object(input.get("document"))
        && let Some(key) = doc.keys().find(|k| k.starts_with('$'))
    {
        errs.push((
            "document",
            "replace_document_is_plain",
            replace_plain_message(key),
        ));
    }
    errs
}

/// The literal object under `node`, unless it is a `{"var": ..}` substitution.
fn literal_object(node: Option<&Value>) -> Option<&Map<String, Value>> {
    let map = node?.as_object()?;
    if map.len() == 1 && map.contains_key("var") {
        return None;
    }
    Some(map)
}

// -- Input schema (F53) --

pub(super) const MONGO_WRITE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the MongoDB connector.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        secret: false,
        alias: None,
    },
    FieldSchema {
        name: "database",
        description: "Mongo database name.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        secret: false,
        alias: None,
    },
    FieldSchema {
        name: "collection",
        description: "Mongo collection name.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        secret: false,
        alias: None,
    },
    FieldSchema {
        name: "op",
        description: "Write operation: insert_one, insert_many, update_one, update_many, replace_one, delete_one, or delete_many.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        secret: false,
        alias: None,
    },
    FieldSchema {
        name: "document",
        description: "The document for insert_one / replace_one (extended JSON; nested arrays/objects pass through). Accepts {\"var\": \"path\"} at any depth.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
        secret: false,
        alias: None,
    },
    FieldSchema {
        name: "documents",
        description: "Array of documents for insert_many; batch size is capped by write.max_rows. Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Array,
        required: false,
        resolvable: true,
        secret: false,
        alias: None,
    },
    FieldSchema {
        name: "filter",
        description: "Selection filter for update/replace/delete ops (extended JSON: $oid, $date, ... work). An empty filter requires \"all\": true and write.allow_unfiltered. Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
        secret: false,
        alias: None,
    },
    FieldSchema {
        name: "update",
        description: "Update document for update_one/update_many; top-level keys must be atomic operators ($set, $inc, $push, ...). Field paths may target array elements with $ (first match), $[] (every element) or $[identifier] (every element an 'array_filters' entry matches). Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
        secret: false,
        alias: None,
    },
    FieldSchema {
        name: "array_filters",
        description: "Array of filter documents naming the $[identifier] paths used in 'update' (update_one/update_many). Each entry constrains one identifier, e.g. {\"s.expiresAt\": {\"$lt\": ...}}. Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Array,
        // Resolvable is safe and necessary here. Safe, because the house rule
        // folds `{"var": …}` nodes only — never arbitrary JSONLogic — and
        // values pulled from the message are not re-scanned, so a request body
        // cannot inject a `var` node of its own. Necessary, because the
        // predicate value almost always comes from the request. `filter` is
        // already resolvable, so this adds no new class of caller influence.
        required: false,
        resolvable: true,
        secret: false,
        alias: None,
    },
    FieldSchema {
        name: "upsert",
        description: "Insert when no document matches (update_one/update_many/replace_one). Gated as 'upsert' on the connector when true. Defaults to false.",
        kind: FieldKind::Bool,
        required: false,
        resolvable: false,
        secret: false,
        alias: None,
    },
    FieldSchema {
        name: "ordered",
        description: "insert_many only: stop at the first failure (true, default) or attempt every document (false).",
        kind: FieldKind::Bool,
        required: false,
        resolvable: false,
        secret: false,
        alias: None,
    },
    FieldSchema {
        name: "all",
        description: "Acknowledge an intentionally unfiltered update/replace/delete (affects every document; also requires write.allow_unfiltered).",
        kind: FieldKind::Bool,
        required: false,
        resolvable: false,
        secret: false,
        alias: None,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the write result is written. Defaults to \"data\".",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        secret: false,
        alias: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().expect("test input is an object").clone()
    }

    // ---- validate_static_input: the authoring-time contract ----

    #[test]
    fn an_unknown_op_is_named_with_the_full_value_set() {
        let errs = validate_static_input(&obj(json!({ "op": "insert" })));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].0, "op");
        assert!(errs[0].2.contains("insert_one/"), "{}", errs[0].2);
    }

    #[test]
    fn op_conditional_requirements_are_authoring_errors() {
        // update_one missing both filter and update: both named at once.
        let errs = validate_static_input(&obj(json!({ "op": "update_one" })));
        let fields: Vec<&str> = errs.iter().map(|e| e.0).collect();
        assert!(fields.contains(&"update"), "{errs:?}");
        assert!(fields.contains(&"filter"), "{errs:?}");

        assert!(
            validate_static_input(&obj(json!({ "op": "insert_one" })))
                .iter()
                .any(|e| e.0 == "document"),
        );
        assert!(
            validate_static_input(&obj(json!({ "op": "insert_many" })))
                .iter()
                .any(|e| e.0 == "documents"),
        );
    }

    #[test]
    fn all_true_stands_in_for_a_missing_filter() {
        let errs = validate_static_input(&obj(json!({
            "op": "delete_many", "all": true
        })));
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn a_field_the_op_ignores_is_refused_not_ignored() {
        let errs = validate_static_input(&obj(json!({
            "op": "delete_one",
            "filter": { "x": 1 },
            "update": { "$set": { "y": 2 } }
        })));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].0, "update");
        assert_eq!(errs[0].1, "field_not_applicable");
    }

    #[test]
    fn a_plain_key_update_is_told_to_use_operators_or_replace() {
        let errs = validate_static_input(&obj(json!({
            "op": "update_one",
            "filter": { "x": 1 },
            "update": { "name": "Ada" }
        })));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].1, "update_requires_operators");
        assert!(errs[0].2.contains("replace_one"), "{}", errs[0].2);
    }

    /// #274: the happy path — every identifier the update uses has a filter,
    /// and every filter is used.
    #[test]
    fn a_matched_array_filter_set_is_accepted() {
        let errs = validate_static_input(&obj(json!({
            "op": "update_many",
            "filter": { "_id": 1 },
            "update": { "$set": { "sessions.$[s].active": false } },
            "array_filters": [{ "s.expiresAt": { "$lt": 100 } }]
        })));
        assert!(errs.is_empty(), "{errs:?}");
    }

    /// The cross-check that earns this feature its validation. MongoDB's own
    /// diagnostics for these are genuinely helpful — and reach the author as an
    /// opaque 500 with the text discarded, because a driver error becomes
    /// `function_execution`. Caught here they are a 400 with the text intact.
    #[test]
    fn array_filter_mismatches_are_named_before_the_driver_sees_them() {
        // An identifier with no filter.
        let errs = validate_static_input(&obj(json!({
            "op": "update_one",
            "filter": { "_id": 1 },
            "update": { "$set": { "sessions.$[s].active": false } },
            "array_filters": [{ "other.x": 1 }]
        })));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].1, "unusable_array_filters");
        assert!(errs[0].2.contains("$[s]"), "{}", errs[0].2);

        // A filter nothing uses.
        let errs = validate_static_input(&obj(json!({
            "op": "update_one",
            "filter": { "_id": 1 },
            "update": { "$set": { "sessions.$[s].active": false } },
            "array_filters": [{ "s.x": 1 }, { "t.y": 2 }]
        })));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].2.contains("'t'"), "{}", errs[0].2);

        // Filters supplied but no `$[ident]` anywhere — including the `$[]`
        // case, which is unconditional and takes no filter.
        for update in [
            json!({ "$set": { "sessions.$.active": false } }),
            json!({ "$set": { "sessions.$[].active": false } }),
        ] {
            let errs = validate_static_input(&obj(json!({
                "op": "update_one",
                "filter": { "_id": 1 },
                "update": update,
                "array_filters": [{ "s.x": 1 }]
            })));
            assert_eq!(errs.len(), 1, "{errs:?}");
            assert!(errs[0].2.contains("never be used"), "{}", errs[0].2);
        }

        // Empty list, and an empty entry.
        let errs = validate_static_input(&obj(json!({
            "op": "update_one", "filter": { "_id": 1 },
            "update": { "$set": { "s.$[a].x": 1 } },
            "array_filters": []
        })));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].2.contains("not be empty"), "{}", errs[0].2);

        // One filter document naming two identifiers.
        let errs = validate_static_input(&obj(json!({
            "op": "update_one", "filter": { "_id": 1 },
            "update": { "$set": { "s.$[a].x": 1, "t.$[b].y": 2 } },
            "array_filters": [{ "a.x": 1, "b.y": 2 }]
        })));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].2.contains("more than one"), "{}", errs[0].2);
    }

    /// `$and`/`$or`/`$nor` take their identifier from their branches, so a
    /// combinator filter is not mistaken for "names no identifier".
    #[test]
    fn a_logical_array_filter_resolves_its_identifier() {
        let errs = validate_static_input(&obj(json!({
            "op": "update_many",
            "filter": { "_id": 1 },
            "update": { "$set": { "sessions.$[s].active": false } },
            "array_filters": [{ "$and": [{ "s.a": 1 }, { "s.b": 2 }] }]
        })));
        assert!(errs.is_empty(), "{errs:?}");
    }

    /// A `{"var": …}` payload cannot be checked statically and must defer to
    /// runtime rather than being refused at authoring time.
    #[test]
    fn a_var_array_filter_defers_shape_checks_to_runtime() {
        let errs = validate_static_input(&obj(json!({
            "op": "update_one",
            "filter": { "_id": 1 },
            "update": { "$set": { "sessions.$[s].active": false } },
            "array_filters": { "var": "temp_data.filters" }
        })));
        assert!(errs.is_empty(), "{errs:?}");
    }

    /// `array_filters` applies to the two update ops and nothing else — the
    /// refusal only happens because the field is in BOTH hand-maintained
    /// lists.
    #[test]
    fn array_filters_is_refused_on_ops_that_cannot_use_it() {
        for op in ["delete_one", "replace_one", "insert_one"] {
            let errs = validate_static_input(&obj(json!({
                "op": op,
                "filter": { "_id": 1 },
                "document": { "a": 1 },
                "array_filters": [{ "s.x": 1 }]
            })));
            assert!(
                errs.iter()
                    .any(|e| e.0 == "array_filters" && e.1 == "field_not_applicable"),
                "{op}: {errs:?}"
            );
        }
    }

    #[test]
    fn an_operator_key_in_a_replacement_document_is_refused() {
        let errs = validate_static_input(&obj(json!({
            "op": "replace_one",
            "filter": { "x": 1 },
            "document": { "$set": { "y": 2 } }
        })));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].1, "replace_document_is_plain");
    }

    /// A `{"var": ..}` payload defers shape judgement to runtime — the value
    /// is not knowable at authoring time.
    #[test]
    fn a_var_update_defers_shape_checks_to_runtime() {
        let errs = validate_static_input(&obj(json!({
            "op": "update_many",
            "filter": { "x": 1 },
            "update": { "var": "temp_data.update" }
        })));
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn a_valid_task_produces_no_errors() {
        let errs = validate_static_input(&obj(json!({
            "op": "update_one",
            "filter": { "_id": { "$oid": "665f1f77bcf86cd799439011" } },
            "update": { "$set": { "status": "done" } },
            "upsert": true
        })));
        assert!(errs.is_empty(), "{errs:?}");
    }

    // ---- unordered outcome classification ----

    /// Unordered means the server attempted everything: a failure at index 1
    /// says nothing about 2 — it landed unless reported, so there is no
    /// `skipped` tail (the ordered case's F28 classification).
    #[test]
    fn an_unordered_failure_has_no_skipped_tail() {
        let out = unordered_insert_outcome(
            3,
            &[(1, json!({ "code": 11000, "message": "duplicate key" }))],
        );
        let j = out.to_json();
        assert_eq!(j["status"], "partial", "{j}");
        assert_eq!(j["inserted"], 2, "{j}");
        assert_eq!(j["failed"], 1, "{j}");
        // The envelope omits `skipped` when nothing was skipped — 0 and 2
        // both landed, so there is no skipped tail to report.
        assert!(j.get("skipped").is_none() || j["skipped"] == 0, "{j}");
    }

    // ---- gate mapping ----

    #[test]
    fn upsert_switches_the_gate_from_update_to_upsert() {
        assert_eq!(MongoOp::UpdateOne.gate(false), "update");
        assert_eq!(MongoOp::UpdateOne.gate(true), "upsert");
        assert_eq!(MongoOp::ReplaceOne.gate(true), "upsert");
        assert_eq!(MongoOp::DeleteMany.gate(false), "delete");
        assert_eq!(MongoOp::InsertMany.gate(false), "insert");
    }
}

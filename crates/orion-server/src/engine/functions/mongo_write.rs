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
    ConnectorCall, apply_output, resolve_value, timed_query, to_connect_error, to_exec_error,
};
use super::data_write::{bulk_result, mongo_write_errors};
use super::mongo_common::{require_document, require_mongo_connector, resolve_document};
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
    pub(super) fn allowed_fields(self) -> &'static [&'static str] {
        match self {
            MongoOp::InsertOne => &["document"],
            MongoOp::InsertMany => &["documents", "ordered"],
            MongoOp::UpdateOne | MongoOp::UpdateMany => &["filter", "update", "upsert", "all"],
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
            Prepared::Update {
                filter: resolve_filter(op, input, ctx)?,
                update,
                upsert,
                many: op == MongoOp::UpdateMany,
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
    let raw = input
        .get("documents")
        .ok_or_else(|| DataflowError::Validation(format!("{NAME} requires 'documents' field")))?;
    let resolved = resolve_value(raw, ctx);
    let Value::Array(items) = resolved else {
        return Err(DataflowError::Validation(format!(
            "{NAME} 'documents' must resolve to an array of objects"
        )));
    };
    let mut docs = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if !item.is_object() {
            return Err(DataflowError::Validation(format!(
                "{NAME} documents[{i}] must be an object"
            )));
        }
        docs.push(mongodb::bson::to_document(item).map_err(|e| {
            DataflowError::Validation(format!("{NAME} documents[{i}] is not valid: {e}"))
        })?);
    }
    Ok(docs)
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
        } => {
            let res = if many {
                coll.update_many(filter, update).upsert(upsert).await
            } else {
                coll.update_one(filter, update).upsert(upsert).await
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
    for field in [
        "document",
        "documents",
        "filter",
        "update",
        "upsert",
        "ordered",
        "all",
    ] {
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
        alias: None,
    },
    FieldSchema {
        name: "database",
        description: "Mongo database name.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "collection",
        description: "Mongo collection name.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "op",
        description: "Write operation: insert_one, insert_many, update_one, update_many, replace_one, delete_one, or delete_many.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "document",
        description: "The document for insert_one / replace_one (extended JSON; nested arrays/objects pass through). Accepts {\"var\": \"path\"} at any depth.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "documents",
        description: "Array of documents for insert_many; batch size is capped by write.max_rows. Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Array,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "filter",
        description: "Selection filter for update/replace/delete ops (extended JSON: $oid, $date, ... work). An empty filter requires \"all\": true and write.allow_unfiltered. Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "update",
        description: "Update document for update_one/update_many; top-level keys must be atomic operators ($set, $inc, $push, ...). Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "upsert",
        description: "Insert when no document matches (update_one/update_many/replace_one). Gated as 'upsert' on the connector when true. Defaults to false.",
        kind: FieldKind::Bool,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "ordered",
        description: "insert_many only: stop at the first failure (true, default) or attempt every document (false).",
        kind: FieldKind::Bool,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "all",
        description: "Acknowledge an intentionally unfiltered update/replace/delete (affects every document; also requires write.allow_unfiltered).",
        kind: FieldKind::Bool,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the write result is written. Defaults to \"data\".",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
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

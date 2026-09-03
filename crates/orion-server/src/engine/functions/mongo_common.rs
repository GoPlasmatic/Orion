//! Shared plumbing for the raw-native MongoDB handlers (`mongo_read`,
//! `mongo_write`, `mongo_aggregate`) — #263 — and, for the result envelopes
//! at the bottom of this file, `data_write`'s Mongo branch as well. The two
//! write paths reach MongoDB differently but must answer in the same shape,
//! and `mongo_write` says so in its own doc; sharing the builders is what
//! makes that true rather than asserted.
//!
//! The design rule for this trio: **documents are extended JSON**. Every
//! workflow-authored document (filter, update, pipeline stage, projection,
//! sort) is folded for `{"var": ..}` nodes and then interpreted through the
//! `bson` serde bridge, so every BSON type with an extended-JSON spelling —
//! `$oid`, `$date`, `$numberDecimal`, `$uuid`, … — works with no
//! Orion-specific code, today and as `bson` grows.

use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use futures::TryStreamExt;
use mongodb::bson::{self, Document};
use serde_json::Value;

use super::connector_helpers::{is_mongo, resolve_value};
use super::templated_input::TemplatedInput;
use crate::connector::DbConnectorConfig;

/// The MongoDB half of a `db` connector's identity.
///
/// The type gate stops at `db`: SQL and MongoDB are one `ConnectorConfig::Db`
/// variant and only the connection string tells them apart, so the kind check
/// (`require_connector::<Db>`, now the handler wrapper's job) cannot answer
/// this and each Mongo handler asks it in its gate.
pub(super) fn require_mongo_backend(
    db_config: &DbConnectorConfig,
    handler_name: &str,
    connector_name: &str,
) -> Result<(), DataflowError> {
    if !is_mongo(&db_config.connection_string) {
        return Err(DataflowError::Validation(format!(
            "{handler_name} requires a MongoDB connector, but '{connector_name}' has a \
             non-MongoDB connection string (expected a mongodb:// or mongodb+srv:// URL)"
        )));
    }
    Ok(())
}

/// Resolve an optional document-shaped field: fold `{"var": ..}` nodes, then
/// interpret through the `bson` serde bridge (extended JSON included). Absent
/// or null is `None`; anything resolving to a non-object is a located error.
pub(super) fn resolve_document(
    input: &Value,
    field: &str,
    handler_name: &str,
    ctx: &TaskContext<'_>,
) -> Result<Option<Document>, DataflowError> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(raw) => {
            let resolved = resolve_value(raw, ctx);
            if !resolved.is_object() {
                return Err(DataflowError::Validation(format!(
                    "{handler_name} '{field}' must resolve to an object"
                )));
            }
            bson::to_document(&resolved).map(Some).map_err(|e| {
                DataflowError::Validation(format!("{handler_name} '{field}' is not valid: {e}"))
            })
        }
    }
}

/// Convert already-resolved values into BSON documents, naming the index in
/// every error — a per-message array only exists at runtime, so this message is
/// the only diagnosis its author gets.
///
/// Split from [`resolve_document_array`] because `mongo_aggregate` must
/// validate its *resolved* stages against the stage allowlist before
/// converting, so it cannot use the resolve-and-convert path — but the
/// conversion itself, and its error wording, are the same job.
pub(super) fn documents_from_values<'a>(
    values: impl IntoIterator<Item = &'a Value>,
    field: &str,
    handler_name: &str,
) -> Result<Vec<Document>, DataflowError> {
    values
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            if !item.is_object() {
                return Err(DataflowError::Validation(format!(
                    "{handler_name} {field}[{i}] must be an object"
                )));
            }
            bson::to_document(item).map_err(|e| {
                DataflowError::Validation(format!("{handler_name} {field}[{i}] is not valid: {e}"))
            })
        })
        .collect()
}

/// [`resolve_document`] for a field holding an **array** of documents.
///
/// One implementation for `insert_many`'s `documents` and `mongo_write`'s
/// `array_filters`, which each carried their own copy; `mongo_aggregate`
/// shares the conversion half through [`documents_from_values`].
/// Extended-JSON support (`$oid`, `$date`, …) therefore falls out for all
/// three, which is what makes a typed date comparison inside an array filter
/// work.
///
/// `None` when the field is absent or null.
pub(super) fn resolve_document_array(
    input: &Value,
    field: &str,
    handler_name: &str,
    ctx: &TaskContext<'_>,
) -> Result<Option<Vec<Document>>, DataflowError> {
    let raw = match input.get(field) {
        None | Some(Value::Null) => return Ok(None),
        Some(raw) => raw,
    };
    let resolved = resolve_value(raw, ctx);
    let Value::Array(items) = resolved else {
        return Err(DataflowError::Validation(format!(
            "{handler_name} '{field}' must resolve to an array of objects"
        )));
    };
    documents_from_values(items.iter(), field, handler_name).map(Some)
}

/// [`resolve_document`] for a required field.
pub(super) fn require_document(
    input: &Value,
    field: &str,
    handler_name: &str,
    ctx: &TaskContext<'_>,
) -> Result<Document, DataflowError> {
    resolve_document(input, field, handler_name, ctx)?.ok_or_else(|| {
        DataflowError::Validation(format!("{handler_name} requires '{field}' field"))
    })
}

/// Resolve an optional non-negative integer field (`limit`, `skip`).
pub(super) fn resolve_u64(
    input: &TemplatedInput,
    field: &str,
    handler_name: &str,
    ctx: &TaskContext<'_>,
) -> Result<Option<u64>, DataflowError> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(raw) => match resolve_value(raw, ctx) {
            Value::Number(n) => n.as_u64().map(Some).ok_or_else(|| {
                DataflowError::Validation(format!(
                    "{handler_name} '{field}' must be a non-negative integer"
                ))
            }),
            other => Err(DataflowError::Validation(format!(
                "{handler_name} '{field}' must resolve to a non-negative integer, got {}",
                super::connector_helpers::json_type_name(&other)
            ))),
        },
    }
}

/// Drain a cursor into documents, refusing past `cap` — an unbounded result
/// must not OOM the process (F10). The message names the knob.
pub(super) async fn drain_capped(
    mut cursor: mongodb::Cursor<Document>,
    cap: usize,
    handler_name: &str,
) -> Result<Vec<Document>, String> {
    let mut docs: Vec<Document> = Vec::new();
    while let Some(doc) = cursor.try_next().await.map_err(|e| e.to_string())? {
        if docs.len() >= cap {
            return Err(format!(
                "{handler_name} result exceeds query.max_limit ({cap} documents) — \
                 add a filter/limit or raise the cap"
            ));
        }
        docs.push(doc);
    }
    Ok(docs)
}

/// Serialize driver documents to the JSON array a workflow consumes. BSON
/// types without a JSON-native form come back in their canonical extended-JSON
/// spelling (`{"$oid": …}`, `{"$date": {"$numberLong": …}}`) — the same
/// spellings every input document accepts, so read output round-trips into the
/// next filter unchanged.
pub(super) fn docs_to_json(docs: &[Document]) -> Value {
    Value::Array(
        docs.iter()
            .filter_map(|doc| serde_json::to_value(doc).ok())
            .collect(),
    )
}

// ============================================================
// Result envelopes
// ============================================================
//
// The shapes a Mongo write answers in. Both write paths — `mongo_write`'s
// raw-native handler and `data_write`'s Mongo branch — produce these, and a
// workflow that switches between the two must not see the keys move.

/// An `insert_many` with nothing to insert. Not an error, and not a bulk
/// outcome either: there are no per-item results to report.
pub(super) fn empty_insert_envelope() -> Value {
    serde_json::json!({ "status": "ok", "inserted": 0, "ids": [] })
}

/// The update/replace envelope. `upserted_id` appears only when the write
/// actually inserted, which is how a caller tells an upsert that matched from
/// one that created.
pub(super) fn update_envelope(res: &mongodb::results::UpdateResult) -> Value {
    let mut out = serde_json::json!({
        "status": "ok",
        "matched": res.matched_count,
        "modified": res.modified_count,
    });
    if let Some(id) = &res.upserted_id {
        out["upserted_id"] = serde_json::to_value(id).unwrap_or(Value::Null);
    }
    out
}

/// The delete envelope.
pub(super) fn delete_envelope(deleted_count: u64) -> Value {
    serde_json::json!({ "status": "ok", "deleted": deleted_count })
}

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
//!
//! On dataflow-rs 4 a document is an `Interpolated` field, rendered for the
//! call and serialized straight into BSON (`rendered_bson`): the same
//! `var` holes 1.12 folded, the same bridge, and no JSON tree of its own
//! unless the document is refused, when 1.12's reading of the folded JSON
//! words the refusal.

use dataflow_rs::Ctx;
use dataflow_rs::expr::{Interpolated, Rendered};
use futures::TryStreamExt;
use mongodb::bson::{self, Bson, Document};
use serde_json::Value;

use super::connector_helpers::is_mongo;
use crate::connector::DbConnectorConfig;
use crate::engine::{ErrorClass, HandlerError};

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
) -> Result<(), HandlerError> {
    if !is_mongo(&db_config.connection_string) {
        return Err(HandlerError::new(
            ErrorClass::CallerInput,
            format!(
                "{handler_name} requires a MongoDB connector, but '{connector_name}' has a \
             non-MongoDB connection string (expected a mongodb:// or mongodb+srv:// URL)"
            ),
        ));
    }
    Ok(())
}

/// The half of [`resolve_document`] after the fold: the folded value as a
/// BSON document, or the located refusal. Shared with the dataflow-rs 4
/// functions, which reach it only for a document that is refused.
pub(super) fn document_from_value(
    resolved: &Value,
    field: &str,
    handler_name: &str,
) -> Result<Document, HandlerError> {
    if !resolved.is_object() {
        return Err(HandlerError::new(
            ErrorClass::CallerInput,
            format!("{handler_name} '{field}' must resolve to an object"),
        ));
    }
    bson::to_document(resolved).map_err(|e| {
        HandlerError::new(
            ErrorClass::CallerInput,
            format!("{handler_name} '{field}' is not valid: {e}"),
        )
    })
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
) -> Result<Vec<Document>, HandlerError> {
    values
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            if !item.is_object() {
                return Err(HandlerError::new(
                    ErrorClass::CallerInput,
                    format!("{handler_name} {field}[{i}] must be an object"),
                ));
            }
            bson::to_document(item).map_err(|e| {
                HandlerError::new(
                    ErrorClass::CallerInput,
                    format!("{handler_name} {field}[{i}] is not valid: {e}"),
                )
            })
        })
        .collect()
}

/// The half of [`resolve_document_array`] after the fold, shared as
/// [`document_from_value`] is.
pub(super) fn document_array_from_value(
    resolved: &Value,
    field: &str,
    handler_name: &str,
) -> Result<Vec<Document>, HandlerError> {
    let Value::Array(items) = resolved else {
        return Err(HandlerError::new(
            ErrorClass::CallerInput,
            format!("{handler_name} '{field}' must resolve to an array of objects"),
        ));
    };
    documents_from_values(items.iter(), field, handler_name)
}

/// `doc` rendered for this call and read by `direct`, straight from the
/// rendered document into BSON with no tree of its own. When `direct` has no
/// answer, `judge` reads the JSON 1.12 folded the field to
/// (`resolve_value`'s fold of the same document), so a refusal is 1.12's,
/// word for word, and only a refused call builds the tree.
///
/// The two readings agree on every document `direct` accepts: the rendered
/// document makes the serializer calls the folded JSON makes (its literal
/// parts are that JSON's, and a hole's value is the message's), so
/// [`bson`] builds the same document from either.
///
/// # Errors
///
/// `judge`'s refusal; a [`ErrorClass::Backend`] error for a document that
/// does not serialize to JSON, which a rendered document always does.
pub(super) fn rendered_bson<T>(
    ctx: &Ctx<'_>,
    doc: &Interpolated,
    direct: impl FnOnce(&Rendered<'_>) -> Option<T>,
    judge: impl FnOnce(&Value) -> Result<T, HandlerError>,
) -> Result<T, HandlerError> {
    let rendered = ctx.render(doc);
    if let Some(read) = direct(&rendered) {
        return Ok(read);
    }
    let folded =
        serde_json::to_value(&rendered).map_err(|e| HandlerError::new(ErrorClass::Backend, e))?;
    judge(&folded)
}

/// [`resolve_document`] on dataflow-rs 4: an optional document field
/// rendered for this call into BSON. Unset or authored `null` is `None`;
/// anything rendering to a non-object is 1.12's located refusal.
///
/// # Errors
///
/// As [`document_from_value`].
pub(super) fn rendered_document(
    ctx: &Ctx<'_>,
    doc: Option<&Interpolated>,
    field: &str,
    handler_name: &str,
) -> Result<Option<Document>, HandlerError> {
    let Some(doc) = doc.filter(|doc| !doc.raw().is_null()) else {
        return Ok(None);
    };
    rendered_bson(
        ctx,
        doc,
        |rendered| bson::to_document(rendered).ok(),
        |folded| document_from_value(folded, field, handler_name),
    )
    .map(Some)
}

/// [`resolve_document_array`] on dataflow-rs 4: an optional array of
/// documents rendered for this call into BSON, `None` when unset or authored
/// `null`.
///
/// # Errors
///
/// As [`document_array_from_value`].
pub(super) fn rendered_document_array(
    ctx: &Ctx<'_>,
    doc: Option<&Interpolated>,
    field: &str,
    handler_name: &str,
) -> Result<Option<Vec<Document>>, HandlerError> {
    let Some(doc) = doc.filter(|doc| !doc.raw().is_null()) else {
        return Ok(None);
    };
    rendered_bson(
        ctx,
        doc,
        |rendered| match bson::to_bson(rendered) {
            Ok(Bson::Array(items)) => items
                .into_iter()
                .map(|item| match item {
                    Bson::Document(doc) => Some(doc),
                    _ => None,
                })
                .collect(),
            _ => None,
        },
        |folded| document_array_from_value(folded, field, handler_name),
    )
    .map(Some)
}

/// The refusal of a document field an op requires and the task leaves unset
/// or `null`, in 1.12's words.
pub(super) fn requires(field: &str, handler_name: &str) -> HandlerError {
    HandlerError::new(
        ErrorClass::CallerInput,
        format!("{handler_name} requires '{field}' field"),
    )
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
///
/// A document that will not serialize is an error naming its index, not a gap
/// in the array. This used to be `filter_map(… .ok())`, which dropped it: the
/// caller got a shorter result set with nothing saying so, and the row count
/// silently disagreed with the database. The SQL side settled the same question
/// the other way — see [`super::connector_helpers::decode_failure`], whose
/// whole point is that a value with no JSON form must say so rather than
/// vanish.
///
/// # Errors
///
/// A [`HandlerError`] naming the offending document's index.
pub(super) fn docs_to_json(docs: &[Document], handler_name: &str) -> Result<Value, HandlerError> {
    docs.iter()
        .enumerate()
        .map(|(i, doc)| {
            serde_json::to_value(doc).map_err(|e| {
                HandlerError::new(
                    ErrorClass::CallerInput,
                    format!("{handler_name}: document [{i}] has no JSON representation: {e}"),
                )
            })
        })
        .collect::<Result<Vec<Value>, _>>()
        .map(Value::Array)
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

// ============================================================
// Driver-error classification (#297)
// ============================================================

/// MongoDB's duplicate-key error codes. `11000` is the one a modern server
/// reports for a unique-index violation; `11001` is the legacy update spelling
/// still seen from older deployments.
const DUPLICATE_KEY_CODES: [i32; 2] = [11000, 11001];

/// A MongoDB driver failure as a task error, with a unique-index violation
/// classified as one.
///
/// Every Mongo write used to reach [`to_exec_error`], so a duplicate key was a
/// 500 while the *identical* condition on SQL was a 409: `QueryFailure`'s
/// integrity classification is reachable only through `From<sqlx::Error>`, and
/// nothing mapped the document stores onto it. `#297` exists so an endpoint
/// whose job is to answer 409 for a resubmission can; that reasoning does not
/// stop at the SQL connectors.
///
/// [`to_exec_error`]: super::connector_helpers::to_exec_error
pub(super) fn mongo_error(e: mongodb::error::Error) -> HandlerError {
    if let mongodb::error::ErrorKind::Write(mongodb::error::WriteFailure::WriteError(we)) =
        e.kind.as_ref()
        && DUPLICATE_KEY_CODES.contains(&we.code)
    {
        return HandlerError::from(crate::errors::service::integrity(
            crate::errors::IntegrityKind::Unique,
            e.to_string(),
        ));
    }
    super::connector_helpers::to_exec_error(e)
}

/// The integrity error for a bulk write that applied **nothing** because every
/// item the driver reported hit a unique index.
///
/// `None` when anything landed, when any failure was something else, or when
/// there are no per-item failures to judge — all three are cases where a 409
/// would be a worse answer than the bulk envelope, which names exactly what
/// happened to each item.
pub(super) fn all_duplicate_key(failed: &[(usize, Value)], what: &str) -> Option<HandlerError> {
    if failed.is_empty() {
        return None;
    }
    let is_duplicate = |detail: &Value| {
        detail
            .get("code")
            .and_then(Value::as_i64)
            .and_then(|c| i32::try_from(c).ok())
            .is_some_and(|c| DUPLICATE_KEY_CODES.contains(&c))
    };
    if !failed.iter().all(|(_, detail)| is_duplicate(detail)) {
        return None;
    }
    let first = failed
        .first()
        .map(|(_, d)| d.to_string())
        .unwrap_or_default();
    Some(HandlerError::from(crate::errors::service::integrity(
        crate::errors::IntegrityKind::Unique,
        format!("{what} failed, no documents were written: {first}"),
    )))
}

/// Test-only: the connectors the Mongo trio's tests reach, and a pool cache
/// as the pure tests build one. Nothing here reaches a server: `docs` names
/// a closed loopback port, so a call that passes every gate waits in the
/// driver's server selection until its connector's `query_timeout_ms`
/// (100 ms) ends it with 1.12's `TIMEOUT_ERROR`.
#[cfg(test)]
pub(super) mod testkit {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use crate::connector::ConnectorRegistry;
    use crate::connector::mongo_pool::MongoPoolCache;

    /// The registry the tests resolve through: `docs`, a MongoDB connector
    /// to a closed port with `extra` merged in; `sql`, a `db` connector with
    /// a SQL connection string; `api`, an HTTP connector.
    pub(crate) async fn connectors(extra: Value) -> Arc<ConnectorRegistry> {
        let mut docs = json!({
            "type": "db",
            "connection_string": "mongodb://127.0.0.1:1",
            "allow_private_urls": true,
            "query_timeout_ms": 100,
        });
        if let (Some(docs), Some(extra)) = (docs.as_object_mut(), extra.as_object()) {
            docs.extend(extra.clone());
        }
        crate::engine::functions::twin::connectors(&[
            ("docs", docs),
            (
                "sql",
                json!({"type": "db", "connection_string": "postgres://127.0.0.1:1/app"}),
            ),
            ("api", json!({"type": "http", "url": "http://127.0.0.1:1"})),
        ])
        .await
    }

    /// A pool cache of its own.
    pub(crate) fn pools() -> Arc<MongoPoolCache> {
        Arc::new(MongoPoolCache::new(2))
    }
}

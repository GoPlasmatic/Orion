//! Shared plumbing for the raw-native MongoDB handlers (`mongo_read`,
//! `mongo_write`, `mongo_aggregate`) — #263.
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

use super::connector_helpers::{is_mongo, require_db_connector, resolve_value};
use crate::connector::{ConnectorConfig, DbConnectorConfig};

/// Extract the Db connector config and require a MongoDB connection string.
///
/// `require_db_connector` only checks the `ConnectorConfig` variant, and both
/// SQL and MongoDB connectors are `Db` — without the scheme check a SQL
/// connector reached the Mongo driver and produced an opaque driver error
/// instead of a validation one (F29).
pub(super) fn require_mongo_connector<'a>(
    config: &'a ConnectorConfig,
    handler_name: &str,
    connector_name: &str,
) -> Result<&'a DbConnectorConfig, DataflowError> {
    let db_config = require_db_connector(config, connector_name)?;
    if !is_mongo(&db_config.connection_string) {
        return Err(DataflowError::Validation(format!(
            "{handler_name} requires a MongoDB connector, but '{connector_name}' has a \
             non-MongoDB connection string (expected a mongodb:// or mongodb+srv:// URL)"
        )));
    }
    Ok(db_config)
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

/// [`resolve_document`] for a field holding an **array** of documents.
///
/// One implementation for `insert_many`'s `documents`, `mongo_aggregate`'s
/// `pipeline` and `mongo_write`'s `array_filters`, which each carried their own
/// copy. Extended-JSON support (`$oid`, `$date`, …) therefore falls out for
/// every one of them, which is what makes a typed date comparison inside an
/// array filter work.
///
/// `None` when the field is absent or null; the index is named in every error,
/// since a per-message array only exists at runtime and this message is the
/// only diagnosis its author gets.
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
    let mut docs = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if !item.is_object() {
            return Err(DataflowError::Validation(format!(
                "{handler_name} {field}[{i}] must be an object"
            )));
        }
        docs.push(bson::to_document(item).map_err(|e| {
            DataflowError::Validation(format!("{handler_name} {field}[{i}] is not valid: {e}"))
        })?);
    }
    Ok(Some(docs))
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
    input: &Value,
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

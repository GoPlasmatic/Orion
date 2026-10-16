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

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use dataflow_rs_v3::engine::error::DataflowError;
use dataflow_rs_v3::engine::task_context::TaskContext;
use dataflow_rs_v3::engine::task_outcome::TaskOutcome;
use mongodb::bson::Document;
use serde_json::{Map, Value};

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, QueryBudget, require_op_allowed, resolve_bool, resolve_bool_or, to_connect_error,
};
use super::data_write::{bulk_result, mongo_write_errors};
use super::mongo_common::{
    all_duplicate_key, delete_envelope, empty_insert_envelope, mongo_error, rendered_document,
    rendered_document_array, require_mongo_backend, requires, resolve_document,
    resolve_document_array, update_envelope as build_update_envelope,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::{Scope, TemplatedInput};
use crate::config::WriteConfig;
use crate::connector::mongo_pool::MongoPoolCache;
use crate::connector::{ConnectorRegistry, DbConnectorConfig};
use crate::engine::HandlerError;
use crate::engine::kit::{self, read};
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

/// A resolved write, with the two facts the gate needs kept alongside it: the
/// operation and whether it upserts together decide *which* gate applies, and
/// a gate chosen after the driver call would not be one.
///
/// Owned by the dataflow-rs 3 handler, which reads the names off the authored
/// input per call; the names borrowed from the compiled config by the
/// dataflow-rs 4 function.
#[derive(Debug)]
pub struct MongoWrite<'a> {
    database: Cow<'a, str>,
    collection: Cow<'a, str>,
    op: MongoOp,
    upsert: bool,
    write: Prepared,
}

impl<'a> MongoWrite<'a> {
    /// The write a task states once its op and switches are read: the op's
    /// documents read from `docs`, then the shared write guards, before
    /// anything touches the network. Both engines' parse, from here on.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        database: Cow<'a, str>,
        collection: Cow<'a, str>,
        op: MongoOp,
        upsert: bool,
        ordered: bool,
        all: bool,
        docs: &impl OpDocuments,
        write_config: &WriteConfig,
    ) -> Result<Self, HandlerError> {
        let write = prepare(op, docs, upsert, ordered)?;
        guard_unfiltered(op, &write, all, write_config)?;
        if let Prepared::InsertMany { docs, .. } = &write
            && docs.len() as u64 > write_config.max_rows
        {
            return Err(DataflowError::from(WriteError::TooManyRows {
                requested: docs.len(),
                max: write_config.max_rows,
            })
            .into());
        }
        Ok(Self {
            database,
            collection,
            op,
            upsert,
            write,
        })
    }
}

/// The refusal of an `op` outside the value set, in 1.12's words.
fn unknown_op() -> HandlerError {
    DataflowError::Validation(format!("{NAME} 'op' must be one of {}", MongoOp::VALUES)).into()
}

/// The connector's gates for a write: insert / update / upsert / delete are
/// separately gateable, and which one this task is depends on the envelope it
/// parsed; then a MongoDB backend. Shared by the dataflow-rs 3 handler and
/// the dataflow-rs 4 function.
fn gate(
    write: &MongoWrite<'_>,
    conn: &DbConnectorConfig,
    connector: &str,
) -> Result<(), HandlerError> {
    require_op_allowed(&conn.operations, write.op.gate(write.upsert), connector)?;
    Ok(require_mongo_backend(conn, NAME, connector)?)
}

/// The write's answer and its outcome, `Success` or F28's partial 207,
/// against a resolved and gated connector. Shared by the dataflow-rs 3
/// handler and the dataflow-rs 4 function. `run_deadline` is the deadline of
/// the run the call belongs to, which the dataflow-rs 4 function passes on
/// ([`QueryBudget::within`]).
async fn write_documents(
    pool_cache: &MongoPoolCache,
    connector: &str,
    db_config: &DbConnectorConfig,
    parsed: MongoWrite<'_>,
    run_deadline: Option<dataflow_rs::run::Instant>,
) -> Result<(Value, TaskOutcome), HandlerError> {
    let client = pool_cache
        .get_client(connector, db_config)
        .await
        .map_err(to_connect_error)?;
    let coll = client
        .database(&parsed.database)
        .collection::<Document>(&parsed.collection);

    // F11: one wall-clock bound over connect + write.
    Ok(
        QueryBudget::within(db_config.query_timeout_ms, run_deadline)
            .run(NAME, async {
                execute_write(&coll, parsed.write)
                    .await
                    .map_err(|e| e.to_string())
            })
            .await?,
    )
}

#[async_trait]
impl ConnectorHandler for MongoWriteHandler {
    const NAME: &'static str = "mongo_write";
    type Kind = crate::connector::kind::Db;
    type Input = TemplatedInput;
    type Parsed = MongoWrite<'static>;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &Scope<'_, '_>,
    ) -> Result<Self::Parsed, HandlerError> {
        let database = call.require_str(input, "database")?.to_string();
        let collection = call.require_str(input, "collection")?.to_string();
        let op = MongoOp::parse(call.require_str(input, "op")?).ok_or_else(unknown_op)?;
        let upsert = resolve_bool(input, "upsert", <Self as ConnectorHandler>::NAME, ctx)?;
        let ordered = resolve_bool_or(
            input,
            "ordered",
            <Self as ConnectorHandler>::NAME,
            ctx,
            true,
        )?;

        MongoWrite::assemble(
            Cow::Owned(database),
            Cow::Owned(collection),
            op,
            upsert,
            ordered,
            literal_bool(input.raw(), "all"),
            &Folded {
                input: input.raw(),
                ctx,
            },
            &self.write_config,
        )
    }

    fn gate(
        parsed: &Self::Parsed,
        conn: &DbConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        gate(parsed, conn, connector)
    }

    async fn run(
        &self,
        parsed: Self::Parsed,
        db_config: &DbConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        let (result, outcome) =
            write_documents(&self.pool_cache, call.connector, db_config, parsed, None).await?;
        // F28: a partially applied ordered batch is a 207, not a success —
        // which is the one outcome a connector handler chooses for itself.
        Ok(Produced::with_outcome(result, outcome))
    }
}

/// Where [`prepare`] reads an op's documents: 1.12's fold of the authored
/// input ([`Folded`]), or a dataflow-rs 4 config rendered for the call
/// ([`RenderedDocs`]). Either way `None` is a field left unset or `null`.
trait OpDocuments {
    /// A document field (`document`, `filter`, `update`).
    fn document(&self, field: &'static str) -> Result<Option<Document>, HandlerError>;
    /// An array-of-documents field (`documents`, `array_filters`).
    fn document_array(&self, field: &'static str) -> Result<Option<Vec<Document>>, HandlerError>;
}

/// The authored input, folded against the message as 1.12 folds it.
struct Folded<'a, 'c> {
    input: &'a Value,
    ctx: &'a TaskContext<'c>,
}

impl OpDocuments for Folded<'_, '_> {
    fn document(&self, field: &'static str) -> Result<Option<Document>, HandlerError> {
        Ok(resolve_document(self.input, field, NAME, self.ctx)?)
    }

    fn document_array(&self, field: &'static str) -> Result<Option<Vec<Document>>, HandlerError> {
        Ok(resolve_document_array(self.input, field, NAME, self.ctx)?)
    }
}

/// A dataflow-rs 4 config's documents, rendered for the call into BSON.
struct RenderedDocs<'a, 'r> {
    cfg: &'a MongoWriteConfig,
    ctx: &'a Ctx<'r>,
}

impl RenderedDocs<'_, '_> {
    /// The config's document named `field`.
    fn field(&self, field: &str) -> Option<&Interpolated> {
        match field {
            "document" => self.cfg.document.as_ref(),
            "documents" => self.cfg.documents.as_ref(),
            "filter" => self.cfg.filter.as_ref(),
            "update" => self.cfg.update.as_ref(),
            "array_filters" => self.cfg.array_filters.as_ref(),
            other => unreachable!("`prepare` reads no document named `{other}`"),
        }
    }
}

impl OpDocuments for RenderedDocs<'_, '_> {
    fn document(&self, field: &'static str) -> Result<Option<Document>, HandlerError> {
        rendered_document(self.ctx, self.field(field), field, NAME)
    }

    fn document_array(&self, field: &'static str) -> Result<Option<Vec<Document>>, HandlerError> {
        rendered_document_array(self.ctx, self.field(field), field, NAME)
    }
}

/// A fully resolved write, ready for the driver call.
#[derive(Debug)]
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

/// Resolve the op-conditional fields (each an extended-JSON document, its
/// `{"var": ..}` nodes filled from the message) and enforce their shape
/// rules.
fn prepare(
    op: MongoOp,
    docs: &impl OpDocuments,
    upsert: bool,
    ordered: bool,
) -> Result<Prepared, HandlerError> {
    let required = |field: &'static str| -> Result<Document, HandlerError> {
        docs.document(field)?
            .ok_or_else(|| requires(field, NAME).into())
    };
    Ok(match op {
        MongoOp::InsertOne => Prepared::InsertOne {
            doc: required("document")?,
        },
        MongoOp::InsertMany => Prepared::InsertMany {
            docs: docs
                .document_array("documents")?
                .ok_or_else(|| HandlerError::from(requires("documents", NAME)))?,
            ordered,
        },
        MongoOp::UpdateOne | MongoOp::UpdateMany => {
            let update = required("update")?;
            require_update_operators(&update)?;
            let array_filters = docs.document_array("array_filters")?;
            if let Some(ref filters) = array_filters {
                require_usable_array_filters(filters, &update)?;
            }
            Prepared::Update {
                filter: resolve_filter(op, docs)?,
                update,
                upsert,
                many: op == MongoOp::UpdateMany,
                array_filters,
            }
        }
        MongoOp::ReplaceOne => {
            let doc = required("document")?;
            if let Some(key) = doc.keys().find(|k| k.starts_with('$')) {
                return Err(DataflowError::Validation(format!(
                    "{NAME} {}",
                    replace_plain_message(key)
                ))
                .into());
            }
            Prepared::Replace {
                filter: resolve_filter(op, docs)?,
                doc,
                upsert,
            }
        }
        MongoOp::DeleteOne | MongoOp::DeleteMany => Prepared::Delete {
            filter: resolve_filter(op, docs)?,
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
fn resolve_filter(op: MongoOp, docs: &impl OpDocuments) -> Result<Document, HandlerError> {
    debug_assert!(op.takes_filter());
    Ok(docs.document("filter")?.unwrap_or_default())
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
            Err(e) => Err(mongo_error(e)),
        },
        Prepared::InsertMany { docs, ordered } => {
            if docs.is_empty() {
                return Ok((empty_insert_envelope(), TaskOutcome::Success));
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
                    Some(failed) => {
                        let outcome = if ordered {
                            insert_outcome(sent, &failed)
                        } else {
                            unordered_insert_outcome(sent, &failed)
                        };
                        // #297, as on `data_write`'s Mongo branch: nothing
                        // landed and every item hit a unique index is the 409
                        // condition, not a 500. A partial batch stays a 207.
                        if outcome.nothing_applied()
                            && let Some(err) = all_duplicate_key(&failed, "MongoDB insert")
                        {
                            return Err(err);
                        }
                        bulk_result(outcome, "MongoDB insert")
                    }
                    None => Err(mongo_error(e)),
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
                Ok(r) => Ok((delete_envelope(r.deleted_count), TaskOutcome::Success)),
                Err(e) => Err(mongo_error(e)),
            }
        }
    }
}

/// The update/replace result, with the driver error mapped. The envelope
/// itself is `mongo_common`'s, shared with `data_write`'s Mongo branch.
fn update_envelope(
    res: mongodb::error::Result<mongodb::results::UpdateResult>,
) -> Result<(Value, TaskOutcome), DataflowError> {
    match res {
        Ok(r) => Ok((build_update_envelope(&r), TaskOutcome::Success)),
        Err(e) => Err(mongo_error(e)),
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

// -- dataflow-rs 4 --

/// `mongo_write`'s input on dataflow-rs 4, in its field table's order (I179).
///
/// The connector, the database, the collection and the op are literals,
/// required at load (I183). The five documents are documents of `var` holes,
/// `$` keys kept as data, as 1.12 folded them. `upsert` and `ordered` are
/// expressions, as 1.12 evaluated them (its connector shell compiled both as
/// templates), read by `resolve_bool`'s and `resolve_bool_or`'s twins; `all`
/// is a literal boolean, as 1.12 read it. Its `check` holds 1.12's
/// authoring-time rules at load.
#[derive(Debug, FunctionConfig)]
#[config(check = check)]
pub struct MongoWriteConfig {
    /// The MongoDB connector written through.
    connector: String,
    /// The database.
    database: String,
    /// The collection.
    collection: String,
    /// The write: `insert_one`, `insert_many`, `update_one`, `update_many`,
    /// `replace_one`, `delete_one` or `delete_many`.
    op: String,
    /// The document of an `insert_one` or a `replace_one`.
    document: Option<Interpolated>,
    /// The documents of an `insert_many`.
    documents: Option<Interpolated>,
    /// The selection filter of an update, a replace or a delete.
    filter: Option<Interpolated>,
    /// The update document, of atomic operators.
    update: Option<Interpolated>,
    /// The filters for the update's `$[identifier]` paths.
    array_filters: Option<Interpolated>,
    /// Insert when nothing matches; gated as `upsert`.
    upsert: Option<Expr<OwnedDataValue>>,
    /// `insert_many`: stop at the first failure (the default) or attempt
    /// every document.
    ordered: Option<Expr<OwnedDataValue>>,
    /// Acknowledges an unfiltered update, replace or delete.
    all: Option<bool>,
    /// Where the write's answer is written.
    #[output(default = "data")]
    output: Output,
}

/// 1.12's authoring-time rules for `mongo_write` ([`validate_static_input`],
/// by which create, update, import, validate and `lint` refused a task), on
/// the config at load: each refusal at the field 1.12 named, in its words.
/// The input judged is the config's, each field as authored, so the two
/// cannot drift.
fn check(cfg: &MongoWriteConfig, cx: &mut CompileCx<'_>) {
    let mut input = Map::new();
    input.insert("op".to_string(), Value::String(cfg.op.clone()));
    for (field, doc) in [
        ("document", &cfg.document),
        ("documents", &cfg.documents),
        ("filter", &cfg.filter),
        ("update", &cfg.update),
        ("array_filters", &cfg.array_filters),
    ] {
        if let Some(doc) = doc {
            input.insert(field.to_string(), doc.raw().clone());
        }
    }
    for (field, expr) in [("upsert", &cfg.upsert), ("ordered", &cfg.ordered)] {
        if let Some(expr) = expr {
            input.insert(field.to_string(), expr.raw().clone());
        }
    }
    if let Some(all) = cfg.all {
        input.insert("all".to_string(), Value::Bool(all));
    }
    super::mongo_common::report_static(validate_static_input(&input), cx);
}

/// A switch for this call, `resolve_bool_or`'s reading: `default` when
/// unset or `null`, a boolean as itself, anything else refused.
fn switch(
    ctx: &Ctx<'_>,
    expr: Option<&Expr<OwnedDataValue>>,
    field: &str,
    default: bool,
) -> Result<bool, HandlerError> {
    let value = expr.map(|expr| ctx.eval(expr)).transpose()?;
    read::bool_or(value.as_deref(), field, NAME, default)
}

/// `mongo_write` as a dataflow-rs 4 function: the write `MongoWriteHandler`
/// makes, read in the same order, a partially applied `insert_many`
/// answering `Outcome::Status(207)`.
pub struct MongoWriteFunction {
    pub pool_cache: Arc<MongoPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// The `[write]` safety bounds (`max_rows`, `allow_unfiltered`).
    pub write_config: WriteConfig,
}

impl MongoWriteFunction {
    /// The write a config states, its documents rendered for this call:
    /// `MongoWriteHandler::parse`'s reading, in its order, the names
    /// borrowed.
    fn write<'c>(
        &self,
        ctx: &Ctx<'_>,
        cfg: &'c MongoWriteConfig,
    ) -> Result<MongoWrite<'c>, HandlerError> {
        // The check refuses an op outside the set at load; this is 1.12's
        // refusal for one that reaches a call regardless.
        let op = MongoOp::parse(&cfg.op).ok_or_else(unknown_op)?;
        let upsert = switch(ctx, cfg.upsert.as_ref(), "upsert", false)?;
        let ordered = switch(ctx, cfg.ordered.as_ref(), "ordered", true)?;
        MongoWrite::assemble(
            Cow::Borrowed(&cfg.database),
            Cow::Borrowed(&cfg.collection),
            op,
            upsert,
            ordered,
            cfg.all.unwrap_or(false),
            &RenderedDocs { cfg, ctx },
            &self.write_config,
        )
    }
}

impl Function for MongoWriteFunction {
    type Config = MongoWriteConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &MongoWriteConfig,
    ) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, then `parse`'s order.
        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
        let write = self.write(ctx, cfg)?;
        let run_deadline = ctx.deadline();
        let mut answer = None;
        let done = kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let db = kit::require_connector::<crate::connector::kind::Db>(&config, &cfg.connector)?;
            gate(&write, db, &cfg.connector)?;
            let (result, outcome) =
                write_documents(&self.pool_cache, &cfg.connector, db, write, run_deadline).await?;
            answer = Some(result);
            Ok(super::data_write::outcome(outcome))
        })
        .await?;
        if let Some(answer) = answer {
            ctx.set(&output, OwnedDataValue::from(&answer))?;
        }
        // F28: a partially applied ordered batch is a 207, not a success.
        Ok(done)
    }
}

// -- Input schema (F53) --

pub(super) const MONGO_WRITE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the MongoDB connector.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "database",
        description: "Mongo database name.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "collection",
        description: "Mongo collection name.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "op",
        description: "Write operation: insert_one, insert_many, update_one, update_many, replace_one, delete_one, or delete_many.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "document",
        description: "The document for insert_one / replace_one (extended JSON; nested arrays/objects pass through). Accepts {\"var\": \"path\"} at any depth.",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "documents",
        description: "Array of documents for insert_many; batch size is capped by write.max_rows. Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Array,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "filter",
        description: "Selection filter for update/replace/delete ops (extended JSON: $oid, $date, ... work). An empty filter requires \"all\": true and write.allow_unfiltered. Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "update",
        description: "Update document for update_one/update_many; top-level keys must be atomic operators ($set, $inc, $push, ...). Field paths may target array elements with $ (first match), $[] (every element) or $[identifier] (every element an 'array_filters' entry matches). Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
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
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "upsert",
        description: "Insert when no document matches (update_one/update_many/replace_one). Gated as 'upsert' on the connector when true. Defaults to false.",
        kind: FieldKind::Bool,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "ordered",
        description: "insert_many only: stop at the first failure (true, default) or attempt every document (false).",
        kind: FieldKind::Bool,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "all",
        description: "Acknowledge an intentionally unfiltered update/replace/delete (affects every document; also requires write.allow_unfiltered).",
        kind: FieldKind::Bool,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the write result is written. Defaults to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().expect("test input is an object").clone()
    }

    fn mongo_connector(
        gates: crate::connector::OperationGates,
    ) -> crate::connector::DbConnectorConfig {
        crate::connector::DbConnectorConfig {
            connection_string: "mongodb://localhost:27017".to_string(),
            max_connections: None,
            connect_timeout_ms: None,
            query_timeout_ms: None,
            allow_private_urls: true,
            operations: gates,
            dialect: Default::default(),
            aggregate_write_stages: false,
        }
    }

    /// Parse a task the way the shell does, so `gate` can be asked about it.
    fn parse(input: Value) -> MongoWrite<'static> {
        let handler = MongoWriteHandler {
            pool_cache: Arc::new(crate::connector::mongo_pool::MongoPoolCache::new(2)),
            registry: Arc::new(ConnectorRegistry::new(Default::default())),
            write_config: Default::default(),
        };
        let datalogic = Arc::new(dataflow_rs_v3::datalogic_rs::Engine::new());
        let mut message = dataflow_rs_v3::Message::from_value(&json!({}));
        let ctx = TaskContext::new(&mut message, &datalogic);
        let call = ConnectorCall {
            name: MongoWriteHandler::NAME,
            connector: "orders",
            channel: "ch".to_string(),
            output: "data".to_string(),
        };
        let input = TemplatedInput::from(input);
        crate::engine::functions::templated_input::with_scope(&ctx, |scope| {
            ConnectorHandler::parse(&handler, &call, &input, scope)
        })
        .expect("the task parses")
    }

    /// The gate `mongo_write` picks is not fixed: an update that upserts is
    /// gated as `upsert`, so a connector that permits updates and refuses
    /// upserts refuses this one and allows the same task without `upsert`.
    ///
    /// This is the reason `gate` takes the parsed input, and it is testable
    /// in-process for the same reason `mongo_aggregate`'s is: the answer is a
    /// function of the task and the connector, not of a live server.
    #[test]
    fn an_upsert_is_gated_as_an_upsert_not_an_update() {
        let update_only = crate::connector::OperationGates {
            upsert: false,
            ..Default::default()
        };
        let task = |upsert: bool| {
            json!({
                "connector": "orders", "database": "db", "collection": "orders",
                "op": "update_one", "upsert": upsert,
                "filter": { "_id": "1" }, "update": { "$set": { "status": "paid" } },
            })
        };

        let err = <MongoWriteHandler as ConnectorHandler>::gate(
            &parse(task(true)),
            &mongo_connector(update_only.clone()),
            "orders",
        )
        .expect_err("an upsert must hit the upsert gate, which is off");
        let detail = err.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("'upsert'"),
            "the refusal must name the gate it hit: {detail:?}"
        );

        assert!(
            <MongoWriteHandler as ConnectorHandler>::gate(
                &parse(task(false)),
                &mongo_connector(update_only),
                "orders",
            )
            .is_ok(),
            "the same update without upsert is gated as 'update', which is on"
        );
    }

    /// A `db` connector holding a SQL connection string is the right type and
    /// the wrong backend — the check that outlived `require_mongo_connector`.
    #[test]
    fn a_sql_connection_string_is_refused_by_the_gate() {
        let mut conn = mongo_connector(Default::default());
        conn.connection_string = "postgres://localhost/app".to_string();
        let task = json!({
            "connector": "orders", "database": "db", "collection": "orders",
            "op": "insert_one", "document": { "id": 1 },
        });

        let err = <MongoWriteHandler as ConnectorHandler>::gate(&parse(task), &conn, "orders")
            .expect_err("mongo_write cannot speak to a SQL connector");
        assert!(
            err.msg.contains("MongoDB connector"),
            "the refusal must say which backend is required: {:?}",
            err.msg
        );
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

    // -- dataflow-rs 4: the config and its check, the seams, and twins of the
    // handler --
    //
    // No MongoDB is reached. A probe of each engine writes the write it
    // parsed, past the connector and its gate, where the driver would be
    // called; the functions themselves run against `testkit`'s connector to
    // a closed port, where a call that passes every gate waits in the
    // driver's server selection until the connector's `query_timeout_ms`.

    use crate::engine::functions::connector_handler::Connector;
    use crate::engine::functions::mongo_common::testkit;
    use crate::engine::functions::twin::{self, Seen};
    use crate::errors::kind::CONNECTOR_DETAIL;
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use dataflow_rs::run::ErrorKind;
    use std::time::Duration;

    /// At most three documents per `insert_many`, unfiltered writes off
    /// unless a test turns them on.
    fn write_config(allow_unfiltered: bool) -> WriteConfig {
        WriteConfig {
            max_rows: 3,
            allow_unfiltered,
        }
    }

    fn handler(registry: Arc<ConnectorRegistry>, write_config: WriteConfig) -> MongoWriteHandler {
        MongoWriteHandler {
            pool_cache: testkit::pools(),
            registry,
            write_config,
        }
    }

    fn function(registry: Arc<ConnectorRegistry>, write_config: WriteConfig) -> MongoWriteFunction {
        MongoWriteFunction {
            pool_cache: testkit::pools(),
            registry,
            write_config,
        }
    }

    /// A task on the `docs` connector's `db.orders`, with `rest`.
    fn task(rest: Value) -> Value {
        let mut input = json!({"connector": "docs", "database": "db", "collection": "orders"});
        if let (Some(input), Some(rest)) = (input.as_object_mut(), rest.as_object()) {
            input.extend(rest.clone());
        }
        input
    }

    /// One task through each engine's real function, each over a registry
    /// of its own, `docs` configured with `extra`.
    async fn both(input: Value, data: Value, extra: Value) -> (Seen, Seen) {
        let v3 = twin::v3(
            NAME,
            Box::new(Connector(handler(
                testkit::connectors(extra.clone()).await,
                write_config(false),
            ))),
            input.clone(),
            data.clone(),
        )
        .await;
        let v4 = twin::v4(
            NAME,
            function(testkit::connectors(extra).await, write_config(false)),
            input,
            data,
        )
        .await;
        (v3, v4)
    }

    /// Writes the write 1.12's handler parsed, at `output`: its prologue,
    /// its parse, the connector and its gate, and no driver call.
    struct WriteV3(MongoWriteHandler);

    #[async_trait]
    impl ConnectorHandler for WriteV3 {
        const NAME: &'static str = "mongo_write";
        type Kind = crate::connector::kind::Db;
        type Input = TemplatedInput;
        type Parsed = MongoWrite<'static>;

        fn registry(&self) -> &Arc<ConnectorRegistry> {
            &self.0.registry
        }

        fn parse(
            &self,
            call: &ConnectorCall<'_>,
            input: &TemplatedInput,
            ctx: &Scope<'_, '_>,
        ) -> Result<Self::Parsed, HandlerError> {
            self.0.parse(call, input, ctx)
        }

        fn gate(
            parsed: &Self::Parsed,
            conn: &DbConnectorConfig,
            connector: &str,
        ) -> Result<(), HandlerError> {
            MongoWriteHandler::gate(parsed, conn, connector)
        }

        async fn run(
            &self,
            write: Self::Parsed,
            _db_config: &DbConnectorConfig,
            _call: &ConnectorCall<'_>,
            _input: &TemplatedInput,
            _ctx: &mut TaskContext<'_>,
        ) -> Result<Produced, HandlerError> {
            Ok(Value::String(format!("{write:?}")).into())
        }
    }

    /// The same, through the function's own reading.
    struct WriteV4(MongoWriteFunction);

    impl Function for WriteV4 {
        type Config = MongoWriteConfig;

        async fn call(
            &self,
            ctx: &mut Ctx<'_>,
            cfg: &MongoWriteConfig,
        ) -> dataflow_rs::Result<Outcome> {
            let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
            let write = self.0.write(ctx, cfg)?;
            let config = kit::resolve_connector(&self.0.registry, &cfg.connector).await?;
            let db = kit::require_connector::<crate::connector::kind::Db>(&config, &cfg.connector)?;
            gate(&write, db, &cfg.connector)?;
            let seen = OwnedDataValue::String(format!("{write:?}"));
            ctx.set(&output, seen)?;
            Ok(Outcome::Ok)
        }
    }

    /// The write each engine parses from `input` over `data`.
    async fn parsed(input: Value, data: Value, write_config: WriteConfig) -> (Seen, Seen) {
        let v3 = twin::v3(
            NAME,
            Box::new(Connector(WriteV3(handler(
                testkit::connectors(json!({})).await,
                write_config.clone(),
            )))),
            input.clone(),
            data.clone(),
        )
        .await;
        let v4 = twin::v4(
            NAME,
            WriteV4(function(testkit::connectors(json!({})).await, write_config)),
            input,
            data,
        )
        .await;
        (v3, v4)
    }

    fn rejected(input: Value) -> Vec<(String, IssueCode)> {
        testing::config_with::<MongoWriteConfig>(&kit::test_exprs(), input)
            .expect_err("refused")
            .into_iter()
            .filter(|i| i.severity == Severity::Rejected)
            .map(|i| (i.path.to_string(), i.code))
            .collect()
    }

    /// Declaration order is the field table's (I179). The five documents
    /// are documents of `var` holes, `$` keys kept as data; `upsert` and
    /// `ordered` are expressions, as 1.12 evaluated them; `all` is a literal
    /// boolean; the connector, the database, the collection and the op are
    /// literals, required (I183); an unknown key is refused (break 2a);
    /// `output` defaults to `data`.
    #[test]
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = MongoWriteConfig::fields()
            .iter()
            .map(|f| &*f.name)
            .collect();
        let table: Vec<&str> = MONGO_WRITE_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

        let update = json!({"$set": {"status": {"var": "data.status"}, "at": {"$date": "2026-01-02T00:00:00Z"}}});
        let cfg = testing::config_with::<MongoWriteConfig>(
            &kit::test_exprs(),
            task(json!({
                "op": "update_many", "filter": {"_id": {"$oid": {"var": "data.id"}}},
                "update": update.clone(), "upsert": {"var": "data.upsert"}, "all": true
            })),
        )
        .expect("compiles");
        let doc = cfg.update.as_ref().expect("set");
        assert_eq!(
            doc.raw(),
            &update,
            "the document as authored, `$` keys kept"
        );
        let holes: Vec<String> = doc.holes().map(ToString::to_string).collect();
        assert_eq!(holes, ["data.status"]);
        let upsert = cfg.upsert.as_ref().expect("set");
        assert_eq!(upsert.raw(), &json!({"var": "data.upsert"}));
        assert!(
            upsert.constant().is_none(),
            "an expression, evaluated per call"
        );
        assert_eq!(cfg.all, Some(true));
        assert_eq!(cfg.op, "update_many");
        let output = cfg.output.constant().map(ToString::to_string);
        assert_eq!(output.as_deref(), Some("data"));

        let named = json!({"connector": "docs", "database": "db", "collection": "c", "op": "delete_many", "all": true});
        for field in ["connector", "database", "collection", "op"] {
            let mut input = named.clone();
            input.as_object_mut().expect("an object").remove(field);
            assert_eq!(
                rejected(input.clone()),
                [(field.to_string(), IssueCode::InputParse)]
            );
            input[field] = json!(7);
            assert_eq!(
                rejected(input),
                [(field.to_string(), IssueCode::InputParse)]
            );
        }
        // A non-boolean `all`, which 1.12 refused at authoring.
        let mut input = named.clone();
        input["all"] = json!("yes");
        assert_eq!(
            rejected(input),
            [("all".to_string(), IssueCode::InputParse)]
        );
        let mut input = named;
        input["pipeline"] = json!([]);
        let issues = rejected(input);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].0, "pipeline");
    }

    /// The config's issues: each Rejected one's path and message, in order.
    fn check_issues(input: &Value) -> Vec<(String, String)> {
        match testing::config_with::<MongoWriteConfig>(&kit::test_exprs(), input.clone()) {
            Ok(_) => Vec::new(),
            Err(issues) => issues
                .into_iter()
                .filter(|i| i.severity == Severity::Rejected)
                .map(|i| {
                    assert_eq!(i.code, IssueCode::InputParse, "{i:?}");
                    (i.path.to_string(), i.message)
                })
                .collect(),
        }
    }

    /// 1.12's authoring-time refusals of the same input, each at the field
    /// it names.
    fn static_refusals(input: &Value) -> Vec<(String, String)> {
        validate_static_input(input.as_object().expect("an object"))
            .into_iter()
            .map(|(field, _code, message)| (field.to_string(), message))
            .collect()
    }

    /// The thirteen `validate_static_input` cases above, through the
    /// config's check: the same refusals, in the same order, each at the
    /// field 1.12 named, and none where 1.12 had none. A refusal is a load
    /// refusal now, a named break: 1.12 refused each of these at create,
    /// update, import, validate and `lint`.
    #[test]
    fn the_check_is_validate_static_input() {
        let cases = [
            // an_unknown_op_is_named_with_the_full_value_set
            json!({"op": "insert"}),
            // op_conditional_requirements_are_authoring_errors
            json!({"op": "update_one"}),
            json!({"op": "insert_one"}),
            json!({"op": "insert_many"}),
            // all_true_stands_in_for_a_missing_filter
            json!({"op": "delete_many", "all": true}),
            // a_field_the_op_ignores_is_refused_not_ignored
            json!({"op": "delete_one", "filter": {"x": 1}, "update": {"$set": {"y": 2}}}),
            // a_plain_key_update_is_told_to_use_operators_or_replace
            json!({"op": "update_one", "filter": {"x": 1}, "update": {"name": "Ada"}}),
            // a_matched_array_filter_set_is_accepted
            json!({"op": "update_many", "filter": {"_id": 1},
                   "update": {"$set": {"sessions.$[s].active": false}},
                   "array_filters": [{"s.expiresAt": {"$lt": 100}}]}),
            // array_filter_mismatches_are_named_before_the_driver_sees_them
            json!({"op": "update_one", "filter": {"_id": 1},
                   "update": {"$set": {"sessions.$[s].active": false}},
                   "array_filters": [{"other.x": 1}]}),
            json!({"op": "update_one", "filter": {"_id": 1},
                   "update": {"$set": {"sessions.$[s].active": false}},
                   "array_filters": [{"s.x": 1}, {"t.y": 2}]}),
            json!({"op": "update_one", "filter": {"_id": 1},
                   "update": {"$set": {"sessions.$.active": false}},
                   "array_filters": [{"s.x": 1}]}),
            json!({"op": "update_one", "filter": {"_id": 1},
                   "update": {"$set": {"sessions.$[].active": false}},
                   "array_filters": [{"s.x": 1}]}),
            json!({"op": "update_one", "filter": {"_id": 1},
                   "update": {"$set": {"s.$[a].x": 1}}, "array_filters": []}),
            json!({"op": "update_one", "filter": {"_id": 1},
                   "update": {"$set": {"s.$[a].x": 1, "t.$[b].y": 2}},
                   "array_filters": [{"a.x": 1, "b.y": 2}]}),
            // a_logical_array_filter_resolves_its_identifier
            json!({"op": "update_many", "filter": {"_id": 1},
                   "update": {"$set": {"sessions.$[s].active": false}},
                   "array_filters": [{"$and": [{"s.a": 1}, {"s.b": 2}]}]}),
            // a_var_array_filter_defers_shape_checks_to_runtime
            json!({"op": "update_one", "filter": {"_id": 1},
                   "update": {"$set": {"sessions.$[s].active": false}},
                   "array_filters": {"var": "temp_data.filters"}}),
            // array_filters_is_refused_on_ops_that_cannot_use_it
            json!({"op": "delete_one", "filter": {"_id": 1}, "document": {"a": 1},
                   "array_filters": [{"s.x": 1}]}),
            json!({"op": "replace_one", "filter": {"_id": 1}, "document": {"a": 1},
                   "array_filters": [{"s.x": 1}]}),
            json!({"op": "insert_one", "filter": {"_id": 1}, "document": {"a": 1},
                   "array_filters": [{"s.x": 1}]}),
            // an_operator_key_in_a_replacement_document_is_refused
            json!({"op": "replace_one", "filter": {"x": 1}, "document": {"$set": {"y": 2}}}),
            // a_var_update_defers_shape_checks_to_runtime
            json!({"op": "update_many", "filter": {"x": 1}, "update": {"var": "temp_data.update"}}),
            // a_valid_task_produces_no_errors
            json!({"op": "update_one", "filter": {"_id": {"$oid": "665f1f77bcf86cd799439011"}},
                   "update": {"$set": {"status": "done"}}, "upsert": true}),
            // And the switches and the empty update, which the cases above
            // leave out: a switch an op does not take, set to an expression;
            // an empty update; a filter left `null`, which is present.
            json!({"op": "insert_one", "document": {"a": 1}, "upsert": {"var": "data.u"},
                   "all": false}),
            json!({"op": "insert_many", "documents": [], "ordered": false}),
            json!({"op": "update_one", "filter": {"x": 1}, "update": {}}),
            json!({"op": "delete_one", "filter": null}),
            // 1.12's quirk, kept: beside a literal update, an `array_filters`
            // element that is a whole `var` hole is read as declaring the
            // identifier `var`, and refused, though the call would fill it.
            json!({"op": "update_one", "filter": {"_id": 1},
                   "update": {"$set": {"sessions.$[s].active": false}},
                   "array_filters": [{"var": "temp_data.filter"}]}),
        ];
        for case in cases {
            let input = task(case.clone());
            let issues = check_issues(&input);
            assert_eq!(issues, static_refusals(&input), "{case}");
        }
        // Spot checks of the twin above, so it cannot pass vacuously.
        let unknown = check_issues(&task(json!({"op": "insert"})));
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].0, "op");
        assert!(unknown[0].1.contains("insert_one/"), "{}", unknown[0].1);
        let both_missing = check_issues(&task(json!({"op": "update_one"})));
        let paths: Vec<&str> = both_missing.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["update", "filter"]);
        let quirk = check_issues(&task(json!({"op": "update_one", "filter": {"_id": 1},
            "update": {"$set": {"sessions.$[s].active": false}},
            "array_filters": [{"var": "temp_data.filter"}]})));
        assert_eq!(quirk.len(), 1, "{quirk:?}");
        assert_eq!(quirk[0].0, "array_filters");
    }

    /// `an_upsert_is_gated_as_an_upsert_not_an_update`, on the function: the
    /// config compiles, and a connector that permits updates and refuses
    /// upserts refuses the task that upserts, whether `upsert` is written as
    /// `true` or computed from the message, and lets the same update without
    /// it through to the driver; each as 1.12's handler did.
    #[tokio::test]
    async fn on_v4_an_upsert_is_gated_as_an_upsert_not_an_update() {
        let update_only = json!({"operations": {"upsert": false}});
        let update = |upsert: Value| {
            task(json!({
                "op": "update_one", "upsert": upsert,
                "filter": {"_id": "1"}, "update": {"$set": {"status": "paid"}}
            }))
        };
        for upsert in [json!(true), json!({"var": "data.upsert"})] {
            testing::config_with::<MongoWriteConfig>(&kit::test_exprs(), update(upsert.clone()))
                .expect("compiles");
            let (v3, v4) = both(
                update(upsert.clone()),
                json!({"upsert": true}),
                update_only.clone(),
            )
            .await;
            assert_eq!(v4, v3, "{upsert}");
            let refused = v4.expect_err("the upsert gate is off");
            assert_eq!(refused.code, CONNECTOR_DETAIL, "{upsert}");
            let detail = refused.detail.as_deref().unwrap_or_default();
            assert!(detail.contains("'upsert'"), "the gate it hit: {detail:?}");
        }
        for upsert in [json!(false), json!({"var": "data.upsert"})] {
            let (v3, v4) = both(
                update(upsert.clone()),
                json!({"upsert": false}),
                update_only.clone(),
            )
            .await;
            assert_eq!(v4, v3, "{upsert}");
            assert_eq!(
                v4.expect_err("past the gate, the driver times out").code,
                "TIMEOUT_ERROR",
                "{upsert}"
            );
        }
    }

    /// `a_sql_connection_string_is_refused_by_the_gate`, on the function: a
    /// `db` connector holding a SQL connection string is the right type and
    /// the wrong backend.
    #[tokio::test]
    async fn on_v4_a_sql_connection_string_is_refused_by_the_gate() {
        let mut input = task(json!({"op": "insert_one", "document": {"id": 1}}));
        input["connector"] = json!("sql");
        testing::config_with::<MongoWriteConfig>(&kit::test_exprs(), input.clone())
            .expect("compiles");
        let (v3, v4) = both(input, Value::Null, json!({})).await;
        assert_eq!(v4, v3);
        let refused = v4.expect_err("mongo_write cannot speak to a SQL connector");
        assert!(
            refused.message.contains("MongoDB connector"),
            "the backend required: {refused:?}"
        );
    }

    /// The write each engine would send, for every op: its documents
    /// rendered into BSON with their holes filled and their extended JSON
    /// read, its switches evaluated; and every refusal the cases above leave
    /// to the call, in 1.12's order, with its code and text.
    #[tokio::test]
    async fn the_write_is_read_as_on_v3() {
        let data = json!({
            "id": "665f1f77bcf86cd799439011", "n": 3, "status": "paid", "flag": true,
            "text": "yes", "tags": ["a"], "doc": {"a": 1, "b": {"c": [1, 2]}},
            "docs": [{"a": 1}, {"a": 2}], "many": [{"a": 1}, {"a": 2}, {"a": 3}, {"a": 4}],
            "plain": {"name": "Ada"}, "empty": {}, "dollar": {"$set": {"a": 1}},
            "filters": [{"t.x": 1}], "when": "2026-01-02T03:04:05Z"
        });
        for (rest, ok) in [
            (
                json!({"op": "insert_one", "document": {
                    "_id": {"$oid": {"var": "data.id"}}, "at": {"$date": {"var": "data.when"}},
                    "n": {"var": "data.n"}, "missing": {"var": ["data.none", "default"]}
                }}),
                true,
            ),
            (
                json!({"op": "insert_one", "document": {"var": "data.doc"}}),
                true,
            ),
            (json!({"op": "insert_one", "document": null}), false),
            (json!({"op": "insert_one", "document": "x"}), false),
            (
                json!({"op": "insert_one", "document": {"var": "data.tags"}}),
                false,
            ),
            (
                json!({"op": "insert_one", "document": {"$oid": {"var": "data.id"}}}),
                false,
            ),
            (
                json!({"op": "insert_many", "documents": [{"a": {"var": "data.n"}}, {"var": "data.doc"}],
                       "ordered": {"var": "data.flag"}}),
                true,
            ),
            (
                json!({"op": "insert_many", "documents": {"var": "data.docs"}, "ordered": null}),
                true,
            ),
            (json!({"op": "insert_many", "documents": []}), true),
            (
                json!({"op": "insert_many", "documents": {"var": "data.doc"}}),
                false,
            ),
            (json!({"op": "insert_many", "documents": [1]}), false),
            (
                json!({"op": "insert_many", "documents": [{"a": 1}, {"var": "data.tags"}]}),
                false,
            ),
            (
                json!({"op": "insert_many", "documents": {"var": "data.many"}}),
                false,
            ),
            (
                json!({"op": "insert_many", "documents": [], "ordered": "no"}),
                false,
            ),
            (
                json!({"op": "update_one", "filter": {"_id": {"$oid": {"var": "data.id"}}},
                       "update": {"$set": {"status": {"var": "data.status"}}},
                       "upsert": {"var": "data.flag"}}),
                true,
            ),
            (
                json!({"op": "update_many", "filter": {"n": {"$gt": 1}},
                       "update": {"$set": {"s.$[s].x": {"var": "data.n"}}},
                       "array_filters": [{"s.y": {"$lt": {"var": "data.n"}}}]}),
                true,
            ),
            (
                json!({"op": "update_one", "filter": {"x": 1}, "update": {"var": "data.plain"}}),
                false,
            ),
            (
                json!({"op": "update_one", "filter": {"x": 1}, "update": {"var": "data.empty"}}),
                false,
            ),
            (
                json!({"op": "update_one", "filter": {"x": 1}, "update": null}),
                false,
            ),
            (
                json!({"op": "update_one", "filter": {"x": 1},
                       "update": {"$set": {"s.$[s].x": 1}}, "array_filters": {"var": "data.filters"}}),
                false,
            ),
            // No filters to cross-check: the driver's to refuse, as on 1.12.
            (
                json!({"op": "update_one", "filter": {"x": 1},
                       "update": {"$set": {"s.$[s].x": 1}}, "array_filters": null}),
                true,
            ),
            (
                json!({"op": "update_one", "filter": {"var": "data.missing"}, "update": {"$set": {"a": 1}}}),
                false,
            ),
            (
                json!({"op": "update_one", "filter": {"var": "data.empty"}, "update": {"$set": {"a": 1}}}),
                false,
            ),
            (
                json!({"op": "update_one", "filter": {"x": 1}, "update": {"$set": {"a": 1}}, "upsert": "yes"}),
                false,
            ),
            (
                json!({"op": "update_one", "filter": {"x": 1}, "update": {"$set": {"a": 1}}, "upsert": {"var": "data.text"}}),
                false,
            ),
            (
                json!({"op": "replace_one", "filter": {"_id": 1}, "document": {"var": "data.doc"}, "upsert": true}),
                true,
            ),
            (
                json!({"op": "replace_one", "filter": {"_id": 1}, "document": {"var": "data.dollar"}}),
                false,
            ),
            (
                json!({"op": "delete_many", "filter": {"tags": {"$in": {"var": "data.tags"}}}}),
                true,
            ),
            (json!({"op": "delete_one", "filter": null}), false),
            (
                json!({"op": "delete_many", "filter": {"var": "data.empty"}, "all": true}),
                false,
            ),
            // The output is read first, then the op, the switches, the
            // documents and the guards.
            (
                json!({"op": "delete_one", "filter": null, "output": {"var": "data.n"}}),
                false,
            ),
            (
                json!({"op": "update_one", "filter": null, "update": null, "upsert": "yes"}),
                false,
            ),
            (
                json!({"op": "update_one", "filter": {"var": "data.empty"}, "update": {"var": "data.plain"}}),
                false,
            ),
        ] {
            let mut input = task(rest.clone());
            if input.get("output").is_none() {
                input["output"] = json!("data.write");
            }
            let (v3, v4) = parsed(input, data.clone(), write_config(false)).await;
            assert_eq!(v4, v3, "{rest}");
            assert_eq!(v4.is_ok(), ok, "{rest}: {v4:?}");
        }
        // An unfiltered write with both halves of the opt-in.
        let input = task(
            json!({"op": "delete_many", "filter": {"var": "data.empty"}, "all": true,
                                "output": "data.write"}),
        );
        let (v3, v4) = parsed(input, data, write_config(true)).await;
        assert_eq!(v4, v3);
        assert!(v4.is_ok(), "{v4:?}");
    }

    /// The function itself, refused at each step as 1.12's handler was, and
    /// past every gate reaching the driver, which waits for a server until
    /// the connector's `query_timeout_ms`, 1.12's `TIMEOUT_ERROR`.
    #[tokio::test]
    async fn the_call_refuses_and_reaches_the_driver_as_on_v3() {
        let insert = task(
            json!({"op": "insert_one", "document": {"n": {"var": "data.n"}},
                                 "output": "data.result"}),
        );
        let with = |key: &str, value: Value| {
            let mut input = insert.clone();
            input[key] = value;
            input
        };
        for (input, extra, code) in [
            (insert.clone(), json!({}), "TIMEOUT_ERROR"),
            (
                with("connector", json!("absent")),
                json!({}),
                "FUNCTION_ERROR",
            ),
            (with("connector", json!("api")), json!({}), CONNECTOR_DETAIL),
            (
                with("connector", json!("sql")),
                json!({}),
                "VALIDATION_ERROR",
            ),
            (
                insert.clone(),
                json!({"operations": {"insert": false}}),
                CONNECTOR_DETAIL,
            ),
            (with("document", json!("x")), json!({}), "VALIDATION_ERROR"),
            (
                with("output", json!({"var": "data.n"})),
                json!({}),
                "VALIDATION_ERROR",
            ),
        ] {
            let (v3, v4) = both(input.clone(), json!({"n": 3}), extra).await;
            assert_eq!(v4, v3, "{input}");
            assert_eq!(v4.expect_err("refused").code, code, "{input}");
        }
    }

    /// `QueryBudget` within the run's deadline (E10): a run deadline before
    /// the connector's `query_timeout_ms` ends the call `TimedOut`, writing
    /// nothing.
    #[tokio::test]
    async fn a_run_deadline_before_the_query_timeout_ends_the_call_timed_out() {
        let registry = testkit::connectors(json!({"query_timeout_ms": 30_000})).await;
        let input =
            task(json!({"op": "insert_one", "document": {"a": 1}, "output": "data.result"}));
        let started = std::time::Instant::now();
        let called = testing::call(function(registry, write_config(false)), input)
            .exprs(kit::test_exprs())
            .name(NAME)
            .deadline(dataflow_rs::run::Instant::now() + Duration::from_millis(150))
            .await;
        let error = called.outcome.expect_err("the run's deadline passed");
        assert_eq!(*error.kind(), ErrorKind::TimedOut);
        assert_eq!(
            error.message(),
            "the run's deadline passed during `mongo_write`"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(called.writes.is_empty());
    }
}

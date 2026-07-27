//! MongoDB rendering.
//!
//! Walks a [`Cond`] + [`QuerySpec`] into a `find` query — a `$match`-shaped filter
//! document plus projection / sort / skip / limit — over the same IR the SQL
//! backend uses. Scalar operators map to the BSON forms in the master table
//! (§4.1); embedded relations render as `$elemMatch` (§4.2). Referenced relations
//! (`$lookup`) raise a capability error for now.
//!
//! `id` maps to `_id` so the common Mongo key works in identity mode; other
//! logical→physical names come from the schema during lowering.

use mongodb::bson::{Bson, Document};

use crate::query::error::QueryError;
use crate::query::ir::{CmpOp, Cond, FieldRef, MongoStorage, Quant, TextOp, Value};
use crate::query::spec::{QuerySpec, SortDir};
use crate::query::write::{ConflictAction, ResolvedWrite, WriteError, WriteOp};

/// A rendered MongoDB `find`: the collection plus filter and options.
#[derive(Debug, Clone, PartialEq)]
pub struct MongoQuery {
    pub collection: String,
    pub filter: Document,
    pub projection: Option<Document>,
    pub sort: Option<Document>,
    pub skip: Option<u64>,
    pub limit: u64,
}

/// Build a `MongoQuery` from the envelope and lowered condition, enforcing the
/// page-size bounds.
pub fn render(
    spec: &QuerySpec,
    cond: &Cond,
    collection: &str,
    default_limit: u64,
    max_limit: u64,
) -> Result<MongoQuery, QueryError> {
    let limit = super::resolve_limit(spec.limit, default_limit, max_limit)?;

    let filter = match cond {
        Cond::True => Document::new(),
        other => match_doc(other)?,
    };

    let projection = if spec.fields.is_empty() {
        None
    } else {
        let mut p = Document::new();
        for f in &spec.fields {
            p.insert(mongo_name(f.as_str()), 1_i32);
        }
        Some(p)
    };

    let sort = if spec.sort.is_empty() {
        None
    } else {
        let mut s = Document::new();
        for k in &spec.sort {
            let dir = match k.dir {
                SortDir::Asc => 1_i32,
                SortDir::Desc => -1_i32,
            };
            s.insert(mongo_name(k.field.as_str()), dir);
        }
        Some(s)
    };

    Ok(MongoQuery {
        collection: collection.to_string(),
        filter,
        projection,
        sort,
        skip: spec.skip,
        limit,
    })
}

/// Render a `Cond` into a `$match`-shaped filter document.
fn match_doc(cond: &Cond) -> Result<Document, QueryError> {
    Ok(match cond {
        Cond::True => Document::new(),
        Cond::False => doc_kv("$expr", Bson::Boolean(false)),
        Cond::And(cs) => doc_kv("$and", bson_docs(cs)?),
        Cond::Or(cs) => doc_kv("$or", bson_docs(cs)?),
        Cond::Not(inner) => doc_kv("$nor", Bson::Array(vec![Bson::Document(match_doc(inner)?)])),
        Cond::Compare { field, op, value } => field_op(field, cmp_key(*op), to_bson(value)),
        Cond::In {
            field,
            values,
            negated,
        } => {
            let key = if *negated { "$nin" } else { "$in" };
            field_op(
                field,
                key,
                Bson::Array(values.iter().map(to_bson).collect()),
            )
        }
        Cond::IsNull { field, negated } => {
            let key = if *negated { "$ne" } else { "$eq" };
            field_op(field, key, Bson::Null)
        }
        Cond::Between {
            field,
            low,
            high,
            low_incl,
            high_incl,
            negated,
        } => {
            let mut inner = Document::new();
            inner.insert(if *low_incl { "$gte" } else { "$gt" }, to_bson(low));
            inner.insert(if *high_incl { "$lte" } else { "$lt" }, to_bson(high));
            let d = doc_kv(mongo_name(field), Bson::Document(inner));
            if *negated {
                doc_kv("$nor", Bson::Array(vec![Bson::Document(d)]))
            } else {
                d
            }
        }
        Cond::Text {
            field,
            op,
            pattern,
            ci,
        } => {
            let escaped = regex_escape(pattern);
            let regex = match op {
                TextOp::StartsWith => format!("^{escaped}"),
                TextOp::EndsWith => format!("{escaped}$"),
                TextOp::Contains => escaped,
            };
            let mut inner = Document::new();
            inner.insert("$regex", Bson::String(regex));
            if *ci {
                inner.insert("$options", Bson::String("i".to_string()));
            }
            doc_kv(mongo_name(field), Bson::Document(inner))
        }
        Cond::Rel { quant, rel, cond } => rel_doc(*quant, &rel.name, rel.mongo, cond)?,
    })
}

/// Render an embedded relation predicate via `$elemMatch`.
fn rel_doc(
    quant: Quant,
    field: &str,
    storage: MongoStorage,
    inner: &Cond,
) -> Result<Document, QueryError> {
    if storage == MongoStorage::Referenced {
        // $lookup-based joins are a later addition; fail loudly rather than
        // silently returning wrong results.
        return Err(QueryError::FeatureUnsupportedByTarget {
            feature: format!("referenced relation '{field}' ($lookup)"),
            target: "mongodb".to_string(),
        });
    }
    let inner_doc = match_doc(inner)?;
    Ok(match quant {
        Quant::Any => doc_kv(
            field,
            Bson::Document(doc_kv("$elemMatch", Bson::Document(inner_doc))),
        ),
        Quant::None => doc_kv(
            field,
            Bson::Document(doc_kv(
                "$not",
                Bson::Document(doc_kv("$elemMatch", Bson::Document(inner_doc))),
            )),
        ),
        Quant::All => {
            // Non-empty AND no element violates the predicate (§5.6).
            let nonempty = doc_kv(
                field,
                Bson::Document(doc_kv("$elemMatch", Bson::Document(Document::new()))),
            );
            let violates = doc_kv("$nor", Bson::Array(vec![Bson::Document(inner_doc)]));
            let no_violation = doc_kv(
                field,
                Bson::Document(doc_kv(
                    "$not",
                    Bson::Document(doc_kv("$elemMatch", Bson::Document(violates))),
                )),
            );
            doc_kv(
                "$and",
                Bson::Array(vec![Bson::Document(nonempty), Bson::Document(no_violation)]),
            )
        }
    })
}

fn bson_docs(cs: &[Cond]) -> Result<Bson, QueryError> {
    let mut out = Vec::with_capacity(cs.len());
    for c in cs {
        out.push(Bson::Document(match_doc(c)?));
    }
    Ok(Bson::Array(out))
}

/// `{ field: { op: value } }`.
fn field_op(field: &FieldRef, op: &str, value: Bson) -> Document {
    doc_kv(mongo_name(field), Bson::Document(doc_kv(op, value)))
}

fn doc_kv(key: impl Into<String>, value: Bson) -> Document {
    let mut d = Document::new();
    d.insert(key.into(), value);
    d
}

fn cmp_key(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "$eq",
        CmpOp::Ne => "$ne",
        CmpOp::Lt => "$lt",
        CmpOp::Le => "$lte",
        CmpOp::Gt => "$gt",
        CmpOp::Ge => "$gte",
    }
}

fn to_bson(v: &Value) -> Bson {
    match v {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Int(i) => Bson::Int64(*i),
        Value::Float(f) => Bson::Double(*f),
        Value::Str(s) => Bson::String(s.clone()),
        Value::List(items) => Bson::Array(items.iter().map(to_bson).collect()),
    }
}

/// The common `id` key maps to Mongo's `_id`; other names pass through.
trait MongoName {
    fn mongo(&self) -> &str;
}
impl MongoName for FieldRef {
    fn mongo(&self) -> &str {
        &self.physical
    }
}
impl MongoName for str {
    fn mongo(&self) -> &str {
        self
    }
}

fn mongo_name<T: MongoName + ?Sized>(f: &T) -> String {
    let n = f.mongo();
    if n == "id" {
        "_id".to_string()
    } else {
        n.to_string()
    }
}

// ---- Write rendering (insert / update / delete / upsert) ----

/// A rendered MongoDB write, ready for the driver call the handler makes.
#[derive(Debug, Clone, PartialEq)]
pub enum MongoWrite {
    Insert {
        collection: String,
        docs: Vec<Document>,
    },
    Update {
        collection: String,
        filter: Document,
        /// Update document (e.g. `{ "$set": {..} }`); already assembled.
        update: Document,
        upsert: bool,
        /// `true` → `update_many`; `false` → `update_one` (upsert).
        multi: bool,
    },
    Delete {
        collection: String,
        filter: Document,
    },
}

/// Render a resolved mutation into a [`MongoWrite`]. The `filter` of an
/// update/delete reuses the query dialect's [`match_doc`]; values become BSON via
/// the same [`to_bson`] the read path uses. `id` maps to `_id`.
pub fn render_write(w: &ResolvedWrite) -> Result<MongoWrite, WriteError> {
    Ok(match w.op {
        WriteOp::Insert => MongoWrite::Insert {
            collection: w.table.clone(),
            docs: build_docs(&w.columns, &w.rows),
        },
        WriteOp::Update => MongoWrite::Update {
            collection: w.table.clone(),
            filter: cond_to_doc(&w.cond)?,
            update: doc_kv("$set", Bson::Document(set_to_doc(&w.set))),
            upsert: false,
            multi: true,
        },
        WriteOp::Delete => MongoWrite::Delete {
            collection: w.table.clone(),
            filter: cond_to_doc(&w.cond)?,
        },
        WriteOp::Upsert => render_upsert(w)?,
    })
}

fn render_upsert(w: &ResolvedWrite) -> Result<MongoWrite, WriteError> {
    let conflict = w
        .conflict
        .as_ref()
        .ok_or_else(|| WriteError::MissingField {
            field: "on_conflict".to_string(),
            op: "upsert".to_string(),
        })?;
    // A single-document upsert keyed on the conflict target. Bulk upsert would
    // need one updateOne per row; deferred (fail loudly, don't guess).
    if w.rows.len() != 1 {
        return Err(WriteError::FeatureUnsupportedByTarget {
            feature: "bulk upsert".to_string(),
            target: "mongodb".to_string(),
        });
    }
    let row = &w.rows[0];

    // Filter matches on the conflict-target columns (also applied on insert).
    let mut filter = Document::new();
    for t in &conflict.targets {
        let idx = w.columns.iter().position(|c| c == t).ok_or_else(|| {
            WriteError::InvalidEnvelope(format!(
                "on_conflict target '{t}' must be one of the inserted columns"
            ))
        })?;
        filter.insert(mongo_name(t.as_str()), to_bson(&row[idx]));
    }

    let mut set = Document::new();
    let mut set_on_insert = Document::new();
    match conflict.action {
        ConflictAction::Update => {
            if w.set.is_empty() {
                // Overwrite every non-target column on conflict.
                for (col, v) in w.columns.iter().zip(row) {
                    if !conflict.targets.contains(col) {
                        set.insert(mongo_name(col.as_str()), to_bson(v));
                    }
                }
            } else {
                for (col, v) in &w.set {
                    set.insert(mongo_name(col.as_str()), to_bson(v));
                }
                // Inserted columns not in `set` (and not targets) apply only on insert.
                for (col, v) in w.columns.iter().zip(row) {
                    if !conflict.targets.contains(col) && !w.set.iter().any(|(c, _)| c == col) {
                        set_on_insert.insert(mongo_name(col.as_str()), to_bson(v));
                    }
                }
            }
        }
        ConflictAction::Nothing => {
            // Insert the row if absent; leave an existing row untouched.
            for (col, v) in w.columns.iter().zip(row) {
                if !conflict.targets.contains(col) {
                    set_on_insert.insert(mongo_name(col.as_str()), to_bson(v));
                }
            }
        }
    }

    let mut update = Document::new();
    if !set.is_empty() {
        update.insert("$set", Bson::Document(set));
    }
    if !set_on_insert.is_empty() {
        update.insert("$setOnInsert", Bson::Document(set_on_insert));
    }
    // Guard against an empty update doc (Mongo rejects it): fall back to keying
    // the targets on insert.
    if update.is_empty() {
        let mut soi = Document::new();
        for t in &conflict.targets {
            let idx = w
                .columns
                .iter()
                .position(|c| c == t)
                .expect("target present");
            soi.insert(mongo_name(t.as_str()), to_bson(&row[idx]));
        }
        update.insert("$setOnInsert", Bson::Document(soi));
    }

    Ok(MongoWrite::Update {
        collection: w.table.clone(),
        filter,
        update,
        upsert: true,
        multi: false,
    })
}

/// One BSON document per row, keyed by physical column name (`id` → `_id`).
fn build_docs(columns: &[String], rows: &[Vec<Value>]) -> Vec<Document> {
    rows.iter()
        .map(|row| {
            let mut d = Document::new();
            for (col, v) in columns.iter().zip(row) {
                d.insert(mongo_name(col.as_str()), to_bson(v));
            }
            d
        })
        .collect()
}

fn set_to_doc(set: &[(String, Value)]) -> Document {
    let mut d = Document::new();
    for (col, v) in set {
        d.insert(mongo_name(col.as_str()), to_bson(v));
    }
    d
}

/// Lower an optional filter to a `$match`-shaped document (empty = match all).
fn cond_to_doc(cond: &Option<Cond>) -> Result<Document, WriteError> {
    Ok(match cond {
        None | Some(Cond::True) => Document::new(),
        Some(c) => match_doc(c).map_err(WriteError::from)?,
    })
}

/// Escape regex metacharacters so user text matches literally inside `$regex`.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{EntityRegistry, translate_mongo};
    use mongodb::bson::doc;
    use serde_json::json;

    fn mongo(query: serde_json::Value) -> MongoQuery {
        translate_mongo(
            &query,
            &serde_json::Map::new(),
            &EntityRegistry::default(),
            100,
            1000,
        )
        .expect("translation should succeed")
    }

    fn mongo_schema(query: serde_json::Value, schema: serde_json::Value) -> MongoQuery {
        let reg = EntityRegistry::from_json(&schema).expect("schema");
        translate_mongo(&query, &serde_json::Map::new(), &reg, 100, 1000).expect("ok")
    }

    #[test]
    fn test_scalar_match() {
        let q = mongo(json!({
            "source": "users",
            "filter": { "and": [
                { ">": [{"field": "age"}, 18] },
                { "==": [{"field": "status"}, "active"] }
            ] }
        }));
        assert_eq!(q.collection, "users");
        assert_eq!(
            q.filter,
            doc! { "$and": [ { "age": { "$gt": 18_i64 } }, { "status": { "$eq": "active" } } ] }
        );
        assert_eq!(q.limit, 100);
    }

    #[test]
    fn test_id_maps_to_underscore_id() {
        let q = mongo(json!({ "source": "users", "filter": { "==": [{"field": "id"}, "u1"] } }));
        assert_eq!(q.filter, doc! { "_id": { "$eq": "u1" } });
    }

    #[test]
    fn test_membership_and_range() {
        let q = mongo(json!({
            "source": "t",
            "filter": { "and": [
                { "in": [{"field": "status"}, ["a", "b"]] },
                { "<=": [1, {"field": "x"}, 10] }
            ] }
        }));
        assert_eq!(
            q.filter,
            doc! { "$and": [
                { "status": { "$in": ["a", "b"] } },
                { "x": { "$gte": 1_i64, "$lte": 10_i64 } }
            ] }
        );
    }

    #[test]
    fn test_is_null() {
        let q = mongo(json!({ "source": "t", "filter": { "==": [{"field": "email"}, null] } }));
        assert_eq!(q.filter, doc! { "email": { "$eq": Bson::Null } });
    }

    #[test]
    fn test_contains_regex_escaped() {
        let q = mongo(json!({ "source": "t", "filter": { "in": ["a.b", {"field": "name"}] } }));
        assert_eq!(q.filter, doc! { "name": { "$regex": "a\\.b" } });
    }

    #[test]
    fn test_projection_and_sort() {
        let q = mongo(json!({
            "source": "users",
            "fields": ["id", "name"],
            "sort": [{ "name": "asc" }, { "age": "desc" }]
        }));
        assert_eq!(q.projection, Some(doc! { "_id": 1_i32, "name": 1_i32 }));
        assert_eq!(q.sort, Some(doc! { "name": 1_i32, "age": -1_i32 }));
    }

    #[test]
    fn test_embedded_relation_elemmatch() {
        let q = mongo_schema(
            json!({
                "source": "users",
                "filter": { "some": [{"field": "orders"}, {">": [{"field": "total"}, 100]}] }
            }),
            json!({ "entities": { "users": { "relations": {
                "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id", "mongo": "embedded" }
            } } } }),
        );
        assert_eq!(
            q.filter,
            doc! { "orders": { "$elemMatch": { "total": { "$gt": 100_i64 } } } }
        );
    }

    #[test]
    fn test_referenced_relation_is_capability_error() {
        let err = translate_mongo(
            &json!({
                "source": "users",
                "filter": { "some": [{"field": "orders"}, {">": [{"field": "total"}, 100]}] }
            }),
            &serde_json::Map::new(),
            &EntityRegistry::from_json(&json!({ "entities": { "users": { "relations": {
                "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id", "mongo": "referenced" }
            } } } }))
            .expect("schema"),
            100,
            1000,
        )
        .expect_err("referenced not supported yet");
        assert!(matches!(err, QueryError::FeatureUnsupportedByTarget { .. }));
    }

    #[test]
    fn test_limit_exceeds_max_rejected() {
        let err = translate_mongo(
            &json!({ "source": "t", "limit": 9999 }),
            &serde_json::Map::new(),
            &EntityRegistry::default(),
            100,
            1000,
        )
        .expect_err("over cap");
        assert!(matches!(err, QueryError::LimitExceeded { .. }));
    }

    // ---- Write rendering ----

    fn resolve(input: serde_json::Value) -> crate::query::write::ResolvedWrite {
        crate::query::write::resolve_write(
            &input,
            &serde_json::Map::new(),
            &EntityRegistry::default(),
        )
        .expect("resolve_write should succeed")
    }

    #[test]
    fn test_mongo_insert_docs() {
        let mw = render_write(&resolve(json!({
            "op": "insert", "target": "users",
            "values": [ { "id": "u1", "name": "Ada" }, { "id": "u2", "name": "Bob" } ]
        })))
        .expect("render");
        // `id` maps to `_id`.
        assert_eq!(
            mw,
            MongoWrite::Insert {
                collection: "users".to_string(),
                docs: vec![
                    doc! { "_id": "u1", "name": "Ada" },
                    doc! { "_id": "u2", "name": "Bob" },
                ],
            }
        );
    }

    #[test]
    fn test_mongo_update_uses_set_and_filter() {
        let mw = render_write(&resolve(json!({
            "op": "update", "target": "users",
            "set": { "status": "inactive" },
            "filter": { "==": [{ "field": "id" }, "u1"] }
        })))
        .expect("render");
        assert_eq!(
            mw,
            MongoWrite::Update {
                collection: "users".to_string(),
                filter: doc! { "_id": { "$eq": "u1" } },
                update: doc! { "$set": { "status": "inactive" } },
                upsert: false,
                multi: true,
            }
        );
    }

    #[test]
    fn test_mongo_delete_filter() {
        let mw = render_write(&resolve(json!({
            "op": "delete", "target": "sessions",
            "filter": { "<": [{ "field": "age" }, 0] }
        })))
        .expect("render");
        assert_eq!(
            mw,
            MongoWrite::Delete {
                collection: "sessions".to_string(),
                filter: doc! { "age": { "$lt": 0_i64 } },
            }
        );
    }

    #[test]
    fn test_mongo_upsert_is_update_one_with_upsert() {
        let mw = render_write(&resolve(json!({
            "op": "upsert", "target": "users",
            "values": { "email": "a@x.io", "name": "Ada" },
            "on_conflict": { "target": ["email"], "action": "update" }
        })))
        .expect("render");
        assert_eq!(
            mw,
            MongoWrite::Update {
                collection: "users".to_string(),
                filter: doc! { "email": "a@x.io" },
                update: doc! { "$set": { "name": "Ada" } },
                upsert: true,
                multi: false,
            }
        );
    }
}

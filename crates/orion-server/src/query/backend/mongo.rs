//! MongoDB rendering.
//!
//! Walks a [`Cond`] + [`QuerySpec`] into a `find` query — a `$match`-shaped filter
//! document plus projection / sort / skip / limit — over the same IR the SQL
//! backend uses. Scalar operators map to their BSON forms; embedded
//! relations render as `$elemMatch`. Referenced relations
//! (`$lookup`) raise a capability error for now.
//!
//! **Names pass through exactly as the schema resolved them (W10).** There is no
//! implicit `id` → `_id` rewrite: `_id` is Mongo's document key, and a collection
//! may legitimately carry an ordinary `id` field beside it. Targeting the
//! document key is an explicit schema decision — `{"columns": {"id": {"name":
//! "_id"}}}` — exactly as it already was for Elasticsearch. The implicit rewrite
//! made a schema that deliberately mapped `key → id` mean something else, and
//! made a genuine non-key `id` field unqueryable, in both cases silently.

use mongodb::bson::{Bson, Document};

use serde_json::Value as Json;

use crate::config::QueryConfig;
use crate::query::bulk::{BulkOutcome, ItemOutcome};
use crate::query::error::QueryError;
use crate::query::ir::{CmpOp, Cond, FieldRef, MongoStorage, Quant, TextOp, Value};
use crate::query::spec::QuerySpec;
use crate::query::write::{ResolvedConflict, ResolvedWrite, WriteError};

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
/// page bounds.
pub fn render(
    spec: &QuerySpec,
    cond: &Cond,
    collection: &str,
    limits: &QueryConfig,
) -> Result<MongoQuery, QueryError> {
    super::reject_include(spec, "mongodb")?;

    let limit = super::resolve_limit(spec.limit, limits)?;
    let skip = super::resolve_skip(spec.skip, limits)?;

    let filter = match cond {
        Cond::True => Document::new(),
        other => match_doc(other)?,
    };

    let projection = super::plan_projection(&spec.fields).map(|fields| {
        let mut p = Document::new();
        for f in fields {
            p.insert(f.as_str(), 1_i32);
        }
        // W9: Mongo returns `_id` by default even when not projected, while
        // SQL/ES return exactly the requested fields. Suppress it unless it
        // was explicitly asked for.
        if !p.contains_key("_id") {
            p.insert("_id", 0_i32);
        }
        p
    });

    // A bare `1`/`-1` realises the planned nulls placement exactly (W8): BSON
    // sorts null (and a missing field) below every other value, so `asc`
    // yields nulls first and `desc` nulls last — which is what `plan_sort`
    // asks for. Under the old rule ("nulls last on asc") Mongo silently
    // disagreed with SQL and ES, because `find` has no way to express it.
    let plans = super::plan_sort(&spec.sort);
    let sort = if plans.is_empty() {
        None
    } else {
        let mut s = Document::new();
        for p in &plans {
            s.insert(p.field, if p.ascending { 1_i32 } else { -1_i32 });
        }
        Some(s)
    };

    Ok(MongoQuery {
        collection: collection.to_string(),
        filter,
        projection,
        sort,
        skip,
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
            let d = doc_kv(field.physical.as_str(), Bson::Document(inner));
            if *negated {
                doc_kv("$nor", Bson::Array(vec![Bson::Document(d)]))
            } else {
                d
            }
        }
        Cond::Text { field, op, pattern } => {
            // W13: `$regex` without `$options: "i"` is case-sensitive — one
            // of the four behaviours the dialect deliberately does not
            // normalise. See the parity table in
            // `docs/src/reference/data-dialect.md`.
            let escaped = regex_escape(pattern);
            let regex = match op {
                TextOp::StartsWith => format!("^{escaped}"),
                TextOp::EndsWith => format!("{escaped}$"),
                TextOp::Contains => escaped,
            };
            let mut inner = Document::new();
            inner.insert("$regex", Bson::String(regex));
            doc_kv(field.physical.as_str(), Bson::Document(inner))
        }
        Cond::Rel { quant, rel, cond } => {
            super::reject_many_to_many(rel, "mongodb")?;
            rel_doc(*quant, &rel.name, rel.mongo, cond)?
        }
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
            // Non-empty AND no element violates the predicate (the `all`
            // null rule).
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
    doc_kv(field.physical.as_str(), Bson::Document(doc_kv(op, value)))
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
        // #263: the tagged values, validated during lowering, become the native
        // BSON types here — the whole point: a filter on a real ObjectId `_id`
        // or a date range matches instead of silently missing.
        Value::ObjectId(bytes) => Bson::ObjectId(mongodb::bson::oid::ObjectId::from_bytes(*bytes)),
        Value::DateTime(ms) => Bson::DateTime(mongodb::bson::DateTime::from_millis(*ms)),
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

/// Read an ordered `insert_many` failure into per-item outcomes (F28).
///
/// `insert_many` defaults to ordered, so the server stops at the first bad
/// document: every earlier document is committed, the failing one is not, and
/// the rest are never attempted. The driver's error carried none of that — no
/// index, no count — so a caller saw one opaque message over a collection that
/// had in fact been partly written.
///
/// `failed` holds the indices the driver reported (`write_errors`) with their
/// detail. Anything after the earliest reported failure that was not itself
/// reported is `skipped`, not failed: the server never looked at it.
pub fn insert_outcome(sent: usize, failed: &[(usize, Json)]) -> BulkOutcome {
    let first_failure = failed.iter().map(|(i, _)| *i).min();
    let items = (0..sent)
        .map(|i| match failed.iter().find(|(idx, _)| *idx == i) {
            Some((_, detail)) => ItemOutcome::error(i, detail.clone()),
            // Ids are not recoverable from the driver's error type, so a
            // committed document reports `ok` without one.
            None if first_failure.is_none_or(|f| i < f) => ItemOutcome::ok(i, None),
            None => ItemOutcome::skipped(i),
        })
        .collect();
    BulkOutcome { items }
}

/// Render a resolved mutation into a [`MongoWrite`]. The `filter` of an
/// update/delete reuses the query dialect's [`match_doc`]; values become BSON via
/// the same [`to_bson`] the read path uses. Column names pass through as the
/// schema resolved them — declare `{"columns": {"id": {"name": "_id"}}}` to
/// write the document key (W10).
pub fn render_write(w: &ResolvedWrite) -> Result<MongoWrite, WriteError> {
    Ok(match w {
        ResolvedWrite::Insert {
            table,
            columns,
            rows,
            ..
        } => MongoWrite::Insert {
            collection: table.clone(),
            docs: build_docs(columns, rows),
        },
        ResolvedWrite::Update {
            table, set, cond, ..
        } => MongoWrite::Update {
            collection: table.clone(),
            filter: cond_to_doc(cond)?,
            update: doc_kv("$set", Bson::Document(set_to_doc(set))),
            upsert: false,
            multi: true,
        },
        ResolvedWrite::Delete { table, cond, .. } => MongoWrite::Delete {
            collection: table.clone(),
            filter: cond_to_doc(cond)?,
        },
        ResolvedWrite::Upsert {
            table,
            columns,
            rows,
            set,
            conflict,
            ..
        } => render_upsert(table, columns, rows, set, conflict)?,
    })
}

fn render_upsert(
    table: &str,
    columns: &[String],
    rows: &[Vec<Value>],
    w_set: &[(String, Value)],
    conflict: &ResolvedConflict,
) -> Result<MongoWrite, WriteError> {
    let plan = super::plan_upsert(columns, rows, w_set, conflict, "mongodb")?;

    // Filter matches on the conflict-target columns (also applied on insert).
    let mut filter = Document::new();
    for t in &conflict.targets {
        let idx = columns.iter().position(|c| c == t).ok_or_else(|| {
            WriteError::Query(QueryError::InvalidEnvelope(format!(
                "on_conflict target '{t}' must be one of the inserted columns"
            )))
        })?;
        filter.insert(t.as_str(), to_bson(&plan.row[idx]));
    }

    // The planned split maps straight onto Mongo's per-column update operators.
    let mut set = Document::new();
    for (col, v) in &plan.on_conflict {
        set.insert(*col, to_bson(v));
    }
    let mut set_on_insert = Document::new();
    for (col, v) in &plan.insert_only {
        set_on_insert.insert(*col, to_bson(v));
    }

    let mut update = Document::new();
    if !set.is_empty() {
        update.insert("$set", Bson::Document(set));
    }
    if !set_on_insert.is_empty() {
        update.insert("$setOnInsert", Bson::Document(set_on_insert));
    }
    // Guard against an empty update doc (Mongo rejects it): fall back to keying
    // the targets on insert. The filter already holds exactly the target columns
    // and their row values, so it doubles as that `$setOnInsert` document.
    if update.is_empty() {
        update.insert("$setOnInsert", Bson::Document(filter.clone()));
    }

    Ok(MongoWrite::Update {
        collection: table.to_string(),
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
                d.insert(col.as_str(), to_bson(v));
            }
            d
        })
        .collect()
}

fn set_to_doc(set: &[(String, Value)]) -> Document {
    let mut d = Document::new();
    for (col, v) in set {
        d.insert(col.as_str(), to_bson(v));
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

    fn limits() -> QueryConfig {
        QueryConfig::default()
    }

    fn mongo(query: serde_json::Value) -> MongoQuery {
        translate_mongo(
            &query,
            &serde_json::Map::new(),
            &EntityRegistry::identity(),
            &limits(),
        )
        .expect("translation should succeed")
    }

    fn mongo_schema(query: serde_json::Value, schema: serde_json::Value) -> MongoQuery {
        let reg = EntityRegistry::from_json(&schema).expect("schema");
        translate_mongo(&query, &serde_json::Map::new(), &reg, &limits()).expect("ok")
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

    /// W10: `id` used to be silently rewritten to `_id` here, while the
    /// Elasticsearch renderer two files away documented the opposite. A
    /// collection with a genuine non-key `id` field was therefore unqueryable,
    /// and a schema deliberately mapping some key to `id` meant `_id` instead.
    #[test]
    fn test_id_is_not_silently_rewritten_to_underscore_id() {
        let q = mongo(json!({ "source": "users", "filter": { "==": [{"field": "id"}, "u1"] } }));
        assert_eq!(q.filter, doc! { "id": { "$eq": "u1" } });
    }

    /// …and the document key is reached the same way Elasticsearch reaches it:
    /// an explicit schema rename.
    #[test]
    fn test_the_document_key_is_an_explicit_schema_rename() {
        let q = mongo_schema(
            json!({ "source": "users", "filter": { "==": [{"field": "id"}, "u1"] } }),
            json!({ "entities": { "users": { "columns": { "id": { "name": "_id" } } } } }),
        );
        assert_eq!(q.filter, doc! { "_id": { "$eq": "u1" } });
    }

    /// #263: the tagged values become native BSON — an ObjectId `_id` filter
    /// and a date range now *match*, instead of comparing a string/number
    /// against a typed value and silently missing.
    #[test]
    fn test_tagged_values_render_native_bson() {
        let q = mongo(json!({
            "source": "meetings",
            "filter": { "and": [
                { "==": [{"field": "_id"}, { "$oid": "665f1f77bcf86cd799439011" }] },
                { ">": [{"field": "created_at"}, { "$date": "2024-05-04T00:00:00Z" }] }
            ] }
        }));
        let oid = mongodb::bson::oid::ObjectId::parse_str("665f1f77bcf86cd799439011")
            .expect("valid test oid");
        assert_eq!(
            q.filter,
            doc! { "$and": [
                { "_id": { "$eq": oid } },
                { "created_at": { "$gt": mongodb::bson::DateTime::from_millis(1_714_780_800_000) } }
            ] }
        );
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
        let q = mongo_schema(
            json!({
                "source": "users",
                "fields": ["id", "name"],
                "sort": [{ "name": "asc" }, { "age": "desc" }]
            }),
            json!({
                "unmapped": "identity",
                "entities": { "users": { "columns": { "id": { "name": "_id" } } } }
            }),
        );
        // `_id` is explicitly projected (via the rename) — no suppression.
        assert_eq!(q.projection, Some(doc! { "_id": 1_i32, "name": 1_i32 }));
        assert_eq!(q.sort, Some(doc! { "name": 1_i32, "age": -1_i32 }));
    }

    /// W8: the shared rule is "a null sorts as the smallest value", which is
    /// exactly what BSON's own ordering does for a bare `1`/`-1` — so Mongo now
    /// agrees with SQL and ES instead of silently inverting them on `asc`.
    #[test]
    fn test_sort_null_ordering_matches_the_other_backends() {
        let q = mongo(json!({ "source": "t", "sort": [{ "a": "asc" }, { "b": "desc" }] }));
        assert_eq!(q.sort, Some(doc! { "a": 1_i32, "b": -1_i32 }));
    }

    /// W9: without `_id: 0`, Mongo returned `{_id, name}` where SQL/ES
    /// returned `{name}` for the same envelope.
    #[test]
    fn test_projection_suppresses_id_unless_requested() {
        let q = mongo(json!({ "source": "users", "fields": ["name"] }));
        assert_eq!(
            q.projection,
            Some(doc! { "name": 1_i32, "_id": 0_i32 }),
            "an unrequested _id must be suppressed"
        );
    }

    #[test]
    fn test_embedded_relation_elemmatch() {
        let q = mongo_schema(
            json!({
                "source": "users",
                "filter": { "some": [{"field": "orders"}, {">": [{"field": "total"}, 100]}] }
            }),
            json!({ "unmapped": "identity", "entities": { "users": { "relations": {
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
            &EntityRegistry::from_json(&json!({ "unmapped": "identity", "entities": { "users": { "relations": {
                "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id", "mongo": "referenced" }
            } } } }))
            .expect("schema"),
            &limits(),
        )
        .expect_err("referenced not supported yet");
        assert!(matches!(err, QueryError::FeatureUnsupportedByTarget { .. }));
    }

    /// W11: a `through` relation predicate used to render as a plain
    /// `$elemMatch` on the relation name — wrong results, no error.
    #[test]
    fn test_many_to_many_relation_filter_is_capability_error() {
        let err = translate_mongo(
            &json!({
                "source": "users",
                "filter": { "some": [{"field": "tags"}, {"==": [{"field": "label"}, "vip"]}] }
            }),
            &serde_json::Map::new(),
            &EntityRegistry::from_json(
                &json!({ "unmapped": "identity", "entities": { "users": { "relations": {
                "tags": {
                    "to": "tags", "kind": "many_to_many", "local": "id", "foreign": "id",
                    "through": { "table": "user_tags", "local": "user_id", "foreign": "tag_id" }
                }
            } } } }),
            )
            .expect("schema"),
            &limits(),
        )
        .expect_err("m2m filter must be gated, not approximated");
        assert!(
            matches!(err, QueryError::FeatureUnsupportedByTarget { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("tags"), "{err}");
    }

    /// F26: `include` used to be silently dropped — parents with no children
    /// and no error.
    ///
    /// Both selection shapes must produce the *capability* error. The unsorted
    /// one is the regression: F27's "an include needs a sort" is the SQL
    /// renderer's rule, and enforcing it during envelope parsing told a MongoDB
    /// caller to add a sort to something Mongo cannot answer at all.
    #[test]
    fn test_include_is_capability_error() {
        for selection in [
            json!({ "sort": [{ "id": "asc" }], "limit": 5 }),
            json!({ "limit": 5 }),
        ] {
            let err = translate_mongo(
                &json!({ "source": "users", "include": { "orders": selection } }),
                &serde_json::Map::new(),
                &EntityRegistry::from_json(&json!({ "unmapped": "identity", "entities": { "users": { "relations": {
                    "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id" }
                } } } }))
                .expect("schema"),
                &limits(),
            )
            .expect_err("include must be gated on mongo");
            assert!(
                matches!(err, QueryError::FeatureUnsupportedByTarget { .. }),
                "{err}"
            );
            assert!(err.to_string().contains("include 'orders'"), "{err}");
        }
    }

    #[test]
    fn test_limit_exceeds_max_rejected() {
        let err = translate_mongo(
            &json!({ "source": "t", "limit": 9999 }),
            &serde_json::Map::new(),
            &EntityRegistry::identity(),
            &limits(),
        )
        .expect_err("over cap");
        assert!(matches!(err, QueryError::LimitExceeded { .. }));
    }

    /// W12: `skip` is bounded on Mongo too — it used to pass through unbounded.
    #[test]
    fn test_skip_exceeds_max_rejected() {
        let err = translate_mongo(
            &json!({ "source": "t", "skip": 10_001 }),
            &serde_json::Map::new(),
            &EntityRegistry::identity(),
            &limits(),
        )
        .expect_err("over the skip cap");
        assert!(matches!(err, QueryError::SkipExceeded { .. }), "{err}");
    }

    // ---- Write rendering ----

    fn resolve(input: serde_json::Value) -> crate::query::write::ResolvedWrite {
        crate::query::write::resolve_write(
            &input,
            &serde_json::Map::new(),
            &EntityRegistry::identity(),
            &crate::config::WriteConfig {
                max_rows: 1000,
                allow_unfiltered: true,
            },
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
        // W10: names pass through — `id` is a field, not the document key.
        assert_eq!(
            mw,
            MongoWrite::Insert {
                collection: "users".to_string(),
                docs: vec![
                    doc! { "id": "u1", "name": "Ada" },
                    doc! { "id": "u2", "name": "Bob" },
                ],
            }
        );
    }

    /// #263: tagged values in `set`/`values` write native BSON types.
    #[test]
    fn test_write_set_renders_tagged_values_natively() {
        let mw = render_write(&resolve(json!({
            "op": "update", "target": "meetings",
            "set": { "expires_at": { "$date": 1_714_780_800_000_i64 } },
            "filter": { "==": [{ "field": "_id" }, { "$oid": "665f1f77bcf86cd799439011" }] }
        })))
        .expect("render");
        let oid = mongodb::bson::oid::ObjectId::parse_str("665f1f77bcf86cd799439011")
            .expect("valid test oid");
        assert_eq!(
            mw,
            MongoWrite::Update {
                collection: "meetings".to_string(),
                filter: doc! { "_id": { "$eq": oid } },
                update: doc! { "$set": {
                    "expires_at": mongodb::bson::DateTime::from_millis(1_714_780_800_000)
                } },
                upsert: false,
                multi: true,
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
                filter: doc! { "id": { "$eq": "u1" } },
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

    // -----------------------------------------------------------------
    // F28: an ordered insert_many failure names the prefix it applied
    // -----------------------------------------------------------------

    fn dup(index: usize) -> (usize, Json) {
        (
            index,
            json!({ "code": 11000, "message": "duplicate key error" }),
        )
    }

    /// The defect: `insert_many` is ordered, so a failure at index 2 has
    /// already committed 0 and 1 and will never attempt 3 or 4. The driver
    /// error carried no index, so the caller could not tell which.
    #[test]
    fn an_ordered_failure_splits_the_batch_into_applied_failed_and_untried() {
        let out = insert_outcome(5, &[dup(2)]);

        assert!(out.is_partial(), "{:?}", out);
        let j = out.to_json();
        assert_eq!(j["status"], "partial", "{j}");
        assert_eq!(j["inserted"], 2, "0 and 1 committed: {j}");
        assert_eq!(j["failed"], 1, "{j}");
        assert_eq!(j["skipped"], 2, "3 and 4 were never attempted: {j}");

        let items = j["items"].as_array().expect("items");
        assert_eq!(items[0]["status"], "ok");
        assert_eq!(items[1]["status"], "ok");
        assert_eq!(items[2]["status"], "error");
        assert_eq!(items[2]["error"]["code"], 11000);
        assert_eq!(items[3]["status"], "skipped");
        assert_eq!(items[4]["status"], "skipped");
    }

    /// A failure on the very first document applies nothing, so it is a plain
    /// failure rather than a partial write.
    #[test]
    fn a_failure_at_index_zero_applies_nothing() {
        let out = insert_outcome(3, &[dup(0)]);
        assert!(out.nothing_applied(), "{:?}", out);
        assert!(!out.is_partial());
        assert_eq!(out.to_json()["skipped"], 2);
    }

    #[test]
    fn no_reported_errors_means_every_document_landed() {
        let out = insert_outcome(3, &[]);
        assert_eq!(out.inserted(), 3);
        assert!(!out.is_partial());
    }
}

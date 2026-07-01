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
    let limit = match spec.limit {
        Some(l) if l > max_limit => {
            return Err(QueryError::LimitExceeded {
                requested: l,
                max: max_limit,
            });
        }
        Some(l) => l,
        None => default_limit.min(max_limit),
    };

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
}

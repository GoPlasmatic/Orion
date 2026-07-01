//! Elasticsearch rendering.
//!
//! Walks a [`Cond`] + [`QuerySpec`] into an ES Query DSL search body over the same
//! IR. Every predicate is emitted in **filter context** (`bool.filter` /
//! `bool.must_not`, `should` + `minimum_should_match` for `or`) so results are
//! set-equivalent to SQL/Mongo, never relevance-ranked (§5.2). Relations render as
//! `nested` / `has_child` (§4.2). `all` and deep pagination are capability-gated
//! (§5.6, §5.8).

use serde_json::{Value as Json, json};

use crate::query::error::QueryError;
use crate::query::ir::{CmpOp, Cond, EsStorage, FieldRef, Quant, TextOp, Value};
use crate::query::spec::{QuerySpec, SortDir};

/// ES bounds `from + size` by `index.max_result_window` (default 10k). Beyond it
/// we raise a capability error rather than return a truncated page (§5.8).
const MAX_RESULT_WINDOW: u64 = 10_000;

/// A rendered Elasticsearch search: the index plus the request body.
#[derive(Debug, Clone, PartialEq)]
pub struct EsQuery {
    pub index: String,
    pub body: Json,
}

/// Build an `EsQuery` from the envelope and lowered condition, enforcing the
/// page-size bounds and the deep-pagination cap.
pub fn render(
    spec: &QuerySpec,
    cond: &Cond,
    index: &str,
    default_limit: u64,
    max_limit: u64,
) -> Result<EsQuery, QueryError> {
    let size = match spec.limit {
        Some(l) if l > max_limit => {
            return Err(QueryError::LimitExceeded {
                requested: l,
                max: max_limit,
            });
        }
        Some(l) => l,
        None => default_limit.min(max_limit),
    };
    let from = spec.skip.unwrap_or(0);
    if from.saturating_add(size) > MAX_RESULT_WINDOW {
        return Err(QueryError::FeatureUnsupportedByTarget {
            feature: format!(
                "deep pagination (from {from} + size {size} exceeds max_result_window {MAX_RESULT_WINDOW})"
            ),
            target: "elasticsearch".to_string(),
        });
    }

    let mut body = json!({
        "query": query_json(cond, "")?,
        "size": size,
        "from": from,
    });

    if !spec.sort.is_empty() {
        let sort: Vec<Json> = spec
            .sort
            .iter()
            .map(|k| {
                let (order, missing) = match k.dir {
                    // nulls last on asc, first on desc (§5.7).
                    SortDir::Asc => ("asc", "_last"),
                    SortDir::Desc => ("desc", "_first"),
                };
                json!({ &k.field: { "order": order, "missing": missing } })
            })
            .collect();
        body["sort"] = Json::Array(sort);
    }

    if !spec.fields.is_empty() {
        body["_source"] = json!(spec.fields);
    }

    Ok(EsQuery {
        index: index.to_string(),
        body,
    })
}

/// Render a `Cond` into an ES query clause (filter context). `prefix` qualifies
/// bare field names inside a `nested` query (e.g. `"orders."`).
fn query_json(cond: &Cond, prefix: &str) -> Result<Json, QueryError> {
    Ok(match cond {
        Cond::True => json!({ "match_all": {} }),
        Cond::False => json!({ "match_none": {} }),
        Cond::And(cs) => json!({ "bool": { "filter": clauses(cs, prefix)? } }),
        Cond::Or(cs) => {
            json!({ "bool": { "should": clauses(cs, prefix)?, "minimum_should_match": 1 } })
        }
        Cond::Not(inner) => json!({ "bool": { "must_not": [query_json(inner, prefix)?] } }),
        Cond::Compare { field, op, value } => {
            let f = fname(field, prefix);
            let v = to_json(value);
            match op {
                CmpOp::Eq => json!({ "term": { f: v } }),
                CmpOp::Ne => json!({ "bool": { "must_not": [{ "term": { f: v } }] } }),
                CmpOp::Lt => json!({ "range": { f: { "lt": v } } }),
                CmpOp::Le => json!({ "range": { f: { "lte": v } } }),
                CmpOp::Gt => json!({ "range": { f: { "gt": v } } }),
                CmpOp::Ge => json!({ "range": { f: { "gte": v } } }),
            }
        }
        Cond::In {
            field,
            values,
            negated,
        } => {
            let f = fname(field, prefix);
            let vs: Vec<Json> = values.iter().map(to_json).collect();
            let terms = json!({ "terms": { f: vs } });
            if *negated {
                json!({ "bool": { "must_not": [terms] } })
            } else {
                terms
            }
        }
        Cond::IsNull { field, negated } => {
            let exists = json!({ "exists": { "field": fname(field, prefix) } });
            if *negated {
                exists
            } else {
                json!({ "bool": { "must_not": [exists] } })
            }
        }
        Cond::Between {
            field,
            low,
            high,
            low_incl,
            high_incl,
            negated,
        } => {
            let f = fname(field, prefix);
            let mut range = serde_json::Map::new();
            range.insert(
                if *low_incl { "gte" } else { "gt" }.to_string(),
                to_json(low),
            );
            range.insert(
                if *high_incl { "lte" } else { "lt" }.to_string(),
                to_json(high),
            );
            let clause = json!({ "range": { f: Json::Object(range) } });
            if *negated {
                json!({ "bool": { "must_not": [clause] } })
            } else {
                clause
            }
        }
        Cond::Text {
            field,
            op,
            pattern,
            ci,
        } => {
            let f = fname(field, prefix);
            match op {
                TextOp::StartsWith => {
                    let mut v = json!({ "value": pattern });
                    if *ci {
                        v["case_insensitive"] = Json::Bool(true);
                    }
                    json!({ "prefix": { f: v } })
                }
                TextOp::EndsWith => wildcard(&f, format!("*{}", wildcard_escape(pattern)), *ci),
                TextOp::Contains => wildcard(&f, format!("*{}*", wildcard_escape(pattern)), *ci),
            }
        }
        Cond::Rel { quant, rel, cond } => rel_json(*quant, &rel.name, rel.es, cond)?,
    })
}

/// Render a relation predicate as a `nested` or `has_child` query.
fn rel_json(
    quant: Quant,
    name: &str,
    storage: EsStorage,
    inner: &Cond,
) -> Result<Json, QueryError> {
    if quant == Quant::All {
        // `all` over ES nesting needs must_not-of-negation with empty-relation
        // caveats; not set-equivalent without an explicit opt-in (§5.6).
        return Err(QueryError::FeatureUnsupportedByTarget {
            feature: format!("`all` over relation '{name}'"),
            target: "elasticsearch".to_string(),
        });
    }
    let positive = match storage {
        EsStorage::Nested => {
            // Inside a nested query, fields are qualified with the path.
            let q = query_json(inner, &format!("{name}."))?;
            json!({ "nested": { "path": name, "query": q } })
        }
        EsStorage::Child => {
            let q = query_json(inner, "")?;
            json!({ "has_child": { "type": name, "query": q } })
        }
    };
    Ok(match quant {
        Quant::Any => positive,
        Quant::None => json!({ "bool": { "must_not": [positive] } }),
        Quant::All => unreachable!("handled above"),
    })
}

fn clauses(cs: &[Cond], prefix: &str) -> Result<Vec<Json>, QueryError> {
    cs.iter().map(|c| query_json(c, prefix)).collect()
}

fn wildcard(field: &str, value: String, ci: bool) -> Json {
    let mut v = json!({ "value": value });
    if ci {
        v["case_insensitive"] = Json::Bool(true);
    }
    json!({ "wildcard": { field: v } })
}

fn fname(field: &FieldRef, prefix: &str) -> String {
    if prefix.is_empty() {
        field.physical.clone()
    } else {
        format!("{prefix}{}", field.physical)
    }
}

fn to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f),
        Value::Str(s) => Json::String(s.clone()),
        Value::List(items) => Json::Array(items.iter().map(to_json).collect()),
    }
}

/// Escape wildcard metacharacters so user text matches literally in a `wildcard`.
fn wildcard_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{EntityRegistry, QueryError};
    use serde_json::json;

    fn es(query: Json) -> EsQuery {
        translate(&query, &EntityRegistry::default())
    }

    fn es_schema(query: Json, schema: Json) -> EsQuery {
        translate(&query, &EntityRegistry::from_json(&schema).expect("schema"))
    }

    /// Local translate helper (mirrors crate::query::translate_es).
    fn translate(query: &Json, reg: &EntityRegistry) -> EsQuery {
        let spec = crate::query::spec::parse(query).expect("spec");
        let cond = match &spec.filter {
            Some(f) => {
                crate::query::lower::lower_with(f, &serde_json::Map::new(), reg, &spec.source)
                    .expect("lower")
            }
            None => crate::query::ir::Cond::True,
        };
        let index = reg.physical_table(&spec.source);
        render(&spec, &cond, &index, 100, 1000).expect("render")
    }

    #[test]
    fn test_bool_filter_and_term() {
        let q = es(json!({
            "source": "users",
            "filter": { "and": [
                { "==": [{"field": "status"}, "active"] },
                { ">": [{"field": "age"}, 18] }
            ] }
        }));
        assert_eq!(q.index, "users");
        assert_eq!(
            q.body["query"],
            json!({ "bool": { "filter": [
                { "term": { "status": "active" } },
                { "range": { "age": { "gt": 18 } } }
            ] } })
        );
        assert_eq!(q.body["size"], json!(100));
        assert_eq!(q.body["from"], json!(0));
    }

    #[test]
    fn test_or_is_should() {
        let q = es(json!({
            "source": "t",
            "filter": { "or": [
                { "==": [{"field": "a"}, 1] },
                { "==": [{"field": "b"}, 2] }
            ] }
        }));
        assert_eq!(
            q.body["query"],
            json!({ "bool": { "should": [
                { "term": { "a": 1 } },
                { "term": { "b": 2 } }
            ], "minimum_should_match": 1 } })
        );
    }

    #[test]
    fn test_membership_terms() {
        let q = es(json!({ "source": "t", "filter": { "in": [{"field": "status"}, ["a", "b"]] } }));
        assert_eq!(
            q.body["query"],
            json!({ "terms": { "status": ["a", "b"] } })
        );
    }

    #[test]
    fn test_is_null_is_must_not_exists() {
        let q = es(json!({ "source": "t", "filter": { "==": [{"field": "email"}, null] } }));
        assert_eq!(
            q.body["query"],
            json!({ "bool": { "must_not": [{ "exists": { "field": "email" } }] } })
        );
    }

    #[test]
    fn test_range_inclusive() {
        let q = es(json!({ "source": "t", "filter": { "<=": [1, {"field": "x"}, 10] } }));
        assert_eq!(
            q.body["query"],
            json!({ "range": { "x": { "gte": 1, "lte": 10 } } })
        );
    }

    #[test]
    fn test_text_prefix_and_wildcard() {
        let starts =
            es(json!({ "source": "t", "filter": { "starts_with": [{"field": "name"}, "sm"] } }));
        assert_eq!(
            starts.body["query"],
            json!({ "prefix": { "name": { "value": "sm" } } })
        );
        let contains = es(json!({ "source": "t", "filter": { "in": ["a*b", {"field": "name"}] } }));
        assert_eq!(
            contains.body["query"],
            json!({ "wildcard": { "name": { "value": "*a\\*b*" } } })
        );
    }

    #[test]
    fn test_nested_relation() {
        let q = es_schema(
            json!({
                "source": "users",
                "filter": { "some": [{"field": "orders"}, {">": [{"field": "total"}, 100]}] }
            }),
            json!({ "entities": { "users": { "relations": {
                "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id", "es": "nested" }
            } } } }),
        );
        assert_eq!(
            q.body["query"],
            json!({ "nested": { "path": "orders", "query": {
                "range": { "orders.total": { "gt": 100 } }
            } } })
        );
    }

    #[test]
    fn test_has_child_relation() {
        let q = es_schema(
            json!({
                "source": "users",
                "filter": { "some": [{"field": "orders"}, {">": [{"field": "total"}, 100]}] }
            }),
            json!({ "entities": { "users": { "relations": {
                "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id", "es": "child" }
            } } } }),
        );
        assert_eq!(
            q.body["query"],
            json!({ "has_child": { "type": "orders", "query": {
                "range": { "total": { "gt": 100 } }
            } } })
        );
    }

    #[test]
    fn test_all_over_relation_is_capability_error() {
        let reg = EntityRegistry::from_json(&json!({ "entities": { "users": { "relations": {
            "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id", "es": "nested" }
        } } } }))
        .expect("schema");
        let spec = crate::query::spec::parse(&json!({
            "source": "users",
            "filter": { "all": [{"field": "orders"}, {">": [{"field": "total"}, 100]}] }
        }))
        .expect("spec");
        let cond = crate::query::lower::lower_with(
            spec.filter.as_ref().expect("filter"),
            &serde_json::Map::new(),
            &reg,
            "users",
        )
        .expect("lower");
        let err = render(&spec, &cond, "users", 100, 1000).expect_err("all is gated on ES");
        assert!(matches!(err, QueryError::FeatureUnsupportedByTarget { .. }));
    }

    #[test]
    fn test_deep_pagination_rejected() {
        let spec = crate::query::spec::parse(&json!({ "source": "t", "skip": 9999, "limit": 100 }))
            .expect("spec");
        let err =
            render(&spec, &crate::query::ir::Cond::True, "t", 100, 1000).expect_err("deep paging");
        assert!(matches!(err, QueryError::FeatureUnsupportedByTarget { .. }));
    }

    #[test]
    fn test_envelope_source_and_sort() {
        let q = es(json!({
            "source": "t",
            "fields": ["id", "name"],
            "sort": [{ "created_at": "desc" }],
            "limit": 20
        }));
        assert_eq!(q.body["_source"], json!(["id", "name"]));
        assert_eq!(
            q.body["sort"],
            json!([{ "created_at": { "order": "desc", "missing": "_first" } }])
        );
        assert_eq!(q.body["size"], json!(20));
    }
}

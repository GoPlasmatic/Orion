//! Elasticsearch rendering.
//!
//! Walks a [`Cond`] + [`QuerySpec`] into an ES Query DSL search body over the same
//! IR. Every predicate is emitted in **filter context** (`bool.filter` /
//! `bool.must_not`, `should` + `minimum_should_match` for `or`) so results are
//! set-equivalent to SQL/Mongo, never relevance-ranked. Relations render as
//! `nested` / `has_child`. `all` and deep pagination are capability-gated —
//! see the parity table in `docs/src/reference/data-dialect.md`.
//!
//! Also renders the write dialect: [`render_write`] turns a [`ResolvedWrite`] into
//! an [`EsWrite`] (`_bulk` / `_update_by_query` / `_delete_by_query` / `_update` /
//! `_doc?op_type=create`), reusing the same condition rendering for the
//! update/delete filter.

use serde_json::{Value as Json, json};

use crate::config::QueryConfig;
use crate::query::bulk::{BulkOutcome, ItemOutcome};
use crate::query::error::QueryError;
use crate::query::ir::{CmpOp, Cond, EsStorage, FieldRef, Quant, TextOp, Value};
use crate::query::spec::QuerySpec;
use crate::query::write::{ConflictAction, ResolvedConflict, ResolvedWrite, WriteError};

/// ES bounds `from + size` by `index.max_result_window` (default 10k). Beyond it
/// we raise a capability error rather than return a truncated page.
const MAX_RESULT_WINDOW: u64 = 10_000;

/// A rendered Elasticsearch search: the index plus the request body.
#[derive(Debug, Clone, PartialEq)]
pub struct EsQuery {
    pub index: String,
    pub body: Json,
}

/// Build an `EsQuery` from the envelope and lowered condition, enforcing the
/// page bounds and the deep-pagination cap.
pub fn render(
    spec: &QuerySpec,
    cond: &Cond,
    index: &str,
    limits: &QueryConfig,
) -> Result<EsQuery, QueryError> {
    super::reject_include(spec, "elasticsearch")?;

    let size = super::resolve_limit(spec.limit, limits)?;
    let from = super::resolve_skip(spec.skip, limits)?.unwrap_or(0);
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

    let plans = super::plan_sort(&spec.sort);
    if !plans.is_empty() {
        let sort: Vec<Json> = plans
            .iter()
            .map(|p| {
                // ES defaults `missing` to `_last` regardless of direction, so
                // the planned placement (W8, see `plan_sort`) is stated
                // explicitly both ways.
                let order = if p.ascending { "asc" } else { "desc" };
                let missing = if p.nulls_first { "_first" } else { "_last" };
                json!({ p.field: { "order": order, "missing": missing } })
            })
            .collect();
        body["sort"] = Json::Array(sort);
    }

    if let Some(fields) = super::plan_projection(&spec.fields) {
        body["_source"] = json!(fields);
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
        Cond::Text { field, op, pattern } => {
            // W13: matching is case-sensitive at query time on `keyword`
            // fields, but a `text` field's analyzer has already folded the
            // indexed tokens, so ES cannot be made case-sensitive there at
            // all. The per-backend truth is in the parity table of
            // `docs/src/reference/data-dialect.md`.
            let f = fname(field, prefix);
            match op {
                TextOp::StartsWith => {
                    json!({ "prefix": { f: { "value": pattern } } })
                }
                TextOp::EndsWith => wildcard(&f, format!("*{}", wildcard_escape(pattern))),
                TextOp::Contains => wildcard(&f, format!("*{}*", wildcard_escape(pattern))),
            }
        }
        Cond::Rel { quant, rel, cond } => {
            super::reject_many_to_many(rel, "elasticsearch")?;
            rel_json(*quant, &rel.name, rel.es, cond)?
        }
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
        // caveats; not set-equivalent without an explicit opt-in (the `all`
        // null rule is unrenderable on nested documents).
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

fn wildcard(field: &str, value: String) -> Json {
    json!({ "wildcard": { field: { "value": value } } })
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

// ---- Write rendering (insert / update / delete / upsert) ----

/// A rendered Elasticsearch write, ready for the HTTP call the handler makes.
/// Each variant implies the method, path, and refresh parameter; bodies are plain
/// JSON (NDJSON for `_bulk`, assembled by [`bulk_ndjson`]).
#[derive(Debug, Clone, PartialEq)]
pub enum EsWrite {
    /// `POST {index}/_bulk?refresh=wait_for` — one `index` action + source line
    /// per row. A `Some` id becomes the action's `_id`; `None` auto-generates.
    BulkInsert {
        index: String,
        docs: Vec<(Option<String>, Json)>,
    },
    /// `POST {index}/_update_by_query?refresh=true` — query + painless script.
    UpdateByQuery { index: String, body: Json },
    /// `POST {index}/_delete_by_query?refresh=true` — query only.
    DeleteByQuery { index: String, body: Json },
    /// `POST {index}/_update/{id}?refresh=wait_for` — single-document upsert
    /// (`doc` / `doc_as_upsert` / `upsert`).
    UpdateDoc {
        index: String,
        id: String,
        body: Json,
    },
    /// `PUT {index}/_doc/{id}?op_type=create&refresh=wait_for` — insert-if-absent;
    /// an HTTP 409 is the "conflict → do nothing" no-op.
    CreateDoc {
        index: String,
        id: String,
        doc: Json,
    },
}

/// Render a resolved mutation into an [`EsWrite`]. The `filter` of an
/// update/delete reuses the query dialect's [`query_json`]; values become JSON via
/// the same [`to_json`] the read path uses.
///
/// Unlike Mongo there is **no** implicit `id` → `_id` mapping: `_id` is metadata
/// outside `_source`, and a genuine `id` field inside `_source` is legal, so the
/// rename is an explicit schema decision (`{"columns": {"id": {"name": "_id"}}}`).
/// A physical `_id` column is lifted out of the source into the action/path.
pub fn render_write(w: &ResolvedWrite) -> Result<EsWrite, WriteError> {
    if !w.returning().is_empty() {
        return Err(WriteError::Query(QueryError::FeatureUnsupportedByTarget {
            feature: "returning".to_string(),
            target: "elasticsearch".to_string(),
        }));
    }
    Ok(match w {
        ResolvedWrite::Insert {
            table,
            columns,
            rows,
            ..
        } => {
            let id_idx = columns.iter().position(|c| c == "_id");
            let mut docs = Vec::with_capacity(rows.len());
            for row in rows {
                let id = match id_idx {
                    Some(i) => id_string(&row[i], "values")?,
                    None => None,
                };
                docs.push((id, source_doc(columns, row, "_id")));
            }
            EsWrite::BulkInsert {
                index: table.clone(),
                docs,
            }
        }
        ResolvedWrite::Update {
            table, set, cond, ..
        } => {
            reject_id_in_set(set)?;
            // Field names AND values travel as painless params — nothing
            // user-controlled is spliced into the script source.
            let mut source = String::new();
            let mut params = serde_json::Map::new();
            for (i, (col, v)) in set.iter().enumerate() {
                source.push_str(&format!("ctx._source[params.f{i}] = params.v{i};"));
                params.insert(format!("f{i}"), Json::String(col.clone()));
                params.insert(format!("v{i}"), to_json(v));
            }
            EsWrite::UpdateByQuery {
                index: table.clone(),
                body: json!({
                    "query": cond_to_query(cond)?,
                    "script": { "lang": "painless", "source": source, "params": params },
                }),
            }
        }
        ResolvedWrite::Delete { table, cond, .. } => EsWrite::DeleteByQuery {
            index: table.clone(),
            body: json!({ "query": cond_to_query(cond)? }),
        },
        ResolvedWrite::Upsert {
            table,
            columns,
            rows,
            set,
            conflict,
            ..
        } => render_es_upsert(table, columns, rows, set, conflict)?,
    })
}

fn render_es_upsert(
    table: &str,
    columns: &[String],
    rows: &[Vec<Value>],
    set: &[(String, Value)],
    conflict: &ResolvedConflict,
) -> Result<EsWrite, WriteError> {
    // A single-document upsert keyed on `_id`. Bulk upsert would need one
    // `_update` call per row; deferred (fail loudly, don't guess).
    if rows.len() != 1 {
        return Err(WriteError::Query(QueryError::FeatureUnsupportedByTarget {
            feature: "bulk upsert".to_string(),
            target: "elasticsearch".to_string(),
        }));
    }
    // ES has no unique constraints; the only conflict key it can express is the
    // document `_id`.
    if conflict.targets.len() != 1 || conflict.targets[0] != "_id" {
        return Err(WriteError::Query(QueryError::FeatureUnsupportedByTarget {
            feature: format!(
                "upsert on conflict target [{}] (Elasticsearch keys upserts on the document `_id`; declare a schema rename to \"_id\")",
                conflict.targets.join(", ")
            ),
            target: "elasticsearch".to_string(),
        }));
    }
    let row = &rows[0];
    let idx = columns.iter().position(|c| c == "_id").ok_or_else(|| {
        WriteError::Query(QueryError::InvalidEnvelope(
            "on_conflict target '_id' must be one of the inserted columns".to_string(),
        ))
    })?;
    let id = id_string(&row[idx], "values")?.ok_or_else(|| {
        WriteError::Query(QueryError::NotRepresentable {
            what: "a null `_id` in an upsert".to_string(),
            at: "values".to_string(),
        })
    })?;
    let doc = source_doc(columns, row, "_id");

    Ok(match conflict.action {
        ConflictAction::Update => {
            reject_id_in_set(set)?;
            if set.is_empty() {
                // Overwrite every non-`_id` column on conflict; index the row
                // when absent.
                EsWrite::UpdateDoc {
                    index: table.to_string(),
                    id,
                    body: json!({ "doc": doc, "doc_as_upsert": true }),
                }
            } else {
                // On conflict apply `set`; on insert index the row overlaid with
                // `set` (Mongo's `$set` + `$setOnInsert` split).
                let mut set_doc = serde_json::Map::new();
                for (col, v) in set {
                    set_doc.insert(col.clone(), to_json(v));
                }
                let mut merged = match &doc {
                    Json::Object(m) => m.clone(),
                    _ => serde_json::Map::new(),
                };
                for (k, v) in &set_doc {
                    merged.insert(k.clone(), v.clone());
                }
                EsWrite::UpdateDoc {
                    index: table.to_string(),
                    id,
                    body: json!({ "doc": Json::Object(set_doc), "upsert": Json::Object(merged) }),
                }
            }
        }
        ConflictAction::Nothing => EsWrite::CreateDoc {
            index: table.to_string(),
            id,
            doc,
        },
    })
}

/// Serialise a rendered bulk insert into the `_bulk` NDJSON body. The trailing
/// newline is mandatory — ES rejects bulk bodies without it.
pub fn bulk_ndjson(docs: &[(Option<String>, Json)]) -> String {
    let mut out = String::new();
    for (id, source) in docs {
        let action = match id {
            Some(id) => json!({ "index": { "_id": id } }),
            None => json!({ "index": {} }),
        };
        out.push_str(&action.to_string());
        out.push('\n');
        out.push_str(&source.to_string());
        out.push('\n');
    }
    out
}

/// Read a `_bulk` response into per-item outcomes (F28).
///
/// `_bulk` is not fail-fast: ES attempts every action and reports each one
/// separately, so *any* subset can have landed. The handler previously took the
/// first `error` out of `items`, discarded the rest and failed the call, which
/// named neither how many documents were written nor which ones — the caller
/// could not compensate without re-reading the index.
///
/// `sent` is the number of documents submitted; an item ES did not report for
/// is recorded as an error rather than assumed written.
pub fn bulk_outcome(body: &Json, sent: usize) -> BulkOutcome {
    let items = body.get("items").and_then(|i| i.as_array());
    let outcomes = (0..sent)
        .map(|i| {
            // Each element is a single-key object naming the action
            // (`index` / `create`); the result sits under that key.
            let result = items
                .and_then(|a| a.get(i))
                .and_then(|it| it.get("index").or_else(|| it.get("create")));
            match result {
                None => ItemOutcome::error(
                    i,
                    json!({ "reason": "Elasticsearch reported no result for this item" }),
                ),
                Some(r) => match r.get("error") {
                    Some(e) if !e.is_null() => ItemOutcome::error(i, e.clone()),
                    _ => ItemOutcome::ok(i, r.get("_id").cloned()),
                },
            }
        })
        .collect();
    BulkOutcome { items: outcomes }
}

/// Lower an optional filter to a query clause (`None` — an acknowledged
/// unfiltered mutation — matches everything).
fn cond_to_query(cond: &Option<Cond>) -> Result<Json, WriteError> {
    Ok(match cond {
        None => json!({ "match_all": {} }),
        Some(c) => query_json(c, "").map_err(WriteError::from)?,
    })
}

/// `_id` is immutable metadata; a `set` touching it cannot be expressed.
fn reject_id_in_set(set: &[(String, Value)]) -> Result<(), WriteError> {
    if set.iter().any(|(c, _)| c == "_id") {
        return Err(WriteError::Query(QueryError::FeatureUnsupportedByTarget {
            feature: "updating `_id`".to_string(),
            target: "elasticsearch".to_string(),
        }));
    }
    Ok(())
}

/// The row as a `_source` document, excluding the `skip` column (`_id` lives in
/// the action/path — ES rejects it inside `_source`).
fn source_doc(columns: &[String], row: &[Value], skip: &str) -> Json {
    let mut m = serde_json::Map::new();
    for (col, v) in columns.iter().zip(row) {
        if col != skip {
            m.insert(col.clone(), to_json(v));
        }
    }
    Json::Object(m)
}

/// Coerce an `_id` value to the string form ES document ids use. `Null` means
/// "let ES auto-generate" (insert only; upsert rejects it).
fn id_string(v: &Value, at: &str) -> Result<Option<String>, WriteError> {
    Ok(match v {
        Value::Null => None,
        Value::Str(s) => Some(s.clone()),
        Value::Int(i) => Some(i.to_string()),
        _ => {
            return Err(WriteError::Query(QueryError::NotRepresentable {
                what: "a non-string/integer `_id` value".to_string(),
                at: at.to_string(),
            }));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{EntityRegistry, QueryError};
    use serde_json::json;

    fn es(query: Json) -> EsQuery {
        translate(&query, &EntityRegistry::identity())
    }

    fn es_schema(query: Json, schema: Json) -> EsQuery {
        translate(&query, &EntityRegistry::from_json(&schema).expect("schema"))
    }

    fn limits() -> QueryConfig {
        QueryConfig::default()
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
        let index = reg.physical_table(&spec.source).expect("validated");
        render(&spec, &cond, &index, &limits()).expect("render")
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
            json!({ "unmapped": "identity", "entities": { "users": { "relations": {
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
            json!({ "unmapped": "identity", "entities": { "users": { "relations": {
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
        let reg = EntityRegistry::from_json(&json!({ "unmapped": "identity", "entities": { "users": { "relations": {
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
        let err = render(&spec, &cond, "users", &limits()).expect_err("all is gated on ES");
        assert!(matches!(err, QueryError::FeatureUnsupportedByTarget { .. }));
    }

    /// W11: a `through` relation predicate used to render as a plain
    /// `nested`/`has_child` on the relation name — wrong results, no error.
    #[test]
    fn test_many_to_many_relation_filter_is_capability_error() {
        let reg = EntityRegistry::from_json(
            &json!({ "unmapped": "identity", "entities": { "users": { "relations": {
            "tags": {
                "to": "tags", "kind": "many_to_many", "local": "id", "foreign": "id",
                "through": { "table": "user_tags", "local": "user_id", "foreign": "tag_id" }
            }
        } } } }),
        )
        .expect("schema");
        let spec = crate::query::spec::parse(&json!({
            "source": "users",
            "filter": { "some": [{"field": "tags"}, {"==": [{"field": "label"}, "vip"]}] }
        }))
        .expect("spec");
        let cond = crate::query::lower::lower_with(
            spec.filter.as_ref().expect("filter"),
            &serde_json::Map::new(),
            &reg,
            "users",
        )
        .expect("lower");
        let err = render(&spec, &cond, "users", &limits())
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
    /// renderer's rule, and enforcing it during envelope parsing told an
    /// Elasticsearch caller to add a sort to something ES cannot answer at all.
    #[test]
    fn test_include_is_capability_error() {
        for selection in [
            json!({ "sort": [{ "id": "asc" }], "limit": 5 }),
            json!({ "limit": 5 }),
        ] {
            let spec = crate::query::spec::parse(
                &json!({ "source": "users", "include": { "orders": selection } }),
            )
            .expect("spec");
            let err = render(&spec, &crate::query::ir::Cond::True, "users", &limits())
                .expect_err("include must be gated on ES");
            assert!(
                matches!(err, QueryError::FeatureUnsupportedByTarget { .. }),
                "{err}"
            );
            assert!(err.to_string().contains("include 'orders'"), "{err}");
        }
    }

    #[test]
    fn test_deep_pagination_rejected() {
        let spec = crate::query::spec::parse(&json!({ "source": "t", "skip": 9999, "limit": 100 }))
            .expect("spec");
        let err =
            render(&spec, &crate::query::ir::Cond::True, "t", &limits()).expect_err("deep paging");
        assert!(matches!(err, QueryError::FeatureUnsupportedByTarget { .. }));
    }

    /// W12: the shared `max_skip` cap applies before the ES result-window
    /// check, with the same error every backend raises.
    #[test]
    fn test_skip_exceeds_max_rejected() {
        let spec =
            crate::query::spec::parse(&json!({ "source": "t", "skip": 10_001 })).expect("spec");
        let err = render(&spec, &crate::query::ir::Cond::True, "t", &limits())
            .expect_err("over the skip cap");
        assert!(matches!(err, QueryError::SkipExceeded { .. }), "{err}");
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
            json!([{ "created_at": { "order": "desc", "missing": "_last" } }])
        );
        assert_eq!(q.body["size"], json!(20));
    }

    // ---- Write rendering ----

    /// A config that lets every envelope shape through — the guards have
    /// their own tests in `write.rs`.
    fn permissive_writes() -> crate::config::WriteConfig {
        crate::config::WriteConfig {
            max_rows: 1000,
            allow_unfiltered: true,
        }
    }

    fn resolve(input: Json) -> crate::query::write::ResolvedWrite {
        crate::query::write::resolve_write(
            &input,
            &serde_json::Map::new(),
            &EntityRegistry::identity(),
            &permissive_writes(),
        )
        .expect("resolve_write should succeed")
    }

    fn resolve_schema(input: Json, schema: Json) -> crate::query::write::ResolvedWrite {
        crate::query::write::resolve_write(
            &input,
            &serde_json::Map::new(),
            &EntityRegistry::from_json(&schema).expect("schema"),
            &permissive_writes(),
        )
        .expect("resolve_write should succeed")
    }

    /// The schema declaring that the logical `id` keys the ES document.
    /// These tests are about ES `_id` handling, not the allowlist: the `id`
    /// rename still applies (declared columns win over the policy), while the
    /// other columns they write resolve by identity.
    fn id_schema() -> Json {
        json!({
            "unmapped": "identity",
            "entities": { "users": { "columns": { "id": { "name": "_id" } } } }
        })
    }

    #[test]
    fn test_es_insert_bulk_lifts_id() {
        let ew = render_write(&resolve_schema(
            json!({
                "op": "insert", "target": "users",
                "values": [ { "id": "u1", "name": "Ada" }, { "id": "u2", "name": "Bob" } ]
            }),
            id_schema(),
        ))
        .expect("render");
        // The physical `_id` column is lifted out of the source into the action.
        assert_eq!(
            ew,
            EsWrite::BulkInsert {
                index: "users".to_string(),
                docs: vec![
                    (Some("u1".to_string()), json!({ "name": "Ada" })),
                    (Some("u2".to_string()), json!({ "name": "Bob" })),
                ],
            }
        );
    }

    #[test]
    fn test_es_insert_id_stays_in_source_without_schema() {
        // No implicit `id` → `_id` mapping: without the schema rename, `id` is an
        // ordinary source field and ES auto-generates the document id.
        let ew = render_write(&resolve(json!({
            "op": "insert", "target": "users",
            "values": { "id": "u1", "name": "Ada" }
        })))
        .expect("render");
        assert_eq!(
            ew,
            EsWrite::BulkInsert {
                index: "users".to_string(),
                docs: vec![(None, json!({ "id": "u1", "name": "Ada" }))],
            }
        );
    }

    #[test]
    fn test_es_insert_null_id_autogenerates() {
        let ew = render_write(&resolve_schema(
            json!({
                "op": "insert", "target": "users",
                "values": { "id": null, "name": "Ada" }
            }),
            id_schema(),
        ))
        .expect("render");
        assert_eq!(
            ew,
            EsWrite::BulkInsert {
                index: "users".to_string(),
                docs: vec![(None, json!({ "name": "Ada" }))],
            }
        );
    }

    #[test]
    fn test_es_bulk_ndjson_shape() {
        let body = bulk_ndjson(&[
            (Some("u1".to_string()), json!({ "name": "Ada" })),
            (None, json!({ "name": "Bob" })),
        ]);
        assert_eq!(
            body,
            "{\"index\":{\"_id\":\"u1\"}}\n{\"name\":\"Ada\"}\n{\"index\":{}}\n{\"name\":\"Bob\"}\n"
        );
    }

    #[test]
    fn test_es_update_by_query_script_params() {
        let ew = render_write(&resolve(json!({
            "op": "update", "target": "users",
            "set": { "status": "inactive", "age": null },
            "filter": { ">": [{ "field": "age" }, 25] }
        })))
        .expect("render");
        // `set` keeps the envelope's key order (serde_json preserve_order):
        // status first, then age.
        assert_eq!(
            ew,
            EsWrite::UpdateByQuery {
                index: "users".to_string(),
                body: json!({
                    "query": { "range": { "age": { "gt": 25 } } },
                    "script": {
                        "lang": "painless",
                        "source": "ctx._source[params.f0] = params.v0;ctx._source[params.f1] = params.v1;",
                        "params": { "f0": "status", "v0": "inactive", "f1": "age", "v1": null }
                    }
                }),
            }
        );
    }

    #[test]
    fn test_es_update_all_true_is_match_all() {
        let ew = render_write(&resolve(json!({
            "op": "update", "target": "users",
            "set": { "status": "x" },
            "all": true
        })))
        .expect("render");
        assert_eq!(
            ew,
            EsWrite::UpdateByQuery {
                index: "users".to_string(),
                body: json!({
                    "query": { "match_all": {} },
                    "script": {
                        "lang": "painless",
                        "source": "ctx._source[params.f0] = params.v0;",
                        "params": { "f0": "status", "v0": "x" }
                    }
                }),
            }
        );
    }

    #[test]
    fn test_es_delete_by_query() {
        let ew = render_write(&resolve(json!({
            "op": "delete", "target": "sessions",
            "filter": { "<": [{ "field": "age" }, 0] }
        })))
        .expect("render");
        assert_eq!(
            ew,
            EsWrite::DeleteByQuery {
                index: "sessions".to_string(),
                body: json!({ "query": { "range": { "age": { "lt": 0 } } } }),
            }
        );
    }

    #[test]
    fn test_es_upsert_update_is_doc_as_upsert() {
        let ew = render_write(&resolve_schema(
            json!({
                "op": "upsert", "target": "users",
                "values": { "id": "u1", "name": "Alice2", "age": 31 },
                "on_conflict": { "target": ["id"], "action": "update" }
            }),
            id_schema(),
        ))
        .expect("render");
        assert_eq!(
            ew,
            EsWrite::UpdateDoc {
                index: "users".to_string(),
                id: "u1".to_string(),
                body: json!({ "doc": { "age": 31, "name": "Alice2" }, "doc_as_upsert": true }),
            }
        );
    }

    #[test]
    fn test_es_upsert_with_set_uses_doc_plus_upsert() {
        let ew = render_write(&resolve_schema(
            json!({
                "op": "upsert", "target": "users",
                "values": { "id": "u1", "name": "Alice2", "age": 31 },
                "set": { "status": "active" },
                "on_conflict": { "target": ["id"], "action": "update" }
            }),
            id_schema(),
        ))
        .expect("render");
        // On conflict apply `set`; on insert the row overlaid with `set`.
        assert_eq!(
            ew,
            EsWrite::UpdateDoc {
                index: "users".to_string(),
                id: "u1".to_string(),
                body: json!({
                    "doc": { "status": "active" },
                    "upsert": { "age": 31, "name": "Alice2", "status": "active" }
                }),
            }
        );
    }

    #[test]
    fn test_es_upsert_nothing_is_create() {
        let ew = render_write(&resolve_schema(
            json!({
                "op": "upsert", "target": "users",
                "values": { "id": "u1", "name": "Ada" },
                "on_conflict": { "target": ["id"], "action": "nothing" }
            }),
            id_schema(),
        ))
        .expect("render");
        assert_eq!(
            ew,
            EsWrite::CreateDoc {
                index: "users".to_string(),
                id: "u1".to_string(),
                doc: json!({ "name": "Ada" }),
            }
        );
    }

    #[test]
    fn test_es_bulk_upsert_rejected() {
        let err = render_write(&resolve_schema(
            json!({
                "op": "upsert", "target": "users",
                "values": [ { "id": "u1", "name": "A" }, { "id": "u2", "name": "B" } ],
                "on_conflict": { "target": ["id"], "action": "update" }
            }),
            id_schema(),
        ))
        .expect_err("bulk upsert is gated on ES");
        assert!(matches!(
            err,
            WriteError::Query(QueryError::FeatureUnsupportedByTarget { .. })
        ));
    }

    #[test]
    fn test_es_upsert_non_id_target_rejected() {
        let err = render_write(&resolve(json!({
            "op": "upsert", "target": "users",
            "values": { "email": "a@x.io", "name": "Ada" },
            "on_conflict": { "target": ["email"], "action": "update" }
        })))
        .expect_err("non-_id conflict target is gated on ES");
        assert!(matches!(
            err,
            WriteError::Query(QueryError::FeatureUnsupportedByTarget { .. })
        ));
    }

    #[test]
    fn test_es_returning_rejected() {
        let err = render_write(&resolve(json!({
            "op": "insert", "target": "users",
            "values": { "name": "Ada" },
            "returning": ["id"]
        })))
        .expect_err("returning is gated on ES");
        assert!(matches!(
            err,
            WriteError::Query(QueryError::FeatureUnsupportedByTarget { .. })
        ));
    }

    #[test]
    fn test_es_update_set_id_rejected() {
        let err = render_write(&resolve_schema(
            json!({
                "op": "update", "target": "users",
                "set": { "id": "u9" },
                "filter": { "==": [{ "field": "name" }, "Ada"] }
            }),
            id_schema(),
        ))
        .expect_err("updating _id is gated on ES");
        assert!(matches!(
            err,
            WriteError::Query(QueryError::FeatureUnsupportedByTarget { .. })
        ));
    }

    // -----------------------------------------------------------------
    // F28: `_bulk` reports every item, not just the first error
    // -----------------------------------------------------------------

    fn bulk_body(items: Vec<Json>) -> Json {
        json!({ "errors": true, "items": items })
    }

    fn ok_item(id: &str) -> Json {
        json!({ "index": { "_id": id, "status": 201 } })
    }

    fn err_item(reason: &str) -> Json {
        json!({ "index": { "status": 409, "error": { "type": "version_conflict_engine_exception", "reason": reason } } })
    }

    /// The defect: ES applies each action independently, so a three-document
    /// bulk can land two. The handler used to return the first error and
    /// discard `items`, naming neither the count nor which ones survived.
    #[test]
    fn a_mixed_bulk_reports_each_item_with_its_index() {
        let body = bulk_body(vec![ok_item("a"), err_item("conflict"), ok_item("c")]);
        let out = bulk_outcome(&body, 3);

        assert!(out.is_partial(), "two of three landed: {:?}", out);
        assert_eq!(out.inserted(), 2);
        assert_eq!(out.ids(), vec![json!("a"), json!("c")]);

        let j = out.to_json();
        assert_eq!(j["status"], "partial", "{j}");
        assert_eq!(j["items"][1]["index"], 1, "{j}");
        assert_eq!(j["items"][1]["status"], "error", "{j}");
        assert_eq!(
            j["items"][1]["error"]["type"], "version_conflict_engine_exception",
            "the item's own error must survive, not just the first one: {j}"
        );
    }

    #[test]
    fn a_clean_bulk_reports_every_id_in_order() {
        let body = json!({ "errors": false, "items": [ok_item("a"), ok_item("b")] });
        let out = bulk_outcome(&body, 2);
        assert!(!out.is_partial());
        assert_eq!(out.ids(), vec![json!("a"), json!("b")]);
        assert_eq!(out.to_json()["status"], "ok");
    }

    /// Nothing landed: there is no partial state, so the handler turns this
    /// into a hard failure rather than a 207.
    #[test]
    fn a_wholly_failed_bulk_reports_nothing_applied() {
        let body = bulk_body(vec![err_item("a"), err_item("b")]);
        let out = bulk_outcome(&body, 2);
        assert!(out.nothing_applied());
        assert!(!out.is_partial());
    }

    /// A short or missing `items` array must not be read as "the rest
    /// succeeded" — an unreported document is one we cannot claim was written.
    #[test]
    fn unreported_items_are_errors_not_silent_successes() {
        let out = bulk_outcome(&bulk_body(vec![ok_item("a")]), 3);
        assert_eq!(out.inserted(), 1, "{:?}", out);
        assert_eq!(out.count(crate::query::bulk::ItemStatus::Error), 2);

        let none = bulk_outcome(&json!({ "errors": true }), 2);
        assert!(none.nothing_applied(), "{:?}", none);
    }

    /// `create` actions (upsert with `action: "nothing"`) report under a
    /// different key than `index` and must be read the same way.
    #[test]
    fn create_actions_are_read_like_index_actions() {
        let body =
            json!({ "errors": false, "items": [{ "create": { "_id": "z", "status": 201 } }] });
        assert_eq!(bulk_outcome(&body, 1).ids(), vec![json!("z")]);
    }
}

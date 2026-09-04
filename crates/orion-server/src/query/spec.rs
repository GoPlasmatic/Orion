//! Envelope parsing: the plain (non-JSONLogic) shell around the `filter`.
//!
//! Parses `source` / `filter` / `fields` / `sort` / `limit` / `skip` / `include`
//! into a [`QuerySpec`]. The `filter` is kept as raw JSON here and lowered
//! separately by [`crate::query::lower`]; `include` is parsed into [`IncludeSpec`]s
//! and hydrated by the handler (a per-relation child query).

use serde_json::Value as Json;

use crate::query::error::QueryError;

/// The parsed query envelope. `filter` is still raw JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct QuerySpec {
    pub source: String,
    pub filter: Option<Json>,
    pub fields: Vec<String>,
    pub sort: Vec<SortKey>,
    pub limit: Option<u64>,
    pub skip: Option<u64>,
    pub include: Vec<IncludeSpec>,
    /// Answer *how many rows match* instead of returning them.
    ///
    /// The one envelope key that changes the shape of the answer: the result is
    /// `{"count": n}`, not a row array. Every key that shapes a row set —
    /// `fields`, `sort`, `limit`, `skip`, `include` — is refused alongside it,
    /// because each has two readings over a count and guessing between them is
    /// what this dialect does not do.
    pub count: bool,
}

/// A related collection to nest in the result:
/// `"orders": { "fields": [..], "sort": [{ "id": "asc" }], "limit": n }`.
#[derive(Debug, Clone, PartialEq)]
pub struct IncludeSpec {
    /// Relation name (declared in the schema) and the output field name.
    pub relation: String,
    /// Child fields to return; empty means all.
    pub fields: Vec<String>,
    /// Ordering of the children within each parent. Required by the SQL planner
    /// (F27): the per-parent page is cut in SQL, so without an order key "the
    /// first `n` children" is whatever the plan happened to emit. Left empty
    /// here rather than refused, so a document store — which cannot answer an
    /// include at all — reaches its capability gate first.
    pub sort: Vec<SortKey>,
    /// Max related rows per parent. Absent means `query.default_limit`, and a
    /// value above `query.max_limit` is rejected — the same policy the
    /// envelope's own `limit` gets.
    pub limit: Option<u64>,
}

/// One ordering key, e.g. `{ "created_at": "desc" }`.
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    pub field: String,
    pub dir: SortDir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

fn invalid(msg: impl Into<String>) -> QueryError {
    QueryError::InvalidEnvelope(msg.into())
}

/// The complete key set of the query envelope. Anything else is a typo, and a
/// typo here is a filter/projection/limit silently not applying (W6).
const ENVELOPE_KEYS: [&str; 8] = [
    "source", "filter", "fields", "sort", "limit", "skip", "include", "count",
];

/// The keys that shape a row set, and so cannot travel with `count`.
const ROW_SHAPE_KEYS: [&str; 5] = ["fields", "sort", "limit", "skip", "include"];

/// Parse the `query` object into a [`QuerySpec`].
pub fn parse(query: &Json) -> Result<QuerySpec, QueryError> {
    let obj = query
        .as_object()
        .ok_or_else(|| invalid("query must be a JSON object"))?;

    // W6: unknown keys were silently ignored — `"fileds"` selected every
    // column, `"lmit"` fell back to the default. Reject, naming the key.
    if let Some(unknown) = obj.keys().find(|k| !ENVELOPE_KEYS.contains(&k.as_str())) {
        let suggestion = crate::query::error::nearest(unknown, ENVELOPE_KEYS)
            .map(|k| format!(" — did you mean \"{k}\"?"))
            .unwrap_or_default();
        return Err(invalid(format!(
            "unknown key '{unknown}' in query envelope (expected \
             source/filter/fields/sort/limit/skip/include){suggestion}"
        )));
    }

    let source = obj
        .get("source")
        .and_then(|v| v.as_str())
        .ok_or_else(|| invalid("missing required string field 'source'"))?
        .to_string();
    if source.is_empty() {
        return Err(invalid("'source' must not be empty"));
    }

    let filter = obj.get("filter").filter(|v| !v.is_null()).cloned();

    let fields = match obj.get("fields") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for (i, f) in arr.iter().enumerate() {
                let s = f
                    .as_str()
                    .ok_or_else(|| invalid(format!("fields[{i}] must be a string")))?;
                out.push(s.to_string());
            }
            out
        }
        Some(_) => return Err(invalid("'fields' must be an array of strings")),
    };

    let sort = parse_sort(obj.get("sort"), "sort")?;
    let limit = parse_u64(obj.get("limit"), "limit")?;
    let skip = parse_u64(obj.get("skip"), "skip")?;
    let include = parse_include(obj.get("include"))?;

    let count = match obj.get("count") {
        None | Some(Json::Null) => false,
        Some(Json::Bool(b)) => *b,
        Some(_) => return Err(invalid("'count' must be a boolean")),
    };
    // Refused rather than ignored. "Count the first 10" and "count all, I also
    // wanted 10 rows" are both readings of `{"count": true, "limit": 10}`, and
    // a projection or an ordering over a single number is not a reading at all.
    if count && let Some(key) = ROW_SHAPE_KEYS.iter().find(|k| obj.contains_key(**k)) {
        return Err(invalid(format!(
            "'{key}' has no meaning alongside \"count\": true, which answers \
             {{\"count\": n}} rather than a row set — remove one of the two"
        )));
    }

    Ok(QuerySpec {
        source,
        filter,
        fields,
        sort,
        limit,
        skip,
        include,
        count,
    })
}

fn parse_include(v: Option<&Json>) -> Result<Vec<IncludeSpec>, QueryError> {
    let obj = match v {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Object(m)) => m,
        Some(_) => {
            return Err(invalid(
                "'include' must be an object of relation → selection",
            ));
        }
    };
    let mut out = Vec::with_capacity(obj.len());
    for (relation, sel) in obj {
        let sel = sel
            .as_object()
            .ok_or_else(|| invalid(format!("include.{relation} must be an object")))?;
        if let Some(unknown) = sel
            .keys()
            .find(|k| !matches!(k.as_str(), "fields" | "sort" | "limit"))
        {
            return Err(invalid(format!(
                "unknown key '{unknown}' in include.{relation} (expected fields/sort/limit)"
            )));
        }
        let fields = match sel.get("fields") {
            None | Some(Json::Null) => Vec::new(),
            Some(Json::Array(arr)) => {
                let mut fs = Vec::with_capacity(arr.len());
                for (i, f) in arr.iter().enumerate() {
                    let s = f.as_str().ok_or_else(|| {
                        invalid(format!("include.{relation}.fields[{i}] must be a string"))
                    })?;
                    fs.push(s.to_string());
                }
                fs
            }
            Some(_) => {
                return Err(invalid(format!(
                    "include.{relation}.fields must be an array"
                )));
            }
        };
        let sort = parse_sort(sel.get("sort"), &format!("include.{relation}.sort"))?;
        let limit = parse_u64(sel.get("limit"), &format!("include.{relation}.limit"))?;
        // The "an include must state a sort" rule (F27) is *not* checked here.
        // It is a property of the SQL renderer — the per-parent page is cut with
        // `ROW_NUMBER() OVER (PARTITION BY …)`, which needs an order key — and
        // this function runs for every backend. Enforcing it at parse time made
        // a MongoDB or Elasticsearch caller fail with "include.orders requires a
        // 'sort'" instead of the capability error that tells them the real
        // answer (include is SQL-only). See `query::plan_sql`.
        out.push(IncludeSpec {
            relation: relation.clone(),
            fields,
            sort,
            limit,
        });
    }
    Ok(out)
}

/// Parse a sort array. `at` names the location for error messages (`sort` at the
/// envelope root, `include.<rel>.sort` for a nested collection).
fn parse_sort(v: Option<&Json>, at: &str) -> Result<Vec<SortKey>, QueryError> {
    let arr = match v {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(a)) => a,
        Some(_) => return Err(invalid(format!("'{at}' must be an array"))),
    };
    let mut out = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        let obj = entry.as_object().ok_or_else(|| {
            invalid(format!(
                "{at}[{i}] must be an object like {{\"field\":\"asc\"}}"
            ))
        })?;
        if obj.len() != 1 {
            return Err(invalid(format!("{at}[{i}] must have exactly one key")));
        }
        let (field, dirv) = obj.iter().next().expect("map has exactly one entry");
        let dir = match dirv.as_str() {
            Some("asc") => SortDir::Asc,
            Some("desc") => SortDir::Desc,
            _ => {
                return Err(invalid(format!(
                    "{at}[{i}].{field} must be \"asc\" or \"desc\""
                )));
            }
        };
        out.push(SortKey {
            field: field.clone(),
            dir,
        });
    }
    Ok(out)
}

fn parse_u64(v: Option<&Json>, name: &str) -> Result<Option<u64>, QueryError> {
    match v {
        None | Some(Json::Null) => Ok(None),
        Some(n) => {
            let u = n
                .as_u64()
                .ok_or_else(|| invalid(format!("'{name}' must be a non-negative integer")))?;
            Ok(Some(u))
        }
    }
}

impl QuerySpec {
    /// Replace every logical name in the projection and sort with its physical
    /// name, honouring renames, the `queryable` allowlist, `unmapped` policy
    /// and identifier rules — the same gate the filter already went through
    /// (W2).
    ///
    /// `resolve_field` used to have exactly one call site, the filter lowerer.
    /// `fields` and `sort` went to the backends as raw logical strings, so with
    /// `"unmapped": "reject"` and `{"secret": {"queryable": false}}`,
    /// `fields: ["secret"]` still emitted `SELECT "secret"` — the allowlist
    /// protected the filter and nothing else. A column rename broke projection
    /// and sort silently for the same reason.
    ///
    /// A read that names *no* fields is resolved too (F24): an empty `fields`
    /// renders `SELECT *` — no projection at all on Mongo/ES — which walked
    /// past the column allowlist entirely, because the caller named nothing to
    /// check. It is replaced by the entity's declared queryable columns; see
    /// [`EntityRegistry::default_projection`](crate::query::EntityRegistry::default_projection)
    /// for the two cases that legitimately stay a wildcard.
    ///
    /// Applied once per translation, before any backend sees the spec, so no
    /// renderer can receive a logical name.
    pub fn resolve_names(mut self, reg: &crate::query::EntityRegistry) -> Result<Self, QueryError> {
        if self.fields.is_empty() {
            self.fields = reg.default_projection(&self.source, "fields")?;
        } else {
            for (i, field) in self.fields.iter_mut().enumerate() {
                *field = reg
                    .resolve_field(&self.source, field, &format!("fields[{i}]"))?
                    .physical;
            }
        }
        for (i, key) in self.sort.iter_mut().enumerate() {
            key.field = reg
                .resolve_field(&self.source, &key.field, &format!("sort[{i}]"))?
                .physical;
        }
        // `include.fields` and `include.sort` name columns on the *related*
        // entity, so they resolve against the relation's target, not the root
        // — including the field-less case, which takes the target's default
        // projection rather than selecting every column blind.
        for inc in self.include.iter_mut() {
            let target = reg
                .resolve_relation(&self.source, &inc.relation, "include")?
                .1;
            if inc.fields.is_empty() {
                inc.fields =
                    reg.default_projection(&target, &format!("include.{}.fields", inc.relation))?;
            } else {
                for (i, field) in inc.fields.iter_mut().enumerate() {
                    *field = reg
                        .resolve_field(
                            &target,
                            field,
                            &format!("include.{}.fields[{i}]", inc.relation),
                        )?
                        .physical;
                }
            }
            for (i, key) in inc.sort.iter_mut().enumerate() {
                key.field = reg
                    .resolve_field(
                        &target,
                        &key.field,
                        &format!("include.{}.sort[{i}]", inc.relation),
                    )?
                    .physical;
            }
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------
    // W6: unknown envelope keys are typos that silently changed results
    // -----------------------------------------------------------------

    /// `"fileds"` used to select every column and `"lmit": 5000` fell back to
    /// the default of 100 — the misspelled intent was silently discarded.
    #[test]
    fn unknown_envelope_keys_are_rejected_naming_the_key() {
        for bad in ["fileds", "lmit", "orderby"] {
            let mut query = json!({ "source": "users" });
            query[bad] = json!(5000);
            let err = parse(&query).expect_err("unknown key must be rejected");
            assert!(err.to_string().contains(bad), "{err}");
            assert!(matches!(err, QueryError::InvalidEnvelope(_)), "{err}");
        }
    }

    #[test]
    fn unknown_include_selection_keys_are_rejected() {
        let err = parse(&json!({
            "source": "users",
            "include": { "orders": { "fields": ["id"], "limt": 5 } }
        }))
        .expect_err("unknown include key must be rejected");
        assert!(err.to_string().contains("limt"), "{err}");
        assert!(err.to_string().contains("include.orders"), "{err}");
    }

    /// F27's "an include must state a sort" is the SQL planner's rule, not the
    /// envelope's: parsing accepts an unordered include so that the document
    /// stores — which cannot answer *any* include — still reach their capability
    /// gate and say so. `query::plan_sql` rejects it (see
    /// `backend::sql::tests::test_include_without_sort_is_rejected`).
    #[test]
    fn an_unordered_include_parses_and_is_left_to_the_backend() {
        let spec = parse(&json!({
            "source": "users",
            "include": { "orders": { "fields": ["id"], "limit": 5 } }
        }))
        .expect("parsing is backend-neutral");
        assert!(spec.include[0].sort.is_empty());
    }

    #[test]
    fn the_full_envelope_still_parses() {
        let spec = parse(&json!({
            "source": "users",
            "filter": { "==": [{ "field": "id" }, 1] },
            "fields": ["id", "name"],
            "sort": [{ "name": "asc" }],
            "limit": 10,
            "skip": 20,
            "include": { "orders": { "fields": ["total"], "sort": [{ "id": "asc" }], "limit": 5 } }
        }))
        .expect("every documented key must be known");
        assert_eq!(spec.source, "users");
        assert_eq!(spec.limit, Some(10));
        assert_eq!(spec.skip, Some(20));
        assert_eq!(spec.include.len(), 1);
    }

    /// `count` is a plain boolean and defaults off, so every envelope written
    /// before it existed parses exactly as it did.
    #[test]
    fn count_defaults_off_and_parses() {
        let spec = parse(&serde_json::json!({ "source": "users" })).expect("parses");
        assert!(!spec.count);
        let spec = parse(&serde_json::json!({ "source": "users", "count": true })).expect("parses");
        assert!(spec.count);
        let spec =
            parse(&serde_json::json!({ "source": "users", "count": false })).expect("parses");
        assert!(!spec.count);
    }

    /// A key that shapes a row set has no reading over a single number, so it
    /// is refused by name rather than ignored.
    #[test]
    fn count_refuses_the_row_shaping_keys() {
        for key in ["fields", "sort", "limit", "skip", "include"] {
            let mut envelope = serde_json::json!({ "source": "users", "count": true });
            envelope[key] = match key {
                "fields" => serde_json::json!(["id"]),
                "sort" => serde_json::json!([{ "id": "asc" }]),
                "include" => serde_json::json!({ "orders": {} }),
                _ => serde_json::json!(10),
            };
            let err = parse(&envelope).expect_err("must be refused").to_string();
            assert!(err.contains(key), "{key}: {err}");
            assert!(err.contains("count"), "{key}: {err}");
        }
    }

    /// Those keys are only refused *alongside* count — on their own they are
    /// the ordinary envelope.
    #[test]
    fn the_row_shaping_keys_are_fine_without_count() {
        let spec = parse(&serde_json::json!({
            "source": "users", "fields": ["id"], "limit": 10
        }))
        .expect("parses");
        assert!(!spec.count);
        assert_eq!(spec.limit, Some(10));
    }

    #[test]
    fn count_must_be_a_boolean() {
        let err = parse(&serde_json::json!({ "source": "users", "count": 1 }))
            .expect_err("must be refused")
            .to_string();
        assert!(err.contains("boolean"), "{err}");
    }
}

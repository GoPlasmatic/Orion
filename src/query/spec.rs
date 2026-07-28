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
}

/// A related collection to nest in the result: `"orders": { "fields": [..], "limit": n }`.
#[derive(Debug, Clone, PartialEq)]
pub struct IncludeSpec {
    /// Relation name (declared in the schema) and the output field name.
    pub relation: String,
    /// Child fields to return; empty means all.
    pub fields: Vec<String>,
    /// Max related rows per parent.
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

/// Parse the `query` object into a [`QuerySpec`].
pub fn parse(query: &Json) -> Result<QuerySpec, QueryError> {
    let obj = query
        .as_object()
        .ok_or_else(|| invalid("query must be a JSON object"))?;

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

    let sort = parse_sort(obj.get("sort"))?;
    let limit = parse_u64(obj.get("limit"), "limit")?;
    let skip = parse_u64(obj.get("skip"), "skip")?;
    let include = parse_include(obj.get("include"))?;

    Ok(QuerySpec {
        source,
        filter,
        fields,
        sort,
        limit,
        skip,
        include,
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
        let limit = parse_u64(sel.get("limit"), &format!("include.{relation}.limit"))?;
        out.push(IncludeSpec {
            relation: relation.clone(),
            fields,
            limit,
        });
    }
    Ok(out)
}

fn parse_sort(v: Option<&Json>) -> Result<Vec<SortKey>, QueryError> {
    let arr = match v {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(a)) => a,
        Some(_) => return Err(invalid("'sort' must be an array")),
    };
    let mut out = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        let obj = entry.as_object().ok_or_else(|| {
            invalid(format!(
                "sort[{i}] must be an object like {{\"field\":\"asc\"}}"
            ))
        })?;
        if obj.len() != 1 {
            return Err(invalid(format!("sort[{i}] must have exactly one key")));
        }
        let (field, dirv) = obj.iter().next().expect("map has exactly one entry");
        let dir = match dirv.as_str() {
            Some("asc") => SortDir::Asc,
            Some("desc") => SortDir::Desc,
            _ => {
                return Err(invalid(format!(
                    "sort[{i}].{field} must be \"asc\" or \"desc\""
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
    /// Applied once per translation, before any backend sees the spec, so no
    /// renderer can receive a logical name.
    pub fn resolve_names(&self, reg: &crate::query::EntityRegistry) -> Result<Self, QueryError> {
        let mut out = self.clone();

        for (i, field) in out.fields.iter_mut().enumerate() {
            *field = reg
                .resolve_field(&self.source, field, &format!("fields[{i}]"))?
                .physical;
        }
        for (i, key) in out.sort.iter_mut().enumerate() {
            key.field = reg
                .resolve_field(&self.source, &key.field, &format!("sort[{i}]"))?
                .physical;
        }
        // `include.fields` name columns on the *related* entity, so they
        // resolve against the relation's target, not the root.
        for inc in out.include.iter_mut() {
            let target = reg
                .resolve_relation(&self.source, &inc.relation, "include")?
                .1;
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
        Ok(out)
    }
}

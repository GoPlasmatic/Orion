//! Lowering: raw JSONLogic `filter` JSON → the neutral [`Cond`] IR.
//!
//! Resolves `{"field": ..}` references (against the schema, or identity mode) and
//! `{"param": ..}` references (from the pre-resolved params map), lowers
//! `some`/`all`/`none` over declared relations into [`Cond::Rel`], and applies the
//! dialect's normalisation rules (`docs/src/reference/data-dialect.md`): empty
//! `and`/`or` fold to `True`/`False`, empty `in` folds to `False`, `== null`
//! becomes `IsNull`, and chained comparisons become `Between` with faithful
//! strict/inclusive bounds.
//!
//! Anything outside the vocabulary, column-to-column comparison, dotted-path
//! fields, and relations not declared in the schema are rejected with a located
//! [`QueryError`] rather than mistranslated.

use serde_json::{Map, Value as Json};

use crate::query::error::QueryError;
use crate::query::ir::{CmpOp, Cond, FieldRef, Quant, TextOp, Value};
use crate::query::schema::EntityRegistry;
use crate::query::vocab::{OpKind, classify};

/// Params map: name → concrete (already message-resolved) JSON value.
pub type Params = Map<String, Json>;

/// Shared, entity-independent lowering context.
struct Ctx<'a> {
    params: &'a Params,
    reg: &'a EntityRegistry,
}

/// A parsed operand: a column reference, a scalar literal, or a flat list of
/// scalars (legal only as the `in` haystack — nested lists are rejected while
/// parsing, where the real filter location is known).
enum Operand {
    Field(FieldRef),
    Val(Value),
    List(Vec<Value>),
}

/// Lower a `filter` in identity mode (no schema). Test convenience —
/// production always goes through [`lower_with`] with a real registry.
#[cfg(test)]
pub fn lower(filter: &Json, params: &Params) -> Result<Cond, QueryError> {
    lower_with(filter, params, &EntityRegistry::identity(), "")
}

/// Lower a `filter` rooted at `root_entity`, resolving fields and relations
/// through `reg`.
pub fn lower_with(
    filter: &Json,
    params: &Params,
    reg: &EntityRegistry,
    root_entity: &str,
) -> Result<Cond, QueryError> {
    let ctx = Ctx { params, reg };
    lower_cond(filter, &ctx, root_entity, "filter")
}

fn lower_cond(node: &Json, ctx: &Ctx, entity: &str, at: &str) -> Result<Cond, QueryError> {
    let map = node.as_object().ok_or_else(|| {
        QueryError::InvalidEnvelope(format!("filter node at {at} must be an object"))
    })?;
    let (op, arg) = single_entry(map).ok_or_else(|| {
        QueryError::InvalidEnvelope(format!(
            "filter node at {at} must have exactly one operator key"
        ))
    })?;
    let kind = classify(op).ok_or_else(|| QueryError::UnsupportedInQuery {
        op: op.to_string(),
        at: at.to_string(),
    })?;
    let child_at = format!("{at}.{op}");

    match kind {
        OpKind::And => {
            let items = as_args(arg, &child_at)?;
            if items.is_empty() {
                return Ok(Cond::True);
            }
            let mut out = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                out.push(lower_cond(it, ctx, entity, &format!("{child_at}[{i}]"))?);
            }
            Ok(Cond::And(out))
        }
        OpKind::Or => {
            let items = as_args(arg, &child_at)?;
            if items.is_empty() {
                return Ok(Cond::False);
            }
            let mut out = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                out.push(lower_cond(it, ctx, entity, &format!("{child_at}[{i}]"))?);
            }
            Ok(Cond::Or(out))
        }
        OpKind::Not => {
            let inner = single_arg(arg, &child_at)?;
            Ok(Cond::Not(Box::new(lower_cond(
                inner, ctx, entity, &child_at,
            )?)))
        }
        OpKind::Cmp(cmp) => lower_cmp(cmp, arg, ctx, entity, &child_at),
        OpKind::In => lower_in(arg, ctx, entity, &child_at),
        OpKind::StartsWith => lower_text(TextOp::StartsWith, arg, ctx, entity, &child_at),
        OpKind::EndsWith => lower_text(TextOp::EndsWith, arg, ctx, entity, &child_at),
        OpKind::Missing => lower_missing(arg, ctx, entity, &child_at),
        OpKind::Some => lower_rel(Quant::Any, arg, ctx, entity, &child_at),
        OpKind::All => lower_rel(Quant::All, arg, ctx, entity, &child_at),
        OpKind::None => lower_rel(Quant::None, arg, ctx, entity, &child_at),
    }
}

fn lower_rel(
    quant: Quant,
    arg: &Json,
    ctx: &Ctx,
    entity: &str,
    at: &str,
) -> Result<Cond, QueryError> {
    let args = as_args(arg, at)?;
    if args.len() != 2 {
        return Err(QueryError::InvalidEnvelope(format!(
            "relation predicate at {at} expects 2 operands"
        )));
    }
    let relation = rel_name(&args[0], at)?;
    let (rel, target_entity) = ctx.reg.resolve_relation(entity, &relation, at)?;
    let inner = lower_cond(&args[1], ctx, &target_entity, &format!("{at}.{relation}"))?;
    Ok(Cond::Rel {
        quant,
        rel,
        cond: Box::new(inner),
    })
}

fn lower_cmp(
    cmp: CmpOp,
    arg: &Json,
    ctx: &Ctx,
    entity: &str,
    at: &str,
) -> Result<Cond, QueryError> {
    let args = as_args(arg, at)?;
    match args.len() {
        2 => {
            let a = parse_operand(&args[0], ctx, entity, at)?;
            let b = parse_operand(&args[1], ctx, entity, at)?;
            lower_binary_cmp(cmp, a, b, at)
        }
        3 => lower_chained(cmp, args, ctx, entity, at),
        n => Err(QueryError::InvalidEnvelope(format!(
            "comparison at {at} expects 2 or 3 operands, got {n}"
        ))),
    }
}

fn lower_binary_cmp(cmp: CmpOp, a: Operand, b: Operand, at: &str) -> Result<Cond, QueryError> {
    let (field, value, op) = match (a, b) {
        (Operand::Field(f), Operand::Val(v)) => (f, v, cmp),
        (Operand::Val(v), Operand::Field(f)) => (f, v, cmp.flipped()),
        (Operand::List(_), _) | (_, Operand::List(_)) => {
            return Err(not_representable("list literal in a scalar comparison", at));
        }
        (Operand::Field(_), Operand::Field(_)) => {
            return Err(not_representable("column-to-column comparison", at));
        }
        (Operand::Val(_), Operand::Val(_)) => {
            return Err(not_representable("comparison of two constants", at));
        }
    };
    if matches!(op, CmpOp::Eq | CmpOp::Ne) && value == Value::Null {
        return Ok(Cond::IsNull {
            field,
            negated: matches!(op, CmpOp::Ne),
        });
    }
    Ok(Cond::Compare { field, op, value })
}

fn lower_chained(
    cmp: CmpOp,
    args: &[Json],
    ctx: &Ctx,
    entity: &str,
    at: &str,
) -> Result<Cond, QueryError> {
    let a = parse_operand(&args[0], ctx, entity, at)?;
    let mid = parse_operand(&args[1], ctx, entity, at)?;
    let b = parse_operand(&args[2], ctx, entity, at)?;

    let field = match mid {
        Operand::Field(f) => f,
        Operand::Val(_) | Operand::List(_) => {
            return Err(not_representable(
                "chained comparison requires the middle operand to be a field",
                at,
            ));
        }
    };
    let (lo, hi) = match (a, b) {
        (Operand::Val(a), Operand::Val(b)) => (a, b),
        _ => {
            return Err(not_representable(
                "chained comparison bounds must be literals",
                at,
            ));
        }
    };

    // `<`/`<=`: a < x < b  → low=a, high=b.  `>`/`>=`: a > x > b → low=b, high=a.
    match cmp {
        CmpOp::Lt => Ok(between(field, lo, hi, false)),
        CmpOp::Le => Ok(between(field, lo, hi, true)),
        CmpOp::Gt => Ok(between(field, hi, lo, false)),
        CmpOp::Ge => Ok(between(field, hi, lo, true)),
        CmpOp::Eq | CmpOp::Ne => Err(not_representable("chained equality", at)),
    }
}

fn between(field: FieldRef, low: Value, high: Value, incl: bool) -> Cond {
    Cond::Between {
        field,
        low,
        high,
        low_incl: incl,
        high_incl: incl,
        negated: false,
    }
}

fn lower_in(arg: &Json, ctx: &Ctx, entity: &str, at: &str) -> Result<Cond, QueryError> {
    let args = as_args(arg, at)?;
    if args.len() != 2 {
        return Err(QueryError::InvalidEnvelope(format!(
            "'in' at {at} expects 2 operands"
        )));
    }
    // datalogic order is [needle, haystack].
    let needle = parse_operand(&args[0], ctx, entity, at)?;
    let haystack = parse_operand(&args[1], ctx, entity, at)?;
    match (needle, haystack) {
        // Membership: field IN (list).
        (Operand::Field(f), Operand::List(items)) => {
            if items.is_empty() {
                Ok(Cond::False) // empty membership folds to false
            } else {
                Ok(Cond::In {
                    field: f,
                    values: items,
                    negated: false,
                })
            }
        }
        // Substring: 'literal' IN field-string.
        (Operand::Val(Value::Str(s)), Operand::Field(f)) => Ok(Cond::Text {
            field: f,
            op: TextOp::Contains,
            pattern: s,
        }),
        (Operand::Field(_), Operand::Field(_)) => {
            Err(not_representable("column-to-column 'in'", at))
        }
        (Operand::Field(_), Operand::Val(_)) => Err(not_representable(
            "'in' membership requires a list as the second operand",
            at,
        )),
        (Operand::Val(_) | Operand::List(_), Operand::Field(_)) => Err(not_representable(
            "'in' substring needle must be a string literal",
            at,
        )),
        _ => Err(not_representable("'in' requires a field operand", at)),
    }
}

fn lower_text(
    op: TextOp,
    arg: &Json,
    ctx: &Ctx,
    entity: &str,
    at: &str,
) -> Result<Cond, QueryError> {
    let args = as_args(arg, at)?;
    if args.len() != 2 {
        return Err(QueryError::InvalidEnvelope(format!(
            "text match at {at} expects 2 operands"
        )));
    }
    let field = match parse_operand(&args[0], ctx, entity, at)? {
        Operand::Field(f) => f,
        Operand::Val(_) | Operand::List(_) => {
            return Err(not_representable(
                "starts_with/ends_with requires a field as the first operand",
                at,
            ));
        }
    };
    let pattern = match parse_operand(&args[1], ctx, entity, at)? {
        Operand::Val(Value::Str(s)) => s,
        _ => {
            return Err(not_representable(
                "starts_with/ends_with pattern must be a string literal",
                at,
            ));
        }
    };
    Ok(Cond::Text { field, op, pattern })
}

fn lower_missing(arg: &Json, ctx: &Ctx, entity: &str, at: &str) -> Result<Cond, QueryError> {
    let names: Vec<String> = match arg {
        Json::Array(a) => {
            let mut v = Vec::with_capacity(a.len());
            for (i, e) in a.iter().enumerate() {
                let s = e.as_str().ok_or_else(|| {
                    QueryError::InvalidEnvelope(format!("missing[{i}] must be a field-name string"))
                })?;
                v.push(s.to_string());
            }
            v
        }
        Json::String(s) => vec![s.clone()],
        _ => {
            return Err(QueryError::InvalidEnvelope(format!(
                "'missing' at {at} expects an array of field names"
            )));
        }
    };
    if names.is_empty() {
        return Ok(Cond::False); // nothing is missing
    }
    let mut conds = Vec::with_capacity(names.len());
    for n in names {
        conds.push(Cond::IsNull {
            field: resolve_field(ctx, entity, &n, at)?,
            negated: false,
        });
    }
    if conds.len() == 1 {
        Ok(conds.pop().expect("len checked as 1"))
    } else {
        Ok(Cond::Or(conds))
    }
}

fn parse_operand(node: &Json, ctx: &Ctx, entity: &str, at: &str) -> Result<Operand, QueryError> {
    match node {
        Json::Object(map) => {
            if let Some(fv) = map.get("field") {
                if map.len() != 1 {
                    return Err(not_representable("field reference with extra keys", at));
                }
                let name = fv.as_str().ok_or_else(|| QueryError::InvalidField {
                    field: fv.to_string(),
                    at: at.to_string(),
                })?;
                return Ok(Operand::Field(resolve_field(ctx, entity, name, at)?));
            }
            if let Some(pv) = map.get("param") {
                if map.len() != 1 {
                    return Err(not_representable("param reference with extra keys", at));
                }
                let name = pv.as_str().ok_or_else(|| {
                    QueryError::InvalidEnvelope("param name must be a string".to_string())
                })?;
                let resolved = ctx
                    .params
                    .get(name)
                    .ok_or_else(|| QueryError::MissingParam {
                        name: name.to_string(),
                        at: at.to_string(),
                    })?;
                return literal_operand(resolved, ctx.params, at);
            }
            // The extended-JSON wrappers (#263) sit at the value position like
            // any other literal, so they are tried before the unknown-operator
            // fallthrough.
            if let Some(v) = extended_json_value(map, ctx.params, at)? {
                return Ok(Operand::Val(v));
            }
            match single_entry(map) {
                Some((inner_op, _)) => Err(QueryError::UnsupportedInQuery {
                    op: inner_op.to_string(),
                    at: at.to_string(),
                }),
                None => Err(not_representable("object literal operand", at)),
            }
        }
        other => literal_operand(other, ctx.params, at),
    }
}

/// Parse a literal JSON node (or a resolved param value): a flat array becomes
/// [`Operand::List`] of scalars, anything else a scalar [`Operand::Val`]. A
/// list inside a list has no portable meaning and is rejected here, during
/// lowering, where `at` names the real filter location — so every backend
/// refuses it identically instead of one erroring late and others nesting
/// silently.
fn literal_operand(j: &Json, params: &Params, at: &str) -> Result<Operand, QueryError> {
    match j {
        Json::Array(arr) => arr
            .iter()
            .map(|e| json_to_value(e, params, at))
            .collect::<Result<Vec<_>, _>>()
            .map(Operand::List),
        other => Ok(Operand::Val(json_to_value(other, params, at)?)),
    }
}

/// Resolve a `{"field": name}` reference: reject dotted paths (identity mode is
/// single-segment; JSON-path extraction is a later phase), else delegate to the
/// schema (renames/types/allowlist, or identity when unmapped).
fn resolve_field(ctx: &Ctx, entity: &str, name: &str, at: &str) -> Result<FieldRef, QueryError> {
    if name.is_empty() {
        return Err(QueryError::InvalidField {
            field: name.to_string(),
            at: at.to_string(),
        });
    }
    if name.contains('.') {
        return Err(QueryError::UnsupportedInQuery {
            op: format!("dotted field path '{name}'"),
            at: at.to_string(),
        });
    }
    ctx.reg.resolve_field(entity, name, at)
}

/// Extract the relation name from a `{"field": "<relation>"}` operand.
fn rel_name(node: &Json, at: &str) -> Result<String, QueryError> {
    node.as_object()
        .and_then(|m| (m.len() == 1).then(|| m.get("field")).flatten())
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            QueryError::InvalidEnvelope(format!(
                "relation at {at} expects {{\"field\":\"<relation>\"}} as its first operand"
            ))
        })
}

fn json_to_value(j: &Json, params: &Params, at: &str) -> Result<Value, QueryError> {
    Ok(match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                return Err(not_representable(
                    "numeric literal out of i64/f64 range",
                    at,
                ));
            }
        }
        Json::String(s) => Value::Str(s.clone()),
        // Only list *elements* reach this arm (a top-level array becomes an
        // `Operand::List` in `literal_operand`), so an array here is a nested list.
        Json::Array(_) => return Err(not_representable("nested list literal", at)),
        Json::Object(m) => match extended_json_value(m, params, at)? {
            Some(v) => v,
            None => return Err(not_representable("object literal value", at)),
        },
    })
}

/// Interpret the extended-JSON wrapper spellings into tagged IR values (#263).
///
/// `Ok(None)` means "not a wrapper" — the caller applies its ordinary object
/// handling (reject, or try other single-key meanings). The recognised set:
///
/// * `{"$oid": "<24 hex>"}` → [`Value::ObjectId`], hex validated here so the
///   error carries the real filter/values location and every backend sees the
///   same refusal.
/// * `{"$date": …}` → [`Value::DateTime`] in epoch milliseconds, from an
///   RFC 3339 string, an integer (millis), or the canonical extended-JSON
///   `{"$numberLong": "<millis>"}` — the spelling `mongo_read` results carry,
///   so a value read from one document can filter the next query unchanged.
///
/// A wrapper's payload may itself be a `{"param": name}` node, resolved from
/// `params` before coercion — so a per-request id composes as
/// `{"$oid": {"param": "id"}}` exactly like any other dialect value.
///
/// Shared with the write envelope (`query/write.rs`), which accepts the same
/// wrappers in `values`/`set`. Future wrappers (`$uuid`, `$numberDecimal`, …)
/// are one match arm plus one IR variant each.
pub(crate) fn extended_json_value(
    map: &Map<String, Json>,
    params: &Params,
    at: &str,
) -> Result<Option<Value>, QueryError> {
    if map.len() != 1 {
        return Ok(None);
    }
    let (key, raw) = map.iter().next().expect("len checked as 1");
    if key != "$oid" && key != "$date" {
        return Ok(None);
    }
    // Fold a `{"param": name}` payload first, mirroring `parse_operand`.
    let payload = match raw.as_object().and_then(|m| {
        (m.len() == 1)
            .then(|| m.get("param"))
            .flatten()
            .and_then(Json::as_str)
    }) {
        Some(name) => ctx_param(params, name, at)?,
        None => raw,
    };
    match key.as_str() {
        "$oid" => {
            let hex = payload.as_str().ok_or_else(|| {
                QueryError::InvalidEnvelope(format!("$oid at {at} expects a hex string"))
            })?;
            let oid = mongodb::bson::oid::ObjectId::parse_str(hex).map_err(|_| {
                QueryError::InvalidEnvelope(format!(
                    "$oid at {at} is not a valid ObjectId (expected 24 hex characters)"
                ))
            })?;
            Ok(Some(Value::ObjectId(oid.bytes())))
        }
        "$date" => Ok(Some(Value::DateTime(date_millis(payload, at)?))),
        _ => unreachable!("key checked above"),
    }
}

/// The three accepted `$date` payload spellings, normalised to epoch millis.
fn date_millis(payload: &Json, at: &str) -> Result<i64, QueryError> {
    match payload {
        Json::String(s) => mongodb::bson::DateTime::parse_rfc3339_str(s)
            .map(|d| d.timestamp_millis())
            .map_err(|_| {
                QueryError::InvalidEnvelope(format!(
                    "$date at {at} is not an RFC 3339 datetime string"
                ))
            }),
        Json::Number(n) => n.as_i64().ok_or_else(|| {
            QueryError::InvalidEnvelope(format!(
                "$date at {at} must be integer epoch milliseconds"
            ))
        }),
        Json::Object(m) if m.len() == 1 && m.contains_key("$numberLong") => m
            .get("$numberLong")
            .and_then(Json::as_str)
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or_else(|| {
                QueryError::InvalidEnvelope(format!(
                    "$date at {at}: $numberLong expects a stringified integer"
                ))
            }),
        _ => Err(QueryError::InvalidEnvelope(format!(
            "$date at {at} expects an RFC 3339 string, epoch milliseconds, or \
             {{\"$numberLong\": \"<millis>\"}}"
        ))),
    }
}

/// A params lookup with the standard missing-param error.
fn ctx_param<'a>(params: &'a Params, name: &str, at: &str) -> Result<&'a Json, QueryError> {
    params.get(name).ok_or_else(|| QueryError::MissingParam {
        name: name.to_string(),
        at: at.to_string(),
    })
}

fn not_representable(what: &str, at: &str) -> QueryError {
    QueryError::NotRepresentable {
        what: what.to_string(),
        at: at.to_string(),
    }
}

fn single_entry(map: &Map<String, Json>) -> Option<(&str, &Json)> {
    if map.len() == 1 {
        map.iter().next().map(|(k, v)| (k.as_str(), v))
    } else {
        None
    }
}

fn as_args<'a>(arg: &'a Json, at: &str) -> Result<&'a [Json], QueryError> {
    arg.as_array().map(Vec::as_slice).ok_or_else(|| {
        QueryError::InvalidEnvelope(format!("operator at {at} expects an array of operands"))
    })
}

/// `!` accepts a bare node or a single-element array.
fn single_arg<'a>(arg: &'a Json, at: &str) -> Result<&'a Json, QueryError> {
    match arg {
        Json::Array(a) if a.len() == 1 => Ok(&a[0]),
        Json::Array(_) => Err(QueryError::InvalidEnvelope(format!(
            "'!' at {at} expects a single operand"
        ))),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ir::{EsStorage, JunctionRef, MongoStorage, RelRef};
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;
    use serde_json::json;

    fn lower_ok(filter: Json) -> Cond {
        lower(&filter, &Params::new()).expect("lowering should succeed")
    }

    /// Arbitrary JSON, with keys biased toward operator-shaped strings so a
    /// meaningful share of the generated trees reach real lowering arms
    /// instead of bouncing off `classify` immediately.
    ///
    /// Depth is capped at 4. Production input is bounded too, one layer up:
    /// `serde_json` refuses to deserialize past 128 levels of nesting, so
    /// `lower_cond`'s recursion cannot be driven arbitrarily deep by a
    /// request body.
    fn arb_json() -> impl Strategy<Value = Json> {
        let leaf = prop_oneof![
            Just(Json::Null),
            any::<bool>().prop_map(Json::from),
            any::<i64>().prop_map(Json::from),
            ".*".prop_map(Json::from),
        ];
        leaf.prop_recursive(4, 24, 3, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Json::from),
                prop::collection::vec(
                    (
                        prop_oneof![
                            Just("and".to_string()),
                            Just("or".to_string()),
                            Just("!".to_string()),
                            Just("<".to_string()),
                            Just(">".to_string()),
                            Just("==".to_string()),
                            Just("in".to_string()),
                            Just("some".to_string()),
                            Just("field".to_string()),
                            Just("param".to_string()),
                            "[a-z]{1,4}",
                        ],
                        inner,
                    ),
                    0..3,
                )
                .prop_map(|pairs| Json::Object(pairs.into_iter().collect())),
            ]
        })
    }

    proptest! {
        /// Totality, in the same spirit as `match_route_is_total`: an
        /// arbitrary filter tree must lower to a `Cond` or to a located
        /// `QueryError`, and never panic or overflow. A panic here aborts a
        /// data-plane request mid-flight rather than answering 400.
        #[test]
        fn lowering_is_total(filter in arb_json()) {
            let _ = lower(&filter, &Params::new());
        }

        /// `a < x < b` and `b > x > a` denote the same range, so they must
        /// lower to the same `Between`. This is the invariant the `>`/`>=`
        /// arms' operand reversal exists to satisfy, stated once as a
        /// symmetry instead of re-asserted per operator — it holds for every
        /// pair of bounds, including the reversed and equal ones that a
        /// hand-written case would not think to try.
        #[test]
        fn ascending_and_descending_chains_agree(a in -10_000i64..10_000, b in -10_000i64..10_000) {
            prop_assert_eq!(
                lower_ok(json!({ "<": [a, {"field": "x"}, b] })),
                lower_ok(json!({ ">": [b, {"field": "x"}, a] })),
            );
            prop_assert_eq!(
                lower_ok(json!({ "<=": [a, {"field": "x"}, b] })),
                lower_ok(json!({ ">=": [b, {"field": "x"}, a] })),
            );
        }

        /// Strictness is carried, not invented: `<`/`>` produce exclusive
        /// bounds on both ends and `<=`/`>=` inclusive ones, whatever the
        /// bounds are.
        #[test]
        fn chained_strictness_matches_the_operator(a in -10_000i64..10_000, b in -10_000i64..10_000) {
            for (op, incl) in [("<", false), (">", false), ("<=", true), (">=", true)] {
                let c = lower_ok(json!({ op: [a, {"field": "x"}, b] }));
                let Cond::Between { low_incl, high_incl, .. } = c else {
                    return Err(TestCaseError::fail(format!("{op} did not lower to Between")));
                };
                prop_assert_eq!(low_incl, incl, "{} low bound", op);
                prop_assert_eq!(high_incl, incl, "{} high bound", op);
            }
        }
    }

    /// A registry with users→orders (has_many) and users↔tags (many-to-many).
    fn schema() -> EntityRegistry {
        EntityRegistry::from_json(&json!({
            "entities": {
                "users": {
                    "columns": { "id": {"type": "int"}, "status": {"type": "keyword"} },
                    "relations": {
                        "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id" },
                        "tags": {
                            "to": "tags", "kind": "many_to_many", "local": "id", "foreign": "id",
                            "through": { "table": "user_tags", "local": "user_id", "foreign": "tag_id" }
                        }
                    }
                },
                "orders": { "columns": { "total": {"type": "float"}, "user_id": {"type": "int"} } },
                "tags": { "columns": { "id": {"type": "int"}, "label": {"type": "keyword"} } }
            }
        }))
        .expect("valid schema")
    }

    fn lower_schema(filter: Json) -> Cond {
        lower_with(&filter, &Params::new(), &schema(), "users").expect("lowering should succeed")
    }

    #[test]
    fn test_simple_comparison() {
        let c = lower_ok(json!({ ">": [{"field": "age"}, 18] }));
        assert_eq!(
            c,
            Cond::Compare {
                field: FieldRef::identity("age"),
                op: CmpOp::Gt,
                value: Value::Int(18),
            }
        );
    }

    #[test]
    fn test_flipped_comparison() {
        let c = lower_ok(json!({ "<": [18, {"field": "age"}] }));
        assert_eq!(
            c,
            Cond::Compare {
                field: FieldRef::identity("age"),
                op: CmpOp::Gt,
                value: Value::Int(18),
            }
        );
    }

    #[test]
    fn test_empty_and_is_true() {
        assert_eq!(lower_ok(json!({ "and": [] })), Cond::True);
    }

    #[test]
    fn test_empty_or_is_false() {
        assert_eq!(lower_ok(json!({ "or": [] })), Cond::False);
    }

    #[test]
    fn test_eq_null_becomes_isnull() {
        let c = lower_ok(json!({ "==": [{"field": "email"}, null] }));
        assert_eq!(
            c,
            Cond::IsNull {
                field: FieldRef::identity("email"),
                negated: false,
            }
        );
    }

    #[test]
    fn test_ne_null_becomes_isnull_negated() {
        let c = lower_ok(json!({ "!=": [{"field": "email"}, null] }));
        assert_eq!(
            c,
            Cond::IsNull {
                field: FieldRef::identity("email"),
                negated: true,
            }
        );
    }

    #[test]
    fn test_chained_strict_range() {
        let c = lower_ok(json!({ "<": [1, {"field": "x"}, 10] }));
        assert_eq!(
            c,
            Cond::Between {
                field: FieldRef::identity("x"),
                low: Value::Int(1),
                high: Value::Int(10),
                low_incl: false,
                high_incl: false,
                negated: false,
            }
        );
    }

    #[test]
    fn test_chained_inclusive_range() {
        let c = lower_ok(json!({ "<=": [1, {"field": "x"}, 10] }));
        assert!(matches!(
            c,
            Cond::Between {
                low_incl: true,
                high_incl: true,
                ..
            }
        ));
    }

    /// `10 > x > 1` is the same range as `1 < x < 10`, so the `>` arm has to
    /// reverse its bounds on the way into `Between` — the one arm of
    /// `lower_chained` that does not pass its operands through in order.
    /// Asserted separately from the `<` arms because a reversal that is
    /// dropped (or applied twice) produces `low=10, high=1`: an empty range
    /// that answers every query with zero rows rather than an error.
    #[test]
    fn test_chained_strict_range_descending() {
        let c = lower_ok(json!({ ">": [10, {"field": "x"}, 1] }));
        assert_eq!(
            c,
            Cond::Between {
                field: FieldRef::identity("x"),
                low: Value::Int(1),
                high: Value::Int(10),
                low_incl: false,
                high_incl: false,
                negated: false,
            }
        );
    }

    #[test]
    fn test_chained_inclusive_range_descending() {
        let c = lower_ok(json!({ ">=": [10, {"field": "x"}, 1] }));
        assert_eq!(
            c,
            Cond::Between {
                field: FieldRef::identity("x"),
                low: Value::Int(1),
                high: Value::Int(10),
                low_incl: true,
                high_incl: true,
                negated: false,
            }
        );
    }

    #[test]
    fn test_membership() {
        let c = lower_ok(json!({ "in": [{"field": "status"}, ["a", "b"]] }));
        assert_eq!(
            c,
            Cond::In {
                field: FieldRef::identity("status"),
                values: vec![Value::Str("a".into()), Value::Str("b".into())],
                negated: false,
            }
        );
    }

    #[test]
    fn test_empty_membership_is_false() {
        assert_eq!(
            lower_ok(json!({ "in": [{"field": "status"}, []] })),
            Cond::False
        );
    }

    #[test]
    fn test_substring_in() {
        let c = lower_ok(json!({ "in": ["smith", {"field": "name"}] }));
        assert_eq!(
            c,
            Cond::Text {
                field: FieldRef::identity("name"),
                op: TextOp::Contains,
                pattern: "smith".into(),
            }
        );
    }

    #[test]
    fn test_starts_with() {
        let c = lower_ok(json!({ "starts_with": [{"field": "name"}, "sm"] }));
        assert_eq!(
            c,
            Cond::Text {
                field: FieldRef::identity("name"),
                op: TextOp::StartsWith,
                pattern: "sm".into(),
            }
        );
    }

    #[test]
    fn test_param_substitution() {
        let mut params = Params::new();
        params.insert("min".into(), json!(100));
        let c = lower(
            &json!({ ">": [{"field": "total"}, {"param": "min"}] }),
            &params,
        )
        .expect("ok");
        assert_eq!(
            c,
            Cond::Compare {
                field: FieldRef::identity("total"),
                op: CmpOp::Gt,
                value: Value::Int(100),
            }
        );
    }

    #[test]
    fn test_missing_param_errors() {
        let err = lower(
            &json!({ ">": [{"field": "total"}, {"param": "min"}] }),
            &Params::new(),
        )
        .expect_err("missing param");
        assert!(matches!(err, QueryError::MissingParam { .. }));
    }

    #[test]
    fn test_nested_list_in_membership_rejected_with_location() {
        // A list inside the `in` haystack is rejected at lowering — before any
        // backend renders — with the real filter location.
        let err = lower(
            &json!({ "in": [{"field": "status"}, ["a", ["b"]]] }),
            &Params::new(),
        )
        .expect_err("nested list");
        assert_eq!(
            err,
            QueryError::NotRepresentable {
                what: "nested list literal".into(),
                at: "filter.in".into(),
            }
        );
    }

    #[test]
    fn test_nested_list_via_param_rejected_with_location() {
        let mut params = Params::new();
        params.insert("xs".into(), json!(["a", ["b"]]));
        let err = lower(
            &json!({ "in": [{"field": "status"}, {"param": "xs"}] }),
            &params,
        )
        .expect_err("nested list from param");
        assert_eq!(
            err,
            QueryError::NotRepresentable {
                what: "nested list literal".into(),
                at: "filter.in".into(),
            }
        );
    }

    #[test]
    fn test_list_in_scalar_comparison_rejected() {
        let err = lower(&json!({ "==": [{"field": "x"}, [1, 2]] }), &Params::new())
            .expect_err("list in scalar comparison");
        assert!(matches!(err, QueryError::NotRepresentable { .. }), "{err}");
    }

    #[test]
    fn test_list_as_chained_bound_rejected() {
        // Previously a list bound survived lowering and only SQL errored (with a
        // fabricated location); now every backend refuses identically, here.
        let err = lower(&json!({ "<": [[1], {"field": "x"}, 10] }), &Params::new())
            .expect_err("list bound");
        assert_eq!(
            err,
            QueryError::NotRepresentable {
                what: "chained comparison bounds must be literals".into(),
                at: "filter.<".into(),
            }
        );
    }

    #[test]
    fn test_column_to_column_rejected() {
        let err = lower(
            &json!({ "<": [{"field": "a"}, {"field": "b"}] }),
            &Params::new(),
        )
        .expect_err("column-to-column");
        assert!(matches!(err, QueryError::NotRepresentable { .. }));
    }

    #[test]
    fn test_unsupported_operator_rejected() {
        let err = lower(&json!({ "cat": ["a", "b"] }), &Params::new()).expect_err("unsupported");
        assert!(matches!(err, QueryError::UnsupportedInQuery { .. }));
    }

    #[test]
    fn test_dotted_field_rejected() {
        let err = lower(
            &json!({ "==": [{"field": "address.city"}, "NYC"] }),
            &Params::new(),
        )
        .expect_err("dotted");
        assert!(matches!(err, QueryError::UnsupportedInQuery { .. }));
    }

    #[test]
    fn test_relation_without_schema_is_unknown() {
        // In identity mode (no schema) a relation is not declared → clear error.
        let err = lower(
            &json!({ "some": [{"field": "orders"}, {">": [{"field": "total"}, 1]}] }),
            &Params::new(),
        )
        .expect_err("no schema");
        assert!(matches!(err, QueryError::UnknownRelation { .. }));
    }

    #[test]
    fn test_relation_some_lowers_to_rel() {
        let c = lower_schema(json!({
            "some": [{"field": "orders"}, {">": [{"field": "total"}, 100]}]
        }));
        assert_eq!(
            c,
            Cond::Rel {
                quant: Quant::Any,
                rel: RelRef {
                    name: "orders".into(),
                    target_table: "orders".into(),
                    local: "id".into(),
                    foreign: "user_id".into(),
                    through: None,
                    mongo: MongoStorage::Embedded,
                    es: EsStorage::Nested,
                },
                cond: Box::new(Cond::Compare {
                    field: FieldRef::identity("total"),
                    op: CmpOp::Gt,
                    value: Value::Int(100),
                }),
            }
        );
    }

    #[test]
    fn test_relation_m2m_carries_junction() {
        let c = lower_schema(json!({
            "some": [{"field": "tags"}, {"==": [{"field": "label"}, "vip"]}]
        }));
        let Cond::Rel { rel, .. } = c else {
            unreachable!("expected a relation predicate");
        };
        assert_eq!(
            rel.through,
            Some(JunctionRef {
                table: "user_tags".into(),
                local: "user_id".into(),
                foreign: "tag_id".into(),
            })
        );
    }

    #[test]
    fn test_unknown_relation_rejected_with_schema() {
        let err = lower_with(
            &json!({ "some": [{"field": "nope"}, {">": [{"field": "total"}, 1]}] }),
            &Params::new(),
            &schema(),
            "users",
        )
        .expect_err("unknown relation");
        assert!(matches!(err, QueryError::UnknownRelation { .. }));
    }

    // ---- Extended-JSON tagged values (#263) ----

    const OID: &str = "665f1f77bcf86cd799439011";

    fn oid_bytes() -> [u8; 12] {
        mongodb::bson::oid::ObjectId::parse_str(OID)
            .expect("valid test oid")
            .bytes()
    }

    /// The headline defect: filtering on a real `_id` used to be
    /// unrepresentable (object literal → error), so callers fell back to plain
    /// strings that silently matched nothing on Mongo.
    #[test]
    fn test_oid_wrapper_lowers_to_a_tagged_value() {
        let c = lower(
            &json!({ "==": [{"field": "_id"}, { "$oid": OID }] }),
            &Params::new(),
        )
        .expect("lower");
        assert_eq!(
            c,
            Cond::Compare {
                field: FieldRef::identity("_id"),
                op: CmpOp::Eq,
                value: Value::ObjectId(oid_bytes()),
            }
        );
    }

    /// All three `$date` spellings normalise to the same epoch milliseconds —
    /// including the canonical `{"$numberLong": …}` form `mongo_read` output
    /// carries, so a value read from one document filters the next query.
    #[test]
    fn test_date_wrapper_spellings_agree() {
        for payload in [
            json!("2024-05-04T00:00:00Z"),
            json!(1_714_780_800_000_i64),
            json!({ "$numberLong": "1714780800000" }),
        ] {
            let c = lower(
                &json!({ ">": [{"field": "created_at"}, { "$date": payload }] }),
                &Params::new(),
            )
            .expect("lower");
            assert_eq!(
                c,
                Cond::Compare {
                    field: FieldRef::identity("created_at"),
                    op: CmpOp::Gt,
                    value: Value::DateTime(1_714_780_800_000),
                },
                "payload {payload} must lower to the same instant"
            );
        }
    }

    /// A wrapper payload may be a `{"param": ..}` node — the per-request id
    /// composes exactly like any other dialect value.
    #[test]
    fn test_wrapper_payload_resolves_params() {
        let mut params = Params::new();
        params.insert("id".to_string(), json!(OID));
        let c = lower(
            &json!({ "==": [{"field": "_id"}, { "$oid": { "param": "id" } }] }),
            &params,
        )
        .expect("lower");
        assert!(
            matches!(c, Cond::Compare { value: Value::ObjectId(b), .. } if b == oid_bytes()),
            "{c:?}"
        );
    }

    /// …and a param *value* carrying the wrapper object (message data echoing
    /// a `mongo_read` result) coerces the same way.
    #[test]
    fn test_param_value_carrying_a_wrapper_coerces() {
        let mut params = Params::new();
        params.insert("id".to_string(), json!({ "$oid": OID }));
        let c = lower(
            &json!({ "==": [{"field": "_id"}, { "param": "id" }] }),
            &params,
        )
        .expect("lower");
        assert!(
            matches!(c, Cond::Compare { value: Value::ObjectId(b), .. } if b == oid_bytes()),
            "{c:?}"
        );
    }

    /// Wrappers work as `in` haystack elements.
    #[test]
    fn test_wrappers_in_a_haystack() {
        let c = lower(
            &json!({ "in": [{"field": "_id"}, [{ "$oid": OID }, { "$oid": OID }]] }),
            &Params::new(),
        )
        .expect("lower");
        assert!(
            matches!(&c, Cond::In { values, .. } if values.len() == 2
                && values.iter().all(|v| *v == Value::ObjectId(oid_bytes()))),
            "{c:?}"
        );
    }

    /// A bad payload is a located envelope error — validated at lowering, so
    /// every backend sees the same refusal at the real filter location.
    #[test]
    fn test_invalid_wrapper_payloads_are_located_errors() {
        for (filter, needle) in [
            (json!({ "==": [{"field": "_id"}, { "$oid": "nope" }] }), "$oid"),
            (
                json!({ "==": [{"field": "at"}, { "$date": "not a date" }] }),
                "$date",
            ),
            (
                json!({ "==": [{"field": "at"}, { "$date": { "$numberLong": "x" } }] }),
                "$numberLong",
            ),
        ] {
            let err = lower(&filter, &Params::new()).expect_err("invalid payload");
            assert!(
                matches!(err, QueryError::InvalidEnvelope(_)),
                "{err:?}"
            );
            assert!(err.to_string().contains(needle), "{err}");
        }
    }

    /// The carve-out is exactly two spellings: any other single-key `$` object
    /// stays an unknown operator, and a plain object stays not-representable.
    #[test]
    fn test_other_objects_keep_their_pre_263_refusals() {
        let err = lower(
            &json!({ "==": [{"field": "x"}, { "$regex": "a.*" }] }),
            &Params::new(),
        )
        .expect_err("not a wrapper");
        assert!(matches!(err, QueryError::UnsupportedInQuery { .. }), "{err:?}");

        let err = lower(
            &json!({ "==": [{"field": "x"}, { "a": 1, "b": 2 }] }),
            &Params::new(),
        )
        .expect_err("plain object");
        assert!(matches!(err, QueryError::NotRepresentable { .. }), "{err:?}");
    }
}

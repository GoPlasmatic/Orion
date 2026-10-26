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
                    did_you_mean: None,
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
                return literal_operand(resolved, at);
            }
            match single_entry(map) {
                Some((inner_op, _)) => Err(QueryError::UnsupportedInQuery {
                    op: inner_op.to_string(),
                    at: at.to_string(),
                }),
                None => Err(not_representable("object literal operand", at)),
            }
        }
        other => literal_operand(other, at),
    }
}

/// Parse a literal JSON node (or a resolved param value): a flat array becomes
/// [`Operand::List`] of scalars, anything else a scalar [`Operand::Val`]. A
/// list inside a list has no portable meaning and is rejected here, during
/// lowering, where `at` names the real filter location — so every backend
/// refuses it identically instead of one erroring late and others nesting
/// silently.
fn literal_operand(j: &Json, at: &str) -> Result<Operand, QueryError> {
    match j {
        Json::Array(arr) => arr
            .iter()
            .map(|e| json_to_value(e, at))
            .collect::<Result<Vec<_>, _>>()
            .map(Operand::List),
        other => Ok(Operand::Val(json_to_value(other, at)?)),
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
            did_you_mean: None,
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

/// Resolve one *scalar* value node exactly as a filter operand resolves one: a
/// `{"param": name}` folds to the named param first, and anything else is the
/// literal.
///
/// The list case [`literal_operand`] admits — `in`'s haystack — is refused
/// here, because the only caller ([`crate::query::keyset`]) carries one value
/// per sort key. It exists so a cursor value spells identically to a filter
/// value by construction rather than by a second implementation, which is what
/// makes a value read out of one page bind into the next unchanged.
pub(crate) fn scalar_value(node: &Json, params: &Params, at: &str) -> Result<Value, QueryError> {
    if let Json::Object(map) = node
        && let Some(pv) = map.get("param")
    {
        if map.len() != 1 {
            return Err(not_representable("param reference with extra keys", at));
        }
        let name = pv.as_str().ok_or_else(|| {
            QueryError::InvalidEnvelope("param name must be a string".to_string())
        })?;
        // One level, matching `parse_operand`: a param's *value* is data, not
        // another reference to fold.
        return scalar_literal(ctx_param(params, name, at)?, at);
    }
    scalar_literal(node, at)
}

fn scalar_literal(j: &Json, at: &str) -> Result<Value, QueryError> {
    if j.is_array() {
        return Err(not_representable(
            "list value where one value was expected",
            at,
        ));
    }
    json_to_value(j, at)
}

fn json_to_value(j: &Json, at: &str) -> Result<Value, QueryError> {
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
        Json::Object(_) => return Err(not_representable("object literal value", at)),
    })
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
#[path = "lower_tests.rs"]
mod tests;

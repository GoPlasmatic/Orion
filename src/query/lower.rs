//! Lowering: raw JSONLogic `filter` JSON → the neutral [`Cond`] IR.
//!
//! Resolves `{"field": ..}` references (identity mode: single-segment column
//! names) and `{"param": ..}` references (from the pre-resolved params map), and
//! applies the proposal §5 normalisations: empty `and`/`or` fold to `True`/`False`,
//! empty `in` folds to `False`, `== null` becomes `IsNull`, and chained
//! comparisons become `Between` with faithful strict/inclusive bounds (§5.11).
//!
//! Anything outside the vocabulary, column-to-column comparison, and dotted-path
//! fields are rejected with a located [`QueryError`] rather than mistranslated.

use serde_json::{Map, Value as Json};

use crate::query::error::QueryError;
use crate::query::ir::{CmpOp, Cond, FieldRef, TextOp, Value};
use crate::query::vocab::{OpKind, classify};

/// Params map: name → concrete (already message-resolved) JSON value.
pub type Params = Map<String, Json>;

/// A parsed operand: either a column reference or a literal value.
enum Operand {
    Field(FieldRef),
    Val(Value),
}

/// Lower a raw `filter` node into a `Cond`.
pub fn lower(filter: &Json, params: &Params) -> Result<Cond, QueryError> {
    lower_cond(filter, params, "filter")
}

fn lower_cond(node: &Json, params: &Params, at: &str) -> Result<Cond, QueryError> {
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
                out.push(lower_cond(it, params, &format!("{child_at}[{i}]"))?);
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
                out.push(lower_cond(it, params, &format!("{child_at}[{i}]"))?);
            }
            Ok(Cond::Or(out))
        }
        OpKind::Not => {
            let inner = single_arg(arg, &child_at)?;
            Ok(Cond::Not(Box::new(lower_cond(inner, params, &child_at)?)))
        }
        OpKind::Cmp(cmp) => lower_cmp(cmp, arg, params, &child_at),
        OpKind::In => lower_in(arg, params, &child_at),
        OpKind::StartsWith => lower_text(TextOp::StartsWith, arg, params, &child_at),
        OpKind::EndsWith => lower_text(TextOp::EndsWith, arg, params, &child_at),
        OpKind::Missing => lower_missing(arg, &child_at),
    }
}

fn lower_cmp(cmp: CmpOp, arg: &Json, params: &Params, at: &str) -> Result<Cond, QueryError> {
    let args = as_args(arg, at)?;
    match args.len() {
        2 => {
            let a = parse_operand(&args[0], params, at)?;
            let b = parse_operand(&args[1], params, at)?;
            lower_binary_cmp(cmp, a, b, at)
        }
        3 => lower_chained(cmp, args, params, at),
        n => Err(QueryError::InvalidEnvelope(format!(
            "comparison at {at} expects 2 or 3 operands, got {n}"
        ))),
    }
}

fn lower_binary_cmp(cmp: CmpOp, a: Operand, b: Operand, at: &str) -> Result<Cond, QueryError> {
    let (field, value, op) = match (a, b) {
        (Operand::Field(f), Operand::Val(v)) => (f, v, cmp),
        (Operand::Val(v), Operand::Field(f)) => (f, v, cmp.flipped()),
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
    if let Value::List(_) = value {
        return Err(not_representable("list literal in a scalar comparison", at));
    }
    Ok(Cond::Compare { field, op, value })
}

fn lower_chained(cmp: CmpOp, args: &[Json], params: &Params, at: &str) -> Result<Cond, QueryError> {
    let a = parse_operand(&args[0], params, at)?;
    let mid = parse_operand(&args[1], params, at)?;
    let b = parse_operand(&args[2], params, at)?;

    let field = match mid {
        Operand::Field(f) => f,
        Operand::Val(_) => {
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

fn lower_in(arg: &Json, params: &Params, at: &str) -> Result<Cond, QueryError> {
    let args = as_args(arg, at)?;
    if args.len() != 2 {
        return Err(QueryError::InvalidEnvelope(format!(
            "'in' at {at} expects 2 operands"
        )));
    }
    // datalogic order is [needle, haystack].
    let needle = parse_operand(&args[0], params, at)?;
    let haystack = parse_operand(&args[1], params, at)?;
    match (needle, haystack) {
        // Membership: field IN (list).
        (Operand::Field(f), Operand::Val(Value::List(items))) => {
            if items.is_empty() {
                Ok(Cond::False) // §5.5 empty membership
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
            ci: false,
        }),
        (Operand::Field(_), Operand::Field(_)) => {
            Err(not_representable("column-to-column 'in'", at))
        }
        (Operand::Field(_), Operand::Val(_)) => Err(not_representable(
            "'in' membership requires a list as the second operand",
            at,
        )),
        (Operand::Val(_), Operand::Field(_)) => Err(not_representable(
            "'in' substring needle must be a string literal",
            at,
        )),
        (Operand::Val(_), Operand::Val(_)) => {
            Err(not_representable("'in' requires a field operand", at))
        }
    }
}

fn lower_text(op: TextOp, arg: &Json, params: &Params, at: &str) -> Result<Cond, QueryError> {
    let args = as_args(arg, at)?;
    if args.len() != 2 {
        return Err(QueryError::InvalidEnvelope(format!(
            "text match at {at} expects 2 operands"
        )));
    }
    let field = match parse_operand(&args[0], params, at)? {
        Operand::Field(f) => f,
        Operand::Val(_) => {
            return Err(not_representable(
                "starts_with/ends_with requires a field as the first operand",
                at,
            ));
        }
    };
    let pattern = match parse_operand(&args[1], params, at)? {
        Operand::Val(Value::Str(s)) => s,
        _ => {
            return Err(not_representable(
                "starts_with/ends_with pattern must be a string literal",
                at,
            ));
        }
    };
    Ok(Cond::Text {
        field,
        op,
        pattern,
        ci: false,
    })
}

fn lower_missing(arg: &Json, at: &str) -> Result<Cond, QueryError> {
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
            field: resolve_field(&n, at)?,
            negated: false,
        });
    }
    if conds.len() == 1 {
        Ok(conds.pop().expect("len checked as 1"))
    } else {
        Ok(Cond::Or(conds))
    }
}

fn parse_operand(node: &Json, params: &Params, at: &str) -> Result<Operand, QueryError> {
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
                return Ok(Operand::Field(resolve_field(name, at)?));
            }
            if let Some(pv) = map.get("param") {
                if map.len() != 1 {
                    return Err(not_representable("param reference with extra keys", at));
                }
                let name = pv.as_str().ok_or_else(|| {
                    QueryError::InvalidEnvelope("param name must be a string".to_string())
                })?;
                let resolved = params.get(name).ok_or_else(|| QueryError::MissingParam {
                    name: name.to_string(),
                    at: at.to_string(),
                })?;
                return Ok(Operand::Val(json_to_value(resolved, at)?));
            }
            // An operator object where a value is expected (arithmetic, etc.).
            match single_entry(map) {
                Some((inner_op, _)) => Err(QueryError::UnsupportedInQuery {
                    op: inner_op.to_string(),
                    at: at.to_string(),
                }),
                None => Err(not_representable("object literal operand", at)),
            }
        }
        other => Ok(Operand::Val(json_to_value(other, at)?)),
    }
}

fn resolve_field(name: &str, at: &str) -> Result<FieldRef, QueryError> {
    if name.is_empty() {
        return Err(QueryError::InvalidField {
            field: name.to_string(),
            at: at.to_string(),
        });
    }
    // Dotted / JSON-path fields need per-dialect extraction — deferred past Phase 1.
    if name.contains('.') {
        return Err(QueryError::UnsupportedInQuery {
            op: format!("dotted field path '{name}'"),
            at: at.to_string(),
        });
    }
    Ok(FieldRef::identity(name))
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
        Json::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for e in arr {
                out.push(json_to_value(e, at)?);
            }
            Value::List(out)
        }
        Json::Object(_) => return Err(not_representable("object literal value", at)),
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
    use serde_json::json;

    fn lower_ok(filter: Json) -> Cond {
        lower(&filter, &Params::new()).expect("lowering should succeed")
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
        // 18 < age  ==  age > 18
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
                ci: false,
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
                ci: false,
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
    fn test_relation_rejected_phase1() {
        let err = lower(
            &json!({ "some": [{"field": "orders"}, {">": [{"field": "total"}, 1]}] }),
            &Params::new(),
        )
        .expect_err("some is Phase 2");
        assert!(matches!(err, QueryError::UnsupportedInQuery { .. }));
    }
}

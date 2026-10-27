use super::*;
use crate::query::ir::{JunctionRef, RelRef};
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
    // A list bound is refused here, in lowering, identically for every
    // backend — not left for SQL to fail on with a fabricated location.
    let err =
        lower(&json!({ "<": [[1], {"field": "x"}, 10] }), &Params::new()).expect_err("list bound");
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

/// An object at a value position is never a literal: a single-key `$`
/// object — `$oid` and `$date` included, the Mongo extended-JSON wrappers
/// the dialect does not read — is an unknown operator, and a plain
/// object is not representable.
#[test]
fn test_object_values_are_refused() {
    for value in [
        json!({ "$regex": "a.*" }),
        json!({ "$oid": "665f1f77bcf86cd799439011" }),
        json!({ "$date": "2024-05-04T00:00:00Z" }),
    ] {
        let err = lower(&json!({ "==": [{"field": "x"}, value] }), &Params::new())
            .expect_err("not a literal");
        assert!(
            matches!(err, QueryError::UnsupportedInQuery { .. }),
            "{err:?}"
        );
    }

    let err = lower(
        &json!({ "==": [{"field": "x"}, { "a": 1, "b": 2 }] }),
        &Params::new(),
    )
    .expect_err("plain object");
    assert!(
        matches!(err, QueryError::NotRepresentable { .. }),
        "{err:?}"
    );
}

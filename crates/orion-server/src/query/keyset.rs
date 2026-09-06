//! The keyset (seek) predicate: `after` → a [`Cond`] AND-ed onto the filter.
//!
//! `after` is deliberately not a renderer feature. It lowers into the same
//! condition IR all three renderers already walk — `And` / `Or` / `Compare` /
//! `IsNull` — inside [`crate::query::prepare`], so five backends get it with no
//! renderer change and no fifth chance to disagree about what a page boundary
//! means.
//!
//! Two passes, because the cursor's names and the sort's names are the same
//! names at different stages. [`pair`] runs *before* `resolve_names` and matches
//! the cursor against the sort the caller wrote, so an error names what they
//! typed rather than a renamed column they have never seen. [`seek`] runs
//! *after* it and builds the predicate from the resolved physical columns. The
//! seek column and the `ORDER BY` column are therefore one resolution, not two
//! that can drift.

use serde_json::Value as Json;

use crate::query::backend::{SortPlan, plan_sort};
use crate::query::error::QueryError;
use crate::query::ir::{CmpOp, Cond, FieldRef, Value};
use crate::query::lower::{Params, scalar_value};
use crate::query::spec::{QuerySpec, SortKey};

/// Pair the cursor with `sort`, in `sort`'s order, and lower each value.
///
/// An empty result means "no cursor" — the first page. That covers an absent
/// `after`, an explicit `null`, and a `{"param": …}` whose value resolved to
/// `null`, which is the spelling that lets one task serve every page.
///
/// Must be called while `spec.sort` still carries the caller's logical names.
pub(crate) fn pair(spec: &QuerySpec, params: &Params) -> Result<Vec<Value>, QueryError> {
    let Some(node) = spec.after.as_ref() else {
        return Ok(Vec::new());
    };
    let Some(obj) = fold_whole(node, params)? else {
        return Ok(Vec::new());
    };

    // `sort` is the only statement of order there is. `serde_json::Map` is a
    // `BTreeMap` in this workspace (no `preserve_order` feature), so the
    // cursor object's own key order is alphabetical — `{"score": 10, "id": 5}`
    // and `{"id": 5, "score": 10}` parse to the same value. Reading order off
    // the JSON would silently reorder the comparison.
    let expected = spec
        .sort
        .iter()
        .map(|k| k.field.as_str())
        .collect::<Vec<_>>()
        .join("/");

    if let Some(unknown) = obj
        .keys()
        .find(|k| !spec.sort.iter().any(|s| &s.field == *k))
    {
        let suggestion =
            crate::query::error::nearest(unknown, spec.sort.iter().map(|k| k.field.as_str()))
                .map(|k| format!(" — did you mean \"{k}\"?"))
                .unwrap_or_default();
        return Err(QueryError::InvalidEnvelope(format!(
            "'after' names '{unknown}', which 'sort' does not order by — a \
             cursor carries one value per sort key (expected {expected}){suggestion}"
        )));
    }

    let mut out = Vec::with_capacity(spec.sort.len());
    for key in &spec.sort {
        let raw = obj.get(&key.field).ok_or_else(|| {
            QueryError::InvalidEnvelope(format!(
                "'after' is missing '{}' — a cursor carries one value per sort \
                 key (expected {expected}), or the page boundary is ambiguous \
                 on the keys it leaves out",
                key.field
            ))
        })?;
        out.push(scalar_value(raw, params, &format!("after.{}", key.field))?);
    }
    Ok(out)
}

/// Fold a whole-cursor `{"param": name}` node, returning the cursor object or
/// `None` when it resolved to `null`.
///
/// The per-key `{"param": …}` spelling is handled by [`scalar_value`] like any
/// other value; this is the *outer* one, and it is the ergonomic point of the
/// whole key. `after` is inside `query`, which is deliberately not a template
/// (see `engine::functions::templated_input`), so the predicate's shape is
/// fixed per task and only `params` vary. Without this fold a workflow needs
/// one task for the first page and another for the rest, because an absent
/// per-key cursor resolves to `null` and a null cursor value is a real
/// position — "after the nulls" — not "from the beginning".
fn fold_whole<'a>(
    node: &'a Json,
    params: &'a Params,
) -> Result<Option<&'a serde_json::Map<String, Json>>, QueryError> {
    let map = node.as_object().ok_or_else(|| {
        QueryError::InvalidEnvelope("'after' must be an object of sort key → value".to_string())
    })?;
    let resolved = match map.get("param") {
        Some(pv) if map.len() == 1 => {
            let name = pv.as_str().ok_or_else(|| {
                QueryError::InvalidEnvelope("param name must be a string".to_string())
            })?;
            params.get(name).ok_or_else(|| QueryError::MissingParam {
                name: name.to_string(),
                at: "after".to_string(),
            })?
        }
        _ => node,
    };
    match resolved {
        Json::Null => Ok(None),
        Json::Object(m) => Ok(Some(m)),
        _ => Err(QueryError::InvalidEnvelope(
            "'after' must resolve to an object of sort key → value, or to null \
             for the first page"
                .to_string(),
        )),
    }
}

/// AND the seek predicate onto `filter`. `values` empty means no cursor.
///
/// `sort` must already be resolved to physical column names.
pub(crate) fn seek(sort: &[SortKey], values: &[Value], filter: Cond) -> Cond {
    if values.is_empty() {
        return filter;
    }
    debug_assert_eq!(
        sort.len(),
        values.len(),
        "`pair` yields one value per sort key"
    );
    let seek = seek_cond(&plan_sort(sort), values);
    match filter {
        // `And([True, seek])` renders `WHERE 1 = 1 AND …` on SQL, an empty
        // document inside Mongo's `$and`, and a `match_all` inside ES's
        // `bool.filter`. A filterless page must render exactly as it did before
        // `after` existed.
        Cond::True => seek,
        other => Cond::And(vec![other, seek]),
    }
}

/// Strictly after the position `values` in the order `plans` states:
///
/// ```text
/// OR over i:  ( AND over j < i of eq(k_j, v_j) )  AND  after(k_i, v_i)
/// ```
///
/// Not a SQL row-value comparison, for two independent reasons. Mixed
/// directions (`score DESC, id ASC` — an ordinary leaderboard) have no
/// row-comparison form at all, and MySQL does not turn `(a, b) < (?, ?)` into
/// an index range scan — the finding
/// `storage::repositories::traces::TraceCursor::condition` already records.
fn seek_cond(plans: &[SortPlan<'_>], values: &[Value]) -> Cond {
    let mut disjuncts: Vec<Cond> = Vec::with_capacity(plans.len());
    // Zipped rather than indexed: the pairing is `pair`'s invariant, and a
    // seek that panicked on a mismatch would be a worse way to learn it broke.
    for (i, (plan, value)) in plans.iter().zip(values).enumerate() {
        let step = after_one(plan, value);
        // Nothing sorts after a null on a descending key, so this leg admits no
        // row. Dropped rather than carried, so the tree stays the one a reader
        // would have written and the renderer goldens stay stable.
        if step.is_always_false() {
            continue;
        }
        let mut legs: Vec<Cond> = plans
            .iter()
            .zip(values)
            .take(i)
            .map(|(p, v)| eq(p.field, v))
            .collect();
        if legs.is_empty() {
            disjuncts.push(step);
        } else {
            legs.push(step);
            disjuncts.push(Cond::And(legs));
        }
    }
    match disjuncts.len() {
        // Every leg was unsatisfiable: the cursor is at the last position the
        // order has, so the page after it is empty.
        0 => Cond::False,
        1 => disjuncts.pop().expect("len checked as 1"),
        _ => Cond::Or(disjuncts),
    }
}

/// The tie-break leg: this key held the cursor's value exactly.
fn eq(field: &str, v: &Value) -> Cond {
    let field = FieldRef {
        physical: field.to_string(),
    };
    match v {
        // No comparison operator on any of the five backends matches a null, so
        // "the cursor was in the null group" has to be said with `IS NULL`.
        Value::Null => Cond::IsNull {
            field,
            negated: false,
        },
        other => Cond::Compare {
            field,
            op: CmpOp::Eq,
            value: other.clone(),
        },
    }
}

/// This key alone is strictly past the cursor.
///
/// Null placement is read off [`SortPlan`] rather than restated: `plan_sort`
/// is the one statement of the W8 rule (a null sorts as the smallest value), and
/// a second copy here would be exactly the "second, invisible sort key" the SQL
/// renderer records removing from MySQL.
fn after_one(plan: &SortPlan<'_>, v: &Value) -> Cond {
    let field = FieldRef {
        physical: plan.field.to_string(),
    };
    if matches!(v, Value::Null) {
        // The cursor sits in the null group: when nulls come first every real
        // value is still ahead, and when they come last nothing is.
        return if plan.nulls_first {
            Cond::IsNull {
                field,
                negated: true,
            }
        } else {
            Cond::False
        };
    }
    let step = Cond::Compare {
        field: field.clone(),
        op: if plan.ascending { CmpOp::Gt } else { CmpOp::Lt },
        value: v.clone(),
    };
    if plan.nulls_first {
        step
    } else {
        // The null group is emitted after every real value, so a page starting
        // from a real value still has to reach it. Without this arm the last
        // page of a descending key silently drops every null-valued row — the
        // same wrong answer on all five backends, with no error anywhere.
        Cond::Or(vec![
            step,
            Cond::IsNull {
                field,
                negated: false,
            },
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::spec::SortDir;
    use serde_json::json;

    fn keys(spec: &[(&str, SortDir)]) -> Vec<SortKey> {
        spec.iter()
            .map(|(f, dir)| SortKey {
                field: (*f).to_string(),
                dir: *dir,
            })
            .collect()
    }

    fn asc(field: &str) -> Vec<SortKey> {
        keys(&[(field, SortDir::Asc)])
    }

    fn cmp(field: &str, op: CmpOp, value: Value) -> Cond {
        Cond::Compare {
            field: FieldRef::identity(field),
            op,
            value,
        }
    }

    fn is_null(field: &str, negated: bool) -> Cond {
        Cond::IsNull {
            field: FieldRef::identity(field),
            negated,
        }
    }

    /// A spec carrying only what `pair` reads.
    fn spec_with(sort: Vec<SortKey>, after: Option<Json>) -> QuerySpec {
        QuerySpec {
            source: "t".to_string(),
            filter: None,
            fields: Vec::new(),
            sort,
            limit: None,
            skip: None,
            after,
            include: Vec::new(),
            count: false,
        }
    }

    // ---------------------------------------------------------------
    // The predicate
    // ---------------------------------------------------------------

    #[test]
    fn a_single_ascending_key_is_one_comparison() {
        let got = seek(&asc("id"), &[Value::Int(7)], Cond::True);
        assert_eq!(got, cmp("id", CmpOp::Gt, Value::Int(7)));
    }

    /// Without the `IS NULL` arm the last page of a descending key silently
    /// drops every null-valued row: nulls sort last on `desc` (W8) and no
    /// comparison operator on any backend matches one.
    #[test]
    fn a_single_descending_key_reaches_the_nulls() {
        let got = seek(
            &keys(&[("score", SortDir::Desc)]),
            &[Value::Int(10)],
            Cond::True,
        );
        assert_eq!(
            got,
            Cond::Or(vec![
                cmp("score", CmpOp::Lt, Value::Int(10)),
                is_null("score", false),
            ])
        );
    }

    /// `score DESC, id ASC` is an ordinary leaderboard, and it has no SQL
    /// row-value form at all — `(a, b) < (x, y)` needs one direction.
    #[test]
    fn mixed_directions_expand_into_one_disjunct_per_key() {
        let got = seek(
            &keys(&[("score", SortDir::Desc), ("id", SortDir::Asc)]),
            &[Value::Int(10), Value::Int(5)],
            Cond::True,
        );
        assert_eq!(
            got,
            Cond::Or(vec![
                Cond::Or(vec![
                    cmp("score", CmpOp::Lt, Value::Int(10)),
                    is_null("score", false),
                ]),
                Cond::And(vec![
                    cmp("score", CmpOp::Eq, Value::Int(10)),
                    cmp("id", CmpOp::Gt, Value::Int(5)),
                ]),
            ])
        );
    }

    #[test]
    fn a_null_cursor_on_an_ascending_key_is_every_real_value() {
        let got = seek(&asc("nickname"), &[Value::Null], Cond::True);
        assert_eq!(got, is_null("nickname", true));
    }

    /// The case a naive implementation gets wrong: `Cond::False` for the
    /// leading key must kill only its own disjunct, or the walk stalls on an
    /// empty page forever instead of finishing the null group.
    #[test]
    fn a_null_cursor_on_a_descending_key_still_walks_the_tie_break() {
        let got = seek(
            &keys(&[("nickname", SortDir::Desc), ("name", SortDir::Asc)]),
            &[Value::Null, Value::Str("Alice".into())],
            Cond::True,
        );
        assert_eq!(
            got,
            Cond::And(vec![
                is_null("nickname", false),
                cmp("name", CmpOp::Gt, Value::Str("Alice".into())),
            ])
        );
    }

    #[test]
    fn a_cursor_at_the_end_of_the_order_is_an_empty_page() {
        let got = seek(
            &keys(&[("score", SortDir::Desc)]),
            &[Value::Null],
            Cond::True,
        );
        assert!(got.is_always_false(), "{got:?}");
    }

    /// Keeps the renderer goldens honest: a lone `And`/`Or` child or a `False`
    /// disjunct renders as noise no reader would have written.
    #[test]
    fn the_tree_never_carries_a_lone_child_or_a_false_disjunct() {
        fn walk(c: &Cond) {
            match c {
                Cond::And(cs) | Cond::Or(cs) => {
                    assert!(cs.len() >= 2, "single-child group: {c:?}");
                    assert!(
                        !cs.iter().any(Cond::is_always_false),
                        "unsatisfiable member: {c:?}"
                    );
                    cs.iter().for_each(walk);
                }
                _ => {}
            }
        }
        for values in [
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Null, Value::Int(2)],
            vec![Value::Int(1), Value::Null],
        ] {
            walk(&seek(
                &keys(&[("a", SortDir::Desc), ("b", SortDir::Asc)]),
                &values,
                Cond::True,
            ));
        }
    }

    /// The anti-drift test. Null placement comes from `plan_sort` — the one
    /// statement of W8 — so if that rule ever moves, the seek moves with it
    /// instead of becoming a second, invisible copy of the ordering.
    #[test]
    fn null_placement_is_read_off_plan_sort() {
        for (dir, expect_nulls_first) in [(SortDir::Asc, true), (SortDir::Desc, false)] {
            let sort = keys(&[("k", dir)]);
            let plans = plan_sort(&sort);
            assert_eq!(plans[0].nulls_first, expect_nulls_first);

            // A real cursor reaches the null group only when it sorts last.
            let real = seek(&sort, &[Value::Int(1)], Cond::True);
            let mentions_null = format!("{real:?}").contains("IsNull");
            assert_eq!(mentions_null, !expect_nulls_first, "{dir:?}: {real:?}");

            // A null cursor has everything after it only when nulls sort first.
            let at_null = seek(&sort, &[Value::Null], Cond::True);
            assert_eq!(at_null.is_always_false(), !expect_nulls_first, "{dir:?}");
        }
    }

    /// A filterless page must render exactly as it did before `after` existed:
    /// `And([True, seek])` becomes `WHERE 1 = 1 AND …` on SQL, `{}` inside
    /// Mongo's `$and` and a `match_all` inside ES's `bool.filter`.
    #[test]
    fn the_seek_replaces_a_vacuous_filter_rather_than_and_ing_it() {
        let bare = seek(&asc("id"), &[Value::Int(1)], Cond::True);
        assert_eq!(bare, cmp("id", CmpOp::Gt, Value::Int(1)));

        let filtered = seek(
            &asc("id"),
            &[Value::Int(1)],
            cmp("status", CmpOp::Eq, Value::Str("active".into())),
        );
        assert!(
            matches!(filtered, Cond::And(ref cs) if cs.len() == 2),
            "{filtered:?}"
        );
    }

    #[test]
    fn no_cursor_leaves_the_filter_untouched() {
        let filter = cmp("status", CmpOp::Eq, Value::Str("active".into()));
        assert_eq!(seek(&asc("id"), &[], filter.clone()), filter);
    }

    // ---------------------------------------------------------------
    // Pairing the cursor with the sort
    // ---------------------------------------------------------------

    /// `serde_json::Map` is a `BTreeMap` here, so the cursor object's own key
    /// order is alphabetical. `sort` is the only statement of order there is,
    /// and reading it off the JSON would silently reorder the comparison.
    #[test]
    fn the_cursor_is_reordered_into_sort_order() {
        let spec = spec_with(
            keys(&[("score", SortDir::Desc), ("id", SortDir::Asc)]),
            Some(json!({ "id": 5, "score": 10 })),
        );
        let got = pair(&spec, &Params::new()).expect("pairs");
        assert_eq!(got, vec![Value::Int(10), Value::Int(5)]);
    }

    /// The spelling that lets one task serve every page: an absent cursor
    /// resolves to null, and a null cursor is the first page rather than a
    /// position after the nulls.
    #[test]
    fn a_whole_cursor_param_folds_and_a_null_one_is_the_first_page() {
        let spec = spec_with(asc("id"), Some(json!({ "param": "cursor" })));

        let mut params = Params::new();
        params.insert("cursor".into(), json!({ "id": 7 }));
        assert_eq!(
            pair(&spec, &params).expect("pairs"),
            vec![Value::Int(7)],
            "a resolved cursor is a position"
        );

        let mut absent = Params::new();
        absent.insert("cursor".into(), Json::Null);
        assert!(
            pair(&spec, &absent).expect("pairs").is_empty(),
            "a null cursor is the first page"
        );
    }

    #[test]
    fn a_cursor_value_spells_the_same_as_a_filter_value() {
        let spec = spec_with(
            asc("created_at"),
            Some(json!({ "created_at": {"$date": 1_700_000_000_000_i64} })),
        );
        assert_eq!(
            pair(&spec, &Params::new()).expect("pairs"),
            vec![Value::DateTime(1_700_000_000_000)]
        );

        let spec = spec_with(asc("id"), Some(json!({ "id": {"param": "c"} })));
        let mut params = Params::new();
        params.insert("c".into(), json!("u7"));
        assert_eq!(
            pair(&spec, &params).expect("pairs"),
            vec![Value::Str("u7".into())]
        );
    }

    #[test]
    fn a_cursor_key_that_sort_does_not_order_by_is_refused() {
        let spec = spec_with(asc("created_at"), Some(json!({ "createdat": 1 })));
        let err = pair(&spec, &Params::new())
            .expect_err("must be refused")
            .to_string();
        assert!(err.contains("createdat"), "{err}");
        assert!(err.contains("did you mean \"created_at\""), "{err}");
    }

    #[test]
    fn a_missing_cursor_key_is_refused_naming_it() {
        let spec = spec_with(
            keys(&[("score", SortDir::Desc), ("id", SortDir::Asc)]),
            Some(json!({ "score": 10 })),
        );
        let err = pair(&spec, &Params::new())
            .expect_err("must be refused")
            .to_string();
        assert!(err.contains("missing 'id'"), "{err}");
        assert!(err.contains("score/id"), "{err}");
    }

    #[test]
    fn a_list_cursor_value_is_refused_naming_the_key() {
        let spec = spec_with(asc("id"), Some(json!({ "id": [1, 2] })));
        let err = pair(&spec, &Params::new())
            .expect_err("must be refused")
            .to_string();
        assert!(err.contains("after.id"), "{err}");
    }

    #[test]
    fn a_missing_whole_cursor_param_names_it() {
        let spec = spec_with(asc("id"), Some(json!({ "param": "cursor" })));
        let err = pair(&spec, &Params::new())
            .expect_err("must be refused")
            .to_string();
        assert!(err.contains("cursor"), "{err}");
    }
}

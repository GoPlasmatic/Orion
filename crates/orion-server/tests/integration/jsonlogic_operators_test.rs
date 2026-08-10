//! The JSONLogic operator vocabulary, and the guard that keeps it documented.
//!
//! datalogic-rs compiles only its core opcodes unless the extension features
//! are switched on, and Orion cannot switch them on directly — the engine is
//! reached through dataflow-rs (see CLAUDE.md), so the lever is dataflow-rs's
//! `all-operators` feature in `Cargo.toml`. Before it was set, a workflow could
//! not call `now`, format a date, uppercase a string or measure a length.
//!
//! Losing that feature set again would be quiet rather than loud, which is what
//! makes this module necessary rather than merely tidy. The two author surfaces
//! disagree about unknown operators:
//!
//! * A **condition** is compiled and evaluated strictly, so an operator this
//!   build does not know raises `Invalid operator` — see
//!   [`an_unsupported_operator_is_rejected_in_a_condition`].
//! * A **`map` mapping** is rendered through dataflow-rs's templating path,
//!   which cannot tell an operator from a single-key data object and so writes
//!   the object through as a *literal* — see
//!   [`an_unknown_operator_in_a_mapping_is_a_silent_literal`]. No error, no
//!   failed task, `200` to the caller.
//!
//! Because most authoring happens in `map` mappings, dropping a feature would
//! turn working workflows into ones that quietly emit `{"upper": [...]}` where
//! a string used to be. [`every_documented_operator_is_compiled`] asserts on
//! *values* rather than on compile success precisely so it fails in that case.
//!
//! Three directions are asserted:
//!
//! 1. Every operator in [`OPERATORS`] really evaluates to its documented result.
//! 2. The reference page lists exactly [`OPERATORS`] — the code is
//!    authoritative and the document may not drift from it, in the same spirit
//!    as `config_docs_drift_test` and `metrics_docs_drift_test`.
//! 3. Both unknown-operator behaviours above stay as documented.

use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common;
use crate::common::{body_json, json_request};

const EXPRESSIONS_MD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/src/reference/expressions.md"
);

/// Every operator Orion supports, with an expression that exercises it and the
/// value that expression must produce.
///
/// The expected *value* is the load-bearing part. An operator whose feature is
/// switched off does not fail to compile — it either raises at evaluation or
/// echoes itself back as an object — so only comparing results distinguishes a
/// working build from a silently degraded one.
///
/// Kept here rather than parsed out of the document so the code is the source
/// of truth and the document is the thing checked.
///
/// The expressions are `fn()` rather than values because `json!` allocates and
/// a `const` cannot.
type OperatorCase = (&'static str, fn() -> Value, fn() -> Value);

const OPERATORS: &[OperatorCase] = &[
    // ---- core ----
    ("var", || json!({"var": "s"}), || json!("Widget")),
    ("val", || json!({"val": "s"}), || json!("Widget")),
    ("==", || json!({"==": [{"var": "n"}, 4]}), || json!(true)),
    ("!=", || json!({"!=": [{"var": "n"}, 5]}), || json!(true)),
    ("===", || json!({"===": [{"var": "n"}, 4]}), || json!(true)),
    (
        "!==",
        || json!({"!==": [{"var": "n"}, "4"]}),
        || json!(true),
    ),
    (">", || json!({">": [{"var": "n"}, 1]}), || json!(true)),
    (">=", || json!({">=": [{"var": "n"}, 4]}), || json!(true)),
    ("<", || json!({"<": [{"var": "n"}, 9]}), || json!(true)),
    ("<=", || json!({"<=": [{"var": "n"}, 4]}), || json!(true)),
    ("and", || json!({"and": [true, true]}), || json!(true)),
    ("or", || json!({"or": [false, true]}), || json!(true)),
    ("!", || json!({"!": [false]}), || json!(true)),
    ("!!", || json!({"!!": [{"var": "s"}]}), || json!(true)),
    ("if", || json!({"if": [true, "y", "n"]}), || json!("y")),
    ("?:", || json!({"?:": [false, "y", "n"]}), || json!("n")),
    ("+", || json!({"+": [1, 2]}), || json!(3)),
    ("-", || json!({"-": [5, 2]}), || json!(3)),
    ("*", || json!({"*": [2, 3]}), || json!(6)),
    ("/", || json!({"/": [6, 2]}), || json!(3)),
    ("%", || json!({"%": [7, 4]}), || json!(3)),
    ("max", || json!({"max": [1, 7, 3]}), || json!(7)),
    ("min", || json!({"min": [4, 2, 9]}), || json!(2)),
    ("cat", || json!({"cat": ["a", "b"]}), || json!("ab")),
    (
        "substr",
        || json!({"substr": [{"var": "s"}, 0, 3]}),
        || json!("Wid"),
    ),
    ("in", || json!({"in": ["b", ["a", "b"]]}), || json!(true)),
    ("merge", || json!({"merge": [[1], [2]]}), || json!([1, 2])),
    (
        "map",
        || json!({"map": [{"var": "nums"}, {"*": [{"var": ""}, 2]}]}),
        || json!([6, 2, 4]),
    ),
    (
        "filter",
        || json!({"filter": [{"var": "nums"}, {">": [{"var": ""}, 1]}]}),
        || json!([3, 2]),
    ),
    (
        "reduce",
        || {
            json!({"reduce": [{"var": "nums"},
                {"+": [{"var": "accumulator"}, {"var": "current"}]}, 0]})
        },
        || json!(6),
    ),
    (
        "all",
        || json!({"all": [{"var": "nums"}, {">": [{"var": ""}, 0]}]}),
        || json!(true),
    ),
    (
        "some",
        || json!({"some": [{"var": "nums"}, {">": [{"var": ""}, 2]}]}),
        || json!(true),
    ),
    (
        "none",
        || json!({"none": [{"var": "nums"}, {">": [{"var": ""}, 9]}]}),
        || json!(true),
    ),
    (
        "missing",
        || json!({"missing": ["nope"]}),
        || json!(["nope"]),
    ),
    (
        "missing_some",
        || json!({"missing_some": [1, ["s", "nope"]]}),
        || json!([]),
    ),
    // ---- datetime ----
    // `now` is the one operator whose value cannot be pinned; the shape check
    // in the test body stands in for an equality assertion.
    ("now", || json!({"now": []}), || Value::Null),
    (
        "datetime",
        || json!({"datetime": ["2026-07-31T00:00:00Z"]}),
        || json!("2026-07-31T00:00:00Z"),
    ),
    // `timestamp` builds a *duration* from a duration string. It is not a
    // datetime-to-epoch conversion, and handing it a datetime is the
    // "Invalid duration format" error rather than a number.
    (
        "timestamp",
        || json!({"timestamp": ["1d"]}),
        || json!("1d:0h:0m:0s"),
    ),
    (
        "parse_date",
        || json!({"format_date": [{"parse_date": ["2026-07-31", "yyyy-MM-dd"]}, "yyyy"]}),
        || json!("2026"),
    ),
    (
        "format_date",
        || json!({"format_date": [{"datetime": ["2026-07-31T00:00:00Z"]}, "yyyy-MM-dd"]}),
        || json!("2026-07-31"),
    ),
    // Units are plural. A singular unit is not an error — `"day"` returns 0 —
    // so this pins the spelling that actually measures something.
    (
        "date_diff",
        || {
            json!({"date_diff": [
                {"datetime": ["2026-07-31T00:00:00Z"]},
                {"datetime": ["2026-07-30T00:00:00Z"]},
                "days"]})
        },
        || json!(1),
    ),
    // ---- ext-string ----
    ("length", || json!({"length": [{"var": "s"}]}), || json!(6)),
    (
        "upper",
        || json!({"upper": [{"var": "s"}]}),
        || json!("WIDGET"),
    ),
    (
        "lower",
        || json!({"lower": [{"var": "s"}]}),
        || json!("widget"),
    ),
    ("trim", || json!({"trim": ["  x  "]}), || json!("x")),
    (
        "split",
        || json!({"split": [{"var": "csv"}, ","]}),
        || json!(["a", "b"]),
    ),
    (
        "starts_with",
        || json!({"starts_with": [{"var": "s"}, "Wid"]}),
        || json!(true),
    ),
    (
        "ends_with",
        || json!({"ends_with": [{"var": "s"}, "get"]}),
        || json!(true),
    ),
    // ---- ext-array / ext-math ----
    (
        "sort",
        || json!({"sort": [{"var": "nums"}]}),
        || json!([1, 2, 3]),
    ),
    (
        "slice",
        || json!({"slice": [{"var": "nums"}, 0, 2]}),
        || json!([3, 1]),
    ),
    ("abs", || json!({"abs": [-7]}), || json!(7)),
    ("ceil", || json!({"ceil": [4.25]}), || json!(5)),
    ("floor", || json!({"floor": [4.25]}), || json!(4)),
    // ---- ext-control / error-handling ----
    (
        "??",
        || json!({"??": [{"var": "nope"}, "fb"]}),
        || json!("fb"),
    ),
    (
        "type",
        || json!({"type": [{"var": "n"}]}),
        || json!("number"),
    ),
    // `exists` takes path *segments* evaluated as literals. The dotted
    // spelling every `var` user reaches for returns `false` instead — pinned
    // by `exists_needs_segments_not_a_dotted_path` below.
    ("exists", || json!({"exists": ["s"]}), || json!(true)),
    (
        "switch",
        || json!({"switch": [{"var": "tier"}, [["gold", 20], ["silver", 10]], 0]}),
        || json!(10),
    ),
    (
        "match",
        || json!({"match": [{"var": "tier"}, [["silver", 1]], 0]}),
        || json!(1),
    ),
    (
        "try",
        || json!({"try": [{"var": "s"}, "fb"]}),
        || json!("Widget"),
    ),
    // `throw` is only observable through `try`, which is also the only pairing
    // that is useful inside a workflow.
    (
        "throw",
        || json!({"try": [{"throw": ["boom"]}, "caught"]}),
        || json!("caught"),
    ),
];

/// Operator names documented in the "Available operators" section.
///
/// A row is a table row whose first cell contains one or more backticked
/// tokens; `` `sort` `` yields one name and `` `upper` / `lower` `` yields two,
/// which is how the tables group operators that share an example. Reading stops
/// at the next `## ` heading so prose elsewhere in the file cannot contribute
/// names.
fn documented_operators() -> BTreeSet<String> {
    let md = std::fs::read_to_string(EXPRESSIONS_MD).expect("read docs/src/reference/expressions.md");
    let heading = "## Available operators";
    let start = md
        .find(heading)
        .expect("expressions.md must document the operator vocabulary");
    let section = &md[start + heading.len()..];
    let end = section.find("\n## ").unwrap_or(section.len());

    let mut out = BTreeSet::new();
    for line in section[..end].lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let Some(first_cell) = line.split('|').nth(1) else {
            continue;
        };
        for token in first_cell.split('`').skip(1).step_by(2) {
            let token = token.trim();
            if !token.is_empty() {
                out.insert(token.to_string());
            }
        }
    }
    out
}

/// The data every expression in [`OPERATORS`] is evaluated against.
fn context() -> Value {
    json!({
        "s": "Widget",
        "n": 4,
        "csv": "a,b",
        "nums": [3, 1, 2],
        "tier": "silver",
    })
}

/// Evaluate one JSONLogic expression through the same pair the channel guards
/// drive (`src/channel/guards.rs`): `compile`, then `session().eval_into()`.
fn try_eval(logic: &Value) -> Result<Value, String> {
    use dataflow_rs::datalogic_rs as datalogic;
    let engine = datalogic::Engine::new();
    let compiled = engine.compile(logic).map_err(|e| e.to_string())?;
    engine
        .session()
        .eval_into::<Value, _>(&compiled, &context())
        .map_err(|e| e.to_string())
}

fn eval(logic: &Value) -> Value {
    try_eval(logic).unwrap_or_else(|e| panic!("evaluating {logic} failed: {e}"))
}

/// Every documented operator is compiled into this build and produces its
/// documented result.
#[test]
fn every_documented_operator_is_compiled() {
    for (name, logic, expected) in OPERATORS {
        let logic = logic();

        if *name == "now" {
            let got = eval(&logic);
            let s = got.as_str().unwrap_or_else(|| {
                panic!("`now` returned {got}, not a string — is the `datetime` feature off?")
            });
            assert!(
                s.len() >= 20 && s.contains('T'),
                "`now` returned {s:?}, which is not an RFC 3339 instant"
            );
            continue;
        }

        let got = try_eval(&logic).unwrap_or_else(|e| {
            panic!(
                "operator `{name}` failed to evaluate {logic}: {e}. If this reads \
                 as an unknown operator, the corresponding feature is missing from \
                 the `dataflow-rs` entry in Cargo.toml."
            )
        });
        assert_eq!(got, expected(), "operator `{name}` evaluated {logic}");
    }
}

/// The reference page documents exactly the operators [`OPERATORS`] pins —
/// no more, no fewer.
#[test]
fn documented_operators_match_the_supported_set() {
    let supported: BTreeSet<String> = OPERATORS.iter().map(|(n, ..)| n.to_string()).collect();
    let documented = documented_operators();

    let undocumented: Vec<&String> = supported.difference(&documented).collect();
    assert!(
        undocumented.is_empty(),
        "operators supported but absent from the 'Available operators' tables in \
         docs/src/reference/expressions.md: {undocumented:?}"
    );

    let invented: Vec<&String> = documented.difference(&supported).collect();
    assert!(
        invented.is_empty(),
        "docs/src/reference/expressions.md documents operators this build does not \
         support: {invented:?}. A reader will write these and get either an error \
         or a silent object literal."
    );
}

/// `exists` reads its arguments as literal path *segments*, so the dotted
/// spelling that works everywhere `var` is accepted silently reports absent.
///
/// Pinned because it is the documented footgun, and because a future datalogic
/// that starts splitting on `.` would make the warning in expressions.md wrong.
#[test]
fn exists_needs_segments_not_a_dotted_path() {
    assert_eq!(
        eval(&json!({"exists": ["nested", "leaf"]})),
        json!(false),
        "sanity: the probe path really is absent from the context"
    );

    let ctx_logic = json!({"exists": ["s"]});
    assert_eq!(eval(&ctx_logic), json!(true));

    // A dotted path is one segment named "s.x", which does not exist — the
    // false here is the footgun, not a missing feature.
    assert_eq!(eval(&json!({"exists": ["s.x"]})), json!(false));
}

/// In a **condition**, an operator this build does not know is a hard error.
///
/// `fractional` belongs to datalogic's `flagd` feature, which `all-operators`
/// deliberately does not include, so it stands in for "any operator that is
/// spelled like one but is not compiled in".
#[test]
fn an_unsupported_operator_is_rejected_in_a_condition() {
    let err = try_eval(&json!({"fractional": [{"var": "s"}, 50]}))
        .expect_err("a disabled operator must not evaluate");
    assert!(
        err.to_lowercase().contains("operator"),
        "expected an invalid-operator error, got {err:?}"
    );
}

/// In a **`map` mapping**, an operator the build does not know is written
/// through as an object literal — no error, no failed task, `200` to the
/// caller.
///
/// This is the behaviour the warning in `docs/src/reference/expressions.md`
/// describes, and the reason the operator table is worth testing at all: a
/// misspelling degrades output silently rather than failing loudly. Exercised
/// end to end through a real channel because the leniency comes from the
/// templating path dataflow-rs uses for mappings, not from the compiler.
#[tokio::test]
async fn an_unknown_operator_in_a_mapping_is_a_silent_literal() {
    let app = common::test_app().await;
    let workflow = common::workflow_with_tasks(
        "unknown-operator",
        json!([
            {
                "id": "parse", "name": "Parse",
                "function": {"name": "parse_json", "input": {"source": "payload", "target": "in"}}
            },
            {
                "id": "shape", "name": "Shape",
                "function": {"name": "map", "input": {"mappings": [
                    // Deliberate misspelling of `upper`.
                    {"path": "data.out.typo", "logic": {"uppr": [{"var": "data.in.s"}]}},
                    {"path": "data.out.ok",   "logic": {"upper": [{"var": "data.in.s"}]}}
                ]}}
            }
        ]),
    );
    common::create_and_activate_channel(&app, "unknown-op-ch", workflow).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/unknown-op-ch",
            Some(json!({"data": {"s": "widget"}})),
        ))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a misspelled operator does not fail the request — that is the point"
    );
    let body = body_json(resp).await;
    assert!(
        body["errors"].as_array().is_some_and(|e| e.is_empty()),
        "expected no errors, got {}",
        body["errors"]
    );

    assert_eq!(
        body["data"]["out"]["ok"],
        json!("WIDGET"),
        "the correctly spelled operator still runs"
    );
    assert!(
        body["data"]["out"]["typo"].is_object(),
        "expected the misspelled operator to survive as an object literal, got {}",
        body["data"]["out"]["typo"]
    );
    assert!(
        body["data"]["out"]["typo"].get("uppr").is_some(),
        "expected the echoed literal to carry the misspelled name, got {}",
        body["data"]["out"]["typo"]
    );
}

//! The formatter's fixture pairs: `tests/fixtures/fmt/<case>/input.json`
//! formats to exactly `expected.json`.
//!
//! One directory per layout rule. Each `expected.json` is the rule's
//! specification, reviewed by a person when it was written — a change to the
//! style shows up here as a diff to read, not as a test that quietly starts
//! passing again. A case directory missing its `expected.json` fails by
//! name, so a rule cannot be added without its example.

use std::path::Path;

use orion::definitions::fmt::{Outcome, format_str};

const CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fmt");

fn cases() -> Vec<(String, std::path::PathBuf)> {
    let mut out: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(CASES)
        .expect("fixtures/fmt")
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    out.sort();
    assert!(
        out.len() >= 25,
        "expected the full fixture set, found {}",
        out.len()
    );
    out
}

fn formatted(text: &str, origin: &str) -> String {
    match format_str(text, origin).unwrap_or_else(|e| panic!("{origin}: {e}")) {
        Outcome::Unchanged => text.to_string(),
        Outcome::Changed(s) => s,
    }
}

#[test]
fn every_case_formats_to_its_expected_output() {
    for (name, dir) in cases() {
        let input = std::fs::read_to_string(dir.join("input.json"))
            .unwrap_or_else(|_| panic!("{name}: no input.json"));
        let expected = std::fs::read_to_string(dir.join("expected.json"))
            .unwrap_or_else(|_| panic!("{name}: no expected.json — every case needs one"));
        let actual = formatted(&input, &name);
        assert_eq!(
            actual, expected,
            "{name}: output differs from expected.json"
        );
    }
}

#[test]
fn every_expected_output_is_a_fixed_point() {
    for (name, dir) in cases() {
        let expected = std::fs::read_to_string(dir.join("expected.json")).unwrap();
        assert_eq!(
            format_str(&expected, &name).unwrap(),
            Outcome::Unchanged,
            "{name}: expected.json is not itself formatted"
        );
    }
}

#[test]
fn the_documented_examples_in_the_plan_hold() {
    // The two before/after pairs `docs/src/reference/fmt.md` shows.
    let condition = r#"{"condition":{"and":[{">=":[{"var":"data.order.amount"},100]},{"<":[{"var":"data.order.amount"},500]}]}}"#;
    assert_eq!(
        formatted(condition, "doc"),
        "{\n  \"condition\": {\n    \"and\": [\n      { \">=\": [{ \"var\": \"data.order.amount\" }, 100] },\n      { \"<\": [{ \"var\": \"data.order.amount\" }, 500] }\n    ]\n  }\n}\n"
    );
    let methods = "{\n  \"methods\": [\n    \"POST\"\n  ]\n}\n";
    assert_eq!(formatted(methods, "doc"), "{ \"methods\": [\"POST\"] }\n");
    let _ = Path::new(CASES);
}

//! `docs/src/reference/clippy.md` lists exactly the registry — id, level,
//! scope and summary — so the page cannot describe a rule that does not
//! exist or omit one that does.

use orion::definitions::clippy;

const CLIPPY_MD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/src/reference/clippy.md"
);

/// The rows of the first table whose header starts with `| Rule |`.
fn documented() -> Vec<Vec<String>> {
    let page = std::fs::read_to_string(CLIPPY_MD).expect("docs/src/reference/clippy.md");
    page.lines()
        .skip_while(|l| !l.starts_with("| Rule |"))
        .skip(2)
        .take_while(|l| l.starts_with("| `"))
        .map(|l| {
            l.trim_matches('|')
                .split(" | ")
                .map(|c| c.trim().trim_matches('`').to_string())
                .collect()
        })
        .collect()
}

#[test]
fn the_rule_table_is_the_registry() {
    let rows = documented();
    let registry = clippy::registry();
    assert_eq!(
        rows.len(),
        registry.len(),
        "the table has {} rows, the registry {} rules",
        rows.len(),
        registry.len()
    );
    for (row, rule) in rows.iter().zip(registry) {
        assert_eq!(row[0], rule.id(), "row order must be registry order");
        assert_eq!(row[1], rule.level().as_str(), "{}", rule.id());
        assert_eq!(row[2], rule.scope().as_str(), "{}", rule.id());
        assert_eq!(row[3], rule.summary(), "{}: summary", rule.id());
    }
}

#[test]
fn every_rule_has_its_own_section() {
    let page = std::fs::read_to_string(CLIPPY_MD).unwrap();
    for rule in clippy::registry() {
        assert!(
            page.contains(&format!("### `{}`", rule.id())),
            "{} has no `### ` section on the page",
            rule.id()
        );
    }
}

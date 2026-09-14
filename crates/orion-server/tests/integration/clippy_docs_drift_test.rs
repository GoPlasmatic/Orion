//! `docs/src/reference/clippy/` lists exactly the registry — id, level, scope
//! and summary on the hub's table, and one page per rule — so the book cannot
//! describe a rule that does not exist or omit one that does.

use orion::definitions::clippy;

const CLIPPY_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/src/reference/clippy"
);

fn hub() -> String {
    std::fs::read_to_string(format!("{CLIPPY_DIR}/index.md"))
        .expect("docs/src/reference/clippy/index.md")
}

/// A rule id becomes its page's file stem: dots and underscores are hyphens.
fn page_stem(id: &str) -> String {
    id.replace(['.', '_'], "-")
}

/// The rows of the first table whose header starts with `| Rule |`. The id cell
/// is a link on the hub, so strip the label out of `[`id`](./page.md)`.
fn documented() -> Vec<Vec<String>> {
    hub()
        .lines()
        .skip_while(|l| !l.starts_with("| Rule |"))
        .skip(2)
        .take_while(|l| l.starts_with("| ["))
        .map(|l| {
            l.trim_matches('|')
                .split(" | ")
                .map(|c| {
                    let c = c.trim();
                    let c = match (c.find('['), c.find("](")) {
                        (Some(a), Some(b)) if a < b => &c[a + 1..b],
                        _ => c,
                    };
                    c.trim().trim_matches('`').to_string()
                })
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
fn every_rule_has_its_own_page() {
    let hub = hub();
    let mut expected: Vec<String> = Vec::new();
    for rule in clippy::registry() {
        let stem = page_stem(rule.id());
        let path = format!("{CLIPPY_DIR}/{stem}.md");
        let page = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{} has no page at {stem}.md", rule.id()));
        assert!(
            page.contains(&format!("\n# `{}`\n", rule.id())),
            "{stem}.md must open on `# `{}``",
            rule.id()
        );
        assert!(
            hub.contains(&format!("](./{stem}.md)")),
            "{} is not linked from the hub's table",
            rule.id()
        );
        expected.push(stem);
    }

    // No stray rule page: every *.md beside the hub is a rule in the registry,
    // the hub itself, or one of the four pages the hub's second table links.
    const NOT_RULES: [&str; 5] = [
        "index",
        "levels",
        "certainty",
        "not-a-rule",
        "adding-a-rule",
    ];
    let mut stray: Vec<String> = std::fs::read_dir(CLIPPY_DIR)
        .expect("docs/src/reference/clippy")
        .filter_map(|e| {
            let name = e.ok()?.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".md")?.to_string();
            (!NOT_RULES.contains(&stem.as_str()) && !expected.contains(&stem)).then_some(stem)
        })
        .collect();
    stray.sort();
    assert!(
        stray.is_empty(),
        "docs/src/reference/clippy/ has pages for rules the registry does not hold: {stray:?}"
    );
}

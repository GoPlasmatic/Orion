//! The acceptance corpus: clippy over the project's own examples and e2e
//! fixtures must produce **nothing**.
//!
//! A rule that fires here is either a real find — fix the example, and say
//! so in the PR — or a rule less certain than its proof claimed. Either way
//! the run is the review, not a snapshot to update.

use std::path::Path;

use crate::clippy_cases_test::run_over;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

#[test]
fn the_example_packages_trip_no_rule() {
    let packages = Path::new(ROOT).join("examples/packages");
    let mut checked = 0;
    for entry in std::fs::read_dir(&packages).unwrap().filter_map(Result::ok) {
        if !entry.path().is_dir() {
            continue;
        }
        let diagnostics = run_over(&entry.path());
        assert!(
            diagnostics.is_empty(),
            "{}: {:#?}",
            entry.path().display(),
            diagnostics
                .iter()
                .map(|d| d.render_text())
                .collect::<Vec<_>>()
        );
        checked += 1;
    }
    assert!(checked >= 8, "only {checked} packages checked");
}

#[test]
fn the_e2e_workflow_fixtures_trip_no_rule() {
    let dir = Path::new(ROOT).join("tests/e2e/fixtures/workflows");
    let diagnostics = run_over(&dir);
    assert!(
        diagnostics.is_empty(),
        "{:#?}",
        diagnostics
            .iter()
            .map(|d| d.render_text())
            .collect::<Vec<_>>()
    );
}

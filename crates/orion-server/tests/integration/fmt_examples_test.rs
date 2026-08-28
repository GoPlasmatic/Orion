//! The CI gate: every JSON file the repository ships as an example or a
//! fixture is in the house style.
//!
//! In-process rather than a workflow step, so it runs wherever `cargo test`
//! does and needs no binary. Its failure message is the diff, and the fix
//! is `just fmt`.

use std::path::Path;

use orion::definitions::fmt::{Outcome, format_str};

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

/// The trees `just fmt` formats. Keep in step with the `fmt` recipe.
const TREES: &[&str] = &[
    "../../examples",
    "../../tests/e2e/cases",
    "../../tests/e2e/fixtures",
];

#[test]
fn every_shipped_json_file_is_formatted() {
    let mut unformatted = Vec::new();
    for tree in TREES {
        let root = Path::new(MANIFEST).join(tree);
        for path in orion::definitions::json_files(&root).unwrap() {
            let text = std::fs::read_to_string(&path).unwrap();
            match format_str(&text, &path.display().to_string()) {
                Ok(Outcome::Unchanged) => {}
                Ok(Outcome::Changed(formatted)) => {
                    let diff = similar::TextDiff::from_lines(&text, &formatted)
                        .unified_diff()
                        .context_radius(2)
                        .header("shipped", "formatted")
                        .to_string();
                    unformatted.push(format!("{}\n{diff}", path.display()));
                }
                Err(e) => panic!("{}: {e}", path.display()),
            }
        }
    }
    assert!(
        unformatted.is_empty(),
        "{} file(s) are not in the house style — run `just fmt`:\n\n{}",
        unformatted.len(),
        unformatted.join("\n")
    );
}

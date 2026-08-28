//! Every clippy rule against its own fixtures, and the registry against the
//! fixture tree.
//!
//! `tests/fixtures/clippy/<rule>/fires/` is a definition set the rule must
//! fire on; `…/quiet/` is one on which **no rule at all** may fire — it is
//! the rule's exclusions written down as a document, and the reason a rule
//! cannot ship without stating when it is silent. A `config.toml` beside a
//! set is passed as `-c`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use orion::definitions::analysis::Analysis;
use orion::definitions::clippy::{self, Diagnostic};
use orion::definitions::{Boundary, DefinitionSet};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/clippy");

/// Run clippy in-process over a set directory: `lint`'s gate, then every
/// rule. Panics on a lint error — a fixture must be a set the API accepts.
pub fn run_over(dir: &Path) -> Vec<Diagnostic> {
    let (raw, _) = DefinitionSet::from_directory_raw(dir).expect("raw load");
    let (compiled, report) = DefinitionSet::from_directory(dir).expect("compiled load");
    let mut findings = report.findings;
    findings.extend(orion::definitions::check(
        &compiled,
        &Boundary::default(),
        false,
    ));
    let errors: Vec<_> = findings.iter().filter(|f| f.is_error()).collect();
    assert!(
        errors.is_empty(),
        "{}: lint errors: {errors:#?}",
        dir.display()
    );

    let config_path = dir.join("config.toml");
    let config = config_path
        .exists()
        .then(|| orion::config::load_config(Some(config_path.to_str().unwrap())).expect("config"));
    let analysis = Analysis::new(&raw, &compiled, &report.shared, config.as_ref());
    clippy::run(&analysis).diagnostics
}

fn rule_dirs() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(FIXTURES)
        .expect("fixtures/clippy")
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    out.sort();
    out
}

#[test]
fn every_rule_has_a_firing_and_a_quiet_fixture_and_nothing_else_does() {
    let rules: BTreeSet<&str> = clippy::registry().iter().map(|r| r.id()).collect();
    let dirs: BTreeSet<String> = rule_dirs().into_iter().map(|(n, _)| n).collect();
    for rule in &rules {
        assert!(
            dirs.contains(*rule),
            "rule `{rule}` has no fixture directory"
        );
        for kind in ["fires", "quiet"] {
            assert!(
                Path::new(FIXTURES).join(rule).join(kind).is_dir(),
                "rule `{rule}` has no `{kind}` fixture"
            );
        }
    }
    for dir in &dirs {
        assert!(
            rules.contains(dir.as_str()),
            "fixture `{dir}` names no rule"
        );
    }
}

#[test]
fn every_rule_fires_on_its_fixture() {
    for (rule, dir) in rule_dirs() {
        let diagnostics = run_over(&dir.join("fires"));
        let fired: Vec<&Diagnostic> = diagnostics.iter().filter(|d| d.rule == rule).collect();
        assert!(
            !fired.is_empty(),
            "`{rule}` did not fire on its `fires` fixture"
        );
        // And nothing else did: a fixture that trips two rules is two
        // fixtures' worth of ambiguity.
        let others: Vec<&str> = diagnostics
            .iter()
            .map(|d| d.rule)
            .filter(|r| *r != rule)
            .collect();
        assert!(
            others.is_empty(),
            "`{rule}`'s fixture also trips {others:?}"
        );
        for d in fired {
            let expected = clippy::find(&rule).unwrap().level();
            assert_eq!(
                d.is_error(),
                expected == clippy::Level::Deny,
                "`{rule}` reported with the wrong severity"
            );
            assert!(
                d.remedy.is_some(),
                "`{rule}`: every diagnostic carries a remedy"
            );
            assert!(d.file.is_some() && d.path.is_some(), "`{rule}`: located");
        }
    }
}

#[test]
fn every_quiet_fixture_is_silent_under_every_rule() {
    for (rule, dir) in rule_dirs() {
        let diagnostics = run_over(&dir.join("quiet"));
        assert!(
            diagnostics.is_empty(),
            "`{rule}`'s quiet fixture trips {:?}",
            diagnostics
                .iter()
                .map(Diagnostic::render_text)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn diagnostics_on_unexpanded_sources_carry_a_line_and_column() {
    // A source with no `use`/`$from` has the compiled form's coordinates, so
    // the front end can point at the line.
    let dir = Path::new(FIXTURES).join("correctness.payload_var/fires");
    let diagnostics = run_over(&dir);
    let d = &diagnostics[0];
    assert!(d.line.is_some(), "{}", d.render_text());
    let text = d.render_text();
    assert!(
        text.contains("w.json:") && text.contains("error: [correctness.payload_var]"),
        "{text}"
    );
}

#[test]
fn json_output_carries_every_field() {
    let dir = Path::new(FIXTURES).join("style.terminal_on_last_step/fires");
    let json = run_over(&dir)[0].render_json();
    for key in [
        "level", "rule", "entity", "file", "path", "line", "column", "message", "remedy",
    ] {
        assert!(json.get(key).is_some(), "missing {key}: {json}");
    }
    assert_eq!(json["level"], "warning");
    assert_eq!(json["rule"], "style.terminal_on_last_step");
}

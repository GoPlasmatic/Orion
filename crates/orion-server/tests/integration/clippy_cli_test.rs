//! `orion-server clippy` as a process: exit codes, the two streams, the
//! flags that are not rules.

use std::process::{Command, Output};

use crate::common::{ScratchDir, orion_bin};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/clippy");

fn clippy(args: &[&str]) -> Output {
    Command::new(orion_bin())
        .arg("clippy")
        .args(args)
        .output()
        .expect("invoke orion-server clippy")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn fixture(rule: &str, kind: &str) -> String {
    format!("{FIXTURES}/{rule}/{kind}")
}

#[test]
fn list_names_every_rule_with_its_level_and_scope() {
    let out = clippy(&["--list"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = text(&out.stdout);
    for rule in orion::definitions::clippy::registry() {
        let line = stdout
            .lines()
            .find(|l| l.starts_with(rule.id()))
            .unwrap_or_else(|| panic!("{} missing from --list", rule.id()));
        assert!(
            line.contains(rule.level().as_str()) && line.contains(rule.scope().as_str()),
            "{line}"
        );
    }
}

#[test]
fn explain_states_the_proof_and_when_the_rule_is_silent() {
    let out = clippy(&["--explain", "correctness.payload_var"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("Proof:") && stdout.contains("Silent when:"),
        "{stdout}"
    );

    let unknown = clippy(&["--explain", "nope.nothing"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(text(&unknown.stderr).contains("--list"));
}

#[test]
fn a_deny_rule_fails_the_run_and_a_warn_rule_does_not_unless_asked() {
    let deny = clippy(&[&fixture("correctness.unreachable_step", "fires")]);
    assert_eq!(deny.status.code(), Some(1), "{}", text(&deny.stderr));
    assert!(text(&deny.stderr).contains("error: [correctness.unreachable_step]"));
    assert!(text(&deny.stdout).contains("1 error(s), 0 warning(s)"));

    let warn = clippy(&[&fixture("style.terminal_on_last_step", "fires")]);
    assert_eq!(warn.status.code(), Some(0), "{}", text(&warn.stderr));
    assert!(text(&warn.stderr).contains("warning: [style.terminal_on_last_step]"));

    let denied = clippy(&[
        "--deny-warnings",
        &fixture("style.terminal_on_last_step", "fires"),
    ]);
    assert_eq!(denied.status.code(), Some(1));

    let clean = clippy(&[&fixture("style.terminal_on_last_step", "quiet")]);
    assert_eq!(clean.status.code(), Some(0));
    assert!(text(&clean.stdout).contains("0 error(s), 0 warning(s)"));
}

#[test]
fn json_output_is_one_object_per_line_on_stdout_only() {
    let out = clippy(&[
        "--format",
        "json",
        &fixture("correctness.payload_var", "fires"),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = text(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(!lines.is_empty());
    for line in &lines {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
        assert!(v.get("rule").is_some());
    }
    assert!(lines.iter().any(|l| l.contains("correctness.payload_var")));
}

#[test]
fn config_rules_are_skipped_with_a_note_without_c_and_run_with_it() {
    let dir = fixture("correctness.metadata_var_undeclared", "fires");
    let without = clippy(&[&dir]);
    assert_eq!(without.status.code(), Some(0), "{}", text(&without.stderr));
    assert!(text(&without.stderr).contains("[correctness.metadata_var_undeclared] skipped"));

    let with = Command::new(orion_bin())
        .args(["-c", &format!("{dir}/config.toml"), "clippy", &dir])
        .output()
        .unwrap();
    assert_eq!(with.status.code(), Some(1), "{}", text(&with.stderr));
    assert!(text(&with.stderr).contains("error: [correctness.metadata_var_undeclared]"));
}

#[test]
fn lint_errors_stop_the_rules_from_running() {
    let dir = ScratchDir::new("clippy_lint_error");
    // A workflow the API would refuse (cache_read without its connector)…
    std::fs::write(
        dir.path().join("broken.json"),
        r#"{"name": "broken", "tasks": [{"id": "t", "name": "T", "function": {"name": "cache_read", "input": {"key": "k"}}}]}"#,
    )
    .unwrap();
    // …beside one every rule would otherwise flag.
    std::fs::write(
        dir.path().join("dead.json"),
        r#"{"name": "dead", "tasks": [{"id": "a", "name": "A", "terminal": true, "function": {"name": "log", "input": {"message": "x"}}}, {"id": "b", "name": "B", "function": {"name": "log", "input": {"message": "y"}}}]}"#,
    )
    .unwrap();
    let out = clippy(&[dir.path().to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("[schema.workflow]"), "{stderr}");
    assert!(
        !stderr.contains("unreachable_step"),
        "rules ran despite a lint error: {stderr}"
    );
    assert!(text(&out.stdout).contains("fix those first"));
}

#[test]
fn a_single_file_is_a_set_of_one() {
    let file = format!("{}/w.json", fixture("correctness.task_never_runs", "fires"));
    let out = clippy(&[&file]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("warning: [correctness.task_never_runs]"));
}

#[test]
fn a_missing_path_is_a_usage_error() {
    let out = clippy(&["/definitely/not/here"]);
    assert_eq!(out.status.code(), Some(2));
    let none = clippy(&[]);
    assert_ne!(none.status.code(), Some(0));
}

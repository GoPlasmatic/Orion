//! Plugins offline: --plugin-dir, the unverifiable note, real execution

use super::*;

/// Single-file lint: a plugin function no manifest accounts for is a
/// `plugin.unverifiable` note and the file is valid; given the manifest, the
/// same task is checked against it like any built-in, and a function the
/// manifest does not declare is an unknown function, not an unverifiable one.
#[test]
fn lint_reports_a_plugin_function_unverifiable_until_its_manifest_is_given() {
    let scratch = ScratchDir::new("lint-plugin-file");
    let plugins = plugin_dir("lint-plugin-manifest", false);
    let good = write_workflow(
        scratch.path(),
        "good.json",
        "test.fixture.wrap",
        r#"{"message":{"var":"data.input.msg"},"output":"data.result"}"#,
    );

    // No manifest anywhere: neither valid nor invalid, so valid with a note.
    let (ok, report) = run_cli(&["lint", &good]);
    assert!(ok, "{report}");
    assert!(report.contains("[plugin.unverifiable]"), "{report}");
    assert!(report.contains("test.fixture.wrap"), "{report}");
    assert!(report.contains("is valid"), "{report}");

    // With the manifest, the same file is checked and clean — no note.
    let (ok, report) = run_cli(&[
        "lint",
        &good,
        "--plugin-dir",
        plugins.path().to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    assert!(!report.contains("plugin.unverifiable"), "{report}");

    // The manifest is the authority: a misspelled field is refused as it is
    // for a built-in, and `required` is enforced.
    let typo = write_workflow(
        scratch.path(),
        "typo.json",
        "test.fixture.wrap",
        r#"{"mesage":"x","output":"data.result"}"#,
    );
    let (ok, report) = run_cli(&[
        "lint",
        &typo,
        "--plugin-dir",
        plugins.path().to_str().unwrap(),
    ]);
    assert!(!ok, "{report}");
    assert!(report.contains("UNKNOWN_FIELD"), "{report}");
    assert!(report.contains("REQUIRED"), "{report}");

    // A function under a plugin the manifest covers but does not declare is
    // unknown, not unverifiable — the manifest answered.
    let undeclared = write_workflow(
        scratch.path(),
        "undeclared.json",
        "test.fixture.nope",
        r#"{"output":"data.result"}"#,
    );
    let (ok, report) = run_cli(&[
        "lint",
        &undeclared,
        "--plugin-dir",
        plugins.path().to_str().unwrap(),
    ]);
    assert!(!ok, "{report}");
    assert!(report.contains("UNKNOWN_FUNCTION"), "{report}");
    assert!(!report.contains("plugin.unverifiable"), "{report}");
}

/// Set mode draws the same line with stable ids: a manifest in the tree is
/// inventoried and validates its callers, an undeclared function of that
/// plugin is `closure.plugin`, and a plugin nothing covers is a note.
#[test]
fn lint_dir_checks_plugin_tasks_against_a_manifest_in_the_tree() {
    let scratch = ScratchDir::new("lint-plugin-dir");
    let dir = scratch.path();
    std::fs::write(dir.join("plugin.toml"), PLUGIN_MANIFEST).unwrap();
    write_workflow(
        dir,
        "good.json",
        "test.fixture.wrap",
        r#"{"message":{"var":"data.input.msg"},"output":"data.result"}"#,
    );
    let (ok, report) = lint_dir(dir, &[]);
    assert!(ok, "{report}");
    assert!(
        report.contains("[plugin.manifest] plugin 'test.fixture'"),
        "{report}"
    );
    assert!(
        report.contains("no component beside the manifest"),
        "the inventory says the bytes are not here: {report}"
    );

    // A second workflow naming a plugin no manifest covers: a note, still valid.
    std::fs::write(
        dir.join("other.json"),
        r#"{"workflow_id":"o","name":"o","tasks":[
          {"id":"t","name":"t","function":{"name":"acme.elsewhere.parse","input":{"x":1,"output":"data.r"}}}]}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(ok, "{report}");
    assert!(report.contains("[plugin.unverifiable]"), "{report}");
    assert!(report.contains("acme.elsewhere.parse"), "{report}");

    // A function the present manifest does not declare is an error with its
    // own id.
    std::fs::write(
        dir.join("undeclared.json"),
        r#"{"workflow_id":"u","name":"u","tasks":[
          {"id":"t","name":"t","function":{"name":"test.fixture.nope","input":{"output":"data.r"}}}]}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "{report}");
    assert!(report.contains("[closure.plugin]"), "{report}");
    assert!(report.contains("test.fixture.nope"), "{report}");
}

/// Offline execution: with the component beside the manifest the plugin
/// function runs for real and its result lands at `output`; without the
/// component the run is refused by name rather than stubbed; without any
/// manifest it is refused before the engine is built.
#[test]
fn dry_run_executes_a_plugin_function_for_real_or_refuses_by_name() {
    let scratch = ScratchDir::new("dry-run-plugin");
    let with_bytes = plugin_dir("dry-run-plugin-full", true);
    let without_bytes = plugin_dir("dry-run-plugin-thin", false);
    let workflow = write_workflow(
        scratch.path(),
        "wf.json",
        "test.fixture.wrap",
        r#"{"message":{"var":"data.input.msg"},"output":"data.result"}"#,
    );
    let input = scratch.path().join("input.json");
    std::fs::write(&input, r#"{"msg":"hi"}"#).unwrap();

    let (ok, report) = run_cli(&[
        "dry-run",
        "-w",
        &workflow,
        "-i",
        input.to_str().unwrap(),
        "--plugin-dir",
        with_bytes.path().to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    assert!(
        report.contains(r#""wrapped""#) && report.contains(r#""len""#),
        "the real component answered: {report}"
    );

    let (ok, report) = run_cli(&[
        "dry-run",
        "-w",
        &workflow,
        "-i",
        input.to_str().unwrap(),
        "--plugin-dir",
        without_bytes.path().to_str().unwrap(),
    ]);
    assert!(!ok, "{report}");
    assert!(report.contains("PLUGIN_ARTIFACT_UNAVAILABLE"), "{report}");
    assert!(report.contains("test.fixture.wrap"), "{report}");

    let (ok, report) = run_cli(&["dry-run", "-w", &workflow, "-i", input.to_str().unwrap()]);
    assert!(!ok, "{report}");
    assert!(report.contains("PLUGIN_ARTIFACT_UNAVAILABLE"), "{report}");
    assert!(report.contains("--plugin-dir"), "{report}");
}

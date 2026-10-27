//! dry-run --stubs, and the `test` runner built on it

use super::*;

/// Without a stub file, a connector-backed task names the stub that would
/// satisfy it — not a bare `Connector '…' not found`, which tells the author
/// nothing actionable.
#[test]
fn dry_run_without_stubs_names_the_missing_stub() {
    let wf = write_temp(CONNECTOR_WORKFLOW, "stub-missing");
    let input = write_temp(r#"{"id":"ORD-1"}"#, "stub-missing-in");

    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input])
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("no stub for 'http_call' on target 'crm'"),
        "expected the error to name the stub to add, got: {stdout}"
    );
    assert!(
        !out.status.success(),
        "an unsatisfied connector task must fail the dry run"
    );
}

/// With a stub, the whole workflow runs offline and the canned response lands
/// at the task's `output` path.
#[test]
fn dry_run_with_stubs_runs_the_whole_workflow_offline() {
    let wf = write_temp(CONNECTOR_WORKFLOW, "stub-ok");
    let input = write_temp(r#"{"id":"ORD-1"}"#, "stub-ok-in");
    let stubs = write_temp(r#"{"http_call":{"crm":{"name":"Ada"}}}"#, "stub-ok-stubs");

    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input, "--stubs", &stubs])
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(out.status.success(), "dry run failed: {stdout}");
    assert!(
        stdout.contains("\"customer_name\": \"Ada\""),
        "the stubbed response must reach the downstream task: {stdout}"
    );
}

/// `"*"` stubs any target, for a workflow whose connector name is not worth
/// pinning in the fixture.
#[test]
fn a_wildcard_stub_matches_any_target() {
    let wf = write_temp(CONNECTOR_WORKFLOW, "stub-wild");
    let input = write_temp(r#"{"id":"ORD-1"}"#, "stub-wild-in");
    let stubs = write_temp(r#"{"http_call":{"*":{"name":"Any"}}}"#, "stub-wild-stubs");

    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input, "--stubs", &stubs])
        .output()
        .expect("run dry-run");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"customer_name\": \"Any\""));
}

/// A stub file naming a function that cannot be stubbed is a typo, and is
/// refused before anything runs.
#[test]
fn a_stub_file_naming_an_unknown_function_is_refused() {
    let wf = write_temp(CONNECTOR_WORKFLOW, "stub-bad");
    let input = write_temp(r#"{"id":"ORD-1"}"#, "stub-bad-in");
    let stubs = write_temp(r#"{"htp_call":{"crm":{}}}"#, "stub-bad-stubs");

    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input, "--stubs", &stubs])
        .output()
        .expect("run dry-run");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("htp_call"),
        "the error must name the offending key"
    );
}

/// A passing suite exits zero and reports each case.
#[test]
fn test_runner_reports_and_exits_zero_on_a_passing_suite() {
    let scratch = temp_suite();
    let dir = scratch.path();
    std::fs::write(
        dir.join("enrich.case.json"),
        r#"{
            "name": "enriches the order",
            "workflow": "wf.json",
            "input": {"id": "ORD-1"},
            "stubs": {"http_call": {"crm": {"name": "Ada"}}},
            "expect": {"data.order.customer_name": "Ada"}
        }"#,
    )
    .unwrap();

    let out = Command::new(orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(out.status.success(), "suite failed: {stdout}");
    assert!(stdout.contains("enriches the order"), "{stdout}");
    assert!(stdout.contains("1 passed, 0 failed"), "{stdout}");
}

/// A failing case prints the diff and exits non-zero, so a suite gates CI.
///
/// The diff is the whole point: "expected X, got Y at this path" is what a
/// bare pass/fail makes an author go and reconstruct by hand.
#[test]
fn test_runner_prints_a_diff_and_exits_nonzero_on_failure() {
    let scratch = temp_suite();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wrong.case.json"),
        r#"{
            "name": "wrong expectation",
            "workflow": "wf.json",
            "input": {"id": "ORD-1"},
            "stubs": {"http_call": {"crm": {"name": "Ada"}}},
            "expect": {"data.order.customer_name": "Grace"}
        }"#,
    )
    .unwrap();

    let out = Command::new(orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(!out.status.success(), "a failing case must gate CI");
    assert!(
        stdout.contains("expected \"Grace\", got \"Ada\""),
        "expected a value diff, got: {stdout}"
    );
}

/// Only `*.case.json` is collected from a directory.
///
/// A suite directory is the natural home for the workflows and fixtures the
/// cases reference; scanning every `*.json` reported the workflow under test
/// as a broken case.
#[test]
fn the_runner_ignores_non_case_json_in_the_suite_directory() {
    let scratch = temp_suite();
    let dir = scratch.path();
    // Fixtures that must not be mistaken for cases.
    std::fs::write(dir.join("input.json"), r#"{"id":"ORD-1"}"#).unwrap();
    std::fs::write(dir.join("stubs.json"), r#"{"http_call":{"crm":{}}}"#).unwrap();
    std::fs::write(
        dir.join("ok.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {"id": "ORD-1"},
            "stubs": {"http_call": {"crm": {"name": "Ada"}}},
            "expect": {"data.order.customer_name": "Ada"}
        }"#,
    )
    .unwrap();

    let out = Command::new(orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains("1 passed, 0 failed"),
        "wf.json / input.json / stubs.json must not be collected as cases: {stdout}"
    );
}

/// A directory with no cases is an error, not a silent pass — a suite that
/// matched nothing must never look like a green run.
#[test]
fn an_empty_suite_is_an_error() {
    let scratch = temp_suite();
    let dir = scratch.path();
    let out = Command::new(orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no test cases found"),
        "the error must say the suite matched nothing"
    );
}

/// `expect_errors` defaults to empty and is checked even when a case omits it,
/// so a workflow that starts failing its tasks cannot pass silently.
#[test]
fn unexpected_task_errors_fail_a_case() {
    let scratch = temp_suite();
    let dir = scratch.path();
    std::fs::write(
        dir.join("unstubbed.case.json"),
        r#"{
            "name": "no stub supplied",
            "workflow": "wf.json",
            "input": {"id": "ORD-1"},
            "expect": {}
        }"#,
    )
    .unwrap();

    let out = Command::new(orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    assert!(
        !out.status.success(),
        "a case whose workflow errored must not pass: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// The other half of that contract: a case that *names* the codes is asserting
/// the failure, so the run halting on them is the pass. Without this a refusal
/// has no test — the run's `Err` failed the case however well the codes matched
/// — and the failing task is credited to `expect_tasks`, since it ran.
#[test]
fn an_expected_task_error_is_the_pass_and_names_the_task_that_failed() {
    let scratch = temp_suite();
    let dir = scratch.path();
    std::fs::write(
        dir.join("refused.case.json"),
        r#"{
            "name": "the missing stub is refused, and nothing downstream runs",
            "workflow": "wf.json",
            "input": {"id": "ORD-1"},
            "expect": {"data.order.customer_name": null},
            "expect_errors": ["FUNCTION_ERROR"],
            "expect_tasks": ["parse", "lookup"]
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(ok, "a case that expects the failure must pass: {out}");

    // And it cannot pass by naming any codes at all: the match stays exact.
    std::fs::write(
        dir.join("refused.case.json"),
        r#"{
            "name": "the wrong codes",
            "workflow": "wf.json",
            "input": {"id": "ORD-1"},
            "expect": {},
            "expect_errors": ["VALIDATION_ERROR"]
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(!ok, "the wrong codes must still fail the case: {out}");
    assert!(
        out.contains("task errors: expected [\"VALIDATION_ERROR\"]"),
        "the diff must name the codes: {out}"
    );
}

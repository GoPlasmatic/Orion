//! #283: case metadata, the recorded call log, and the expect roots

use super::*;

/// The headline of #283: a case can set `metadata`, so a header-gated branch is
/// reachable offline. Also pins the two fidelity rules that make an offline
/// pass mean a production pass — lowercased keys, masked credentials.
#[test]
fn a_case_can_set_metadata_and_reach_a_header_gated_branch() {
    let scratch = suite_with(HEADER_BRANCH_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("device.case.json"),
        r#"{
            "name": "device login",
            "workflow": "wf.json",
            "metadata": {
                "headers": {"DeviceId": "device-abc", "Authorization": "Bearer s3cret"},
                "auth": {"claims": {"sub": "asha@example.com"}},
                "query": {"page": "2"}
            },
            "input": {},
            "expect": {
                "data.mode": "device",
                "data.device": "device-abc",
                "data.subject": "asha@example.com",
                "data.page": "2",
                "data.token": "******"
            }
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(ok, "the device branch must be reachable offline: {out}");
}

/// A metadata shape the HTTP ingress could never produce fails the case, naming
/// the field — not silently, and not as a mystery `<absent>` diff later.
#[test]
fn malformed_case_metadata_fails_naming_the_field() {
    let scratch = suite_with(HEADER_BRANCH_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("bad.case.json"),
        r#"{
            "workflow": "wf.json",
            "metadata": {"headers": ["nope"]},
            "input": {},
            "expect": {}
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(!ok, "a malformed metadata block must fail the case: {out}");
    assert!(out.contains("metadata.headers"), "{out}");
}

/// `expect` reaches the other four documents, not just `data`.
#[test]
fn expect_reaches_metadata_temp_data_calls_and_the_audit_trail() {
    let scratch = suite_with(VERBATIM_LOGIC_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("roots.case.json"),
        r#"{
            "workflow": "wf.json",
            "metadata": {"channel": "rotate"},
            "input": {},
            "stubs": {"cache_write": {"sessions": {"ok": true}}},
            "expect": {
                "data.done": true,
                "metadata.channel": "rotate",
                "temp_data.sid": "sess-1",
                "calls.cache_write[0].stub_target": "sessions",
                "calls.cache_write[0].input.value._id": "sess-1",
                "calls.cache_write[0].task_id": "persist",
                "audit_trail[1].task_id": "persist"
            }
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(ok, "every root must resolve: {out}");
}

/// The regression #283 is about. The workflow writes `{"if": [...]}` verbatim
/// because `cache_write` folds `{"var": ..}` and nothing else; a case asserting
/// the intended number must fail, not stay green behind a stub.
#[test]
fn expect_calls_catches_jsonlogic_written_verbatim() {
    let scratch = suite_with(VERBATIM_LOGIC_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("rotate.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {},
            "stubs": {"cache_write": {"sessions": {"ok": true}}},
            "expect_calls": {
                "cache_write": [
                    {"key": "session",
                     "value": {"generation": 2, "revokedAt": null}}
                ]
            }
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(!ok, "the verbatim-logic write must be caught: {out}");
    assert!(
        out.contains("generation"),
        "the diff must name the field that was not what it looks like: {out}"
    );
}

/// Presence is strict in `expect_calls`, unlike `expect`: `null` asserts
/// *written as null*, so a field the workflow never wrote is a failure rather
/// than a pass. That distinction is the whole point of the session-lifecycle
/// case in the issue.
#[test]
fn expect_calls_treats_a_null_expectation_as_written_not_absent() {
    let scratch = suite_with(VERBATIM_LOGIC_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("absent.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {},
            "stubs": {"cache_write": {"sessions": {"ok": true}}},
            "expect_calls": {
                "cache_write": [{"value": {"neverWritten": null}}]
            }
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(!ok, "an unwritten field must not pass as null: {out}");
    assert!(out.contains("not written"), "{out}");
}

/// The count is part of the assertion, so an unexpected extra call fails — and
/// an empty list asserts a function was never called at all.
#[test]
fn expect_calls_checks_the_call_count() {
    let scratch = suite_with(VERBATIM_LOGIC_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("count.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {},
            "stubs": {"cache_write": {"sessions": {"ok": true}}},
            "expect_calls": {"cache_write": [], "db_write": []}
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(
        !ok,
        "one recorded write against zero expected must fail: {out}"
    );
    assert!(
        out.contains("expected 0 call(s), recorded 1"),
        "the diff must give both counts: {out}"
    );
}

/// Branch coverage in one line — and it fails when a condition sends the run
/// down a different path.
#[test]
fn expect_tasks_asserts_which_tasks_ran() {
    let scratch = suite_with(VERBATIM_LOGIC_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("ran.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {},
            "stubs": {"cache_write": {"sessions": {"ok": true}}},
            "expect_tasks": ["seed", "persist"]
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wrong.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {},
            "stubs": {"cache_write": {"sessions": {"ok": true}}},
            "expect_tasks": ["seed"]
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(!ok, "the wrong expectation must fail: {out}");
    assert!(out.contains("1 passed, 1 failed"), "{out}");
    assert!(out.contains("tasks: expected"), "{out}");
}

/// The root is required. A bare path is refused before the workflow runs,
/// with the fix in the message.
#[test]
fn an_unrooted_expect_path_is_refused_with_the_fix() {
    let scratch = suite_with(HEADER_BRANCH_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("bare.case.json"),
        r#"{"workflow": "wf.json", "input": {}, "expect": {"mode": "password"}}"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(!ok, "an unrooted path must fail: {out}");
    assert!(out.contains("did you mean 'data.mode'"), "{out}");
    assert!(out.contains("temp_data"), "the roots must be listed: {out}");
}

/// The silent miss the roots exist to remove: `metadata.absent` resolved as
/// `data.metadata.absent` would come back absent and — because an expected
/// null matches absent — *pass*. A typo'd root is the same class and fails too.
#[test]
fn a_typo_in_the_root_no_longer_passes_silently() {
    let scratch = suite_with(HEADER_BRANCH_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("typo.case.json"),
        r#"{"workflow": "wf.json", "input": {}, "expect": {"dat.mode": null}}"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(
        !ok,
        "a typo'd root expecting null must not pass silently: {out}"
    );
    assert!(out.contains("has no root"), "{out}");
}

/// `dry-run` publishes the same roots the case format uses, so a path read off
/// a dry run can be pasted into a case unchanged.
#[test]
fn dry_run_prints_the_call_log_and_the_context_documents() {
    let wf = write_temp(VERBATIM_LOGIC_WORKFLOW, "dryrun-calls");
    let input = write_temp("{}", "dryrun-calls-in");
    let stubs = write_temp(
        r#"{"cache_write":{"sessions":{"ok":true}}}"#,
        "dryrun-stubs",
    );
    let metadata = write_temp(r#"{"channel":"rotate"}"#, "dryrun-meta");

    let out = Command::new(orion_bin())
        .args([
            "dry-run",
            "-w",
            &wf,
            "-i",
            &input,
            "--stubs",
            &stubs,
            "--metadata",
            &metadata,
        ])
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");

    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("dry-run prints JSON");
    assert_eq!(parsed["metadata"]["channel"], "rotate");
    assert_eq!(parsed["temp_data"]["sid"], "sess-1");
    // Grouped by function, the same shape a case's `calls.<fn>[i]` addresses —
    // one shape on both surfaces, so a path lifted from here works in a case.
    // Global ordering is not lost to the grouping: each record carries `seq`.
    assert_eq!(parsed["calls"]["cache_write"][0]["function"], "cache_write");
    assert_eq!(parsed["calls"]["cache_write"][0]["task_id"], "persist");
    assert_eq!(parsed["calls"]["cache_write"][0]["seq"], 0);
    assert_eq!(
        parsed["calls"]["cache_write"][0]["input"]["value"]["generation"],
        serde_json::json!({"if": [true, 2, 1]}),
        "the log shows the node the cache would have stored"
    );
    assert!(parsed["audit_trail"].is_array());

    for path in [&wf, &input, &stubs, &metadata] {
        let _ = std::fs::remove_file(path);
    }
}

/// The warning is advisory by default, so no existing pipeline breaks on
/// upgrade — and `--deny-warnings` is the opt-in that gates a PR.
#[test]
fn lint_warns_on_unresolvable_logic_and_denies_it_on_request() {
    let wf = write_temp(VERBATIM_LOGIC_WORKFLOW, "lint-logic");

    let out = Command::new(orion_bin())
        .args(["lint", &wf])
        .output()
        .expect("run lint");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a warning must not fail lint by default: {stderr}"
    );
    assert!(stderr.contains("warning:"), "{stderr}");
    assert!(
        stderr.contains("'if'"),
        "the operator must be named: {stderr}"
    );

    let out = Command::new(orion_bin())
        .args(["lint", &wf, "--deny-warnings"])
        .output()
        .expect("run lint --deny-warnings");
    assert!(
        !out.status.success(),
        "--deny-warnings must gate: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_file(&wf);
}

#!/usr/bin/env bash
# Suite: Read-only command groups
#
# Smoke coverage for the command groups the rest of the suite never reaches.
# These are all read-only against the server the runner already has up, so they
# cost a request each and cannot leave state behind. They exist because the
# gap they close was invisible: a command group that no suite invokes can break
# its output shape, its flag parsing, or its envelope unwrapping and stay green
# all the way to a release.
#
# `config` and `completions` are local-only (no server), and are covered here
# for the same reason.

begin_suite "Read-only command groups"

test_functions_list() {
    cli functions list
    assert_exit_code 0 "$CLI_EXIT"
    # The registry is compiled in, so the set is non-empty on any server.
    assert_contains "$CLI_OUTPUT" "http_call"
    assert_contains "$CLI_OUTPUT" "map"
}

test_metrics_reports_the_endpoint_being_off() {
    # The suite's server runs with `[metrics] enabled = false`, so `/metrics`
    # is not routed. What is worth pinning is that the command surfaces that
    # as a clean non-zero failure rather than printing an empty success —
    # scripts branch on the exit code.
    cli_raw metrics
    assert_exit_code 1 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "NOT_FOUND"
}

test_audit_logs_list() {
    cli audit-logs list
    assert_exit_code 0 "$CLI_EXIT"
    # Shape, not contents: earlier suites may or may not have left entries.
    assert_json_has_key "$CLI_OUTPUT" '.'
}

test_backups_list() {
    cli backups list
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_has_key "$CLI_OUTPUT" '.'
}

test_packages_list() {
    cli packages list
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_has_key "$CLI_OUTPUT" '.'
}

test_dlq_list() {
    cli dlq list
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_has_key "$CLI_OUTPUT" '.'
}

test_completions_generates_a_script() {
    cli_raw completions bash
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "orion-cli"
}

test_config_show() {
    cli_raw config show
    assert_exit_code 0 "$CLI_EXIT"
}

run_test "functions list names the built-in functions" test_functions_list
run_test "metrics fails cleanly when the endpoint is off" test_metrics_reports_the_endpoint_being_off
run_test "audit-logs list returns a JSON page"         test_audit_logs_list
run_test "backups list returns a JSON page"            test_backups_list
run_test "packages list returns a JSON page"           test_packages_list
run_test "dlq list returns a JSON page"                test_dlq_list
run_test "completions bash emits a script"             test_completions_generates_a_script
run_test "config show succeeds"                        test_config_show

end_suite

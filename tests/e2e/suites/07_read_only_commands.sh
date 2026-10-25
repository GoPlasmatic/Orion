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

test_connections_list() {
    cli connections list
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_has_key "$CLI_OUTPUT" '.connections'
}

test_deployments_capabilities() {
    # The built-in drivers a compiled connection names by digest are listed
    # here; `deployments apply -d` reads them, so their shape is pinned.
    cli deployments capabilities
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$(jq -r '[.provider_backends[].driver_id] | join(",")' <<< "$CLI_OUTPUT")" "org.orion.http"
}

test_dlq_list() {
    cli dlq list
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_has_key "$CLI_OUTPUT" '.'
}

test_audit_logs_filters_are_accepted() {
    # The endpoint rejects an unrecognised parameter with a 400 rather than
    # answering a 200 of unfiltered rows, so a filter name the CLI spells
    # wrong fails loudly here rather than returning a silently wider answer.
    cli audit-logs list \
        --action v2_definition_upload --resource-type definition --resource-id nothing \
        --principal anonymous --start-time 2020-01-01T00:00:00Z \
        --end-time 2999-01-01T00:00:00Z --limit 5
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_has_key "$CLI_OUTPUT" '.'
    # Every filter applied: no row can match resource_id "nothing".
    assert_json_length "$CLI_OUTPUT" '.data' 0
}

test_list_flags_are_accepted() {
    # Paging (and, for traces, sorting) exist on the server for these lists;
    # the flag wiring is what this pins — a dropped parameter would still
    # exit 0.
    cli traces list --limit 5 --offset 0 --sort-by created_at --sort-order asc
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.limit' '5'

    cli dlq list --limit 5 --offset 0
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.limit' '5'

    cli cron list --limit 5 --offset 0
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.limit' '5'
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
run_test "connections list returns the revisions"      test_connections_list
run_test "deployments capabilities lists the drivers"  test_deployments_capabilities
run_test "dlq list returns a JSON page"                test_dlq_list
run_test "audit-logs filters are all accepted"         test_audit_logs_filters_are_accepted
run_test "list paging and sorting flags reach the server" test_list_flags_are_accepted
run_test "completions bash emits a script"             test_completions_generates_a_script
run_test "config show succeeds"                        test_config_show

end_suite

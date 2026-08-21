#!/usr/bin/env bash
# Suite: Workflow Status Lifecycle

begin_suite "Workflow Status Lifecycle"

test_activate_draft_workflow() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local workflow_id="$CLI_OUTPUT"

    cli workflows get "$workflow_id"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'draft'

    cli_quiet workflows activate "$workflow_id"
    assert_exit_code 0 "$CLI_EXIT"

    cli workflows get "$workflow_id"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'active'
}

test_archive_workflow() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local workflow_id="$CLI_OUTPUT"
    cli_quiet workflows activate "$workflow_id"

    cli_quiet workflows archive "$workflow_id"
    assert_exit_code 0 "$CLI_EXIT"

    cli workflows get "$workflow_id"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'archived'
}

test_status_filter_list() {
    reset_server_state
    cli_quiet workflows create -d '{"name":"Active Workflow","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"a"}}}]}'
    local active_id="$CLI_OUTPUT"
    cli_quiet workflows activate "$active_id"

    cli_quiet workflows create -d '{"name":"Draft Workflow","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"d"}}}]}'

    cli workflows list --status active
    assert_json_length "$CLI_OUTPUT" '.data' 1
    assert_json_eq "$CLI_OUTPUT" '.data[0].name' 'Active Workflow'

    cli workflows list --status draft
    assert_json_length "$CLI_OUTPUT" '.data' 1
    assert_json_eq "$CLI_OUTPUT" '.data[0].name' 'Draft Workflow'
}

test_activation_preflight_passes() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local workflow_id="$CLI_OUTPUT"

    cli workflows activate "$workflow_id" --dry-run
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.valid' 'true'

    # A pre-flight writes nothing — the workflow is still a draft afterwards.
    cli workflows get "$workflow_id"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'draft'
}

test_activation_preflight_fails_nonzero() {
    # The server reports a refused transition as `valid: false` inside a 200,
    # so a caller reading only the HTTP status sees a failing pre-flight as a
    # passing one. The exit code is what a promotion script branches on.
    reset_server_state

    cli_raw workflows activate no-such-workflow --dry-run
    assert_exit_code 1 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "INVALID"
}

test_deferred_reload_leaves_the_engine_alone() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local workflow_id="$CLI_OUTPUT"

    cli_raw workflows activate "$workflow_id" --defer-reload
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "Reload deferred"

    # The row is committed either way; only the engine rebuild waits.
    cli workflows get "$workflow_id"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'active'

    cli engine reload
    assert_exit_code 0 "$CLI_EXIT"
}

run_test "activate draft workflow"           test_activate_draft_workflow
run_test "archive workflow"                  test_archive_workflow
run_test "list workflows filtered by status" test_status_filter_list
run_test "activation pre-flight passes"      test_activation_preflight_passes
run_test "failed pre-flight exits non-zero"  test_activation_preflight_fails_nonzero
run_test "deferred reload defers the reload" test_deferred_reload_leaves_the_engine_alone

end_suite

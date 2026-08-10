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

run_test "activate draft workflow"           test_activate_draft_workflow
run_test "archive workflow"                  test_archive_workflow
run_test "list workflows filtered by status" test_status_filter_list

end_suite

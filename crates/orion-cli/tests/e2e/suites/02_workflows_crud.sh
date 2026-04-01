#!/usr/bin/env bash
# Suite: Workflows CRUD

begin_suite "Workflows CRUD"

test_create_workflow_from_file() {
    reset_server_state
    cli workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'E2E Simple Log'
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'draft'
    assert_json_eq "$CLI_OUTPUT" '.data.version' '1'
    assert_json_has_key "$CLI_OUTPUT" '.data.workflow_id'
}

test_create_workflow_inline() {
    reset_server_state
    cli workflows create -d '{"name":"Inline Workflow","condition":true,"tasks":[{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"inline"}}}]}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'Inline Workflow'
}

test_create_workflow_quiet_returns_id() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_matches "$CLI_OUTPUT" '^[0-9a-f-]{36}$'
}

test_get_workflow() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local workflow_id="$CLI_OUTPUT"

    cli workflows get "$workflow_id"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'E2E Simple Log'
    assert_json_eq "$CLI_OUTPUT" '.data.workflow_id' "$workflow_id"
    assert_json_has_key "$CLI_OUTPUT" '.data.version'
}

test_list_workflows() {
    reset_server_state
    cli_quiet workflows create -d '{"name":"Workflow 1","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"1"}}}]}'
    cli_quiet workflows create -d '{"name":"Workflow 2","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"2"}}}]}'

    cli workflows list
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_length "$CLI_OUTPUT" '.data' 2
}

test_update_workflow() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local workflow_id="$CLI_OUTPUT"

    cli workflows update "$workflow_id" -d '{"name":"Updated Name","priority":99}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'Updated Name'
    assert_json_eq "$CLI_OUTPUT" '.data.priority' '99'
}

test_delete_workflow() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local workflow_id="$CLI_OUTPUT"

    cli_quiet workflows delete "$workflow_id"
    assert_exit_code 0 "$CLI_EXIT"

    cli workflows get "$workflow_id"
    assert_exit_code 1 "$CLI_EXIT"
}

test_get_nonexistent_workflow() {
    reset_server_state
    cli workflows get "nonexistent-id-00000000"
    assert_exit_code 1 "$CLI_EXIT"
}

run_test "create workflow from file"              test_create_workflow_from_file
run_test "create workflow inline"                 test_create_workflow_inline
run_test "create workflow quiet returns UUID"     test_create_workflow_quiet_returns_id
run_test "get workflow by id"                     test_get_workflow
run_test "list workflows"                         test_list_workflows
run_test "update workflow"                        test_update_workflow
run_test "delete workflow"                        test_delete_workflow
run_test "get nonexistent workflow returns error" test_get_nonexistent_workflow

end_suite

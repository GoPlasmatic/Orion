#!/usr/bin/env bash
# Suite: Workflow Testing / Dry Run

begin_suite "Workflow Testing / Dry Run"

test_workflow_match() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/conditional.json"
    local workflow_id="$CLI_OUTPUT"

    cli workflows test "$workflow_id" -d '{"amount": 250}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.matched' 'true'
    assert_json_has_key "$CLI_OUTPUT" '.output'
    assert_json_eq "$CLI_OUTPUT" '.output.order.label' 'High value: $250'
}

test_workflow_no_match() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/conditional.json"
    local workflow_id="$CLI_OUTPUT"

    # Condition is amount > 100 (task-level); sending 50 should skip the transform task
    cli workflows test "$workflow_id" -d '{"amount": 50}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.matched' 'true'
    # Parse still runs, but transform is skipped — no label added
    assert_json_eq "$CLI_OUTPUT" '.output.order.amount' '50'
    assert_json_eq "$CLI_OUTPUT" '.output.order.label' 'null'
}

test_workflow_test_with_file() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/conditional.json"
    local workflow_id="$CLI_OUTPUT"

    cli workflows test "$workflow_id" -f "$FIXTURES_DIR/data/order_high.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.matched' 'true'
}

run_test "workflow test matches with inline data" test_workflow_match
run_test "workflow test condition skips tasks"     test_workflow_no_match
run_test "workflow test with file input"          test_workflow_test_with_file

end_suite

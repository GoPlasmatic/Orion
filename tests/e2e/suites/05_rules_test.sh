#!/usr/bin/env bash
# Suite: Rule Testing / Dry Run

begin_suite "Rule Testing / Dry Run"

test_rule_match() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/conditional.json"
    local rule_id="$CLI_OUTPUT"

    cli rules test "$rule_id" -d '{"amount": 250}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.matched' 'true'
    assert_json_has_key "$CLI_OUTPUT" '.output'
    assert_json_eq "$CLI_OUTPUT" '.output.order.label' 'High value: $250'
}

test_rule_no_match() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/conditional.json"
    local rule_id="$CLI_OUTPUT"

    # Condition is amount > 100 (task-level); sending 50 should skip the transform task
    cli rules test "$rule_id" -d '{"amount": 50}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.matched' 'true'
    # Parse still runs, but transform is skipped — no label added
    assert_json_eq "$CLI_OUTPUT" '.output.order.amount' '50'
    assert_json_eq "$CLI_OUTPUT" '.output.order.label' 'null'
}

test_rule_test_with_file() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/conditional.json"
    local rule_id="$CLI_OUTPUT"

    cli rules test "$rule_id" -f "$FIXTURES_DIR/data/order_high.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.matched' 'true'
}

run_test "rule test matches with inline data" test_rule_match
run_test "rule test condition skips tasks"     test_rule_no_match
run_test "rule test with file input"          test_rule_test_with_file

end_suite

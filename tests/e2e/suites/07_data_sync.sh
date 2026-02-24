#!/usr/bin/env bash
# Suite: Synchronous Data Processing

begin_suite "Synchronous Data Processing"

test_sync_process_data() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    cli_quiet engine reload

    cli send orders -d '{"order_id":"ORD-001","amount":150}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'
    assert_json_has_key "$CLI_OUTPUT" '.id'
    assert_json_has_key "$CLI_OUTPUT" '.data'
}

test_sync_process_with_transform() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/conditional.json"
    cli_quiet engine reload

    cli send orders -d '{"amount":250,"product":"Widget Pro"}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'
    assert_json_has_key "$CLI_OUTPUT" '.data'
    assert_json_eq "$CLI_OUTPUT" '.data.order.label' 'High value: $250'
    assert_json_eq "$CLI_OUTPUT" '.data.order.product' 'Widget Pro'
}

test_sync_no_matching_rules() {
    reset_server_state
    cli_quiet engine reload

    cli send nonexistent-channel -d '{"key":"value"}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'
}

test_sync_from_file() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    cli_quiet engine reload

    cli send orders -f "$FIXTURES_DIR/data/order_high.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'
}

run_test "sync process data"                test_sync_process_data
run_test "sync process with transform rule" test_sync_process_with_transform
run_test "sync process no matching rules"   test_sync_no_matching_rules
run_test "sync process from file"           test_sync_from_file

end_suite

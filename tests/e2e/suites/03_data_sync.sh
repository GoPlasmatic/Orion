#!/usr/bin/env bash
# Suite: Synchronous Data Processing

begin_suite "Synchronous Data Processing"

test_sync_process_data() {
    deploy_one "$FIXTURES_DIR/workflows/simple_log.json" simple-log orders

    cli send orders -d '{"order_id":"ORD-001","amount":150}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'
    assert_json_has_key "$CLI_OUTPUT" '.id'
    assert_json_has_key "$CLI_OUTPUT" '.data'
}

test_sync_process_with_transform() {
    deploy_one "$FIXTURES_DIR/workflows/conditional.json" conditional orders

    cli send orders -d '{"amount":250,"product":"Widget Pro"}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'
    assert_json_eq "$CLI_OUTPUT" '.data.order.label' 'High value: $250'
    assert_json_eq "$CLI_OUTPUT" '.data.order.product' 'Widget Pro'
}

test_sync_unknown_channel_fails() {
    deploy_one "$FIXTURES_DIR/workflows/simple_log.json" simple-log orders

    # A send resolves to a channel the head deploys; any other name is
    # refused, not silently accepted.
    cli send nonexistent-channel -d '{"key":"value"}'
    assert_ne "$CLI_EXIT" 0 "sending to an unknown channel must fail"
}

test_sync_from_file() {
    deploy_one "$FIXTURES_DIR/workflows/conditional.json" conditional orders

    cli send orders -f "$FIXTURES_DIR/data/order_high.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'
}

# The head is the deployment: a second apply that leaves a channel out
# stops it serving, and the one it carries serves at once.
test_a_redeploy_replaces_what_serves() {
    deploy_one "$FIXTURES_DIR/workflows/simple_log.json" simple-log orders
    cli send orders -d '{}'
    assert_exit_code 0 "$CLI_EXIT"

    deploy_one "$FIXTURES_DIR/workflows/conditional.json" conditional labelled
    cli send labelled -d '{"amount":500}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.order.label' 'High value: $500'

    cli send orders -d '{}'
    assert_ne "$CLI_EXIT" 0 "a channel the new head does not carry must stop serving"
}

run_test "sync process data"                    test_sync_process_data
run_test "sync process with transform"          test_sync_process_with_transform
run_test "sync send to unknown channel fails"   test_sync_unknown_channel_fails
run_test "sync process from file"               test_sync_from_file
run_test "a redeploy replaces what serves"      test_a_redeploy_replaces_what_serves

end_suite

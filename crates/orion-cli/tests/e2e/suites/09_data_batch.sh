#!/usr/bin/env bash
# Suite: Batch Data Processing

begin_suite "Batch Data Processing"

test_batch_from_file() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    cli_quiet engine reload

    cli send --batch -f "$FIXTURES_DIR/data/batch_messages.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_length "$CLI_OUTPUT" '.results' 3

    local ok_count
    ok_count=$(echo "$CLI_OUTPUT" | jq '[.results[] | select(.status == "ok")] | length')
    assert_eq "$ok_count" "3"
}

test_batch_inline() {
    reset_server_state
    cli_quiet engine reload

    cli send --batch -d '{"messages":[{"channel":"test","data":{"x":1}},{"channel":"test","data":{"x":2}}]}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_length "$CLI_OUTPUT" '.results' 2
}

run_test "batch process from file" test_batch_from_file
run_test "batch process inline"    test_batch_inline

end_suite

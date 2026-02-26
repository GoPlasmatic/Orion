#!/usr/bin/env bash
# Suite: Error Handling

begin_suite "Error Handling"

test_invalid_json_body() {
    reset_server_state
    cli rules create -d 'not valid json'
    assert_exit_code 1 "$CLI_EXIT"
}

test_delete_nonexistent_rule() {
    reset_server_state
    cli_quiet rules delete "does-not-exist-12345"
    assert_exit_code 1 "$CLI_EXIT"
}

test_update_nonexistent_rule() {
    reset_server_state
    cli rules update "does-not-exist-12345" -d '{"name":"Ghost"}'
    assert_exit_code 1 "$CLI_EXIT"
}

test_empty_batch_rejected() {
    reset_server_state
    cli send --batch -d '{"messages":[]}'
    assert_exit_code 1 "$CLI_EXIT"
}

run_test "invalid JSON body rejected"     test_invalid_json_body
run_test "delete nonexistent rule errors"  test_delete_nonexistent_rule
run_test "update nonexistent rule errors"  test_update_nonexistent_rule
run_test "empty batch rejected"            test_empty_batch_rejected

end_suite

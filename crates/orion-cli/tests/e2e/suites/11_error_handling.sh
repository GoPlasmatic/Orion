#!/usr/bin/env bash
# Suite: Error Handling

begin_suite "Error Handling"

test_invalid_json_body() {
    reset_server_state
    cli workflows create -d 'not valid json'
    assert_exit_code 1 "$CLI_EXIT"
}

test_delete_nonexistent_workflow() {
    reset_server_state
    cli_quiet workflows delete "does-not-exist-12345"
    assert_exit_code 1 "$CLI_EXIT"
}

test_update_nonexistent_workflow() {
    reset_server_state
    cli workflows update "does-not-exist-12345" -d '{"name":"Ghost"}'
    assert_exit_code 1 "$CLI_EXIT"
}

run_test "invalid JSON body rejected"         test_invalid_json_body
run_test "delete nonexistent workflow errors"  test_delete_nonexistent_workflow
run_test "update nonexistent workflow errors"  test_update_nonexistent_workflow

end_suite

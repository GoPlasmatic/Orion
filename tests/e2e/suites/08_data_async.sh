#!/usr/bin/env bash
# Suite: Asynchronous Data Processing

begin_suite "Asynchronous Data Processing"

test_async_submit_and_poll() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    cli_quiet engine reload

    # submit uses quiet mode (--async-mode doesn't support --output json for submit)
    cli_quiet send orders --async-mode -d '{"order_id":"ASYNC-001","amount":99}'
    assert_exit_code 0 "$CLI_EXIT"
    local job_id="$CLI_OUTPUT"
    assert_matches "$job_id" '^[0-9a-f-]{36}$'

    # poll with json output
    cli jobs wait "$job_id" --timeout 15
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'completed'
}

test_async_with_wait_flag() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    cli_quiet engine reload

    cli send orders --async-mode --wait --timeout 15 -d '{"order_id":"ASYNC-002"}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'completed'
}

test_async_job_get() {
    reset_server_state

    cli_quiet send events --async-mode -d '{"event":"click"}'
    assert_exit_code 0 "$CLI_EXIT"
    local job_id="$CLI_OUTPUT"

    sleep 1

    cli jobs get "$job_id"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_has_key "$CLI_OUTPUT" '.id'
    assert_json_has_key "$CLI_OUTPUT" '.status'
    assert_json_has_key "$CLI_OUTPUT" '.created_at'
}

test_async_quiet_returns_job_id() {
    reset_server_state
    cli_quiet send orders --async-mode -d '{"order_id":"ASYNC-Q"}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_matches "$CLI_OUTPUT" '^[0-9a-f-]{36}$'
}

run_test "async submit and poll job"  test_async_submit_and_poll
run_test "async with --wait flag"     test_async_with_wait_flag
run_test "async job get"              test_async_job_get
run_test "async quiet returns job ID" test_async_quiet_returns_job_id

end_suite

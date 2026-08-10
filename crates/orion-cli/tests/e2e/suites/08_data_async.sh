#!/usr/bin/env bash
# Suite: Asynchronous Data Processing

begin_suite "Asynchronous Data Processing"

test_async_submit_and_poll() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local wf="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf"
    create_channel orders "$wf" async
    cli_quiet engine reload

    # v1.0: reading a trace needs its trace_token (or an admin credential),
    # so submit in json mode and thread the token through.
    cli send orders --async-mode -d '{"order_id":"ASYNC-001","amount":99}'
    assert_exit_code 0 "$CLI_EXIT"
    local trace_id trace_token
    trace_id=$(echo "$CLI_OUTPUT" | jq -r '.trace_id')
    trace_token=$(echo "$CLI_OUTPUT" | jq -r '.trace_token')
    assert_matches "$trace_id" '^[0-9a-f-]{36}$'

    # poll with json output using traces wait
    cli traces wait "$trace_id" --token "$trace_token" --timeout 15
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'completed'
}

test_async_with_wait_flag() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local wf="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf"
    create_channel orders "$wf" async
    cli_quiet engine reload

    cli send orders --async-mode --wait --timeout 15 -d '{"order_id":"ASYNC-002"}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'completed'
}

test_async_trace_get() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local wf="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf"
    create_channel events "$wf" async
    cli_quiet engine reload

    cli send events --async-mode -d '{"event":"click"}'
    assert_exit_code 0 "$CLI_EXIT"
    local trace_id trace_token
    trace_id=$(echo "$CLI_OUTPUT" | jq -r '.trace_id')
    trace_token=$(echo "$CLI_OUTPUT" | jq -r '.trace_token')

    sleep 1

    cli traces get "$trace_id" --token "$trace_token"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_has_key "$CLI_OUTPUT" '.id'
    assert_json_has_key "$CLI_OUTPUT" '.status'
    assert_json_has_key "$CLI_OUTPUT" '.created_at'
}

test_async_quiet_returns_trace_id() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local wf="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf"
    create_channel orders "$wf" async
    cli_quiet engine reload

    cli_quiet send orders --async-mode -d '{"order_id":"ASYNC-Q"}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_matches "$CLI_OUTPUT" '^[0-9a-f-]{36}$'
}

run_test "async submit and poll trace"   test_async_submit_and_poll
run_test "async with --wait flag"        test_async_with_wait_flag
run_test "async trace get"               test_async_trace_get
run_test "async quiet returns trace ID"  test_async_quiet_returns_trace_id

end_suite

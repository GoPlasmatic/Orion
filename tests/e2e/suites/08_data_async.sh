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

# `traces wait` had no e2e coverage at all, and it shares its implementation
# with `send --wait` since the two were unified — so these cover both.

test_traces_wait_emits_json_on_success() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local wf="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf"
    create_channel orders "$wf" async
    cli_quiet engine reload

    cli send orders --async-mode -d '{"order_id":"WAIT-JSON"}'
    assert_exit_code 0 "$CLI_EXIT"
    local trace_id trace_token
    trace_id=$(echo "$CLI_OUTPUT" | jq -r '.trace_id')
    trace_token=$(echo "$CLI_OUTPUT" | jq -r '.trace_token')

    cli traces wait "$trace_id" --token "$trace_token" --timeout 15
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'completed'
}

# The contract a caller parses: under --output json the trace is printed on
# every outcome, and a timeout is exit 2 rather than 1.
#
# `--timeout 0` breaks out on the first poll unless the trace is already
# terminal, so this asserts what holds either way: stdout is a parseable trace
# document, and the exit code is never 1 (which would mean the trace failed).
# Before the fix a timed-out `traces wait` wrote a human TIMEOUT line to stdout
# and no JSON, and a timed-out `send --wait` exited 1.
assert_wait_output_is_parseable_json() {
    local label="$1"
    if ! echo "$CLI_OUTPUT" | jq -e '.' >/dev/null 2>&1; then
        echo "ASSERTION FAILED: $label: stdout must be parseable JSON, got: $CLI_OUTPUT" >&2
        return 1
    fi
    assert_ne "$CLI_EXIT" "1" "$label: a timeout must not report as a failed trace" || return 1
}

test_wait_timeout_still_emits_json_and_never_exits_one() {
    reset_server_state
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local wf="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf"
    create_channel orders "$wf" async
    cli_quiet engine reload

    cli send orders --async-mode -d '{"order_id":"WAIT-TO"}'
    local trace_id trace_token
    trace_id=$(echo "$CLI_OUTPUT" | jq -r '.trace_id')
    trace_token=$(echo "$CLI_OUTPUT" | jq -r '.trace_token')

    cli traces wait "$trace_id" --token "$trace_token" --timeout 0
    assert_wait_output_is_parseable_json "traces wait --timeout 0" || return 1

    cli send orders --async-mode --wait --timeout 0 -d '{"order_id":"WAIT-TO-2"}'
    assert_wait_output_is_parseable_json "send --wait --timeout 0" || return 1
}

run_test "async submit and poll trace"   test_async_submit_and_poll
run_test "async with --wait flag"        test_async_with_wait_flag
run_test "async trace get"               test_async_trace_get
run_test "async quiet returns trace ID"  test_async_quiet_returns_trace_id
run_test "traces wait emits JSON on success" test_traces_wait_emits_json_on_success
run_test "wait timeout emits JSON, never exit 1" test_wait_timeout_still_emits_json_and_never_exits_one

end_suite

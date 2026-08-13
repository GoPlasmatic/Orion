#!/usr/bin/env bash
# Suite: Engine Control

begin_suite "Engine Control"

test_engine_reload_updates_count() {
    reset_server_state

    cli engine status
    assert_json_eq "$CLI_OUTPUT" '.active_workflows' '0'

    # v1.0: the engine loads workflows through the channels that name them.
    cli_quiet workflows create -d '{"name":"W1","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"1"}}}]}'
    local wf1="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf1"
    create_channel w1 "$wf1"
    cli_quiet workflows create -d '{"name":"W2","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"2"}}}]}'
    local wf2="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf2"
    create_channel w2 "$wf2"

    cli_quiet engine reload
    assert_exit_code 0 "$CLI_EXIT"

    cli engine status
    assert_json_eq "$CLI_OUTPUT" '.active_workflows' '2'
}

test_engine_channels_reported() {
    reset_server_state
    cli_quiet workflows create -d '{"name":"Alpha Workflow","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"a"}}}]}'
    local wf_a="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf_a"
    create_channel alpha "$wf_a"
    cli_quiet workflows create -d '{"name":"Beta Workflow","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"b"}}}]}'
    local wf_b="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf_b"
    create_channel beta "$wf_b"
    cli_quiet engine reload

    cli engine status
    local channels
    channels=$(echo "$CLI_OUTPUT" | jq -r '.channels | sort | join(",")')
    assert_contains "$channels" "alpha"
    assert_contains "$channels" "beta"
}

run_test "engine reload updates workflow count" test_engine_reload_updates_count
run_test "engine reports active channels"       test_engine_channels_reported

end_suite

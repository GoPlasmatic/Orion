#!/usr/bin/env bash
# Suite: Engine Control

begin_suite "Engine Control"

test_engine_reload_updates_count() {
    reset_server_state

    cli engine status
    assert_json_eq "$CLI_OUTPUT" '.rules_count' '0'

    cli_quiet rules create -d '{"name":"R1","channel":"c1","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"1"}}}]}'
    cli_quiet rules create -d '{"name":"R2","channel":"c2","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"2"}}}]}'

    cli engine status
    assert_json_eq "$CLI_OUTPUT" '.active_rules' '2'

    cli_quiet engine reload
    assert_exit_code 0 "$CLI_EXIT"

    cli engine status
    assert_json_eq "$CLI_OUTPUT" '.active_rules' '2'
}

test_engine_channels_reported() {
    reset_server_state
    cli_quiet rules create -d '{"name":"Alpha Rule","channel":"alpha","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"a"}}}]}'
    cli_quiet rules create -d '{"name":"Beta Rule","channel":"beta","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"b"}}}]}'

    cli engine status
    local channels
    channels=$(echo "$CLI_OUTPUT" | jq -r '.channels | sort | join(",")')
    assert_contains "$channels" "alpha"
    assert_contains "$channels" "beta"
}

run_test "engine reload updates rule count" test_engine_reload_updates_count
run_test "engine reports active channels"   test_engine_channels_reported

end_suite

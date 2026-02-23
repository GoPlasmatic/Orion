#!/usr/bin/env bash
# Suite: Rule Status Lifecycle

begin_suite "Rule Status Lifecycle"

test_pause_rule() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    local rule_id="$CLI_OUTPUT"

    cli_quiet rules pause "$rule_id"
    assert_exit_code 0 "$CLI_EXIT"

    cli rules get "$rule_id"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'paused'
}

test_archive_rule() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    local rule_id="$CLI_OUTPUT"

    cli_quiet rules archive "$rule_id"
    assert_exit_code 0 "$CLI_EXIT"

    cli rules get "$rule_id"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'archived'
}

test_reactivate_rule() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    local rule_id="$CLI_OUTPUT"

    cli_quiet rules pause "$rule_id"
    cli_quiet rules activate "$rule_id"
    assert_exit_code 0 "$CLI_EXIT"

    cli rules get "$rule_id"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'active'
}

test_status_filter_list() {
    reset_server_state
    cli_quiet rules create -d '{"name":"Active Rule","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"a"}}}]}'
    local active_id="$CLI_OUTPUT"

    cli_quiet rules create -d '{"name":"Paused Rule","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"p"}}}]}'
    local paused_id="$CLI_OUTPUT"
    cli_quiet rules pause "$paused_id"

    cli rules list --status active
    assert_json_length "$CLI_OUTPUT" '.data' 1
    assert_json_eq "$CLI_OUTPUT" '.data[0].name' 'Active Rule'

    cli rules list --status paused
    assert_json_length "$CLI_OUTPUT" '.data' 1
    assert_json_eq "$CLI_OUTPUT" '.data[0].name' 'Paused Rule'
}

run_test "pause rule"                    test_pause_rule
run_test "archive rule"                  test_archive_rule
run_test "reactivate paused rule"        test_reactivate_rule
run_test "list rules filtered by status" test_status_filter_list

end_suite

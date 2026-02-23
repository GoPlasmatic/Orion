#!/usr/bin/env bash
# Suite: Rules CRUD

begin_suite "Rules CRUD"

test_create_rule_from_file() {
    reset_server_state
    cli rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'E2E Simple Log'
    assert_json_eq "$CLI_OUTPUT" '.data.channel' 'orders'
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'active'
    assert_json_eq "$CLI_OUTPUT" '.data.version' '1'
    assert_json_has_key "$CLI_OUTPUT" '.data.id'
}

test_create_rule_inline() {
    reset_server_state
    cli rules create -d '{"name":"Inline Rule","channel":"test","condition":true,"tasks":[{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"inline"}}}]}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'Inline Rule'
    assert_json_eq "$CLI_OUTPUT" '.data.channel' 'test'
}

test_create_rule_quiet_returns_id() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_matches "$CLI_OUTPUT" '^[0-9a-f-]{36}$'
}

test_get_rule() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    local rule_id="$CLI_OUTPUT"

    cli rules get "$rule_id"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'E2E Simple Log'
    assert_json_eq "$CLI_OUTPUT" '.data.id' "$rule_id"
    assert_json_has_key "$CLI_OUTPUT" '.version_count'
}

test_list_rules() {
    reset_server_state
    cli_quiet rules create -d '{"name":"Rule 1","channel":"ch1","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"1"}}}]}'
    cli_quiet rules create -d '{"name":"Rule 2","channel":"ch2","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"2"}}}]}'

    cli rules list
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_length "$CLI_OUTPUT" '.data' 2
}

test_list_rules_filter_by_channel() {
    reset_server_state
    cli_quiet rules create -d '{"name":"Rule A","channel":"alpha","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"a"}}}]}'
    cli_quiet rules create -d '{"name":"Rule B","channel":"beta","condition":true,"tasks":[{"id":"t1","name":"L","function":{"name":"log","input":{"message":"b"}}}]}'

    cli rules list --channel alpha
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_length "$CLI_OUTPUT" '.data' 1
    assert_json_eq "$CLI_OUTPUT" '.data[0].name' 'Rule A'
}

test_update_rule() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    local rule_id="$CLI_OUTPUT"

    cli rules update "$rule_id" -d '{"name":"Updated Name","priority":99}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'Updated Name'
    assert_json_eq "$CLI_OUTPUT" '.data.priority' '99'
    assert_json_eq "$CLI_OUTPUT" '.data.version' '2'
}

test_delete_rule() {
    reset_server_state
    cli_quiet rules create -f "$FIXTURES_DIR/rules/simple_log.json"
    local rule_id="$CLI_OUTPUT"

    cli_quiet rules delete "$rule_id"
    assert_exit_code 0 "$CLI_EXIT"

    cli rules get "$rule_id"
    assert_exit_code 1 "$CLI_EXIT"
}

test_get_nonexistent_rule() {
    reset_server_state
    cli rules get "nonexistent-id-00000000"
    assert_exit_code 1 "$CLI_EXIT"
}

run_test "create rule from file"              test_create_rule_from_file
run_test "create rule inline"                 test_create_rule_inline
run_test "create rule quiet returns UUID"     test_create_rule_quiet_returns_id
run_test "get rule by id"                     test_get_rule
run_test "list rules"                         test_list_rules
run_test "list rules filter by channel"       test_list_rules_filter_by_channel
run_test "update rule increments version"     test_update_rule
run_test "delete rule"                        test_delete_rule
run_test "get nonexistent rule returns error" test_get_nonexistent_rule

end_suite

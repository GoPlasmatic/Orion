#!/usr/bin/env bash
# Suite: Connectors CRUD

begin_suite "Connectors CRUD"

test_create_connector() {
    reset_server_state
    cli connectors create -f "$FIXTURES_DIR/connectors/http_connector.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'e2e-test-http'
    assert_json_eq "$CLI_OUTPUT" '.data.connector_type' 'http'
    assert_json_has_key "$CLI_OUTPUT" '.data.id'
}

test_get_connector_details() {
    reset_server_state
    cli_quiet connectors create -f "$FIXTURES_DIR/connectors/http_connector.json"
    local conn_id="$CLI_OUTPUT"

    cli connectors get "$conn_id"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'e2e-test-http'
    assert_json_eq "$CLI_OUTPUT" '.data.connector_type' 'http'
    assert_json_eq "$CLI_OUTPUT" '.data.enabled' 'true'
    assert_json_has_key "$CLI_OUTPUT" '.data.config_json'
    assert_json_has_key "$CLI_OUTPUT" '.data.created_at'
}

test_list_connectors() {
    reset_server_state
    cli_quiet connectors create -f "$FIXTURES_DIR/connectors/http_connector.json"

    cli connectors list
    assert_exit_code 0 "$CLI_EXIT"
    # Should have at least 1 (our connector) + the internal __data_api__
    local count
    count=$(echo "$CLI_OUTPUT" | jq '.data | length' 2>/dev/null)
    assert_ne "$count" "0" "Expected at least 1 connector"
}

test_update_connector() {
    reset_server_state
    cli_quiet connectors create -f "$FIXTURES_DIR/connectors/http_connector.json"
    local conn_id="$CLI_OUTPUT"

    cli connectors update "$conn_id" -d '{"name":"updated-http","connector_type":"http","config":{"type":"http","url":"https://example.com/updated","method":"GET"}}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'updated-http'
}

test_delete_connector() {
    reset_server_state
    cli_quiet connectors create -f "$FIXTURES_DIR/connectors/http_connector.json"
    local conn_id="$CLI_OUTPUT"

    cli_quiet connectors delete "$conn_id"
    assert_exit_code 0 "$CLI_EXIT"

    cli connectors get "$conn_id"
    assert_exit_code 1 "$CLI_EXIT"
}

run_test "create connector"              test_create_connector
run_test "get connector details"         test_get_connector_details
run_test "list connectors"               test_list_connectors
run_test "update connector"              test_update_connector
run_test "delete connector"              test_delete_connector

end_suite

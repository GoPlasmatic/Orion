#!/usr/bin/env bash
# Suite: Rules Import & Export

begin_suite "Rules Import & Export"

test_import_rules() {
    reset_server_state
    cli_raw rules import -f "$FIXTURES_DIR/rules/import_batch.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "Imported"

    cli rules list
    assert_json_length "$CLI_OUTPUT" '.data' 3
}

test_import_dry_run() {
    reset_server_state
    cli_raw rules import -f "$FIXTURES_DIR/rules/import_batch.json" --dry-run
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "Would import 3"

    # Verify nothing was actually imported
    cli rules list
    assert_json_length "$CLI_OUTPUT" '.data' 0
}

test_export_rules() {
    reset_server_state
    cli_raw rules import -f "$FIXTURES_DIR/rules/import_batch.json"

    cli_raw rules export
    assert_exit_code 0 "$CLI_EXIT"

    local export_count
    export_count=$(echo "$CLI_OUTPUT" | jq 'if type == "array" then length else .data | length end' 2>/dev/null)
    assert_eq "$export_count" "3"
}

test_export_with_channel_filter() {
    reset_server_state
    cli_raw rules import -f "$FIXTURES_DIR/rules/import_batch.json"

    cli_raw rules export --channel alpha
    assert_exit_code 0 "$CLI_EXIT"

    local count
    count=$(echo "$CLI_OUTPUT" | jq 'if type == "array" then length else .data | length end' 2>/dev/null)
    assert_eq "$count" "2"
}

run_test "import rules from file"     test_import_rules
run_test "import dry-run"             test_import_dry_run
run_test "export rules"               test_export_rules
run_test "export with channel filter" test_export_with_channel_filter

end_suite

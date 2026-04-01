#!/usr/bin/env bash
# Suite: Workflows Import & Export

begin_suite "Workflows Import & Export"

test_import_workflows() {
    reset_server_state
    cli_raw workflows import -f "$FIXTURES_DIR/workflows/import_batch.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "Imported"

    cli workflows list
    assert_json_length "$CLI_OUTPUT" '.data' 3
}

test_import_dry_run() {
    reset_server_state
    cli_raw workflows import -f "$FIXTURES_DIR/workflows/import_batch.json" --dry-run
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "Would import 3"

    # Verify nothing was actually imported
    cli workflows list
    assert_json_length "$CLI_OUTPUT" '.data' 0
}

test_export_workflows() {
    reset_server_state
    cli_raw workflows import -f "$FIXTURES_DIR/workflows/import_batch.json"

    cli_raw workflows export
    assert_exit_code 0 "$CLI_EXIT"

    local export_count
    export_count=$(echo "$CLI_OUTPUT" | jq 'if type == "array" then length else .data | length end' 2>/dev/null)
    assert_eq "$export_count" "3"
}

run_test "import workflows from file" test_import_workflows
run_test "import dry-run"             test_import_dry_run
run_test "export workflows"           test_export_workflows

end_suite

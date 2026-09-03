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
    assert_contains "$CLI_OUTPUT" "Would import: 3"

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

test_diff_round_trips_its_own_export() {
    # The contract that makes `diff` mean anything: export the estate, diff the
    # file straight back, and nothing has drifted. It compares the fields an
    # import writes — a mismatch on `version`, `status` or `created_at` would
    # report every workflow modified and the command could never say "clean".
    reset_server_state
    cli_raw workflows import -f "$FIXTURES_DIR/workflows/import_batch.json"

    local export_file
    export_file=$(mktemp "${TMPDIR:-/tmp}/e2e-export-XXXXXX.json")
    cli_raw workflows export
    echo "$CLI_OUTPUT" > "$export_file"

    cli_raw workflows diff -f "$export_file"
    rm -f "$export_file"
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "0 new, 0 modified, 0 deleted"
}

test_diff_reports_drift_nonzero() {
    reset_server_state
    cli_raw workflows import -f "$FIXTURES_DIR/workflows/import_batch.json"

    local export_file
    export_file=$(mktemp "${TMPDIR:-/tmp}/e2e-drift-XXXXXX.json")
    cli_raw workflows export
    # Change one workflow's content, and drop the hash so the comparison falls
    # back to the fields — the hand-authored-file path.
    echo "$CLI_OUTPUT" \
        | jq '(.[0].description) = "drifted" | map(del(.content_hash))' > "$export_file"

    cli_raw workflows diff -f "$export_file"
    rm -f "$export_file"
    assert_exit_code 1 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "1 modified"
}

# The no-hash fallback on its hardest input: a hand-authored file that omits
# every field with a default, diffed against the full row the server stored
# from it. Nothing the file leaves out is a difference, so this must read as
# clean — it is the path `workflows diff` takes for any file a human wrote,
# and the one where a projection that disagreed with the server's would show.
test_diff_of_a_minimal_authored_file_is_clean() {
    reset_server_state
    cli_raw workflows import -f "$FIXTURES_DIR/workflows/minimal_authored.json"
    assert_exit_code 0 "$CLI_EXIT"

    cli_raw workflows diff -f "$FIXTURES_DIR/workflows/minimal_authored.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "0 new, 0 modified, 0 deleted"
}

run_test "import workflows from file" test_import_workflows
run_test "import dry-run"             test_import_dry_run
run_test "export workflows"           test_export_workflows
run_test "diff round-trips its own export" test_diff_round_trips_its_own_export
run_test "diff exits non-zero on drift"    test_diff_reports_drift_nonzero
run_test "diff of a minimal authored file is clean" test_diff_of_a_minimal_authored_file_is_clean

end_suite

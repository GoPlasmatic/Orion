#!/usr/bin/env bash
# Suite: Full End-to-End Lifecycle

begin_suite "Full End-to-End Lifecycle"

test_complete_workflow() {
    reset_server_state

    # 1. Verify server is healthy
    cli health
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'

    # 2. Import a set of workflows
    cli_raw workflows import -f "$FIXTURES_DIR/workflows/import_batch.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "Imported"

    # 3. Verify workflows are listed
    cli workflows list
    assert_json_length "$CLI_OUTPUT" '.data' 3

    # 4. Activate all imported workflows
    local workflow_ids
    workflow_ids=$(echo "$CLI_OUTPUT" | jq -r '.data[].workflow_id')
    while IFS= read -r wid; do
        [[ -z "$wid" ]] && continue
        cli_quiet workflows activate "$wid"
    done <<< "$workflow_ids"

    # 5. Add a conditional workflow and activate it
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/conditional.json"
    local cond_workflow_id="$CLI_OUTPUT"
    cli_quiet workflows activate "$cond_workflow_id"

    # 6. Dry-run test the conditional workflow
    cli workflows test "$cond_workflow_id" -d '{"amount": 500}'
    assert_json_eq "$CLI_OUTPUT" '.matched' 'true'
    assert_json_eq "$CLI_OUTPUT" '.output.order.label' 'High value: $500'

    # 7. Archive one of the imported workflows, verify it's archived
    local first_workflow_id
    first_workflow_id=$(echo "$CLI_OUTPUT" | jq -r '.data[0].workflow_id' 2>/dev/null || true)
    # Re-list to get a workflow id
    cli workflows list
    first_workflow_id=$(echo "$CLI_OUTPUT" | jq -r '.data[0].workflow_id')
    cli_quiet workflows archive "$first_workflow_id"

    cli workflows get "$first_workflow_id"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'archived'

    # 8. Send sync data
    cli_quiet engine reload
    cli send orders -d '{"amount":300,"product":"Lifecycle Test"}'
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'
    assert_json_eq "$CLI_OUTPUT" '.data.order.label' 'High value: $300'

    # 9. Send async data and wait
    cli send orders --async-mode --wait --timeout 15 -d '{"amount":150}'
    assert_exit_code 0 "$CLI_EXIT"

    # 10. Clean up: delete all workflows
    clean_all_workflows

    cli workflows list
    assert_json_length "$CLI_OUTPUT" '.data' 0

    # 11. Reload engine with empty state
    cli_quiet engine reload
    cli engine status
    assert_json_eq "$CLI_OUTPUT" '.active_workflows' '0'
}

run_test "complete end-to-end lifecycle" test_complete_workflow

end_suite

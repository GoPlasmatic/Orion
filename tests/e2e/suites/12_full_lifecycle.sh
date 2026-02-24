#!/usr/bin/env bash
# Suite: Full End-to-End Lifecycle

begin_suite "Full End-to-End Lifecycle"

test_complete_workflow() {
    reset_server_state

    # 1. Verify server is healthy
    cli health
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'

    # 2. Import a set of rules
    cli_raw rules import -f "$FIXTURES_DIR/rules/import_batch.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "Imported"

    # 3. Verify rules are listed
    cli rules list
    assert_json_length "$CLI_OUTPUT" '.data' 3

    # 4. Add a conditional rule
    cli_quiet rules create -f "$FIXTURES_DIR/rules/conditional.json"
    local cond_rule_id="$CLI_OUTPUT"

    # 5. Dry-run test the conditional rule
    cli rules test "$cond_rule_id" -d '{"amount": 500}'
    assert_json_eq "$CLI_OUTPUT" '.matched' 'true'
    assert_json_eq "$CLI_OUTPUT" '.output.order.label' 'High value: $500'

    # 6. Pause one of the imported rules, verify it's paused
    cli rules list --channel alpha
    local first_alpha_id
    first_alpha_id=$(echo "$CLI_OUTPUT" | jq -r '.data[0].id')
    cli_quiet rules pause "$first_alpha_id"

    cli rules get "$first_alpha_id"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'paused'

    # 7. Send sync data
    cli_quiet engine reload
    cli send orders -d '{"amount":300,"product":"Lifecycle Test"}'
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'
    assert_json_eq "$CLI_OUTPUT" '.data.order.label' 'High value: $300'

    # 8. Send async data and wait
    cli send orders --async-mode --wait --timeout 15 -d '{"amount":150}'
    assert_exit_code 0 "$CLI_EXIT"

    # 9. Batch process
    cli send --batch -f "$FIXTURES_DIR/data/batch_messages.json"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_length "$CLI_OUTPUT" '.results' 3

    # 10. Clean up: delete all rules
    clean_all_rules

    cli rules list
    assert_json_length "$CLI_OUTPUT" '.data' 0

    # 11. Reload engine with empty state
    cli_quiet engine reload
    cli engine status
    assert_json_eq "$CLI_OUTPUT" '.rules_count' '0'
}

run_test "complete end-to-end lifecycle" test_complete_workflow

end_suite

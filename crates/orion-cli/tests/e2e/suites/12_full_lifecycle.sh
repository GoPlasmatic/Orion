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

    # 4. Activate all imported rules
    local rule_ids
    rule_ids=$(echo "$CLI_OUTPUT" | jq -r '.data[].rule_id')
    while IFS= read -r rid; do
        [[ -z "$rid" ]] && continue
        cli_quiet rules activate "$rid"
    done <<< "$rule_ids"

    # 5. Add a conditional rule and activate it
    cli_quiet rules create -f "$FIXTURES_DIR/rules/conditional.json"
    local cond_rule_id="$CLI_OUTPUT"
    cli_quiet rules activate "$cond_rule_id"

    # 6. Dry-run test the conditional rule
    cli rules test "$cond_rule_id" -d '{"amount": 500}'
    assert_json_eq "$CLI_OUTPUT" '.matched' 'true'
    assert_json_eq "$CLI_OUTPUT" '.output.order.label' 'High value: $500'

    # 7. Archive one of the imported rules, verify it's archived
    cli rules list --channel alpha
    local first_alpha_id
    first_alpha_id=$(echo "$CLI_OUTPUT" | jq -r '.data[0].rule_id')
    cli_quiet rules archive "$first_alpha_id"

    cli rules get "$first_alpha_id"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'archived'

    # 8. Send sync data
    cli_quiet engine reload
    cli send orders -d '{"amount":300,"product":"Lifecycle Test"}'
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'
    assert_json_eq "$CLI_OUTPUT" '.data.order.label' 'High value: $300'

    # 9. Send async data and wait
    cli send orders --async-mode --wait --timeout 15 -d '{"amount":150}'
    assert_exit_code 0 "$CLI_EXIT"

    # 10. Clean up: delete all rules
    clean_all_rules

    cli rules list
    assert_json_length "$CLI_OUTPUT" '.data' 0

    # 11. Reload engine with empty state
    cli_quiet engine reload
    cli engine status
    assert_json_eq "$CLI_OUTPUT" '.active_rules' '0'
}

run_test "complete end-to-end lifecycle" test_complete_workflow

end_suite

#!/usr/bin/env bash
# Suite: Error Handling
#
# Failure paths and their exit codes: a malformed payload, definitions the
# compiler refuses before anything reaches a server, and an apply of
# something that is not a deployment. A refused deploy leaves the head
# serving what it served.

begin_suite "Error Handling"

test_invalid_json_payload_rejected() {
    deploy_one "$FIXTURES_DIR/workflows/simple_log.json" simple-log orders
    cli send orders -d 'not valid json'
    assert_exit_code 1 "$CLI_EXIT"
}

test_a_channel_naming_a_missing_workflow_is_refused() {
    local dir
    dir=$(new_set)
    set_channel "$dir" orphan no-such-workflow
    deploy_set "$dir"
    assert_ne "$CLI_EXIT" 0 "a channel bound to a workflow the set lacks must not compile"
    assert_contains "$CLI_STDERR" "no-such-workflow"
}

test_an_unknown_function_is_refused() {
    local dir
    dir=$(new_set)
    set_workflow "$dir" bad-fn '{"name":"Bad","condition":true,"tasks":[
      {"id":"t1","name":"t1","function":{"name":"no_such_function","input":{}}}]}'
    set_channel "$dir" bad-fn bad-fn
    deploy_set "$dir"
    assert_ne "$CLI_EXIT" 0 "a workflow calling an unknown function must not compile"
    assert_contains "$CLI_STDERR" "no_such_function"
}

# A deployment carries references only: a credential written into a
# connector is refused at compile, with the fix named.
test_an_inline_credential_is_refused() {
    local dir
    dir=$(new_set)
    cat > "$dir/connector.json" <<'JSON'
{ "name": "inline-token", "connector_type": "http",
  "config": { "type": "http", "url": "https://api.example.com", "method": "POST",
              "auth": { "type": "bearer", "token": "an-inline-token" } } }
JSON
    deploy_set "$dir"
    assert_ne "$CLI_EXIT" 0 "an inline credential must not compile"
    assert_contains "$CLI_STDERR" "secret_locators"
    assert_not_contains "$(ls "$dir.dist" 2>/dev/null || true)" "deployment.json" \
        "a refused compile must write nothing"
}

test_apply_of_a_directory_that_is_not_a_deployment_fails() {
    cli deployments apply -d "$TEST_TMPDIR"
    assert_exit_code 1 "$CLI_EXIT"
    assert_contains "$CLI_STDERR" "deployment.json"
}

test_a_refused_deploy_leaves_the_head_serving() {
    deploy_one "$FIXTURES_DIR/workflows/conditional.json" conditional orders

    local dir
    dir=$(new_set broken)
    set_channel "$dir" orphan no-such-workflow
    deploy_set "$dir"
    assert_ne "$CLI_EXIT" 0

    cli send orders -d '{"amount":300}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.order.label' 'High value: $300'
}

run_test "invalid JSON payload rejected"                    test_invalid_json_payload_rejected
run_test "a channel naming a missing workflow is refused"   test_a_channel_naming_a_missing_workflow_is_refused
run_test "an unknown function is refused"                   test_an_unknown_function_is_refused
run_test "an inline credential is refused"                  test_an_inline_credential_is_refused
run_test "apply of a non-deployment directory fails"        test_apply_of_a_directory_that_is_not_a_deployment_fails
run_test "a refused deploy leaves the head serving"         test_a_refused_deploy_leaves_the_head_serving

end_suite

#!/usr/bin/env bash
# Suite: Vars and Secrets
#
# The one layer that can exercise the whole config-file → boot → serve path.
# Every in-process test builds an `AppConfig` in Rust, so nothing below the
# `[vars]` / `[secrets]` structs is covered there: `${VAR}` substitution into a
# var, an `env://` reference in `[secrets]` resolved during startup, and both
# reaching a workflow on a server that is a real process.
#
# `start_server` in helpers.sh declares them:
#   [vars]     topic_prefix = "${E2E_VAR_TOPIC_PREFIX}"   → "eu-west"
#   [secrets]  partner_hmac = "env://ORION_SECRET_E2E_PARTNER_HMAC"

begin_suite "Vars and Secrets"

VARS_WF='{"name":"Reads a var","condition":true,"tasks":[
  {"id":"topic","name":"Topic","function":{"name":"map","input":{"mappings":[
    {"path":"data.topic","logic":{"cat":[{"var":"metadata.vars.topic_prefix"},".order.placed"]}},
    {"path":"data.retries","logic":{"var":"metadata.vars.max_retries"}}
  ]}}}]}'

SECRET_WF='{"name":"Signs with the store","condition":true,"tasks":[
  {"id":"sign","name":"Sign","function":{"name":"crypto","input":{
    "op":"hmac","algorithm":"sha256","key":{"secret":"partner_hmac"},
    "data":"order-4711","output":"data.mac"}}}]}'

# A `${VAR}` in the config file becomes a var, and the type survives TOML →
# JSON: `max_retries` is the number 3, not the string "3".
test_a_declared_var_reaches_a_workflow() {
    deploy_one "$VARS_WF" reads-a-var vars-ch

    cli send vars-ch -d '{}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.topic' 'eu-west.order.placed'
    assert_json_eq "$CLI_OUTPUT" '.data.retries' '3'
}

# Envelope mode merges the caller's own metadata, so the stamp has to land on
# top of it — otherwise a request names the topic prefix its own run uses.
test_a_caller_cannot_forge_a_var() {
    deploy_one "$VARS_WF" reads-a-var forge-ch

    local response
    response=$(curl -sf -X POST "$ORION_URL/api/v2/data/forge-ch" \
        -H 'content-type: application/json' \
        -d '{"data":{},"metadata":{"vars":{"topic_prefix":"attacker"}}}')
    assert_json_eq "$response" '.data.topic' 'eu-west.order.placed'
    assert_not_contains "$response" "attacker" "a caller-supplied var must be overwritten"
}

# The reference in `[secrets]` resolved during startup, and the workflow
# reaches its value by name.
test_a_declared_secret_signs() {
    deploy_one "$SECRET_WF" signs sign-ch

    cli send sign-ch -d '{}'
    assert_exit_code 0 "$CLI_EXIT"
    # The MAC of "order-4711" under the declared key. A hard-coded expectation
    # rather than "is non-empty": it is the only assertion that proves the
    # store resolved to the *right* value rather than merely to something.
    assert_json_eq "$CLI_OUTPUT" '.data.mac' \
        "$(printf 'order-4711' | openssl dgst -sha256 -hmac "$ORION_SECRET_E2E_PARTNER_HMAC" -hex | awk '{print $NF}')"
}

# The guarantee, from outside the process: the value is in neither the
# response nor any trace the server hands back.
test_a_secret_reaches_no_response_or_trace() {
    deploy_one "$SECRET_WF" signs leak-ch sync '{"tracing":{"task_details":true}}'

    cli send leak-ch -d '{}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_not_contains "$CLI_OUTPUT" "$ORION_SECRET_E2E_PARTNER_HMAC" \
        "the secret reached the response body"

    local traces
    traces=$(curl -sf "$ORION_URL/api/v2/admin/traces?limit=50")
    assert_not_contains "$traces" "$ORION_SECRET_E2E_PARTNER_HMAC" \
        "the secret reached a trace listing"

    local trace_id detail
    trace_id=$(echo "$traces" | jq -r '.data[0].id')
    detail=$(curl -sf "$ORION_URL/api/v2/admin/traces/$trace_id")
    assert_not_contains "$detail" "$ORION_SECRET_E2E_PARTNER_HMAC" \
        "the secret reached a per-step trace"

    # And the definition the node stored holds the name, which is what makes
    # the deployment promotable between instances unchanged.
    local digest stored
    digest=$(jq -r '.members[] | select(.kind == "workflow") | .definition' \
        "$TEST_TMPDIR/signs.dist/deployment.json")
    stored=$(curl -sf "$ORION_URL/api/v2/admin/definitions/$digest")
    assert_contains "$stored" "partner_hmac"
    assert_not_contains "$stored" "$ORION_SECRET_E2E_PARTNER_HMAC" \
        "the stored definition must hold the name, not the value"
}

# A name this instance does not declare is refused when the node stores the
# definition, so the deployment never commits and nothing serves with the key
# resolving to null. `compile` cannot know the target's [secrets] and only
# notes the requirement.
test_an_undeclared_secret_is_refused() {
    local dir
    dir=$(new_set)
    set_workflow "$dir" undeclared '{"name":"Undeclared","condition":true,"tasks":[
      {"id":"sign","name":"Sign","function":{"name":"crypto","input":{
        "op":"hmac","algorithm":"sha256","key":{"secret":"no_such_secret"},
        "data":"x","output":"data.mac"}}}]}'
    set_channel "$dir" unknown-ch undeclared
    deploy_set "$dir"
    assert_ne "$CLI_EXIT" 0 "a deployment naming an undeclared secret must not apply"

    # The node's own refusal, with the field and the name.
    local response
    response=$(curl -s -X POST "$ORION_URL/api/v2/admin/definitions" \
        -H 'content-type: application/json' \
        -d @"$(grep -l '"workflow"' "$dir.dist"/definitions/*.json)")
    assert_json_eq "$response" '.error.code' 'VALIDATION_ERROR'
    assert_contains "$response" "no_such_secret"

    cli send unknown-ch -d '{}'
    assert_ne "$CLI_EXIT" 0 "a workflow naming an undeclared secret must not serve"
}

# The authoring-time half: a reference in a field that resolves none is
# refused at compile, not deployed and then requested as a literal URL.
test_a_stray_reference_is_refused_at_compile() {
    local dir
    dir=$(new_set)
    set_workflow "$dir" stray '{"name":"Stray","condition":true,"tasks":[
      {"id":"call","name":"Call","function":{"name":"http_call","input":{
        "connector":"crm","path":"env://API_BASE"}}}]}'
    deploy_set "$dir"
    assert_ne "$CLI_EXIT" 0 "a reference in a field that resolves none must be refused"
    assert_contains "$CLI_STDERR" "env://API_BASE"
}

run_test "a declared var reaches a workflow"        test_a_declared_var_reaches_a_workflow
run_test "a caller cannot forge a var"              test_a_caller_cannot_forge_a_var
run_test "a declared secret signs"                  test_a_declared_secret_signs
run_test "a secret reaches no response or trace"    test_a_secret_reaches_no_response_or_trace
run_test "an undeclared secret is refused"          test_an_undeclared_secret_is_refused
run_test "a stray reference is refused at compile"  test_a_stray_reference_is_refused_at_compile

end_suite

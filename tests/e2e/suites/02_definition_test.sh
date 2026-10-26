#!/usr/bin/env bash
# Suite: Definition test runs
#
# `POST /api/v2/admin/definitions/test` runs a bare workflow definition once
# on this node's engine without deploying it — the server-side dry run. The
# definition is the one `orion-cli compile` wrote for the fixture, so the
# run sees exactly what a deployment of it would serve.

begin_suite "Definition test runs"

# The compiled definition of the conditional fixture, as `compile` wrote it.
_conditional_definition() {
    local dir
    dir=$(new_set)
    set_workflow "$dir" conditional "$(cat "$FIXTURES_DIR/workflows/conditional.json")"
    compile_set "$dir" >/dev/null
    definition_of "$dir.dist" workflow conditional | jq -c '.definition'
}

test_definition_runs_its_tasks() {
    test_definition "$(_conditional_definition)" '{"amount": 250}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.matched' 'true'
    assert_json_eq "$CLI_OUTPUT" '.output.order.label' 'High value: $250'
    assert_json_length "$CLI_OUTPUT" '.errors' 0
}

test_a_task_condition_skips_the_task() {
    # The transform's condition is amount > 100: parse still runs, the label
    # is never added, and the trace says the step was skipped.
    test_definition "$(_conditional_definition)" '{"amount": 50}'
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.matched' 'true'
    assert_json_eq "$CLI_OUTPUT" '.output.order.amount' '50'
    assert_json_eq "$CLI_OUTPUT" '.output.order.label' 'null'
    assert_json_eq "$CLI_OUTPUT" '.trace.steps[] | select(.task_id == "transform") | .result' 'skipped'
}

test_a_definition_that_would_not_serve_is_refused() {
    local response
    response=$(curl -s -X POST "$ORION_URL/api/v2/admin/definitions/test" \
        -H 'content-type: application/json' \
        -d '{"definition": {"name": "Bad", "tasks": [{"id": "t", "function": {"name": "no_such_function", "input": {}}}]}, "data": {}}')
    assert_json_eq "$response" '.error.code' 'VALIDATION_ERROR'
    assert_contains "$response" "no_such_function"
}

test_a_test_run_deploys_nothing() {
    undeploy_all
    test_definition "$(_conditional_definition)" '{"amount": 250}'
    assert_json_eq "$CLI_OUTPUT" '.matched' 'true'
    cli engine status
    assert_json_eq "$CLI_OUTPUT" '.workflows_count' '0'
}

run_test "a definition test runs its tasks"               test_definition_runs_its_tasks
run_test "a task condition skips the task"                test_a_task_condition_skips_the_task
run_test "a definition that would not serve is refused"   test_a_definition_that_would_not_serve_is_refused
run_test "a test run deploys nothing"                     test_a_test_run_deploys_nothing

end_suite

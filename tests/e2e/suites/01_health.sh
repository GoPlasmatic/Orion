#!/usr/bin/env bash
# Suite: Health & Engine Status
#
# Health and the engine's view of what it serves: nothing, then one
# deployed definition set.

begin_suite "Health & Engine Status"

test_health_check() {
    undeploy_all
    cli health
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.status' 'ok'
    assert_json_has_key "$CLI_OUTPUT" '.version'
    assert_json_has_key "$CLI_OUTPUT" '.uptime_seconds'
    assert_json_eq "$CLI_OUTPUT" '.components.database' 'ok'
    assert_json_eq "$CLI_OUTPUT" '.components.engine' 'ok'
}

test_health_quiet_mode() {
    cli_quiet health
    assert_exit_code 0 "$CLI_EXIT"
    assert_eq "$CLI_OUTPUT" "ok"
}

test_health_yaml_output() {
    cli_raw --output yaml health
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "status: ok"
    assert_contains "$CLI_OUTPUT" "components:"
    assert_contains "$CLI_OUTPUT" "database: ok"
    assert_contains "$CLI_OUTPUT" "engine: ok"
}

test_engine_status_empty() {
    undeploy_all
    cli engine status
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_has_key "$CLI_OUTPUT" '.version'
    assert_json_has_key "$CLI_OUTPUT" '.uptime_seconds'
    assert_json_eq "$CLI_OUTPUT" '.workflows_count' '0'
    assert_json_length "$CLI_OUTPUT" '.channels' 0
}

# Two services deployed as one set: the engine counts both workflows and
# names both channels.
test_engine_status_reports_the_deployed_set() {
    local dir
    dir=$(new_set)
    set_workflow "$dir" alpha-wf "$(cat "$FIXTURES_DIR/workflows/simple_log.json")"
    set_channel "$dir" alpha alpha-wf
    set_workflow "$dir" beta-wf "$(cat "$FIXTURES_DIR/workflows/conditional.json")"
    set_channel "$dir" beta beta-wf
    deploy_ok "$dir"

    cli engine status
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.active_workflows' '2'
    assert_json_eq "$CLI_OUTPUT" '.channels | sort | join(",")' 'alpha,beta'
}

run_test "health check returns ok with components"       test_health_check
run_test "health quiet mode prints ok"                   test_health_quiet_mode
run_test "health YAML output includes nested components" test_health_yaml_output
run_test "engine status with nothing deployed"           test_engine_status_empty
run_test "engine status reports the deployed set"        test_engine_status_reports_the_deployed_set

end_suite

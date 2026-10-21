#!/usr/bin/env bash
# Suite: Health & Connectivity

begin_suite "Health & Connectivity"

test_health_check() {
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
    reset_server_state
    cli engine status
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_has_key "$CLI_OUTPUT" '.version'
    assert_json_has_key "$CLI_OUTPUT" '.uptime_seconds'
    assert_json_eq "$CLI_OUTPUT" '.workflows_count' '0'
}

run_test "health check returns ok with components" test_health_check
run_test "health quiet mode prints ok"             test_health_quiet_mode
run_test "health YAML output includes nested components" test_health_yaml_output
run_test "engine status on empty server"           test_engine_status_empty

end_suite

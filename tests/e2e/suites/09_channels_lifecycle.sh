#!/usr/bin/env bash
# Suite: Channels Lifecycle
#
# Channels had no suite of their own — they were exercised only incidentally,
# through `helpers.sh::create_channel`, on the way to testing something else.
# That left the whole channel half of the shared entity commands uncovered, and
# `versions` / `new-version` uncovered for either entity.
#
# One test per command, so a descriptor wired to the wrong endpoint family
# fails here rather than in someone's terminal.

begin_suite "Channels Lifecycle"

# Activating a channel needs an active workflow to bind to. Every test that
# gets as far as a status change starts from one.
_seed_workflow() {
    cli_quiet workflows create -f "$FIXTURES_DIR/workflows/simple_log.json"
    local wf="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf"
    echo "$wf"
}

_channel_body() {
    local wf="$1"
    echo "{\"name\":\"lifecycle-ch\",\"channel_type\":\"sync\",\"protocol\":\"http\",\"workflow_id\":\"$wf\",\"methods\":[\"POST\"],\"route_pattern\":\"/lifecycle-ch\"}"
}

test_create_channel() {
    reset_server_state
    local wf
    wf=$(_seed_workflow)

    cli channels create -d "$(_channel_body "$wf")"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'lifecycle-ch'
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'draft'
    assert_json_has_key "$CLI_OUTPUT" '.data.channel_id'
}

# The id field a channel answers with is `channel_id`, not `id` — quiet mode
# prints exactly that, so this pins the descriptor's `id_field`.
test_create_channel_quiet_prints_the_channel_id() {
    reset_server_state
    local wf
    wf=$(_seed_workflow)

    cli_quiet channels create -d "$(_channel_body "$wf")"
    assert_exit_code 0 "$CLI_EXIT"
    local quiet_id="$CLI_OUTPUT"

    cli channels get "$quiet_id"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.channel_id' "$quiet_id"
}

test_update_channel_reports_the_new_version() {
    reset_server_state
    local wf
    wf=$(_seed_workflow)
    cli_quiet channels create -d "$(_channel_body "$wf")"
    local ch="$CLI_OUTPUT"

    local body
    body=$(jq -c '.name = "lifecycle-renamed"' <<< "$(_channel_body "$wf")")
    cli channels update "$ch" -d "$body"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data.name' 'lifecycle-renamed'

    # A versioned entity prints its version back; a connector must not.
    cli_raw channels update "$ch" -d "$body"
    assert_contains "$CLI_OUTPUT" "Channel updated"
    assert_contains "$CLI_OUTPUT" "(v"
}

test_delete_channel() {
    reset_server_state
    local wf
    wf=$(_seed_workflow)
    cli_quiet channels create -d "$(_channel_body "$wf")"
    local ch="$CLI_OUTPUT"

    cli_raw channels delete "$ch"
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "deleted"

    cli channels get "$ch"
    assert_exit_code 1 "$CLI_EXIT"
}

test_validate_channel_rejects_a_bad_definition() {
    reset_server_state
    # No workflow_id and no route_pattern: refused, and reported as a finding
    # inside a 200, so the exit code is the answer.
    cli_raw channels validate -d '{"name":"bad","channel_type":"sync","protocol":"http"}'
    assert_exit_code 1 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "INVALID"
}

test_channel_status_transitions() {
    reset_server_state
    local wf
    wf=$(_seed_workflow)
    cli_quiet channels create -d "$(_channel_body "$wf")"
    local ch="$CLI_OUTPUT"

    # A completed transition reports in prose, not JSON — same as
    # `workflows activate` — so the status itself is read back with `get`.
    cli_raw channels activate "$ch"
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "status changed to"
    cli channels get "$ch"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'active'

    cli_quiet channels archive "$ch"
    assert_exit_code 0 "$CLI_EXIT"
    cli channels get "$ch"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'archived'
}

# The pre-flight reports a refused transition as `valid: false` inside a 200,
# so reading only the HTTP status makes a failing check look like a passing
# one. Both directions are asserted, and that a dry run writes nothing.
test_channel_activate_dry_run() {
    reset_server_state
    local wf
    wf=$(_seed_workflow)
    cli_quiet channels create -d "$(_channel_body "$wf")"
    local ch="$CLI_OUTPUT"

    cli channels activate "$ch" --dry-run
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.valid' 'true'

    cli channels get "$ch"
    assert_json_eq "$CLI_OUTPUT" '.data.status' 'draft'

    cli_raw channels activate no-such-channel --dry-run
    assert_exit_code 1 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "INVALID"
}

test_channel_activate_defer_reload() {
    reset_server_state
    local wf
    wf=$(_seed_workflow)
    cli_quiet channels create -d "$(_channel_body "$wf")"
    local ch="$CLI_OUTPUT"

    cli_raw channels activate "$ch" --defer-reload
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "Reload deferred"
}

# `versions` and `new-version` had no coverage for either entity.
test_channel_versions_and_new_version() {
    reset_server_state
    local wf
    wf=$(_seed_workflow)
    cli_quiet channels create -d "$(_channel_body "$wf")"
    local ch="$CLI_OUTPUT"
    cli_quiet channels activate "$ch"

    cli_quiet channels new-version "$ch"
    assert_exit_code 0 "$CLI_EXIT"
    assert_eq "$CLI_OUTPUT" "2"

    cli channels versions "$ch"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_length "$CLI_OUTPUT" '.data' 2

    # The table carries Priority, which is a real field on a channel version.
    cli_raw channels versions "$ch"
    assert_contains "$CLI_OUTPUT" "Priority"

    cli channels versions "$ch" --limit 1
    assert_json_length "$CLI_OUTPUT" '.data' 1
}

test_channel_export_filters() {
    reset_server_state
    local wf
    wf=$(_seed_workflow)
    cli_quiet channels create -d "$(_channel_body "$wf")"
    cli_quiet channels activate "$CLI_OUTPUT"

    cli_raw channels export
    assert_exit_code 0 "$CLI_EXIT"
    assert_eq "$(echo "$CLI_OUTPUT" | jq 'length')" "1"

    # The filter set stays per-entity rather than shared, so it needs its own
    # assertion that it still reaches the server.
    cli_raw channels export --protocol http
    assert_eq "$(echo "$CLI_OUTPUT" | jq 'length')" "1"
    cli_raw channels export --protocol rest
    assert_eq "$(echo "$CLI_OUTPUT" | jq 'length')" "0"
}

run_test "create channel"                     test_create_channel
run_test "create channel quiet prints channel_id" test_create_channel_quiet_prints_the_channel_id
run_test "update channel reports new version" test_update_channel_reports_the_new_version
run_test "delete channel"                     test_delete_channel
run_test "validate rejects a bad definition"  test_validate_channel_rejects_a_bad_definition
run_test "channel status transitions"         test_channel_status_transitions
run_test "channel activate --dry-run"         test_channel_activate_dry_run
run_test "channel activate --defer-reload"    test_channel_activate_defer_reload
run_test "channel versions and new-version"   test_channel_versions_and_new_version
run_test "channel export filters"             test_channel_export_filters

end_suite

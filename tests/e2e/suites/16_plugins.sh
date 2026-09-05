#!/usr/bin/env bash
# Suite: Plugins
#
# The plugin entity through the CLI against a real server with the sandbox
# on: upload from a manifest (the component read beside it), activate, call
# the function from a workflow over the data plane, the archive gate, and
# the round trip out of `functions list`. The fixture component is the one
# the in-process tests use, so this proves the same bytes serve over HTTP.

begin_suite "Plugins"

PLUGIN_FIXTURES="$REPO_ROOT/crates/orion-server/tests/fixtures/plugins"

_wrap_workflow() {
    echo '{"name":"Wrap via plugin","condition":true,"tasks":[
      {"id":"parse","name":"parse","function":{"name":"parse_json","input":{"source":"payload","target":"input"}}},
      {"id":"wrap","name":"wrap","function":{"name":"test.fixture.wrap","input":{"message":{"var":"data.input.msg"},"output":"data.result"}}}
    ]}'
}

_upper_workflow() {
    echo '{"name":"Upper via plugin","condition":true,"tasks":[
      {"id":"parse","name":"parse","function":{"name":"parse_json","input":{"source":"payload","target":"input"}}},
      {"id":"up","name":"up","function":{"name":"test.fixture.upper","input":{"text":{"cat":["hello ",{"var":"data.input.who"}]},"output":"data.up"}}}
    ]}'
}

test_plugin_upload_and_get() {
    reset_server_state
    cli_quiet plugins create -f "$PLUGIN_FIXTURES/fixture-upload.toml" --tag e2e
    assert_exit_code 0 "$CLI_EXIT" "create: $CLI_STDERR"
    assert_eq "$CLI_OUTPUT" "test.fixture"

    cli plugins get test.fixture
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" ".data.status" "draft"
    assert_json_eq "$CLI_OUTPUT" ".data.abi" "orion:plugin@1.0.0"
    assert_json_eq "$CLI_OUTPUT" ".data.health.state" "inactive"
    assert_json_eq "$CLI_OUTPUT" ".data.functions | length" "8"

    cli plugins list --tag e2e
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" ".data[0].plugin_id" "test.fixture"

    cli plugins validate -f "$PLUGIN_FIXTURES/fixture-upload.toml"
    assert_exit_code 0 "$CLI_EXIT" "validate: $CLI_STDERR"
    # The CLI prints the unwrapped envelope in JSON mode.
    assert_json_eq "$CLI_OUTPUT" ".valid" "true"
}

test_plugin_serves_a_workflow_and_gates_archive() {
    cli_quiet plugins activate test.fixture
    assert_exit_code 0 "$CLI_EXIT" "activate: $CLI_STDERR"

    cli plugins get test.fixture
    assert_json_eq "$CLI_OUTPUT" ".data.status" "active"
    assert_json_eq "$CLI_OUTPUT" ".data.health.state" "loaded"

    cli functions list
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "test.fixture.wrap"
    assert_json_eq "$CLI_OUTPUT" '.data[] | select(.name == "test.fixture.wrap") | .source' "plugin"

    cli_quiet workflows create -d "$(_wrap_workflow)"
    assert_exit_code 0 "$CLI_EXIT" "workflow create: $CLI_STDERR"
    local wf="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf"
    assert_exit_code 0 "$CLI_EXIT" "workflow activate: $CLI_STDERR"
    create_channel "plugin-wrap" "$wf"

    cli send plugin-wrap -d '{"msg":"hi"}'
    assert_exit_code 0 "$CLI_EXIT" "send: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".data.result.wrapped.message" "hi"
    assert_json_eq "$CLI_OUTPUT" ".data.result.len" "16"

    # A `template_at` field: the engine evaluates the expression and the
    # guest receives the result. `upper` upper-cases the JSON it is given.
    cli_quiet workflows create -d "$(_upper_workflow)"
    assert_exit_code 0 "$CLI_EXIT" "upper workflow create: $CLI_STDERR"
    local upper_wf="$CLI_OUTPUT"
    cli_quiet workflows activate "$upper_wf"
    assert_exit_code 0 "$CLI_EXIT" "upper workflow activate: $CLI_STDERR"
    create_channel "plugin-upper" "$upper_wf"
    cli send plugin-upper -d '{"who":"world"}'
    assert_exit_code 0 "$CLI_EXIT" "send upper: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".data.up.TEXT" "HELLO WORLD"
    cli_quiet workflows archive "$upper_wf"
    assert_exit_code 0 "$CLI_EXIT"

    cli plugins dependencies test.fixture
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" ".data.workflows[0]" "$wf"

    # Archiving is refused while the workflow is active (409).
    cli plugins archive test.fixture
    assert_exit_code 1 "$CLI_EXIT" "archive must be refused while a workflow calls the plugin"
    assert_contains "$CLI_STDERR$CLI_OUTPUT" "$wf"

    cli_quiet workflows archive "$wf"
    assert_exit_code 0 "$CLI_EXIT"
    cli_quiet plugins archive test.fixture
    assert_exit_code 0 "$CLI_EXIT" "archive after the workflow: $CLI_STDERR"

    cli functions list
    if [[ "$CLI_OUTPUT" == *"test.fixture.wrap"* ]]; then
        echo "ASSERTION FAILED: an archived plugin's functions must leave the catalogue" >&2
        return 1
    fi
}

test_plugin_export_import_and_delete() {
    cli plugins export --include-artifacts --tag e2e
    assert_exit_code 0 "$CLI_EXIT"
    local export_file
    export_file=$(mktemp "${TMPDIR:-/tmp}/plugins-export-XXXXXX")
    echo "$CLI_OUTPUT" > "$export_file"
    assert_json_eq "$CLI_OUTPUT" ".[0].plugin_id" "test.fixture"
    assert_json_eq "$CLI_OUTPUT" ".[0] | has(\"component\")" "true"

    # The plugin was archived by the previous test, so an upsert of the same
    # content cuts a new draft version rather than reporting it unchanged —
    # the same rule a workflow follows.
    cli plugins import -f "$export_file" --on-conflict new_version
    assert_exit_code 0 "$CLI_EXIT" "import: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".imported" "1"
    assert_json_eq "$CLI_OUTPUT" ".results[0].action" "new_version"
    rm -f "$export_file"

    cli_quiet plugins versions test.fixture
    assert_exit_code 0 "$CLI_EXIT"

    cli_quiet plugins delete test.fixture
    assert_exit_code 0 "$CLI_EXIT" "delete: $CLI_STDERR"
    cli plugins get test.fixture
    assert_exit_code 1 "$CLI_EXIT" "a deleted plugin is gone"
}

run_test "plugins: upload from a manifest, get, list, validate" test_plugin_upload_and_get
run_test "plugins: activate, serve through a workflow, archive gate" test_plugin_serves_a_workflow_and_gates_archive
run_test "plugins: export with artifacts, import unchanged, delete" test_plugin_export_import_and_delete

end_suite

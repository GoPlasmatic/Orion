#!/usr/bin/env bash
# Suite: Plugins
#
# A plugin deployed as a member of a definition set, against a real server
# with the sandbox on: the manifest and its component compiled into the
# deployment, the functions it declares served through a workflow over the
# data plane (a `template_at` field included), the catalogue reporting them,
# and a deployment that leaves the plugin out taking them away. The fixture
# component is the one the in-process tests use, so this proves the same
# bytes serve over HTTP.

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

# The plugin and both workflows that call it, behind a channel each.
_plugin_set() {
    local dir
    dir=$(new_set plugin-set)
    set_plugin "$dir" "$PLUGIN_FIXTURES/fixture-upload.toml"
    set_workflow "$dir" plugin-wrap "$(_wrap_workflow)"
    set_channel "$dir" plugin-wrap plugin-wrap
    set_workflow "$dir" plugin-upper "$(_upper_workflow)"
    set_channel "$dir" plugin-upper plugin-upper
    echo "$dir"
}

test_plugin_compiles_into_the_deployment() {
    local dir
    dir=$(_plugin_set)
    compile_set "$dir"
    assert_contains "$COMPILE_OUTPUT" "plugin 'test.fixture'"
    assert_json_eq "$(cat "$dir.dist/deployment.json")" \
        '[.members[] | select(.kind == "plugin") | .id] | join(",")' "test.fixture"
    assert_json_eq "$(definition_of "$dir.dist" plugin test.fixture)" '.kind' "plugin"
}

test_plugin_serves_its_workflows() {
    deploy_ok "$(_plugin_set)"

    cli functions list
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "test.fixture.wrap"
    assert_json_eq "$CLI_OUTPUT" '.data[] | select(.name == "test.fixture.wrap") | .source' "plugin"

    cli health
    assert_json_eq "$CLI_OUTPUT" '[.plugins.loaded[].plugin] | index("test.fixture") != null' "true"

    cli send plugin-wrap -d '{"msg":"hi"}'
    assert_exit_code 0 "$CLI_EXIT" "send: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".data.result.wrapped.message" "hi"
    assert_json_eq "$CLI_OUTPUT" ".data.result.len" "16"

    # A `template_at` field: the engine evaluates the expression and the
    # guest receives the result. `upper` upper-cases the JSON it is given.
    cli send plugin-upper -d '{"who":"world"}'
    assert_exit_code 0 "$CLI_EXIT" "send upper: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".data.up.TEXT" "HELLO WORLD"
}

test_a_redeploy_is_a_no_op() {
    # The previous test deployed this set; applying it again commits nothing.
    local dir
    dir=$(_plugin_set)
    compile_set "$dir"
    cli deployments apply -d "$dir.dist"
    assert_exit_code 0 "$CLI_EXIT" "apply: $CLI_STDERR"
    assert_contains "$CLI_STDERR" "Up to date"
}

# A workflow cannot be deployed to a node that serves no plugin providing its
# function. `compile` cannot know (it notes the function as unverifiable when
# the set carries no manifest for it); the node refuses the definition at
# upload, so nothing commits.
test_a_workflow_without_its_plugin_is_refused() {
    undeploy_all
    local dir
    dir=$(new_set)
    set_workflow "$dir" plugin-wrap "$(_wrap_workflow)"
    set_channel "$dir" plugin-wrap plugin-wrap
    compile_set "$dir"
    assert_contains "$COMPILE_OUTPUT" "plugin.unverifiable"

    # Upload cannot know which plugins the deployment brings, so the refusal
    # is the deployment's: the head it would build calls a function nothing
    # in it serves.
    apply_dist "$dir.dist"
    assert_ne "$CLI_EXIT" 0 "a workflow calling a function no plugin serves must not deploy"
    assert_contains "$CLI_STDERR" "DEPLOYMENT_WOULD_NOT_BUILD"
    assert_contains "$CLI_STDERR" "test.fixture.wrap"
}

test_a_deployment_without_the_plugin_drops_its_functions() {
    deploy_one "$FIXTURES_DIR/workflows/simple_log.json" simple-log orders

    cli functions list
    assert_exit_code 0 "$CLI_EXIT"
    assert_not_contains "$CLI_OUTPUT" "test.fixture.wrap" \
        "a plugin the head no longer deploys must leave the catalogue"

    cli send plugin-wrap -d '{"msg":"hi"}'
    assert_ne "$CLI_EXIT" 0 "the plugin's channel left with it"
}

run_test "plugins: a manifest compiles into the deployment"        test_plugin_compiles_into_the_deployment
run_test "plugins: deployed, its functions serve two workflows"   test_plugin_serves_its_workflows
run_test "plugins: the same set applies nothing the second time"  test_a_redeploy_is_a_no_op
run_test "plugins: a deployment without the plugin drops it"      test_a_deployment_without_the_plugin_drops_its_functions
run_test "plugins: a workflow without its plugin is refused"      test_a_workflow_without_its_plugin_is_refused

end_suite

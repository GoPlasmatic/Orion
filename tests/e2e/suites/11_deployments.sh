#!/usr/bin/env bash
# Suite: Deployments
#
# The v2 deployment path end to end: an example set compiled to a deployment
# directory with `orion-server compile --format deployment`, deployed with
# `orion-cli deployments apply -d`, and the channel serving it. Then one
# workflow changes, the set recompiles and re-applies, and only that member's
# revision moves; an unchanged directory applies nothing; a member left out of
# the next apply stops serving; and a rollback serves the earlier revision
# again.
#
# The suite runs its own server, so the revision numbers it asserts are the
# only ones that server ever committed.

begin_suite "Deployments"

DEP_EXAMPLE="$REPO_ROOT/examples/packages/order-classification"

# Suite-level, started before the tests and stopped after them: run_test runs
# each test in a subshell, so a variable set inside one test is not seen by
# the next.
DEP_DIR=""
DEP_PID=""
DEP_SHARED_URL="$ORION_URL"

start_deployment_server() {
    DEP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/orion-e2e-deploy-XXXXXX")
    mkdir -p "$DEP_DIR/set"
    cp "$DEP_EXAMPLE/workflow.json" "$DEP_EXAMPLE/channel.json" "$DEP_DIR/set/"
    # A connection rides along, so its revision is assigned and kept too.
    cat > "$DEP_DIR/set/connector.json" <<'JSON'
{ "name": "scratch-cache", "connector_type": "cache",
  "config": { "type": "cache", "backend": "memory" } }
JSON

    local port
    port=$(find_free_port)
    ORION_STORAGE__URL="sqlite:$DEP_DIR/orion.db?mode=rwc" \
        ORION_SERVER__HOST=127.0.0.1 \
        ORION_SERVER__PORT="$port" \
        ORION_SERVER__SHUTDOWN_DRAIN_SECS=0 \
        ORION_LOGGING__LEVEL=warn \
        "$ORION_BIN" > "$DEP_DIR/server.log" 2>&1 &
    DEP_PID=$!
    ORION_URL="http://127.0.0.1:${port}"

    local waited=0
    while ! curl -sf "$ORION_URL/readyz" >/dev/null 2>&1; do
        if ! kill -0 "$DEP_PID" 2>/dev/null || [[ $waited -gt 60 ]]; then
            echo "the deployment server did not start:" >&2
            cat "$DEP_DIR/server.log" >&2
            return 1
        fi
        sleep 0.25
        waited=$((waited + 1))
    done
}

stop_deployment_server() {
    if [[ -n "$DEP_PID" ]] && kill -0 "$DEP_PID" 2>/dev/null; then
        kill "$DEP_PID" 2>/dev/null || true
        wait "$DEP_PID" 2>/dev/null || true
    fi
    DEP_PID=""
    [[ -n "$DEP_DIR" ]] && rm -rf "$DEP_DIR"
    DEP_DIR=""
    ORION_URL="$DEP_SHARED_URL"
}

compile_deployment() {
    "$ORION_BIN" compile "$DEP_DIR/set" --format deployment -o "$DEP_DIR/dist" >/dev/null 2>&1
}

# member_revision <deployment revision> <kind> <id>
member_revision() {
    cli deployments get "$1"
    echo "$CLI_OUTPUT" | jq -r --arg k "$2" --arg i "$3" \
        '.manifest.members[] | select(.entity.kind == $k and .entity.id == $i) | .entity.revision'
}

test_deployment_compiles_and_serves() {
    set -e
    compile_deployment
    [[ -f "$DEP_DIR/dist/deployment.json" ]]
    assert_json_eq "$(cat "$DEP_DIR/dist/deployment.json")" '.members | length' "3"

    cli deployments plan -d "$DEP_DIR/dist"
    assert_exit_code 0 "$CLI_EXIT" "plan: $CLI_STDERR"
    assert_contains "$CLI_STDERR" "+ workflow order-classification@1"
    assert_matches "$(echo "$CLI_OUTPUT" | jq -r '.reviewed_manifest_digest')" '^sha256:[0-9a-f]{64}$'

    cli deployments apply -d "$DEP_DIR/dist"
    assert_exit_code 0 "$CLI_EXIT" "apply: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".committed_revision" "1"
    assert_json_eq "$CLI_OUTPUT" ".evaluation.converged" "true"

    cli send order-tiers -d '{"amount": 750}'
    assert_exit_code 0 "$CLI_EXIT" "send: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".data.order.tier" "vip"
    assert_json_eq "$CLI_OUTPUT" ".data.order.discount_pct" "15"
}

test_redeploy_moves_only_the_changed_member() {
    set -e
    sed -i.bak 's/"logic": 15 }/"logic": 20 }/' "$DEP_DIR/set/workflow.json"
    rm -f "$DEP_DIR/set/workflow.json.bak"
    assert_contains "$(cat "$DEP_DIR/set/workflow.json")" '"logic": 20 }'
    compile_deployment

    cli deployments apply -d "$DEP_DIR/dist"
    assert_exit_code 0 "$CLI_EXIT" "apply: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".committed_revision" "2"

    assert_eq "$(member_revision 2 workflow order-classification)" "2"
    assert_eq "$(member_revision 2 channel order-tiers)" "1"
    assert_eq "$(member_revision 2 connection scratch-cache)" "1"

    cli send order-tiers -d '{"amount": 750}'
    assert_exit_code 0 "$CLI_EXIT" "send: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".data.order.discount_pct" "20"
}

test_an_unchanged_directory_applies_nothing() {
    set -e
    cli_quiet deployments apply -d "$DEP_DIR/dist"
    assert_exit_code 0 "$CLI_EXIT" "apply: $CLI_STDERR"
    assert_eq "$CLI_OUTPUT" "2" "quiet apply prints the head it is already at"

    cli deployments get 3
    assert_exit_code 1 "$CLI_EXIT" "no revision 3 was committed"
}

# An apply replaces the head's members with the directory's: the channel
# file removed from the set, its channel is removed from the head.
test_a_member_left_out_stops_serving() {
    set -e
    rm "$DEP_DIR/set/channel.json"
    compile_deployment

    cli deployments apply -d "$DEP_DIR/dist"
    assert_exit_code 0 "$CLI_EXIT" "apply: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".committed_revision" "3"
    assert_contains "$CLI_STDERR" "- channel order-tiers@1"

    cli send order-tiers -d '{"amount": 750}'
    assert_ne "$CLI_EXIT" 0 "a channel the head no longer deploys must not serve"
}

test_a_rollback_serves_the_earlier_revision() {
    set -e
    cli deployments rollback --target-revision 2 --expected-head-revision 3 \
        --idempotency-key e2e-rollback-to-2 --reason "e2e rollback"
    assert_exit_code 0 "$CLI_EXIT" "rollback: $CLI_STDERR"
    assert_eq "$(member_revision 4 channel order-tiers)" "1"
    assert_eq "$(member_revision 4 workflow order-classification)" "2"

    cli send order-tiers -d '{"amount": 750}'
    assert_exit_code 0 "$CLI_EXIT" "send: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".data.order.discount_pct" "20"
}

if start_deployment_server; then
    run_test "deployments: a compiled set deploys with apply -d and serves" test_deployment_compiles_and_serves
    run_test "deployments: a recompiled change moves only that member's revision" test_redeploy_moves_only_the_changed_member
    run_test "deployments: an unchanged directory applies nothing" test_an_unchanged_directory_applies_nothing
    run_test "deployments: a member left out of the next apply stops serving" test_a_member_left_out_stops_serving
    run_test "deployments: a rollback serves the earlier revision again" test_a_rollback_serves_the_earlier_revision
else
    log_fail "the deployment server did not start; the deployment tests did not run"
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_FAILED=$((TESTS_FAILED + 1))
    FAILED_NAMES+=("deployments: server start")
fi

stop_deployment_server

end_suite

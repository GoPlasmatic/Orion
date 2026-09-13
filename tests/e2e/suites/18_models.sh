#!/usr/bin/env bash
# Suite: Models
#
# The model entity through the CLI against a real server with the runtime
# on, and the c4-tournament example package on top of it. A bucket stands in
# for object storage: a Python static server over a temp directory, which
# answers the signed HEAD and GET an admission makes (SigV4 headers are
# ignored, the query string is dropped) — so the whole path runs for real:
# register from the manifest, follow admission with --wait, activate, then
# the plugin, the connector, the workflows and the channels of the package
# deployed with the CLI and a turn, a match and the leaderboard served over
# the data plane. Then the refusals: a wrong digest fails admission at the
# `digest` stage, and delete is refused while an active workflow names the
# model by literal id.

begin_suite "Models"

C4_PACKAGE="$REPO_ROOT/examples/packages/c4-tournament"
C4_ENTRANT="$C4_PACKAGE/entrant"
C4_MODEL="example.c4-tiny"

# Suite-level, started before the tests and stopped after them: run_test
# runs each test in a subshell, so a variable set inside one test is not
# seen by the next, and the bucket has to outlive the per-test TEST_TMPDIR.
C4_BUCKET_DIR=""
C4_BUCKET_PID=""
C4_BUCKET_PORT=""
C4_LEADERBOARD_DB=""

# The bucket: `models/` is the bucket name a path-style GET puts in the path,
# and c4-tiny.onnx the key the registration names.
start_bucket() {
    C4_BUCKET_DIR=$(mktemp -d "${TMPDIR:-/tmp}/orion-e2e-bucket-XXXXXX")
    mkdir -p "$C4_BUCKET_DIR/models"
    cp "$C4_ENTRANT/c4-tiny.onnx" "$C4_BUCKET_DIR/models/c4-tiny.onnx"
    C4_BUCKET_PORT=$(find_free_port)
    python3 -m http.server "$C4_BUCKET_PORT" --bind 127.0.0.1 --directory "$C4_BUCKET_DIR" >/dev/null 2>&1 &
    C4_BUCKET_PID=$!
    local waited=0
    while ! curl -sfI "http://127.0.0.1:${C4_BUCKET_PORT}/models/c4-tiny.onnx" >/dev/null 2>&1; do
        sleep 0.2
        waited=$((waited + 1))
        if [[ $waited -gt 50 ]]; then
            echo "the bucket did not come up on port $C4_BUCKET_PORT" >&2
            return 1
        fi
    done
    C4_LEADERBOARD_DB="$C4_BUCKET_DIR/leaderboard.db"
}

stop_bucket() {
    if [[ -n "$C4_BUCKET_PID" ]] && kill -0 "$C4_BUCKET_PID" 2>/dev/null; then
        kill "$C4_BUCKET_PID" 2>/dev/null || true
        wait "$C4_BUCKET_PID" 2>/dev/null || true
    fi
    C4_BUCKET_PID=""
    [[ -n "$C4_BUCKET_DIR" ]] && rm -rf "$C4_BUCKET_DIR"
    C4_BUCKET_DIR=""
}

entrant_digest() {
    echo "sha256:$(shasum -a 256 "$C4_ENTRANT/c4-tiny.onnx" | cut -d' ' -f1)"
}

# The storage connector the registration reads through: the same shape the
# in-process tests use, pointed at the Python server.
_bucket_connector() {
    jq -n --arg endpoint "http://127.0.0.1:${C4_BUCKET_PORT}" '{
        name: "c4-bucket", connector_type: "storage",
        config: { type: "storage", endpoint: $endpoint, region: "us-east-1", bucket: "models",
                  access_key: "AKIAEXAMPLE", secret_key: "example-secret",
                  force_path_style: true, allow_private_urls: true } }'
}

# A workflow naming the model by literal id: what the archive/delete gate
# sees. The package's own workflows route by a computed id, which the gate
# cannot see — by design, and documented on the function.
_literal_workflow() {
    echo '{"name":"Names the entrant literally","condition":true,"tasks":[
      {"id":"parse","name":"parse","function":{"name":"parse_json","input":{"source":"payload","target":"view"}}},
      {"id":"infer","name":"infer","function":{"name":"model_infer","input":{"model":"'"$C4_MODEL"'","input":{"var":"data.view"},"output":"data.answer"}}}
    ]}'
}

test_model_register_admit_activate() {
    reset_server_state
    [[ -n "$C4_BUCKET_PID" ]] || { echo "the bucket is not running" >&2; return 1; }

    cli_quiet connectors create -d "$(_bucket_connector)"
    assert_exit_code 0 "$CLI_EXIT" "connector create: $CLI_STDERR"

    cli models create -f "$C4_ENTRANT/model.json" --connector c4-bucket --key c4-tiny.onnx \
        --digest "$(entrant_digest)" --tag e2e --wait --timeout 60
    assert_exit_code 0 "$CLI_EXIT" "create --wait must exit 0 when admission passes: $CLI_STDERR $CLI_OUTPUT"

    cli models get "$C4_MODEL"
    assert_exit_code 0 "$CLI_EXIT" "get: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".data.model_id" "$C4_MODEL"
    assert_json_eq "$CLI_OUTPUT" ".data.status" "draft"
    assert_json_eq "$CLI_OUTPUT" ".data.admission.state" "passed"
    assert_json_eq "$CLI_OUTPUT" ".data.stats.parameters" "1479"
    assert_json_eq "$CLI_OUTPUT" ".data.artifact.digest" "$(entrant_digest)"

    cli models list --tag e2e
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" ".data[0].model_id" "$C4_MODEL"

    cli models validate -f "$C4_ENTRANT/model.json" --connector c4-bucket --key c4-tiny.onnx --digest "$(entrant_digest)"
    assert_exit_code 0 "$CLI_EXIT" "validate: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".valid" "true"

    cli_quiet models activate "$C4_MODEL"
    assert_exit_code 0 "$CLI_EXIT" "activate: $CLI_STDERR"
    cli models get "$C4_MODEL"
    assert_json_eq "$CLI_OUTPUT" ".data.status" "active"
    assert_matches "$(echo "$CLI_OUTPUT" | jq -r '.data.health.state')" '^(admitted|loaded)$' "an active admitted model is admitted or loaded on this node"
}

# Deploy the package the way an organiser would: the plugin first (its
# functions must exist before a workflow names them), the connector with the
# leaderboard pointed at a file under the suite's temp dir, every workflow,
# every channel.
_deploy_package() {
    cli_quiet plugins create -f "$C4_PACKAGE/plugin.toml"
    assert_exit_code 0 "$CLI_EXIT" "plugin create: $CLI_STDERR"
    cli_quiet plugins activate c4.rules
    assert_exit_code 0 "$CLI_EXIT" "plugin activate: $CLI_STDERR"

    local connector
    connector=$(jq -c --arg url "sqlite:${C4_LEADERBOARD_DB}?mode=rwc" \
        '.config.connection_string = $url' "$C4_PACKAGE/connector-leaderboard.json")
    cli_quiet connectors create -d "$connector"
    assert_exit_code 0 "$CLI_EXIT" "leaderboard connector: $CLI_STDERR"

    local wf
    for wf in workflow.json workflow-turn.json workflow-match.json workflow-leaderboard.json workflow-round.json; do
        cli_quiet workflows create -f "$C4_PACKAGE/$wf"
        assert_exit_code 0 "$CLI_EXIT" "$wf create: $CLI_STDERR"
        cli_quiet workflows activate "$CLI_OUTPUT"
        assert_exit_code 0 "$CLI_EXIT" "$wf activate: $CLI_STDERR"
    done
    local ch
    for ch in channel.json channel-turn.json channel-match.json channel-leaderboard.json channel-round.json; do
        cli_quiet channels create -f "$C4_PACKAGE/$ch"
        assert_exit_code 0 "$CLI_EXIT" "$ch create: $CLI_STDERR"
        cli_quiet channels activate "$CLI_OUTPUT"
        assert_exit_code 0 "$CLI_EXIT" "$ch activate: $CLI_STDERR"
    done
}

test_model_serves_the_tournament_package() {
    _deploy_package || return 1

    # Register: the probe on an empty board, then the leaderboard row with
    # the parameter count the server measured at admission.
    cli send c4-register -d "$(jq -c .data "$C4_PACKAGE/request.json")"
    assert_exit_code 0 "$CLI_EXIT" "register: $CLI_STDERR $CLI_OUTPUT"
    assert_json_eq "$CLI_OUTPUT" ".data.registered.model" "$C4_MODEL"
    assert_json_eq "$CLI_OUTPUT" ".data.registered.parameters" "1479"
    assert_json_eq "$CLI_OUTPUT" ".data.stats.runtime" "tract"

    # One turn on the sample board: a legal column, one more disc.
    cli send c4-turn -d "$(jq -c .data "$C4_PACKAGE/request-turn.json")"
    assert_exit_code 0 "$CLI_EXIT" "turn: $CLI_STDERR $CLI_OUTPUT"
    assert_matches "$(echo "$CLI_OUTPUT" | jq -r '.data.answer.column[0]')" '^[0-6]$' "the answer is a column"
    assert_json_eq "$CLI_OUTPUT" ".data.state.moves" "5"
    assert_json_eq "$CLI_OUTPUT" ".data.state.illegal" "false"
    assert_json_eq "$CLI_OUTPUT" ".data.state.to_move" "1"
    assert_json_eq "$CLI_OUTPUT" ".data.inference.parameters" "1479"
    assert_json_eq "$CLI_OUTPUT" ".data.inference.cold_load" "false"

    # A match of the entrant against itself: over within 42 moves.
    cli send c4-match -d "$(jq -c .data "$C4_PACKAGE/request-match.json")"
    assert_exit_code 0 "$CLI_EXIT" "match: $CLI_STDERR $CLI_OUTPUT"
    assert_matches "$(echo "$CLI_OUTPUT" | jq -r '.data.result.winner')" '^[0-2]$' "winner is 0, 1 or 2"
    local moves
    moves=$(echo "$CLI_OUTPUT" | jq -r '.data.result.moves')
    assert_matches "$moves" '^[0-9]+$'
    if [[ "$moves" -gt 42 ]]; then
        echo "ASSERTION FAILED: a game has at most 42 moves, got $moves" >&2
        return 1
    fi

    # The leaderboard, over the data plane as a GET. Self-play credits both
    # sides of the one row, so its three counters sum to two.
    local board
    board=$(curl -sf "$ORION_URL/api/v1/data/c4/leaderboard")
    assert_json_eq "$board" ".data.leaderboard[0].model" "$C4_MODEL"
    assert_json_eq "$board" ".data.leaderboard[0].parameters" "1479"
    assert_json_eq "$board" ".data.leaderboard[0] | .wins + .losses + .draws" "2"

    # A round now: with one entrant there is no pair to play, and the
    # occurrence still lands in the ledger.
    cli channels trigger c4-round
    assert_exit_code 0 "$CLI_EXIT" "trigger: $CLI_STDERR"
}

test_model_wrong_digest_fails_at_the_digest_stage() {
    local manifest="$TEST_TMPDIR/model-bad.json"
    jq '.name = "example.c4-bad"' "$C4_ENTRANT/model.json" > "$manifest"
    local claimed="sha256:0000000000000000000000000000000000000000000000000000000000000000"

    cli models create -f "$manifest" --connector c4-bucket --key c4-tiny.onnx --digest "$claimed" --wait --timeout 60
    assert_exit_code 1 "$CLI_EXIT" "create --wait must exit 1 when admission fails"
    assert_contains "$CLI_STDERR$CLI_OUTPUT" "digest"

    cli models get example.c4-bad
    assert_json_eq "$CLI_OUTPUT" ".data.admission.state" "failed"
    assert_json_eq "$CLI_OUTPUT" ".data.admission.stage" "digest"

    # Activation is refused until admission has passed.
    cli models activate example.c4-bad
    assert_exit_code 1 "$CLI_EXIT" "a failed model must not activate"

    cli_quiet models delete example.c4-bad
    assert_exit_code 0 "$CLI_EXIT" "delete of an unreferenced draft: $CLI_STDERR"
}

test_model_delete_gated_by_a_workflow_naming_it() {
    cli_quiet workflows create -d "$(_literal_workflow)"
    assert_exit_code 0 "$CLI_EXIT" "literal workflow create: $CLI_STDERR"
    local wf="$CLI_OUTPUT"
    cli_quiet workflows activate "$wf"
    assert_exit_code 0 "$CLI_EXIT" "literal workflow activate: $CLI_STDERR"

    cli models dependencies "$C4_MODEL"
    assert_exit_code 0 "$CLI_EXIT"
    assert_contains "$CLI_OUTPUT" "$wf"

    cli models delete "$C4_MODEL"
    assert_exit_code 1 "$CLI_EXIT" "delete must be refused while an active workflow names the model"
    assert_contains "$CLI_STDERR$CLI_OUTPUT" "$wf"
    cli models archive "$C4_MODEL"
    assert_exit_code 1 "$CLI_EXIT" "archive must be refused too"

    cli_quiet workflows archive "$wf"
    assert_exit_code 0 "$CLI_EXIT"
    cli_quiet models delete "$C4_MODEL"
    assert_exit_code 0 "$CLI_EXIT" "delete after the workflow is archived: $CLI_STDERR"
    cli models get "$C4_MODEL"
    assert_exit_code 1 "$CLI_EXIT" "a deleted model is gone"
}

start_bucket || log_fail "the bucket did not start; every test below will fail"

run_test "models: register through a bucket, wait for admission, activate" test_model_register_admit_activate
run_test "models: the tournament package serves a turn, a match and the leaderboard" test_model_serves_the_tournament_package
run_test "models: a wrong digest fails admission at the digest stage" test_model_wrong_digest_fails_at_the_digest_stage
run_test "models: delete is gated by a workflow naming the model" test_model_delete_gated_by_a_workflow_naming_it

stop_bucket

end_suite

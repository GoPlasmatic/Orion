#!/usr/bin/env bash
# tests/e2e/helpers.sh — All shared E2E test functions
#
# Sourced by run.sh. Provides: framework, assertions, CLI wrappers,
# server lifecycle, definition-set deployment, and the case-file runner.

# ═══════════════════════════════════════════════════════════════════
# FRAMEWORK
# ═══════════════════════════════════════════════════════════════════

# ── Colors ──────────────────────────────────────────────────────
if [[ -t 1 ]] && [[ -z "${NO_COLOR:-}" ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[0;33m'
    BLUE='\033[0;34m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    DIM='\033[2m'
    RESET='\033[0m'
else
    RED='' GREEN='' YELLOW='' BLUE='' CYAN='' BOLD='' DIM='' RESET=''
fi

# ── Counters ────────────────────────────────────────────────────
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0
TESTS_SKIPPED=0
FAILED_NAMES=()
SUITE_START_TIME=0

# ── Paths ───────────────────────────────────────────────────────
E2E_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"       # <repo>/tests/e2e
REPO_ROOT="$(cd "$E2E_DIR/../.." && pwd)"                     # workspace root (cargo target dir lives here)
FIXTURES_DIR="$E2E_DIR/fixtures"
# Data-driven use cases live with the other runnable examples.
CASES_DIR="$REPO_ROOT/examples/use-cases"

# Both binaries default to the in-tree debug build; override to test
# against a foreign binary (e.g. a released orion-server).
ORION_BIN="${ORION_BIN:-$REPO_ROOT/target/debug/orion-server}"
ORION_CLI="${ORION_CLI:-$REPO_ROOT/target/debug/orion-cli}"

# ── Test Port & Server URL ──────────────────────────────────────
E2E_PORT="${E2E_PORT:-0}"
ORION_URL=""

# ── Per-Test Temp Directory ─────────────────────────────────────
TEST_TMPDIR=""

# ── Logging ─────────────────────────────────────────────────────
log_info()  { echo -e "${BLUE}[INFO]${RESET}  $*"; }
log_pass()  { echo -e "${GREEN}[PASS]${RESET}  $*"; }
log_fail()  { echo -e "${RED}[FAIL]${RESET}  $*"; }
log_skip()  { echo -e "${YELLOW}[SKIP]${RESET}  $*"; }
log_debug() { [[ -n "${E2E_DEBUG:-}" ]] && echo -e "${DIM}[DBG]   $*${RESET}" || true; }

# ── Suite Lifecycle ─────────────────────────────────────────────
begin_suite() {
    local suite_name="$1"
    echo ""
    echo -e "${BOLD}${CYAN}=== Suite: ${suite_name} ===${RESET}"
    SUITE_START_TIME=$(date +%s)
}

end_suite() {
    local elapsed=$(( $(date +%s) - SUITE_START_TIME ))
    echo -e "${DIM}    (${elapsed}s)${RESET}"
}

# ── Individual Test ─────────────────────────────────────────────
# Usage: run_test "test name" test_function_name
#
# The function runs in a subshell under errexit, so the first failing
# assertion fails the test rather than only the last one.
run_test() {
    local name="$1"
    local func="$2"
    TESTS_RUN=$((TESTS_RUN + 1))

    TEST_TMPDIR=$(mktemp -d "${TMPDIR:-/tmp}/orion-e2e-XXXXXX")

    set +e
    local output
    output=$(set -e; "$func" 2>&1)
    local exit_code=$?
    set -e

    rm -rf "$TEST_TMPDIR"

    if [[ $exit_code -eq 0 ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        log_pass "$name"
    elif [[ $exit_code -eq 77 ]]; then
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        log_skip "$name"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        FAILED_NAMES+=("$name")
        log_fail "$name"
        if [[ -n "$output" ]]; then
            echo "$output" | sed 's/^/        /' >&2
        fi
    fi
}

# ── Summary Report ──────────────────────────────────────────────
print_summary() {
    echo ""
    echo -e "${BOLD}════════════════════════════════════════${RESET}"
    echo -e "${BOLD}  E2E Test Results${RESET}"
    echo -e "${BOLD}════════════════════════════════════════${RESET}"
    echo -e "  Passed:  ${GREEN}${TESTS_PASSED}${RESET}"
    echo -e "  Failed:  ${RED}${TESTS_FAILED}${RESET}"
    echo -e "  Skipped: ${YELLOW}${TESTS_SKIPPED}${RESET}"
    echo -e "  Total:   ${TESTS_RUN}"
    echo -e "${BOLD}════════════════════════════════════════${RESET}"

    if [[ ${#FAILED_NAMES[@]} -gt 0 ]]; then
        echo ""
        echo -e "${RED}${BOLD}Failed tests:${RESET}"
        for name in "${FAILED_NAMES[@]}"; do
            echo -e "  ${RED}- ${name}${RESET}"
        done
    fi

    echo ""
    if [[ $TESTS_FAILED -eq 0 ]]; then
        echo -e "${GREEN}${BOLD}All tests passed.${RESET}"
        return 0
    else
        echo -e "${RED}${BOLD}${TESTS_FAILED} test(s) failed.${RESET}"
        return 1
    fi
}


# ═══════════════════════════════════════════════════════════════════
# ASSERTIONS
# ═══════════════════════════════════════════════════════════════════

# assert_eq <actual> <expected> [message]
assert_eq() {
    local actual="$1" expected="$2"
    local msg="${3:-Expected '$expected', got '$actual'}"
    if [[ "$actual" != "$expected" ]]; then
        echo "ASSERTION FAILED: $msg" >&2
        echo "  expected: $expected" >&2
        echo "  actual:   $actual" >&2
        return 1
    fi
}

# assert_ne <actual> <unexpected> [message]
assert_ne() {
    local actual="$1" unexpected="$2"
    local msg="${3:-Expected value to differ from '$unexpected'}"
    if [[ "$actual" == "$unexpected" ]]; then
        echo "ASSERTION FAILED: $msg" >&2
        return 1
    fi
}

# assert_contains <haystack> <needle> [message]
assert_contains() {
    local haystack="$1" needle="$2"
    local msg="${3:-Expected output to contain '$needle'}"
    if [[ "$haystack" != *"$needle"* ]]; then
        echo "ASSERTION FAILED: $msg" >&2
        echo "  output: $haystack" >&2
        return 1
    fi
}

# assert_not_contains <haystack> <needle> [message]
assert_not_contains() {
    local haystack="$1" needle="$2"
    local msg="${3:-Expected output NOT to contain '$needle'}"
    if [[ "$haystack" == *"$needle"* ]]; then
        echo "ASSERTION FAILED: $msg" >&2
        echo "  output: $haystack" >&2
        return 1
    fi
}

# assert_matches <value> <regex_pattern> [message]
assert_matches() {
    local value="$1" pattern="$2"
    local msg="${3:-Expected '$value' to match pattern '$pattern'}"
    if [[ ! "$value" =~ $pattern ]]; then
        echo "ASSERTION FAILED: $msg" >&2
        return 1
    fi
}

# assert_exit_code <expected> <actual> [message]
assert_exit_code() {
    local expected="$1" actual="$2"
    local msg="${3:-Expected exit code $expected, got $actual}"
    if [[ "$actual" -ne "$expected" ]]; then
        echo "ASSERTION FAILED: $msg" >&2
        return 1
    fi
}

# assert_json_eq <json> <jq_expr> <expected> [message]
# Uses jq -r (raw output), so string comparison is unquoted.
assert_json_eq() {
    local json="$1" jq_expr="$2" expected="$3"
    local msg="${4:-JSON $jq_expr: expected '$expected'}"
    local actual
    actual=$(echo "$json" | jq -r "$jq_expr" 2>/dev/null) || {
        echo "ASSERTION FAILED: jq parse error on '$jq_expr'" >&2
        echo "  input: $json" >&2
        return 1
    }
    if [[ "$actual" != "$expected" ]]; then
        echo "ASSERTION FAILED: $msg" >&2
        echo "  jq expr:  $jq_expr" >&2
        echo "  expected: $expected" >&2
        echo "  actual:   $actual" >&2
        return 1
    fi
}

# assert_json_length <json> <jq_array_expr> <expected_length> [message]
assert_json_length() {
    local json="$1" jq_expr="$2" expected="$3"
    local msg="${4:-JSON array $jq_expr: expected length $expected}"
    local actual
    actual=$(echo "$json" | jq "$jq_expr | length" 2>/dev/null) || {
        echo "ASSERTION FAILED: jq parse error" >&2
        return 1
    }
    if [[ "$actual" -ne "$expected" ]]; then
        echo "ASSERTION FAILED: $msg" >&2
        echo "  expected length: $expected" >&2
        echo "  actual length:   $actual" >&2
        return 1
    fi
}

# assert_json_has_key <json> <jq_path> [message]
assert_json_has_key() {
    local json="$1" jq_expr="$2"
    local msg="${3:-Expected JSON to have path $jq_expr}"
    local result
    result=$(echo "$json" | jq -e "$jq_expr" >/dev/null 2>&1 && echo "yes" || echo "no")
    if [[ "$result" != "yes" ]]; then
        echo "ASSERTION FAILED: $msg" >&2
        return 1
    fi
}

# assert_json_not_has_key <json> <jq_path> [message]
assert_json_not_has_key() {
    local json="$1" jq_expr="$2"
    local msg="${3:-Expected JSON to NOT have path $jq_expr}"
    local result
    result=$(echo "$json" | jq -e "$jq_expr" >/dev/null 2>&1 && echo "yes" || echo "no")
    if [[ "$result" == "yes" ]]; then
        echo "ASSERTION FAILED: $msg" >&2
        return 1
    fi
}


# ═══════════════════════════════════════════════════════════════════
# CLI WRAPPERS
# ═══════════════════════════════════════════════════════════════════

CLI_OUTPUT=""
CLI_STDERR=""
CLI_EXIT=0

# cli <args...> — JSON output mode (stdout only, stderr captured separately)
cli() {
    local _stderr_file
    _stderr_file=$(mktemp "${TMPDIR:-/tmp}/cli-stderr-XXXXXX")
    set +e
    CLI_OUTPUT=$("$ORION_CLI" --server "$ORION_URL" --output json --yes --no-color "$@" 2>"$_stderr_file")
    CLI_EXIT=$?
    set -e
    CLI_STDERR=$(cat "$_stderr_file")
    rm -f "$_stderr_file"
    log_debug "cli $* => exit=$CLI_EXIT"
    log_debug "output: $CLI_OUTPUT"
}

# cli_quiet <args...> — quiet mode (IDs/counts only)
cli_quiet() {
    local _stderr_file
    _stderr_file=$(mktemp "${TMPDIR:-/tmp}/cli-stderr-XXXXXX")
    set +e
    CLI_OUTPUT=$("$ORION_CLI" --server "$ORION_URL" --quiet --yes --no-color "$@" 2>"$_stderr_file")
    CLI_EXIT=$?
    set -e
    CLI_STDERR=$(cat "$_stderr_file")
    rm -f "$_stderr_file"
    log_debug "cli_quiet $* => exit=$CLI_EXIT"
    log_debug "output: $CLI_OUTPUT"
}

# cli_raw <args...> — no output format flags (for commands without --output json support)
cli_raw() {
    set +e
    CLI_OUTPUT=$("$ORION_CLI" --server "$ORION_URL" --yes --no-color "$@" 2>&1)
    CLI_EXIT=$?
    set -e
    log_debug "cli_raw $* => exit=$CLI_EXIT"
    log_debug "output: $CLI_OUTPUT"
}


# ═══════════════════════════════════════════════════════════════════
# SERVER LIFECYCLE
# ═══════════════════════════════════════════════════════════════════

ORION_PID=""
ORION_DB_PATH=""
ORION_LOG_FILE=""
ORION_CONFIG_FILE=""

# find_free_port — returns an available TCP port
find_free_port() {
    if command -v python3 &>/dev/null; then
        python3 -c 'import socket; s=socket.socket(); s.bind(("",0)); print(s.getsockname()[1]); s.close()'
    else
        echo $(( RANDOM % 10000 + 20000 ))
    fi
}

# start_server — start Orion server for E2E testing
start_server() {
    local port
    if [[ "$E2E_PORT" -eq 0 ]]; then
        port=$(find_free_port)
    else
        port="$E2E_PORT"
    fi

    # BSD mktemp expands Xs only at the end of the template. A suffix after
    # XXXXXX produced a literal shared filename on macOS.
    ORION_DB_PATH=$(mktemp "${TMPDIR:-/tmp}/orion-e2e-db.XXXXXX")
    ORION_LOG_FILE=$(mktemp "${TMPDIR:-/tmp}/orion-e2e-log.XXXXXX")
    ORION_CONFIG_FILE=$(mktemp "${TMPDIR:-/tmp}/orion-e2e-config.XXXXXX")

    # The instance declares one var and one secret, so suite 08 can drive the
    # whole config-file → boot → serve path: `${VAR}` substitution into
    # `[vars]`, an `env://` reference in `[secrets]` resolved at startup, and
    # both reaching a workflow. The in-process tests build an `AppConfig` in
    # Rust and cannot cover any of that. Exported rather than written inline
    # so the config file holds a *reference*, which is the shape `[secrets]`
    # accepts — and in the reserved `ORION_SECRET_*` namespace, which the
    # startup scan for misspelled overrides deliberately skips.
    #
    # The heredoc below is unquoted, because `$port` and `$ORION_DB_PATH` have
    # to interpolate — so the `[vars]` placeholder is written `\${...}`.
    # Without the backslash bash substitutes it before the file is written and
    # the config holds a literal, which would leave Orion's own `${VAR}` pass —
    # the thing this suite exists to cover — never actually run.
    export E2E_VAR_TOPIC_PREFIX="eu-west"
    export ORION_SECRET_E2E_PARTNER_HMAC="e2e-partner-hmac-value"
    # The credentials the connector-secrets case's connections reference as
    # `env://` locators: a deployment carries the reference, and the node
    # resolves it from its own environment.
    export E2E_CONNECTOR_BEARER_TOKEN="super-secret-bearer-token-123"
    export E2E_CONNECTOR_BASIC_PASSWORD="s3cret-p4ss!"

    # Orion 1.0 config schema: storage.url (not path), trace_queue (not
    # queue), and [server] no longer takes a workers count.
    cat > "$ORION_CONFIG_FILE" <<TOMLEOF
[server]
host = "127.0.0.1"
port = $port
shutdown_drain_secs = 0
shutdown_force_timeout_secs = 2

[storage]
url = "sqlite:$ORION_DB_PATH?mode=rwc"
max_connections = 5

[trace_queue]
workers = 2
buffer_size = 100

[logging]
level = "warn"
format = "pretty"

[metrics]
enabled = false

[plugins]
enabled = true

[vars]
topic_prefix = "\${E2E_VAR_TOPIC_PREFIX}"
max_retries = 3

[secrets]
partner_hmac = "env://ORION_SECRET_E2E_PARTNER_HMAC"
TOMLEOF

    log_info "Starting Orion on port $port (db: $ORION_DB_PATH)"

    "$ORION_BIN" --config "$ORION_CONFIG_FILE" > "$ORION_LOG_FILE" 2>&1 &
    ORION_PID=$!
    ORION_URL="http://127.0.0.1:${port}"

    wait_for_server "$port" 15
}

# wait_for_server <port> <timeout_seconds>
wait_for_server() {
    local port="$1" timeout="${2:-15}" elapsed=0

    while [[ $elapsed -lt $timeout ]]; do
        if curl -sf "http://127.0.0.1:${port}/health" >/dev/null 2>&1; then
            log_info "Server ready (${elapsed}s)"
            return 0
        fi

        if ! kill -0 "$ORION_PID" 2>/dev/null; then
            log_fail "Server process died during startup"
            echo "=== Server Log ===" >&2
            cat "$ORION_LOG_FILE" >&2
            echo "=== End Server Log ===" >&2
            return 1
        fi

        sleep 0.5
        elapsed=$((elapsed + 1))
    done

    log_fail "Server did not become healthy within ${timeout}s"
    echo "=== Server Log (last 50 lines) ===" >&2
    tail -50 "$ORION_LOG_FILE" >&2
    echo "=== End Server Log ===" >&2
    stop_server
    return 1
}

# stop_server — gracefully shut down the Orion server
stop_server() {
    if [[ -n "$ORION_PID" ]] && kill -0 "$ORION_PID" 2>/dev/null; then
        log_info "Stopping Orion (PID: $ORION_PID)"

        kill -TERM "$ORION_PID" 2>/dev/null || true

        local waited=0
        while kill -0 "$ORION_PID" 2>/dev/null && [[ $waited -lt 10 ]]; do
            sleep 0.5
            waited=$((waited + 1))
        done

        if kill -0 "$ORION_PID" 2>/dev/null; then
            log_info "Force killing server"
            kill -9 "$ORION_PID" 2>/dev/null || true
        fi
        wait "$ORION_PID" 2>/dev/null || true
    fi

    ORION_PID=""

    [[ -n "${ORION_DB_PATH:-}" ]]     && rm -f "$ORION_DB_PATH" "${ORION_DB_PATH}-wal" "${ORION_DB_PATH}-shm"
    [[ -n "${ORION_LOG_FILE:-}" ]]    && rm -f "$ORION_LOG_FILE"
    [[ -n "${ORION_CONFIG_FILE:-}" ]] && rm -f "$ORION_CONFIG_FILE"
}

# show_server_log — print server log (useful for debugging)
show_server_log() {
    if [[ -f "${ORION_LOG_FILE:-}" ]]; then
        echo "=== Server Log ==="
        cat "$ORION_LOG_FILE"
        echo "=== End Server Log ==="
    fi
}


# ═══════════════════════════════════════════════════════════════════
# DEFINITION SETS AND DEPLOYMENTS
# ═══════════════════════════════════════════════════════════════════
#
# A node serves exactly its committed deployment head. A test writes a
# definition set (a directory of workflow/channel/connector JSON, plus any
# plugin.toml with its component), compiles it with `orion-server compile`
# and deploys it with `orion-cli deployments apply -d`. An apply *replaces*
# the head's members with the directory's: whatever the previous test
# deployed and this set does not carry stops serving. So a test that needs
# several services deploys them as one set, and no test cleans up after
# another.

# new_set [name] — a fresh definition-set directory inside the test's temp
# dir; prints its path.
new_set() {
    local dir="$TEST_TMPDIR/${1:-set}"
    mkdir -p "$dir"
    echo "$dir"
}

# set_workflow <set_dir> <workflow_id> <workflow json> — writes the workflow
# into the set under that id.
set_workflow() {
    local dir="$1" id="$2" body="$3"
    jq --arg id "$id" '. + {workflow_id: $id}' <<< "$body" > "$dir/workflow-$id.json"
}

# set_channel <set_dir> <name> <workflow_id> [channel_type] [config json] —
# an HTTP channel routed at /<name>, accepting POST, bound to the workflow.
# A 5th argument is merged in as the channel's `config` — how a test
# exercises a config-dependent ingress (`request.body_mode`, tracing).
set_channel() {
    local dir="$1" name="$2" workflow_id="$3" channel_type="${4:-sync}" config="${5:-}"
    local body
    body=$(jq -n --arg n "$name" --arg w "$workflow_id" --arg t "$channel_type" \
        '{channel_id: $n, name: $n, channel_type: $t, protocol: "http",
          workflow_id: $w, methods: ["POST"], route_pattern: ("/" + $n)}')
    if [[ -n "$config" && "$config" != "null" ]]; then
        body=$(jq --argjson cfg "$config" '. + {config: $cfg}' <<< "$body")
    fi
    echo "$body" > "$dir/channel-$name.json"
}

# set_plugin <set_dir> <manifest> — copies a plugin.toml and the component it
# names (read beside it) into the set, in a directory of its own.
set_plugin() {
    local dir="$1" manifest="$2"
    local component name
    component=$(sed -n 's/^component[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$manifest" | head -1)
    name=$(basename "$manifest" .toml)
    mkdir -p "$dir/plugin-$name"
    cp "$manifest" "$dir/plugin-$name/"
    cp "$(dirname "$manifest")/$component" "$dir/plugin-$name/"
}

COMPILE_OUTPUT=""
COMPILE_EXIT=0

# compile_set <set_dir> — `orion-server compile` into <set_dir>.dist.
# COMPILE_OUTPUT holds everything it printed; returns its exit code.
compile_set() {
    local dir="$1"
    rm -rf "$dir.dist"
    set +e
    COMPILE_OUTPUT=$("$ORION_BIN" compile "$dir" -o "$dir.dist" 2>&1)
    COMPILE_EXIT=$?
    set -e
    log_debug "compile $dir => exit=$COMPILE_EXIT"
    log_debug "output: $COMPILE_OUTPUT"
    return $COMPILE_EXIT
}

# apply_dist <dist_dir> — `deployments apply -d`, leaving CLI_OUTPUT,
# CLI_STDERR and CLI_EXIT for the caller.
#
# Plugins first: `POST /definitions` checks a workflow against the functions
# the node serves *now*, so a workflow calling a plugin function the head
# does not deploy yet is refused at upload, even though the same deployment
# carries the plugin. When an apply that carries plugins fails, the plugins
# are deployed alone (the compiled directory with every other member left
# out), and the whole directory is applied again. A directory the head
# already deploys applies nothing, so a re-run never takes this path.
apply_dist() {
    local dist="$1"
    cli deployments apply -d "$dist"
    if [[ "$CLI_EXIT" -ne 0 ]] && jq -e 'any(.members[]; .kind == "plugin")' "$dist/deployment.json" >/dev/null; then
        local stage="$dist.plugins"
        rm -rf "$stage"
        mkdir -p "$stage/connections"
        cp -R "$dist/definitions" "$stage/"
        jq '.members |= map(select(.kind == "plugin"))' "$dist/deployment.json" > "$stage/deployment.json"
        cli deployments apply -d "$stage"
        [[ "$CLI_EXIT" -eq 0 ]] || return 0
        cli deployments apply -d "$dist"
    fi
}

# deploy_set <set_dir> — compile, then apply. On a compile failure CLI_EXIT
# is the compiler's exit code and CLI_STDERR what it printed, so a caller
# asserts on one pair either way.
deploy_set() {
    local dir="$1"
    if ! compile_set "$dir"; then
        CLI_OUTPUT=""
        CLI_STDERR="$COMPILE_OUTPUT"
        CLI_EXIT=$COMPILE_EXIT
        return 0
    fi
    apply_dist "$dir.dist"
}

# deploy_ok <set_dir> — deploy_set, failing the test unless it deployed.
deploy_ok() {
    deploy_set "$1"
    if [[ "$CLI_EXIT" -ne 0 ]]; then
        echo "DEPLOY FAILED (exit $CLI_EXIT): $1" >&2
        echo "$CLI_STDERR" >&2
        return 1
    fi
}

# deploy_one <workflow json | file> <workflow_id> <channel> [channel_type] [config]
# — the common case: one workflow behind one HTTP channel, deployed as the
# whole head.
deploy_one() {
    local wf="$1" id="$2" channel="$3" channel_type="${4:-sync}" config="${5:-}"
    local dir
    dir=$(new_set "$id")
    [[ -f "$wf" ]] && wf=$(cat "$wf")
    set_workflow "$dir" "$id" "$wf"
    set_channel "$dir" "$channel" "$id" "$channel_type" "$config"
    deploy_ok "$dir"
}

# undeploy_all — commit an empty head, so nothing serves. A no-op when the
# head already deploys nothing. `compile` refuses an empty set, so this is
# the one place a manifest is applied with -f.
undeploy_all() {
    local summary head members
    summary=$(curl -sf "$ORION_URL/api/v2/admin/deployments") || return 1
    head=$(jq -r '.head_revision' <<< "$summary")
    members=$(jq -r --argjson h "$head" \
        '[.revisions[] | select(.revision == $h) | .manifest.members[]] | length' <<< "$summary")
    [[ "$head" -eq 0 || "$members" -eq 0 ]] && return 0

    local manifest digest
    manifest=$(mktemp "${TMPDIR:-/tmp}/orion-e2e-empty-XXXXXX")
    echo '{"schema_version": 2, "providers": [], "members": []}' > "$manifest"
    cli deployments plan -f "$manifest" --base-revision "$head"
    digest=$(jq -r '.reviewed_manifest_digest' <<< "$CLI_OUTPUT")
    cli deployments apply -f "$manifest" --base-revision "$head" \
        --reviewed-digest "$digest" --idempotency-key "e2e-undeploy-$head"
    rm -f "$manifest"
    [[ "$CLI_EXIT" -eq 0 ]] || { echo "undeploy failed: $CLI_STDERR" >&2; return 1; }
}

# definition_of <dist_dir> <kind> <id> — the compiled definition document a
# member names (`{"kind": …, "definition": {…}}`), read from the directory.
definition_of() {
    local dist="$1" kind="$2" id="$3" digest
    digest=$(jq -r --arg k "$kind" --arg i "$id" \
        '.members[] | select(.kind == $k and .id == $i) | .definition' "$dist/deployment.json")
    cat "$dist/definitions/${digest#sha256:}.json"
}

# test_definition <workflow definition json> <data json> — POST
# /definitions/test: run a bare workflow definition once on this node.
# There is no CLI verb for it; the answer lands in CLI_OUTPUT.
test_definition() {
    local body
    body=$(jq -n --argjson d "$1" --argjson x "$2" '{definition: $d, data: $x}')
    set +e
    CLI_OUTPUT=$(curl -sS -X POST "$ORION_URL/api/v2/admin/definitions/test" \
        -H 'content-type: application/json' -d "$body")
    CLI_EXIT=$?
    set -e
}


# ═══════════════════════════════════════════════════════════════════
# DATA-DRIVEN TEST CASES
# ═══════════════════════════════════════════════════════════════════

# _run_case_test <case_file> <test_index>
# Execute a single test from a case file and assert expectations.
_run_case_test() {
    local case_file="$1"
    local test_idx="$2"
    local t=".tests[$test_idx]"

    local response=""
    local mode
    mode=$(jq -r "$t | if has(\"dry_run\") then \"dry_run\"
                       elif has(\"read_connection\") then \"read_connection\"
                       else \"send\" end" "$case_file")
    local input
    input=$(jq -c "$t.input // {}" "$case_file")

    case "$mode" in
        dry_run)
            # The named workflow as the case's deployment compiled it, run
            # once through POST /definitions/test.
            local wf_id definition
            wf_id=$(jq -r "$t.dry_run" "$case_file")
            definition=$(definition_of "$_CASE_DIST" workflow "$wf_id" | jq -c '.definition')
            test_definition "$definition" "$input"
            response="$CLI_OUTPUT"
            ;;
        read_connection)
            # The stored connection revision the case's n-th connector
            # compiled to, as `connections list` answers it.
            local idx conn_id
            idx=$(jq -r "$t.read_connection" "$case_file")
            conn_id=$(jq -r ".connectors[$idx] | .id // .name" "$case_file")
            cli connections list
            response=$(jq -c --arg id "$conn_id" \
                '[.connections[] | select(.connection_id == $id)] | max_by(.revision)' <<< "$CLI_OUTPUT")
            ;;
        send)
            local channel raw
            channel=$(jq -r "$t.channel" "$case_file")
            # `"raw": true` sends the payload unwrapped — the only way to
            # address a channel with request.body_mode = "payload", which
            # would otherwise see the envelope itself as its data.
            raw=$(jq -r "$t.raw // false" "$case_file")
            if [[ "$raw" == "true" ]]; then
                cli send "$channel" --raw -d "$input"
            else
                cli send "$channel" -d "$input"
            fi
            response="$CLI_OUTPUT"
            ;;
    esac

    # expect_error asserts the command failed (a send to a channel the
    # deployment does not carry, say).
    local expect_error
    expect_error=$(jq -r "$t.expect_error // false" "$case_file")
    if [[ "$expect_error" == "true" ]]; then
        if [[ "$CLI_EXIT" -eq 0 ]]; then
            echo "ASSERTION FAILED: expected the command to fail, got exit 0" >&2
            echo "  response: $response" >&2
            return 1
        fi
        return 0
    fi

    local expect_keys
    expect_keys=$(jq -r "$t.expect | keys[]" "$case_file")

    while IFS= read -r jq_expr; do
        [[ -z "$jq_expr" ]] && continue
        local expected
        expected=$(jq -r "$t.expect[$(jq -aR <<< "$jq_expr")]" "$case_file")
        local actual
        actual=$(echo "$response" | jq -r "$jq_expr" 2>/dev/null) || {
            echo "ASSERTION FAILED: jq parse error on '$jq_expr'" >&2
            echo "  response: $response" >&2
            return 1
        }
        if [[ "$actual" != "$expected" ]]; then
            echo "ASSERTION FAILED: $jq_expr" >&2
            echo "  expected: $expected" >&2
            echo "  actual:   $actual" >&2
            echo "  response: $response" >&2
            return 1
        fi
    done <<< "$expect_keys"
}

# Global state for the data-driven runner (passed through run_test, which
# runs each test in a subshell).
_CASE_FILE=""
_CASE_TEST_IDX=0
_CASE_DIST=""

_case_test_wrapper() {
    _run_case_test "$_CASE_FILE" "$_CASE_TEST_IDX"
}

# _build_case_set <case_file> <set_dir> — write the case's definition set.
_build_case_set() {
    local case_file="$1" set_dir="$2"
    local case_dir
    case_dir="$(cd "$(dirname "$case_file")" && pwd)"

    # Whole definition sets — an example package as it ships, its own
    # channels included.
    local pkg
    while IFS= read -r pkg; do
        [[ -z "$pkg" ]] && continue
        cp -R "$case_dir/$pkg" "$set_dir/$(basename "$pkg")"
    done < <(jq -r '.packages // [] | .[]' "$case_file")

    local i count
    count=$(jq '.plugins // [] | length' "$case_file")
    for ((i=0; i<count; i++)); do
        set_plugin "$set_dir" "$case_dir/$(jq -r ".plugins[$i].manifest" "$case_file")"
    done

    count=$(jq '.connectors // [] | length' "$case_file")
    for ((i=0; i<count; i++)); do
        jq ".connectors[$i]" "$case_file" > "$set_dir/connector-$i.json"
    done

    # A workflow entry is {"file": "<path>"} (relative to the case file) or
    # an inline definition; an inline one without a workflow_id gets
    # `wf-<n>`.
    local first_workflow=""
    count=$(jq '.workflows // [] | length' "$case_file")
    for ((i=0; i<count; i++)); do
        local wf_file body
        wf_file=$(jq -r ".workflows[$i].file // empty" "$case_file")
        if [[ -n "$wf_file" ]]; then
            body=$(jq -c . "$case_dir/$wf_file")
        else
            body=$(jq -c --arg id "wf-$i" ".workflows[$i] | .workflow_id //= \$id" "$case_file")
        fi
        echo "$body" > "$set_dir/workflow-$i.json"
        [[ -n "$first_workflow" ]] || first_workflow=$(jq -r '.workflow_id' <<< "$body")
    done

    # Channels: explicit ones, each bound to the workflow it names (what a
    # channel_call composition needs); otherwise, for a case that brings its
    # own workflows, one sync channel per channel name the tests send to,
    # bound to the first workflow, with the case-level `channel_config`.
    count=$(jq '.channels // [] | length' "$case_file")
    if [[ "$count" -gt 0 ]]; then
        for ((i=0; i<count; i++)); do
            set_channel "$set_dir" "$(jq -r ".channels[$i].name" "$case_file")" \
                "$(jq -r ".channels[$i].workflow_id" "$case_file")"
        done
    elif [[ -n "$first_workflow" ]]; then
        local config ch
        config=$(jq -c '.channel_config // empty' "$case_file")
        while IFS= read -r ch; do
            [[ -z "$ch" ]] && continue
            set_channel "$set_dir" "$ch" "$first_workflow" sync "$config"
        done < <(jq -r '[.tests[].channel // empty] | unique | .[]' "$case_file")
    fi
}

# run_case_file <case_file>
# Build the case's definition set, deploy it, and run every test it lists.
run_case_file() {
    local case_file="$1"
    local case_name
    case_name=$(jq -r '.name' "$case_file")

    begin_suite "$case_name"

    local set_dir
    set_dir="$(mktemp -d "${TMPDIR:-/tmp}/orion-e2e-case-XXXXXX")/set"
    mkdir -p "$set_dir"
    _build_case_set "$case_file" "$set_dir"

    deploy_set "$set_dir"
    if [[ "$CLI_EXIT" -ne 0 ]]; then
        TESTS_RUN=$((TESTS_RUN + 1))
        TESTS_FAILED=$((TESTS_FAILED + 1))
        FAILED_NAMES+=("$case_name: deploy")
        log_fail "$case_name: the case's definition set did not deploy"
        echo "$CLI_STDERR" | sed 's/^/        /' >&2
        rm -rf "$(dirname "$set_dir")"
        end_suite
        return 0
    fi

    _CASE_FILE="$case_file"
    _CASE_DIST="$set_dir.dist"
    local test_count i
    test_count=$(jq '.tests | length' "$case_file")
    for ((i=0; i<test_count; i++)); do
        _CASE_TEST_IDX=$i
        run_test "$(jq -r ".tests[$i].name" "$case_file")" _case_test_wrapper
    done

    rm -rf "$(dirname "$set_dir")"
    end_suite
}

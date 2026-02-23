#!/usr/bin/env bash
# tests/e2e/helpers.sh — All shared E2E test functions
#
# Sourced by run.sh. Provides: framework, assertions, CLI wrappers,
# server lifecycle, and cleanup utilities.

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
E2E_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$E2E_DIR/../.." && pwd)"
FIXTURES_DIR="$E2E_DIR/fixtures"

ORION_BIN="${ORION_BIN:-$PROJECT_ROOT/target/debug/orion}"
ORION_CLI="${ORION_CLI:-$PROJECT_ROOT/target/debug/orion-cli}"

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
run_test() {
    local name="$1"
    local func="$2"
    TESTS_RUN=$((TESTS_RUN + 1))

    TEST_TMPDIR=$(mktemp -d "${TMPDIR:-/tmp}/orion-e2e-XXXXXX")

    set +e
    local output
    output=$("$func" 2>&1)
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

    ORION_DB_PATH=$(mktemp "${TMPDIR:-/tmp}/orion-e2e-XXXXXX.db")
    ORION_LOG_FILE=$(mktemp "${TMPDIR:-/tmp}/orion-e2e-XXXXXX.log")
    ORION_CONFIG_FILE=$(mktemp "${TMPDIR:-/tmp}/orion-e2e-XXXXXX.toml")

    cat > "$ORION_CONFIG_FILE" <<TOMLEOF
[server]
host = "127.0.0.1"
port = $port
workers = 2

[storage]
path = "$ORION_DB_PATH"
max_connections = 5

[queue]
workers = 2
buffer_size = 100

[logging]
level = "warn"
format = "pretty"

[metrics]
enabled = false
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
# CLEANUP
# ═══════════════════════════════════════════════════════════════════

# clean_all_rules — delete every rule from the server
clean_all_rules() {
    local ids
    ids=$("$ORION_CLI" --server "$ORION_URL" --quiet --yes --no-color rules list 2>/dev/null) || return 0

    while IFS= read -r id; do
        [[ -z "$id" ]] && continue
        "$ORION_CLI" --server "$ORION_URL" --quiet --yes --no-color rules delete "$id" 2>/dev/null || true
    done <<< "$ids"
}

# clean_all_connectors — delete all non-internal connectors
clean_all_connectors() {
    local json_output
    json_output=$("$ORION_CLI" --server "$ORION_URL" --output json --yes --no-color connectors list 2>/dev/null) || return 0

    local ids
    ids=$(echo "$json_output" | jq -r '.data[]? | select(.name != "__data_api__") | .id' 2>/dev/null) || return 0

    while IFS= read -r id; do
        [[ -z "$id" ]] && continue
        "$ORION_CLI" --server "$ORION_URL" --quiet --yes --no-color connectors delete "$id" 2>/dev/null || true
    done <<< "$ids"
}

# reset_server_state — clean everything and reload engine
reset_server_state() {
    clean_all_rules
    clean_all_connectors
    "$ORION_CLI" --server "$ORION_URL" --quiet --yes --no-color engine reload 2>/dev/null || true
}

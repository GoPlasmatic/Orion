#!/usr/bin/env bash
# tests/benchmark/bench.sh — Performance benchmarking suite for Orion
#
# Uses `hey` HTTP load generator to measure throughput, latency, and
# concurrency behaviour across 7 scenarios.
#
# Usage:
#   ./tests/benchmark/bench.sh                        # Run all local scenarios
#   ./tests/benchmark/bench.sh baseline simple        # Run specific scenarios
#   BENCH_RELEASE=1 BENCH_DURATION=30s ./tests/benchmark/bench.sh
#   ./tests/benchmark/bench.sh cluster                # Opt-in: drive the HA compose
#                                                     # stack through its LB (needs
#                                                     # docker-compose.ha.yml up)
#
# Dependencies: hey, jq, curl
# Install hey: brew install hey

set -euo pipefail

# ═══════════════════════════════════════════════════════════════════
# PATHS & CONFIGURATION
# ═══════════════════════════════════════════════════════════════════

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$BENCH_DIR/../.." && pwd)"
FIXTURES_DIR="$BENCH_DIR/fixtures"

BENCH_DURATION="${BENCH_DURATION:-10s}"
BENCH_CONCURRENCY="${BENCH_CONCURRENCY:-50}"
BENCH_OUTPUT_DIR="${BENCH_OUTPUT_DIR:-}"

# Binary selection
if [[ -n "${BENCH_RELEASE:-}" ]]; then
    ORION_BIN="$PROJECT_ROOT/target/release/orion-server"
    BUILD_PROFILE="release"
else
    ORION_BIN="$PROJECT_ROOT/target/debug/orion-server"
    BUILD_PROFILE="debug"
fi

# ═══════════════════════════════════════════════════════════════════
# COLORS (sourced pattern from e2e/helpers.sh)
# ═══════════════════════════════════════════════════════════════════

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

# Logs go to stderr, not stdout.
#
# `create_workflow` returns the new id by echoing it, so callers read it with
# `$(...)` — and command substitution captures stdout. A log line emitted
# anywhere inside that call (a retry notice, a warning) would be captured as
# part of the id, and the channel built from it then fails to activate with no
# hint as to why. Keeping the two streams apart is what makes the id reliable.
log_info()  { echo -e "${BLUE}[BENCH]${RESET} $*" >&2; }
log_ok()    { echo -e "${GREEN}[BENCH]${RESET} $*" >&2; }
log_warn()  { echo -e "${YELLOW}[BENCH]${RESET} $*" >&2; }
log_error() { echo -e "${RED}[BENCH]${RESET} $*" >&2; }

# ═══════════════════════════════════════════════════════════════════
# DEPENDENCY CHECKS
# ═══════════════════════════════════════════════════════════════════

check_dependencies() {
    local missing=()
    for cmd in hey jq curl; do
        if ! command -v "$cmd" &>/dev/null; then
            missing+=("$cmd")
        fi
    done

    if [[ ${#missing[@]} -gt 0 ]]; then
        log_error "Missing dependencies: ${missing[*]}"
        log_error "Install with: brew install ${missing[*]}"
        exit 1
    fi
}

# ═══════════════════════════════════════════════════════════════════
# BUILD
# ═══════════════════════════════════════════════════════════════════

build_orion() {
    if [[ -n "${BENCH_SKIP_BUILD:-}" ]]; then
        log_info "Skipping build (BENCH_SKIP_BUILD set)"
        if [[ ! -f "$ORION_BIN" ]]; then
            log_error "Binary not found: $ORION_BIN"
            exit 1
        fi
        return 0
    fi

    log_info "Building Orion ($BUILD_PROFILE)..."
    if [[ "$BUILD_PROFILE" == "release" ]]; then
        cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" 2>&1 | tail -5
    else
        cargo build --manifest-path "$PROJECT_ROOT/Cargo.toml" 2>&1 | tail -5
    fi
    log_ok "Build complete"
}

# ═══════════════════════════════════════════════════════════════════
# SERVER LIFECYCLE
# ═══════════════════════════════════════════════════════════════════

BENCH_PID=""
BENCH_PORT=""
BENCH_URL=""
# Extra curl args for the admin API. Empty for the local server (no auth);
# the cluster scenario points BENCH_URL at a stack that enforces admin auth
# and sets a Bearer header here. Expanded with the set -u-safe idiom because
# macOS bash 3.2 treats an empty array as unbound.
CURL_AUTH=()
BENCH_TMP_DIR=""
BENCH_DB_PATH=""
BENCH_LOG_FILE=""
BENCH_CONFIG_FILE=""

find_free_port() {
    if command -v python3 &>/dev/null; then
        python3 -c 'import socket; s=socket.socket(); s.bind(("",0)); print(s.getsockname()[1]); s.close()'
    else
        echo $(( RANDOM % 10000 + 20000 ))
    fi
}

start_bench_server() {
    BENCH_PORT=$(find_free_port)
    BENCH_URL="http://127.0.0.1:${BENCH_PORT}"

    # One private directory, named files inside it.
    #
    # These were three `mktemp .../orion-bench-XXXXXX.db` calls, which is not
    # portable: BSD/macOS mktemp only substitutes X's at the *end* of the
    # template, so a suffixed template is taken literally and every run created
    # the same `orion-bench-XXXXXX.db`. That worked only as long as each run
    # reached its cleanup trap — the moment one was killed, the literal file
    # survived and every subsequent run died at startup with "mkstemp failed:
    # File exists", which reads like a full disk rather than a leftover file.
    BENCH_TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/orion-bench-XXXXXX")
    BENCH_DB_PATH="$BENCH_TMP_DIR/bench.db"
    BENCH_LOG_FILE="$BENCH_TMP_DIR/bench.log"
    BENCH_CONFIG_FILE="$BENCH_TMP_DIR/bench.toml"

    cat > "$BENCH_CONFIG_FILE" <<TOMLEOF
[server]
host = "127.0.0.1"
port = $BENCH_PORT

[storage]
url = "sqlite:$BENCH_DB_PATH"
max_connections = 10

[trace_queue]
workers = 4
buffer_size = 200

[logging]
level = "error"
format = "pretty"

[metrics]
enabled = false
TOMLEOF

    log_info "Starting Orion on port $BENCH_PORT ($BUILD_PROFILE mode)"

    "$ORION_BIN" --config "$BENCH_CONFIG_FILE" > "$BENCH_LOG_FILE" 2>&1 &
    BENCH_PID=$!

    # Wait for server to be ready
    local elapsed=0
    while [[ $elapsed -lt 30 ]]; do
        if curl -sf "${BENCH_URL}/health" >/dev/null 2>&1; then
            log_ok "Server ready (${elapsed}s)"
            return 0
        fi

        if ! kill -0 "$BENCH_PID" 2>/dev/null; then
            log_error "Server process died during startup"
            cat "$BENCH_LOG_FILE" >&2
            exit 1
        fi

        sleep 0.5
        elapsed=$((elapsed + 1))
    done

    log_error "Server did not become healthy within 30s"
    tail -20 "$BENCH_LOG_FILE" >&2
    stop_bench_server
    exit 1
}

stop_bench_server() {
    if [[ -n "$BENCH_PID" ]] && kill -0 "$BENCH_PID" 2>/dev/null; then
        log_info "Stopping Orion (PID: $BENCH_PID)"
        kill -TERM "$BENCH_PID" 2>/dev/null || true

        local waited=0
        while kill -0 "$BENCH_PID" 2>/dev/null && [[ $waited -lt 10 ]]; do
            sleep 0.5
            waited=$((waited + 1))
        done

        if kill -0 "$BENCH_PID" 2>/dev/null; then
            kill -9 "$BENCH_PID" 2>/dev/null || true
        fi
    fi

    BENCH_PID=""
    # The whole directory goes, so the WAL/SHM sidecars cannot be left behind.
    [[ -n "${BENCH_TMP_DIR:-}" ]] && rm -rf "$BENCH_TMP_DIR"
    BENCH_TMP_DIR=""
}

# ═══════════════════════════════════════════════════════════════════
# WORKFLOW MANAGEMENT (curl-based, no orion-cli dependency)
# ═══════════════════════════════════════════════════════════════════

create_workflow() {
    local workflow_file="$1"
    local response
    response=$(curl -sf ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} -X POST "${BENCH_URL}/api/v1/admin/workflows" \
        -H "Content-Type: application/json" \
        -d @"$workflow_file" 2>/dev/null) || {
        log_error "Failed to create workflow from $workflow_file"
        return 1
    }

    local workflow_id
    workflow_id=$(echo "$response" | jq -r '.data.workflow_id // empty')
    if [[ -z "$workflow_id" ]]; then
        log_error "No workflow ID in response: $response"
        return 1
    fi
    echo "$workflow_id"
}

activate_workflow() {
    local workflow_id="$1"
    curl -sf ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} -X PATCH "${BENCH_URL}/api/v1/admin/workflows/${workflow_id}/status" \
        -H "Content-Type: application/json" \
        -d '{"status": "active"}' >/dev/null 2>&1 || {
        log_error "Failed to activate workflow $workflow_id"
        return 1
    }
}

create_and_activate_workflow() {
    local workflow_file="$1"
    local workflow_id
    workflow_id=$(create_workflow "$workflow_file") || return 1
    activate_workflow "$workflow_id" || return 1
    echo "$workflow_id"
}

import_workflows() {
    local workflows_file="$1"
    local response
    response=$(curl -sf ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} -X POST "${BENCH_URL}/api/v1/admin/workflows/import" \
        -H "Content-Type: application/json" \
        -d @"$workflows_file" 2>/dev/null) || {
        log_error "Failed to import workflows from $workflows_file"
        return 1
    }

    local imported
    # T41: the import response is enveloped — {"data":{"imported":N,...}}.
    imported=$(echo "$response" | jq -r '.data.imported // 0')
    log_info "Imported $imported workflows"

    # Activate all imported (draft) workflows
    local workflows_json
    workflows_json=$(curl -sf ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} "${BENCH_URL}/api/v1/admin/workflows?status=draft" 2>/dev/null) || return 0

    local ids
    ids=$(echo "$workflows_json" | jq -r '.data[]?.workflow_id // empty' 2>/dev/null) || return 0

    while IFS= read -r id; do
        [[ -z "$id" ]] && continue
        activate_workflow "$id"
    done <<< "$ids"
}

reload_engine() {
    curl -sf ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} -X POST "${BENCH_URL}/api/v1/admin/engine/reload" >/dev/null 2>&1 || true
}

# Bind a channel to a workflow.
#
# 1.0 routes channel -> workflow (`channels.workflow_id`). Without a channel
# the data plane answers 404 for an unregistered name, so every scenario below
# measured error responses until this was added.
create_and_activate_channel() {
    local channel_name="$1"
    local workflow_id="$2"

    if [[ -z "$workflow_id" || "$workflow_id" == *" "* ]]; then
        log_error "Refusing to create channel '$channel_name': bad workflow id '$workflow_id'"
        return 1
    fi

    # `protocol: "http"` requires both `methods` and `route_pattern`; a channel
    # missing either is refused at create.
    local payload
    payload=$(jq -n --arg n "$channel_name" --arg w "$workflow_id" \
        '{channel_id: $n, name: $n, channel_type: "sync", protocol: "http",
          methods: ["POST"], route_pattern: ("/" + $n), workflow_id: $w}')

    curl -sf ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} -X POST "${BENCH_URL}/api/v1/admin/channels" \
        -H "Content-Type: application/json" \
        -d "$payload" >/dev/null 2>&1 || {
        log_error "Failed to create channel $channel_name"
        return 1
    }
    curl -sf ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} -X PATCH "${BENCH_URL}/api/v1/admin/channels/${channel_name}/status" \
        -H "Content-Type: application/json" \
        -d '{"status": "active"}' >/dev/null 2>&1 || {
        log_error "Failed to activate channel $channel_name"
        return 1
    }
}

# Remove every channel, so a scenario's channel does not outlive it.
clear_channels() {
    local channels_json
    channels_json=$(curl -sf ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} "${BENCH_URL}/api/v1/admin/channels" 2>/dev/null) || return 0

    local ids
    ids=$(echo "$channels_json" | jq -r '.data[]?.channel_id // empty' 2>/dev/null) || return 0

    while IFS= read -r id; do
        [[ -z "$id" ]] && continue
        curl -sf ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} -X DELETE "${BENCH_URL}/api/v1/admin/channels/${id}" >/dev/null 2>&1 || true
    done <<< "$ids"
}

clear_workflows() {
    clear_channels

    local workflows_json
    workflows_json=$(curl -sf ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} "${BENCH_URL}/api/v1/admin/workflows" 2>/dev/null) || return 0

    local ids
    ids=$(echo "$workflows_json" | jq -r '.data[]?.workflow_id // empty' 2>/dev/null) || return 0

    while IFS= read -r id; do
        [[ -z "$id" ]] && continue
        curl -sf ${CURL_AUTH[@]+"${CURL_AUTH[@]}"} -X DELETE "${BENCH_URL}/api/v1/admin/workflows/${id}" >/dev/null 2>&1 || true
    done <<< "$ids"

    reload_engine
}

# ═══════════════════════════════════════════════════════════════════
# HEY OUTPUT PARSING
# ═══════════════════════════════════════════════════════════════════

# Parse hey text output and extract key metrics
# Sets: RESULT_RPS, RESULT_AVG_MS, RESULT_P99_MS, RESULT_ERRORS
parse_hey_output() {
    local hey_output="$1"

    # Requests/sec
    RESULT_RPS=$(echo "$hey_output" | grep 'Requests/sec:' | awk '{print $2}')
    RESULT_RPS="${RESULT_RPS:-0}"

    # Average latency (hey reports in seconds, convert to ms)
    local avg_secs
    avg_secs=$(echo "$hey_output" | grep 'Average:' | head -1 | awk '{print $2}')
    if [[ -n "$avg_secs" ]] && [[ "$avg_secs" != "0" ]]; then
        RESULT_AVG_MS=$(echo "$avg_secs" | awk '{printf "%.2f", $1 * 1000}')
    else
        RESULT_AVG_MS="0.00"
    fi

    # P99 latency (hey reports in seconds as "99% in X.XXXX secs", convert to ms)
    local p99_secs
    p99_secs=$(echo "$hey_output" | grep -E '99(%|%%)' | head -1 | awk '{for(i=1;i<=NF;i++) if($i ~ /^[0-9]+\./) {print $i; exit}}')
    if [[ -n "$p99_secs" ]] && [[ "$p99_secs" != "0" ]]; then
        RESULT_P99_MS=$(echo "$p99_secs" | awk '{printf "%.2f", $1 * 1000}')
    else
        RESULT_P99_MS="0.00"
    fi

    # Error count: every response that was not a 200/202, plus every request
    # that never got a response at all.
    #
    # hey writes two tallies, and they are keyed differently:
    #
    #   Status code distribution:
    #     [200]	103688 responses          <- bracket is the CODE, count is $2
    #   Error distribution:
    #     [37]	Get http://...: EOF       <- bracket is the COUNT
    #
    # Both were read wrong. The status arm took `$NF`, which is the word
    # "responses" — so a run of 503s fed a bare word to `$(( ))`, where under
    # `set -u` it is an unbound variable and aborts the whole benchmark. The
    # error arm counted matching *lines*, so 1000 refused connections reported
    # as 1. A benchmark whose error column reads 0 while the server is failing
    # is worse than one with no error column.
    RESULT_ERRORS=0

    local line code count
    while IFS= read -r line; do
        [[ "$line" =~ \[([0-9]+)\][[:space:]]+([0-9]+) ]] || continue
        code="${BASH_REMATCH[1]}"
        count="${BASH_REMATCH[2]}"
        if [[ "$code" != "200" ]] && [[ "$code" != "202" ]]; then
            RESULT_ERRORS=$((RESULT_ERRORS + count))
        fi
    done <<< "$(echo "$hey_output" | sed -n '/Status code distribution/,/^$/p' || true)"

    while IFS= read -r line; do
        [[ "$line" =~ ^[[:space:]]*\[([0-9]+)\] ]] || continue
        count="${BASH_REMATCH[1]}"
        RESULT_ERRORS=$((RESULT_ERRORS + count))
    done <<< "$(echo "$hey_output" | sed -n '/Error distribution/,/^$/p' || true)"
}

# Run hey and parse results. Optionally saves raw output.
# Usage: run_hey <method> <url> [body_file] [concurrency] [duration]
run_hey() {
    local method="$1"
    local url="$2"
    local body_file="${3:-}"
    local concurrency="${4:-$BENCH_CONCURRENCY}"
    local duration="${5:-$BENCH_DURATION}"

    local hey_args=(-z "$duration" -c "$concurrency" -m "$method")

    if [[ -n "$body_file" ]]; then
        hey_args+=(-T "application/json" -D "$body_file")
    fi

    hey_args+=("$url")

    local hey_output
    hey_output=$(hey "${hey_args[@]}" 2>&1)

    # Save raw output if requested
    if [[ -n "$BENCH_OUTPUT_DIR" ]] && [[ -n "${CURRENT_SCENARIO:-}" ]]; then
        mkdir -p "$BENCH_OUTPUT_DIR"
        echo "$hey_output" > "${BENCH_OUTPUT_DIR}/${CURRENT_SCENARIO}.txt"
    fi

    parse_hey_output "$hey_output"
}

# ═══════════════════════════════════════════════════════════════════
# RESULTS TABLE
# ═══════════════════════════════════════════════════════════════════

# Arrays to accumulate results
RESULT_NAMES=()
RESULT_RPS_LIST=()
RESULT_AVG_LIST=()
RESULT_P99_LIST=()
RESULT_ERR_LIST=()

record_result() {
    local name="$1" rps="$2" avg="$3" p99="$4" errors="$5"
    RESULT_NAMES+=("$name")
    RESULT_RPS_LIST+=("$rps")
    RESULT_AVG_LIST+=("$avg")
    RESULT_P99_LIST+=("$p99")
    RESULT_ERR_LIST+=("$errors")

    # Print inline progress
    printf "  ${GREEN}%-42s${RESET} %10s req/s  avg=%s ms  p99=%s ms  errors=%s\n" \
        "$name" "$rps" "$avg" "$p99" "$errors"
}

print_results_table() {
    if [[ ${#RESULT_NAMES[@]} -eq 0 ]]; then
        log_warn "No benchmark results to display"
        return
    fi

    echo ""
    echo -e "${BOLD}${CYAN}Benchmark Results${RESET}  ${DIM}(duration=${BENCH_DURATION}, concurrency=${BENCH_CONCURRENCY}, profile=${BUILD_PROFILE})${RESET}"
    echo ""
    echo "╔══════════════════════════════════════════╦════════════╦══════════╦══════════╦════════╗"
    echo "║ Scenario                                 ║  Req/sec   ║ Avg (ms) ║ P99 (ms) ║ Errors ║"
    echo "╠══════════════════════════════════════════╬════════════╬══════════╬══════════╬════════╣"

    for i in "${!RESULT_NAMES[@]}"; do
        printf "║ %-40s ║ %10s ║ %8s ║ %8s ║ %6s ║\n" \
            "${RESULT_NAMES[$i]}" \
            "${RESULT_RPS_LIST[$i]}" \
            "${RESULT_AVG_LIST[$i]}" \
            "${RESULT_P99_LIST[$i]}" \
            "${RESULT_ERR_LIST[$i]}"
    done

    echo "╚══════════════════════════════════════════╩════════════╩══════════╩══════════╩════════╝"
    echo ""
}

# ═══════════════════════════════════════════════════════════════════
# BENCHMARK SCENARIOS
# ═══════════════════════════════════════════════════════════════════

# A: Health check baseline — raw Axum overhead
scenario_baseline() {
    log_info "A: Health check baseline"
    CURRENT_SCENARIO="A_health_baseline"

    run_hey GET "${BENCH_URL}/health"
    record_result "A: Health check baseline" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
}

# B: Simple workflow — 1 workflow, 1 log task
scenario_simple() {
    log_info "B: Simple workflow (1 log task)"
    CURRENT_SCENARIO="B_simple_workflow"

    clear_workflows
    local wf
    wf=$(create_and_activate_workflow "$FIXTURES_DIR/workflows/bench_simple_log.json")
    create_and_activate_channel "bench" "$wf"

    run_hey POST "${BENCH_URL}/api/v1/data/bench" "$FIXTURES_DIR/data/simple_payload.json"
    record_result "B: Simple workflow (1 log task)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
}

# C: Complex workflow — 4-task ecommerce workflow
scenario_complex() {
    log_info "C: Complex workflow (4 tasks)"
    CURRENT_SCENARIO="C_complex_workflow"

    clear_workflows
    local wf
    wf=$(create_and_activate_workflow "$FIXTURES_DIR/workflows/bench_complex_ecommerce.json")
    create_and_activate_channel "orders" "$wf"

    run_hey POST "${BENCH_URL}/api/v1/data/orders" "$FIXTURES_DIR/data/complex_payload.json"
    record_result "C: Complex workflow (4 tasks)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
}

# D: Loaded estate — 12 workflows, each behind its own channel
#
# This scenario used to be "12 workflows on the same channel", which measured
# the engine picking one of 12 candidates by condition. 1.0 has no such state to
# construct: a channel names exactly one `workflow_id`, and `activate` archives
# the versions it supersedes — a full rollout archives all of them, a partial
# rollout keeps only the primary — so the candidate set for a channel is at most
# two (primary + canary), never twelve.
#
# It imported the 12 anyway and pointed one channel at `.data[0]`, leaving the
# other 11 referenced by nothing. `build_engine_workflows` iterates *channels*,
# so those 11 were never converted, never loaded, and never evaluated: the
# scenario was scenario B with a larger `workflows` table, and it scored like B
# (81,063 vs 82,454 req/s) because it *was* B.
#
# What "many workflows" means in 1.0 is many channels, so that is what this
# builds: 12 active channels over 12 active workflows, load against one of them.
# It measures route resolution and registry lookup against a populated estate
# rather than a single-entry one. Not comparable to the pre-1.0 D — the thing
# that one measured no longer exists.
scenario_multi() {
    log_info "D: Loaded estate (12 workflows, 12 channels)"
    CURRENT_SCENARIO="D_multi_workflows"

    clear_workflows
    import_workflows "$FIXTURES_DIR/workflows/bench_multi_rules.json"

    local ids
    ids=$(curl -sf "${BENCH_URL}/api/v1/admin/workflows?status=active&limit=100" 2>/dev/null \
        | jq -r '.data[]?.workflow_id // empty')

    local n=0
    while IFS= read -r wf; do
        [[ -z "$wf" ]] && continue
        # `bench` first, so the URL under load is the same one scenario B uses
        # and the two differ only in how much else is registered.
        local name="bench"
        [[ $n -gt 0 ]] && name="bench-${n}"
        create_and_activate_channel "$name" "$wf" || true
        n=$((n + 1))
    done <<< "$ids"

    if [[ $n -lt 2 ]]; then
        log_error "D: expected a multi-channel estate, built $n channel(s) — check the import"
        return 1
    fi
    log_info "  Estate: $n active channels"

    run_hey POST "${BENCH_URL}/api/v1/data/bench" "$FIXTURES_DIR/data/simple_payload.json"
    record_result "D: Loaded estate (${n} channels)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
}

# E: Concurrency scaling — c=1, 10, 50, 100
scenario_concurrency() {
    log_info "E: Concurrency scaling"

    clear_workflows
    local wf
    wf=$(create_and_activate_workflow "$FIXTURES_DIR/workflows/bench_simple_log.json")
    create_and_activate_channel "bench" "$wf"

    for c in 1 10 50 100; do
        CURRENT_SCENARIO="E_concurrency_${c}"
        log_info "  Concurrency: $c"

        run_hey POST "${BENCH_URL}/api/v1/data/bench" "$FIXTURES_DIR/data/simple_payload.json" "$c"
        record_result "E: Concurrency c=${c}" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
    done
}

# F: Reload under load — hey in background + engine reload every 500ms
scenario_reload() {
    log_info "F: Reload under load"
    CURRENT_SCENARIO="F_reload_under_load"

    clear_workflows
    local wf
    wf=$(create_and_activate_workflow "$FIXTURES_DIR/workflows/bench_simple_log.json")
    create_and_activate_channel "bench" "$wf"

    # Start hey in background
    local hey_output_file="$BENCH_TMP_DIR/hey-reload.txt"

    hey -z "$BENCH_DURATION" -c "$BENCH_CONCURRENCY" -m POST \
        -T "application/json" \
        -D "$FIXTURES_DIR/data/simple_payload.json" \
        "${BENCH_URL}/api/v1/data/bench" > "$hey_output_file" 2>&1 &
    local hey_pid=$!

    # Reload engine every 500ms while hey runs
    local reload_count=0
    while kill -0 "$hey_pid" 2>/dev/null; do
        sleep 0.5
        curl -sf -X POST "${BENCH_URL}/api/v1/admin/engine/reload" >/dev/null 2>&1 || true
        reload_count=$((reload_count + 1))
    done

    wait "$hey_pid" 2>/dev/null || true

    local hey_output
    hey_output=$(cat "$hey_output_file")
    rm -f "$hey_output_file"

    parse_hey_output "$hey_output"
    record_result "F: Reload under load (${reload_count}x)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"

    # Save raw output if requested
    if [[ -n "$BENCH_OUTPUT_DIR" ]]; then
        mkdir -p "$BENCH_OUTPUT_DIR"
        echo "$hey_output" > "${BENCH_OUTPUT_DIR}/${CURRENT_SCENARIO}.txt"
    fi
}

# G: Cluster through the load balancer — the HA compose stack (opt-in)
#
# Runs the same simple workflow as B, but through docker-compose.ha.yml's
# topology: nginx → 2 replicas in cluster mode → shared Postgres + Redis.
# This is the half the single-process scenarios cannot measure — the
# per-request price of cluster mode (shared Redis dedup / rate-limit /
# response-cache round trips ride the hot path) plus the LB hop, and what a
# second node actually buys. Compare G against B for per-node overhead;
# re-run against a scaled stack for efficiency at N=2/3.
#
# Opt-in and never in the default set: it needs Docker and a running stack,
# and its numbers are about the topology, not the binary. Start it first:
#
#   export ORION_ADMIN_API_KEYS="$(openssl rand -hex 32)"
#   docker compose -f docker-compose.ha.yml up -d --wait
scenario_cluster() {
    log_info "G: Cluster via load balancer (simple workflow)"
    CURRENT_SCENARIO="G_cluster_lb"

    local cluster_url="${BENCH_CLUSTER_URL:-http://localhost:8080}"
    local admin_key="${ORION_ADMIN_API_KEYS:-}"
    admin_key="${admin_key%%,*}" # the stack takes a comma-separated list; one is enough

    if ! curl -sf "${cluster_url}/healthz" >/dev/null 2>&1; then
        log_error "Nothing healthy at ${cluster_url}. Start the stack first:"
        log_error '  export ORION_ADMIN_API_KEYS="$(openssl rand -hex 32)"'
        log_error '  docker compose -f docker-compose.ha.yml up -d --wait'
        log_error "(or point BENCH_CLUSTER_URL at your load balancer)"
        return 1
    fi
    if [[ -z "$admin_key" ]]; then
        log_error "ORION_ADMIN_API_KEYS is not set; the HA stack enforces admin auth"
        return 1
    fi

    # Aim every admin helper at the cluster, authenticated. The data-plane
    # load itself needs no credential. The stack is a benchmarking/drill
    # topology, so clearing its estate is acceptable the same way it is on
    # the throwaway local server.
    local saved_url="$BENCH_URL"
    BENCH_URL="$cluster_url"
    CURL_AUTH=(-H "Authorization: Bearer ${admin_key}")

    clear_workflows
    local wf
    wf=$(create_and_activate_workflow "$FIXTURES_DIR/workflows/bench_simple_log.json")
    create_and_activate_channel "bench-cluster" "$wf"
    # Activation propagates over the config epoch (2s poll default). Wait it
    # out, or the not-yet-synced replica answers 404s into the numbers.
    sleep 3

    run_hey POST "${cluster_url}/api/v1/data/bench-cluster" "$FIXTURES_DIR/data/simple_payload.json"
    record_result "G: Cluster via LB (simple workflow)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"

    # Leave the stack as found.
    clear_workflows
    CURL_AUTH=()
    BENCH_URL="$saved_url"
}

# ═══════════════════════════════════════════════════════════════════
# SCENARIO REGISTRY
# ═══════════════════════════════════════════════════════════════════

# `cluster` (G) is deliberately absent: it needs Docker and a running HA
# stack, so it only runs when named explicitly.
ALL_SCENARIOS=(baseline simple complex multi concurrency reload)

run_scenario() {
    local name="$1"
    case "$name" in
        baseline)    scenario_baseline ;;
        simple)      scenario_simple ;;
        complex)     scenario_complex ;;
        multi)       scenario_multi ;;
        concurrency) scenario_concurrency ;;
        reload)      scenario_reload ;;
        cluster)     scenario_cluster ;;
        *)
            log_error "Unknown scenario: $name"
            log_error "Available: ${ALL_SCENARIOS[*]}, plus opt-in: cluster"
            return 1
            ;;
    esac
}

# ═══════════════════════════════════════════════════════════════════
# MAIN
# ═══════════════════════════════════════════════════════════════════

main() {
    local start_time
    start_time=$(date +%s)

    echo ""
    echo -e "${BOLD}${CYAN}Orion Performance Benchmark${RESET}"
    echo -e "${DIM}Duration: ${BENCH_DURATION} | Concurrency: ${BENCH_CONCURRENCY} | Profile: ${BUILD_PROFILE}${RESET}"
    echo ""

    # Determine which scenarios to run
    local scenarios=()
    if [[ $# -gt 0 ]]; then
        scenarios=("$@")
    else
        scenarios=("${ALL_SCENARIOS[@]}")
    fi

    # `cluster` drives an external stack through its LB; build and start the
    # local server only when a local scenario asked for it.
    local need_local=0
    local sc
    for sc in "${scenarios[@]}"; do
        [[ "$sc" != "cluster" ]] && need_local=1
    done

    check_dependencies
    if [[ "$need_local" -eq 1 ]]; then
        build_orion
        start_bench_server

        # Ensure cleanup on exit
        trap stop_bench_server EXIT
    fi

    echo ""
    log_info "Running ${#scenarios[@]} scenario(s): ${scenarios[*]}"
    echo ""

    for scenario in "${scenarios[@]}"; do
        run_scenario "$scenario"
        echo ""
    done

    print_results_table

    local elapsed=$(( $(date +%s) - start_time ))
    log_ok "Benchmark complete in ${elapsed}s"
}

main "$@"

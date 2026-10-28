#!/usr/bin/env bash
# tests/benchmark/bench.sh — Performance benchmarking suite for Orion
#
# Uses `hey` HTTP load generator to measure throughput, latency, and
# concurrency behaviour across 8 scenarios.
#
# Usage:
#   ./crates/orion-server/tests/benchmark/bench.sh                        # Run all local scenarios
#   ./crates/orion-server/tests/benchmark/bench.sh baseline simple        # Run specific scenarios
#   BENCH_RELEASE=1 BENCH_DURATION=30s ./crates/orion-server/tests/benchmark/bench.sh
#   ./crates/orion-server/tests/benchmark/bench.sh cluster                # Opt-in: drive the HA compose
#                                                     # stack through its LB (needs
#                                                     # docker-compose.ha.yml up)
#
# Dependencies: hey, jq, curl, python3, matching orion-server and orion-cli
# Install hey: brew install hey

set -euo pipefail

# ═══════════════════════════════════════════════════════════════════
# PATHS & CONFIGURATION
# ═══════════════════════════════════════════════════════════════════

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$BENCH_DIR/../../../.." && pwd)"
FIXTURES_DIR="$BENCH_DIR/fixtures"

BENCH_DURATION="${BENCH_DURATION:-10s}"
BENCH_CONCURRENCY="${BENCH_CONCURRENCY:-50}"
BENCH_OUTPUT_DIR="${BENCH_OUTPUT_DIR:-}"

# Build both binaries from the same checkout. Overrides support candidate builds.
if [[ -n "${BENCH_RELEASE:-}" ]]; then
    BUILD_PROFILE="release"
else
    BUILD_PROFILE="debug"
fi
ORION_BIN="${ORION_BIN:-$PROJECT_ROOT/target/$BUILD_PROFILE/orion-server}"
ORION_CLI="${ORION_CLI:-$PROJECT_ROOT/target/$BUILD_PROFILE/orion-cli}"

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

# Keep diagnostics separate from JSON returned by the deployment helpers.
log_info()  { echo -e "${BLUE}[BENCH]${RESET} $*" >&2; }
log_ok()    { echo -e "${GREEN}[BENCH]${RESET} $*" >&2; }
log_warn()  { echo -e "${YELLOW}[BENCH]${RESET} $*" >&2; }
log_error() { echo -e "${RED}[BENCH]${RESET} $*" >&2; }

# ═══════════════════════════════════════════════════════════════════
# DEPENDENCY CHECKS
# ═══════════════════════════════════════════════════════════════════

check_dependencies() {
    local missing=()
    for cmd in hey jq curl python3; do
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
        if [[ ! -x "$ORION_BIN" || ! -x "$ORION_CLI" ]]; then
            log_error "Build both binaries or set ORION_BIN and ORION_CLI"
            exit 1
        fi
        return 0
    fi

    log_info "Building Orion ($BUILD_PROFILE)..."
    if [[ "$BUILD_PROFILE" == "release" ]]; then
        cargo build --locked -p orion-server -p orion-cli --release --manifest-path "$PROJECT_ROOT/Cargo.toml" 2>&1 | tail -5
    else
        cargo build --locked -p orion-server -p orion-cli --manifest-path "$PROJECT_ROOT/Cargo.toml" 2>&1 | tail -5
    fi
    log_ok "Build complete"
}

# ═══════════════════════════════════════════════════════════════════
# SERVER LIFECYCLE
# ═══════════════════════════════════════════════════════════════════

BENCH_PID=""
BENCH_PORT=""
BENCH_URL=""
BENCH_ADMIN_KEY=""
BENCH_HEY_PID=""
BENCH_CLUSTER_BASE=""
BENCH_CLUSTER_HEAD=""
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

    # The runner owns one private directory, named files inside it.
    #
    # Earlier versions used three `mktemp .../orion-bench-XXXXXX.db` calls, which is not
    # portable: BSD/macOS mktemp only substitutes X's at the *end* of the
    # template, so a suffixed template is taken literally and every run created
    # the same `orion-bench-XXXXXX.db`. That worked only as long as each run
    # reached its cleanup trap — the moment one was killed, the literal file
    # survived and every subsequent run died at startup with "mkstemp failed:
    # File exists", which reads like a full disk rather than a leftover file.
    BENCH_DB_PATH="$BENCH_TMP_DIR/bench.db"
    BENCH_LOG_FILE="$BENCH_TMP_DIR/bench.log"
    BENCH_CONFIG_FILE="$BENCH_TMP_DIR/bench.toml"

    cat > "$BENCH_CONFIG_FILE" <<TOMLEOF
[server]
host = "127.0.0.1"
port = $BENCH_PORT
shutdown_drain_secs = 0

[storage]
url = "sqlite:$BENCH_DB_PATH?mode=rwc"
max_connections = 10

[trace_queue]
workers = 4
buffer_size = 200

[logging]
level = "error"
format = "pretty"

[metrics]
enabled = false

# Scenario H uploads a plugin; everywhere else the sandbox is idle.
[plugins]
enabled = true
TOMLEOF

    log_info "Starting Orion on port $BENCH_PORT ($BUILD_PROFILE mode)"

    "$ORION_BIN" --config "$BENCH_CONFIG_FILE" > "$BENCH_LOG_FILE" 2>&1 &
    BENCH_PID=$!

    # Wait for server to be ready
    local elapsed=0
    while [[ $elapsed -lt 60 ]]; do
        if curl -sf "${BENCH_URL}/readyz" >/dev/null 2>&1; then
            log_ok "Server ready ($((elapsed / 2))s)"
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

    [[ -z "$BENCH_PID" ]] || wait "$BENCH_PID" 2>/dev/null || true
    BENCH_PID=""
    # The whole directory goes, so the WAL/SHM sidecars cannot be left behind.
    [[ -n "${BENCH_TMP_DIR:-}" ]] && rm -rf "$BENCH_TMP_DIR"
    BENCH_TMP_DIR=""
}

# ═══════════════════════════════════════════════════════════════════
# DEPLOYMENT MANAGEMENT (compile + apply through the matching CLI)
# ═══════════════════════════════════════════════════════════════════

cli() {
    local auth=()
    [[ -z "$BENCH_ADMIN_KEY" ]] || auth=(--api-key "$BENCH_ADMIN_KEY")
    "$ORION_CLI" --server "$BENCH_URL" ${auth[@]+"${auth[@]}"} --output json --yes "$@"
}

# A new set replaces the whole deployment head. Definitions are inert until apply.
prepare_set() {
    BENCH_SET=$(mktemp -d "$BENCH_TMP_DIR/set-XXXXXX")
}

add_workflow_channel() {
    local fixture="$1" name="$2"
    jq --arg id "$name" '.workflow_id = $id' "$fixture" > "$BENCH_SET/workflow-$name.json"
    jq -n --arg id "$name" \
        '{channel_id: $id, name: $id, channel_type: "sync", protocol: "http",
          methods: ["POST"], route_pattern: ("/" + $id), workflow_id: $id}' \
        > "$BENCH_SET/channel-$name.json"
}

compile_set() {
    "$ORION_CLI" compile "$BENCH_SET" --format deployment -o "$1" >&2
}

apply_directory() {
    local result
    result=$(cli deployments apply -d "$1")
    if [[ -z "$result" ]]; then
        # The CLI emits no receipt for an unchanged directory. Verify its head
        # is applied locally; scenario probes still check serving behavior.
        result=$(cli deployments list)
        echo "$result" | jq -e '.head_revision > 0 and .head_revision == .applied_revision' >/dev/null
        BENCH_HEAD=$(echo "$result" | jq -er '.head_revision')
    else
        BENCH_HEAD=$(echo "$result" | jq -er '.committed_revision')
        [[ -z "$BENCH_CLUSTER_BASE" ]] || BENCH_CLUSTER_HEAD="$BENCH_HEAD"
        echo "$result" | jq -e '.evaluation.converged == true' >/dev/null || {
            log_error "Deployment did not converge: $result"
            return 1
        }
    fi
}

deploy_set() {
    compile_set "$BENCH_SET.dist"
    apply_directory "$BENCH_SET.dist"
}

deploy_single() {
    prepare_set
    add_workflow_channel "$1" "$2"
    deploy_set
}

# Probe real output before timing: a fast error or a skipped workflow is invalid.
probe() {
    local name="$1" body="$2" assertion="$3" response
    response=$(curl --fail-with-body -sS -X POST "$BENCH_URL/api/v2/data/$name" \
        -H 'Content-Type: application/json' --data-binary @"$body")
    echo "$response" | jq -e "$assertion" >/dev/null || {
        log_error "Unexpected response from $name: $response"
        return 1
    }
}

restore_cluster() {
    [[ -n "$BENCH_CLUSTER_BASE" && -n "$BENCH_CLUSTER_HEAD" ]] || return 0
    local now
    now=$(cli deployments list | jq -er '.head_revision')
    [[ "$now" == "$BENCH_CLUSTER_HEAD" ]] || {
        log_error "Cluster head changed outside the benchmark; refusing to restore over revision $now"
        return 1
    }
    if [[ "$BENCH_CLUSTER_BASE" -gt 0 ]]; then
        cli deployments rollback --target-revision "$BENCH_CLUSTER_BASE" \
            --expected-head-revision "$now" --idempotency-key "bench-restore-$now" \
            --reason 'benchmark: restore prior head' >/dev/null
    else
        local manifest="$BENCH_TMP_DIR/empty.json" digest
        echo '{"schema_version":2,"providers":[],"members":[]}' > "$manifest"
        digest=$(cli deployments plan -f "$manifest" --base-revision "$now" | jq -er '.reviewed_manifest_digest')
        cli deployments apply -f "$manifest" --base-revision "$now" --reviewed-digest "$digest" \
            --idempotency-key "bench-empty-$now" --reason 'benchmark: restore empty head' >/dev/null
    fi
    BENCH_CLUSTER_BASE=""
    BENCH_CLUSTER_HEAD=""
}

cleanup() {
    local status=$?
    if [[ -n "$BENCH_HEY_PID" ]] && kill -0 "$BENCH_HEY_PID" 2>/dev/null; then
        kill "$BENCH_HEY_PID" 2>/dev/null || true
        wait "$BENCH_HEY_PID" 2>/dev/null || true
    fi
    restore_cluster || status=1
    stop_bench_server
    exit "$status"
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

    deploy_single "$FIXTURES_DIR/workflows/bench_simple_log.json" bench
    probe bench "$FIXTURES_DIR/data/simple_payload.json" '.status == "ok" and .data != null and (.errors | length) == 0'

    run_hey POST "${BENCH_URL}/api/v2/data/bench" "$FIXTURES_DIR/data/simple_payload.json"
    record_result "B: Simple workflow (1 log task)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
}

# C: Complex workflow — 4-task ecommerce workflow
scenario_complex() {
    log_info "C: Complex workflow (parse + 4 tasks)"
    CURRENT_SCENARIO="C_complex_workflow"

    deploy_single "$FIXTURES_DIR/workflows/bench_complex_ecommerce.json" orders
    probe orders "$FIXTURES_DIR/data/complex_payload.json" '.data.tier == "vip" and .data.label == "vip-US"'

    run_hey POST "${BENCH_URL}/api/v2/data/orders" "$FIXTURES_DIR/data/complex_payload.json"
    record_result "C: Complex workflow (parse + 4 tasks)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
}

# D: One head with 12 workflows and 12 channels. Each runs B's exact workload;
# the measured route differs only in the size of the serving estate.
scenario_multi() {
    log_info "D: Loaded estate (12 workflows, 12 channels in one deployment)"
    CURRENT_SCENARIO="D_multi_workflows"
    prepare_set
    local n name
    for ((n = 0; n < 12; n++)); do
        name="bench"
        [[ $n -eq 0 ]] || name="bench-$n"
        add_workflow_channel "$FIXTURES_DIR/workflows/bench_simple_log.json" "$name"
    done
    compile_set "$BENCH_SET.dist"
    jq -e '([.members[] | select(.kind == "channel")] | length) == 12 and
           ([.members[] | select(.kind == "workflow")] | length) == 12' \
        "$BENCH_SET.dist/deployment.json" >/dev/null
    apply_directory "$BENCH_SET.dist"
    for ((n = 0; n < 12; n++)); do
        name="bench"
        [[ $n -eq 0 ]] || name="bench-$n"
        probe "$name" "$FIXTURES_DIR/data/simple_payload.json" '.status == "ok" and .data != null and (.errors | length) == 0'
    done
    run_hey POST "${BENCH_URL}/api/v2/data/bench" "$FIXTURES_DIR/data/simple_payload.json"
    record_result "D: Loaded estate (12 channels)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
}

# E: Concurrency scaling — c=1, 10, 50, 100
scenario_concurrency() {
    log_info "E: Concurrency scaling"

    deploy_single "$FIXTURES_DIR/workflows/bench_simple_log.json" bench
    probe bench "$FIXTURES_DIR/data/simple_payload.json" '.status == "ok" and .data != null and (.errors | length) == 0'

    for c in 1 10 50 100; do
        CURRENT_SCENARIO="E_concurrency_${c}"
        log_info "  Concurrency: $c"

        run_hey POST "${BENCH_URL}/api/v2/data/bench" "$FIXTURES_DIR/data/simple_payload.json" "$c"
        record_result "E: Concurrency c=${c}" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
    done
}

# F: Commit alternating workflow definitions under load. An unchanged apply is
# a no-op in v2, so compile both variants first and require every commit to move
# the head. The request workload stays the same (one disabled debug log).
scenario_reload() {
    log_info "F: Deployment replacement under load"
    CURRENT_SCENARIO="F_reload_under_load"
    prepare_set
    add_workflow_channel "$FIXTURES_DIR/workflows/bench_simple_log.json" bench
    local first="$BENCH_SET.first" second="$BENCH_SET.second"
    compile_set "$first"
    jq '.name = "Bench Simple Log alternate"' "$BENCH_SET/workflow-bench.json" > "$BENCH_SET/alternate.tmp"
    mv "$BENCH_SET/alternate.tmp" "$BENCH_SET/workflow-bench.json"
    compile_set "$second"
    apply_directory "$first"
    probe bench "$FIXTURES_DIR/data/simple_payload.json" '.status == "ok" and .data != null and (.errors | length) == 0'

    local hey_output_file="$BENCH_TMP_DIR/hey-reload.txt"
    hey -z "$BENCH_DURATION" -c "$BENCH_CONCURRENCY" -m POST -T application/json \
        -D "$FIXTURES_DIR/data/simple_payload.json" "$BENCH_URL/api/v2/data/bench" \
        > "$hey_output_file" 2>&1 &
    BENCH_HEY_PID=$!

    local reload_count=0 previous target
    while kill -0 "$BENCH_HEY_PID" 2>/dev/null; do
        sleep 0.5
        kill -0 "$BENCH_HEY_PID" 2>/dev/null || break
        previous="$BENCH_HEAD"
        target="$second"
        [[ $((reload_count % 2)) -eq 0 ]] || target="$first"
        apply_directory "$target"
        [[ "$BENCH_HEAD" -gt "$previous" ]] || { log_error 'Replacement did not move the head'; return 1; }
        reload_count=$((reload_count + 1))
    done
    wait "$BENCH_HEY_PID"
    BENCH_HEY_PID=""
    [[ "$reload_count" -gt 0 ]] || { log_error 'Duration too short to exercise replacement'; return 1; }
    local hey_output
    hey_output=$(cat "$hey_output_file")
    parse_hey_output "$hey_output"
    record_result "F: Deployment replacement (${reload_count}x)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
    probe bench "$FIXTURES_DIR/data/simple_payload.json" '.status == "ok" and .data != null and (.errors | length) == 0'
    if [[ -n "$BENCH_OUTPUT_DIR" ]]; then
        mkdir -p "$BENCH_OUTPUT_DIR"
        cp "$hey_output_file" "${BENCH_OUTPUT_DIR}/${CURRENT_SCENARIO}.txt"
        echo "$reload_count" > "${BENCH_OUTPUT_DIR}/F_committed_replacements.txt"
    fi
}

# G: Cluster through the load balancer — the HA compose stack (opt-in)
#
# Runs the same simple workflow as B, but through docker-compose.ha.yml's
# topology: nginx → 2 replicas in cluster mode → shared Postgres.
# This is the half the single-process scenarios cannot measure — the
# per-request price of cluster mode (shared-store dedup / rate-limit /
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
        log_error '  Set ORION_ADMIN_API_KEYS to the disposable stack admin key.'
        log_error '  docker compose -f docker-compose.ha.yml up -d --wait'
        log_error "(or point BENCH_CLUSTER_URL at your load balancer)"
        return 1
    fi
    if [[ -z "$admin_key" ]]; then
        log_error "ORION_ADMIN_API_KEYS is not set; the HA stack enforces admin auth"
        return 1
    fi

    # Use a disposable benchmark stack. Restore the prior head through CAS on exit.
    local saved_url="$BENCH_URL"
    BENCH_URL="$cluster_url"
    BENCH_ADMIN_KEY="$admin_key"
    BENCH_CLUSTER_BASE=$(cli deployments list | jq -er '.head_revision')
    deploy_single "$FIXTURES_DIR/workflows/bench_simple_log.json" bench-cluster
    # Local convergence is reported by apply; require repeated success through
    # the LB before timing so peer catch-up is outside the measured window.
    local attempt i ready=0
    for ((attempt = 0; attempt < 30; attempt++)); do
        ready=1
        for ((i = 0; i < 30; i++)); do
            if ! probe bench-cluster "$FIXTURES_DIR/data/simple_payload.json" '.status == "ok" and .data != null and (.errors | length) == 0'; then
                ready=0
                break
            fi
        done
        [[ "$ready" -eq 0 ]] || break
        sleep 1
    done
    [[ "$ready" -eq 1 ]] || { log_error 'Cluster did not converge through the LB'; return 1; }
    run_hey POST "${cluster_url}/api/v2/data/bench-cluster" "$FIXTURES_DIR/data/simple_payload.json"
    record_result "G: Cluster via LB (simple workflow)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
    restore_cluster
    BENCH_ADMIN_KEY=""
    BENCH_URL="$saved_url"
}

# H: The same parse + echo through Wasm and through JSONLogic.
scenario_plugin() {
    log_info "H: Plugin echo vs JSONLogic echo"
    prepare_set
    add_workflow_channel "$FIXTURES_DIR/workflows/bench_plugin_echo.json" bench-plugin
    mkdir -p "$BENCH_SET/plugin"
    cp "$PROJECT_ROOT/crates/orion-server/tests/fixtures/plugins/fixture-upload.toml" \
        "$PROJECT_ROOT/crates/orion-server/tests/fixtures/plugins/fixture.wasm" "$BENCH_SET/plugin/"
    deploy_set
    probe bench-plugin "$FIXTURES_DIR/data/simple_payload.json" \
        '.data.echo.message.customer_id == "cust-12345"'
    CURRENT_SCENARIO="H_plugin_echo"
    run_hey POST "${BENCH_URL}/api/v2/data/bench-plugin" "$FIXTURES_DIR/data/simple_payload.json"
    record_result "H: Plugin echo (test.fixture.identity)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"

    deploy_single "$FIXTURES_DIR/workflows/bench_jsonlogic_echo.json" bench-logic
    probe bench-logic "$FIXTURES_DIR/data/simple_payload.json" \
        '.data.echo.message.customer_id == "cust-12345"'
    CURRENT_SCENARIO="H_jsonlogic_echo"
    run_hey POST "${BENCH_URL}/api/v2/data/bench-logic" "$FIXTURES_DIR/data/simple_payload.json"
    record_result "H: JSONLogic echo (map)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
}

# ═══════════════════════════════════════════════════════════════════
# SCENARIO REGISTRY
# ═══════════════════════════════════════════════════════════════════

# `cluster` (G) is deliberately absent: it needs Docker and a running HA
# stack, so it only runs when named explicitly.
ALL_SCENARIOS=(baseline simple complex multi concurrency reload plugin)

run_scenario() {
    local name="$1"
    case "$name" in
        baseline)    scenario_baseline ;;
        simple)      scenario_simple ;;
        complex)     scenario_complex ;;
        multi)       scenario_multi ;;
        concurrency) scenario_concurrency ;;
        reload)      scenario_reload ;;
        plugin)      scenario_plugin ;;
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
    BENCH_TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/orion-bench-XXXXXX")
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    if [[ "$need_local" -eq 1 ]]; then
        build_orion
        start_bench_server
    fi

    if [[ -n "$BENCH_OUTPUT_DIR" ]]; then
        mkdir -p "$BENCH_OUTPUT_DIR"
        {
            git -C "$PROJECT_ROOT" rev-parse HEAD
            if git -C "$PROJECT_ROOT" diff --quiet; then echo 'tracked_tree=clean'; else echo 'tracked_tree=modified'; fi
            "$ORION_CLI" --version
            [[ "$need_local" -eq 0 ]] || "$ORION_BIN" --version
            uname -sm
            echo "profile=$BUILD_PROFILE duration=$BENCH_DURATION concurrency=$BENCH_CONCURRENCY"
            echo "scenarios=${scenarios[*]}"
        } > "$BENCH_OUTPUT_DIR/session.txt"
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
    local errors
    for errors in "${RESULT_ERR_LIST[@]}"; do
        [[ "$errors" -eq 0 ]] || { log_error 'Benchmark recorded failed requests'; return 1; }
    done
    log_ok "Benchmark complete in ${elapsed}s"
}

main "$@"

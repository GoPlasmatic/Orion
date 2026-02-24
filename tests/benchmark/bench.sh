#!/usr/bin/env bash
# tests/benchmark/bench.sh — Performance benchmarking suite for Orion
#
# Uses `hey` HTTP load generator to measure throughput, latency, and
# concurrency behavior across 7 scenarios.
#
# Usage:
#   ./tests/benchmark/bench.sh                        # Run all scenarios
#   ./tests/benchmark/bench.sh baseline simple        # Run specific scenarios
#   BENCH_RELEASE=1 BENCH_DURATION=30s ./tests/benchmark/bench.sh
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
    ORION_BIN="$PROJECT_ROOT/target/release/orion"
    BUILD_PROFILE="release"
else
    ORION_BIN="$PROJECT_ROOT/target/debug/orion"
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

log_info()  { echo -e "${BLUE}[BENCH]${RESET} $*"; }
log_ok()    { echo -e "${GREEN}[BENCH]${RESET} $*"; }
log_warn()  { echo -e "${YELLOW}[BENCH]${RESET} $*"; }
log_error() { echo -e "${RED}[BENCH]${RESET} $*"; }

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
    BENCH_DB_PATH=$(mktemp "${TMPDIR:-/tmp}/orion-bench-XXXXXX.db")
    BENCH_LOG_FILE=$(mktemp "${TMPDIR:-/tmp}/orion-bench-XXXXXX.log")
    BENCH_CONFIG_FILE=$(mktemp "${TMPDIR:-/tmp}/orion-bench-XXXXXX.toml")

    cat > "$BENCH_CONFIG_FILE" <<TOMLEOF
[server]
host = "127.0.0.1"
port = $BENCH_PORT
workers = 4

[storage]
path = "$BENCH_DB_PATH"
max_connections = 10

[queue]
workers = 4
buffer_size = 200

[logging]
level = "error"
format = "pretty"

[metrics]
enabled = false

[ingest]
batch_size = 200
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
    [[ -n "${BENCH_DB_PATH:-}" ]]     && rm -f "$BENCH_DB_PATH" "${BENCH_DB_PATH}-wal" "${BENCH_DB_PATH}-shm"
    [[ -n "${BENCH_LOG_FILE:-}" ]]    && rm -f "$BENCH_LOG_FILE"
    [[ -n "${BENCH_CONFIG_FILE:-}" ]] && rm -f "$BENCH_CONFIG_FILE"
}

# ═══════════════════════════════════════════════════════════════════
# RULE MANAGEMENT (curl-based, no orion-cli dependency)
# ═══════════════════════════════════════════════════════════════════

create_rule() {
    local rule_file="$1"
    local response
    response=$(curl -sf -X POST "${BENCH_URL}/api/v1/admin/rules" \
        -H "Content-Type: application/json" \
        -d @"$rule_file" 2>/dev/null) || {
        log_error "Failed to create rule from $rule_file"
        return 1
    }

    local rule_id
    rule_id=$(echo "$response" | jq -r '.data.id // empty')
    if [[ -z "$rule_id" ]]; then
        log_error "No rule ID in response: $response"
        return 1
    fi
    echo "$rule_id"
}

import_rules() {
    local rules_file="$1"
    local response
    response=$(curl -sf -X POST "${BENCH_URL}/api/v1/admin/rules/import" \
        -H "Content-Type: application/json" \
        -d @"$rules_file" 2>/dev/null) || {
        log_error "Failed to import rules from $rules_file"
        return 1
    }

    local imported
    imported=$(echo "$response" | jq -r '.imported // 0')
    log_info "Imported $imported rules"
}

reload_engine() {
    curl -sf -X POST "${BENCH_URL}/api/v1/admin/engine/reload" >/dev/null 2>&1 || true
}

clear_rules() {
    local rules_json
    rules_json=$(curl -sf "${BENCH_URL}/api/v1/admin/rules" 2>/dev/null) || return 0

    local ids
    ids=$(echo "$rules_json" | jq -r '.data[]?.id // empty' 2>/dev/null) || return 0

    while IFS= read -r id; do
        [[ -z "$id" ]] && continue
        curl -sf -X DELETE "${BENCH_URL}/api/v1/admin/rules/${id}" >/dev/null 2>&1 || true
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

    # Error count: sum non-200/202 status codes
    RESULT_ERRORS=0
    local status_section
    status_section=$(echo "$hey_output" | sed -n '/Status code distribution/,/^$/p' || true)
    if [[ -n "$status_section" ]]; then
        while IFS= read -r line; do
            # Only process lines that contain [NNN] pattern
            local code count
            code=$(echo "$line" | grep -oE '\[([0-9]+)\]' | tr -d '[]' || true)
            [[ -z "$code" ]] && continue
            count=$(echo "$line" | awk '{print $NF}' | tr -d ' ')
            if [[ "$code" != "200" ]] && [[ "$code" != "202" ]] && [[ -n "$count" ]]; then
                RESULT_ERRORS=$((RESULT_ERRORS + count))
            fi
        done <<< "$status_section"
    fi

    # Also check for error distribution section (connection errors etc.)
    local error_count
    error_count=$(echo "$hey_output" | sed -n '/Error distribution/,/^$/p' | grep -cE '^\s+\[' || true)
    if [[ "$error_count" -gt 0 ]]; then
        RESULT_ERRORS=$((RESULT_ERRORS + error_count))
    fi
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

# B: Simple rule — 1 rule, 1 log task
scenario_simple() {
    log_info "B: Simple rule (1 log task)"
    CURRENT_SCENARIO="B_simple_rule"

    clear_rules
    create_rule "$FIXTURES_DIR/rules/bench_simple_log.json" >/dev/null

    run_hey POST "${BENCH_URL}/api/v1/data/bench" "$FIXTURES_DIR/data/simple_payload.json"
    record_result "B: Simple rule (1 log task)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
}

# C: Complex rule — 5-task ecommerce rule
scenario_complex() {
    log_info "C: Complex rule (5 tasks)"
    CURRENT_SCENARIO="C_complex_rule"

    clear_rules
    create_rule "$FIXTURES_DIR/rules/bench_complex_ecommerce.json" >/dev/null

    run_hey POST "${BENCH_URL}/api/v1/data/orders" "$FIXTURES_DIR/data/complex_payload.json"
    record_result "C: Complex rule (5 tasks)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
}

# D: Multi-rule channel — 12 rules on same channel
scenario_multi() {
    log_info "D: Multi-rule channel (12 rules)"
    CURRENT_SCENARIO="D_multi_rules"

    clear_rules
    import_rules "$FIXTURES_DIR/rules/bench_multi_rules.json"

    run_hey POST "${BENCH_URL}/api/v1/data/bench" "$FIXTURES_DIR/data/simple_payload.json"
    record_result "D: Multi-rule channel (12 rules)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
}

# E: Batch sizing — batch sizes 1, 10, 50, 100
scenario_batch() {
    log_info "E: Batch sizing"

    clear_rules
    create_rule "$FIXTURES_DIR/rules/bench_simple_log.json" >/dev/null

    for size in 1 10 50 100; do
        CURRENT_SCENARIO="E_batch_${size}"
        log_info "  Batch size: $size"

        run_hey POST "${BENCH_URL}/api/v1/data/batch" "$FIXTURES_DIR/data/batch_${size}.json"
        record_result "E: Batch (${size} messages)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
    done
}

# F: Concurrency scaling — c=1, 10, 50, 100
scenario_concurrency() {
    log_info "F: Concurrency scaling"

    clear_rules
    create_rule "$FIXTURES_DIR/rules/bench_simple_log.json" >/dev/null

    for c in 1 10 50 100; do
        CURRENT_SCENARIO="F_concurrency_${c}"
        log_info "  Concurrency: $c"

        run_hey POST "${BENCH_URL}/api/v1/data/bench" "$FIXTURES_DIR/data/simple_payload.json" "$c"
        record_result "F: Concurrency c=${c}" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"
    done
}

# G: Reload under load — hey in background + engine reload every 500ms
scenario_reload() {
    log_info "G: Reload under load"
    CURRENT_SCENARIO="G_reload_under_load"

    clear_rules
    create_rule "$FIXTURES_DIR/rules/bench_simple_log.json" >/dev/null

    # Start hey in background
    local hey_output_file
    hey_output_file=$(mktemp "${TMPDIR:-/tmp}/orion-bench-hey-XXXXXX.txt")

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
    record_result "G: Reload under load (${reload_count}x)" "$RESULT_RPS" "$RESULT_AVG_MS" "$RESULT_P99_MS" "$RESULT_ERRORS"

    # Save raw output if requested
    if [[ -n "$BENCH_OUTPUT_DIR" ]]; then
        mkdir -p "$BENCH_OUTPUT_DIR"
        echo "$hey_output" > "${BENCH_OUTPUT_DIR}/${CURRENT_SCENARIO}.txt"
    fi
}

# ═══════════════════════════════════════════════════════════════════
# SCENARIO REGISTRY
# ═══════════════════════════════════════════════════════════════════

ALL_SCENARIOS=(baseline simple complex multi batch concurrency reload)

run_scenario() {
    local name="$1"
    case "$name" in
        baseline)    scenario_baseline ;;
        simple)      scenario_simple ;;
        complex)     scenario_complex ;;
        multi)       scenario_multi ;;
        batch)       scenario_batch ;;
        concurrency) scenario_concurrency ;;
        reload)      scenario_reload ;;
        *)
            log_error "Unknown scenario: $name"
            log_error "Available: ${ALL_SCENARIOS[*]}"
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

    check_dependencies
    build_orion
    start_bench_server

    # Ensure cleanup on exit
    trap stop_bench_server EXIT

    # Determine which scenarios to run
    local scenarios=()
    if [[ $# -gt 0 ]]; then
        scenarios=("$@")
    else
        scenarios=("${ALL_SCENARIOS[@]}")
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

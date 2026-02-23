#!/usr/bin/env bash
# tests/e2e/run.sh — Main E2E test runner for Orion
#
# Usage:
#   ./tests/e2e/run.sh                    # Run all suites
#   ./tests/e2e/run.sh 01_health          # Run a specific suite
#   ./tests/e2e/run.sh 01 07 08           # Run multiple suites (prefix match)
#
# Environment:
#   E2E_PORT=9999        Use specific port (default: auto)
#   E2E_DEBUG=1          Enable debug logging
#   E2E_KEEP_SERVER=1    Don't stop server after tests
#   E2E_SKIP_BUILD=1     Skip cargo build (use existing binaries)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Source all shared functions
source "$SCRIPT_DIR/helpers.sh"

# ── Prerequisites ───────────────────────────────────────────────
check_prerequisites() {
    local missing=()
    command -v jq   >/dev/null 2>&1 || missing+=("jq")
    command -v curl >/dev/null 2>&1 || missing+=("curl")

    if [[ ${#missing[@]} -gt 0 ]]; then
        echo -e "${RED}Missing required tools: ${missing[*]}${RESET}" >&2
        echo "Install with: brew install ${missing[*]}" >&2
        exit 1
    fi
}

# ── Build ───────────────────────────────────────────────────────
build_orion() {
    if [[ -n "${E2E_SKIP_BUILD:-}" ]]; then
        log_info "Skipping build (E2E_SKIP_BUILD=1)"
    else
        echo -e "${BOLD}Building Orion...${RESET}"
        if ! cargo build --manifest-path "$PROJECT_ROOT/Cargo.toml" 2>&1; then
            echo -e "${RED}Build failed${RESET}" >&2
            exit 1
        fi
    fi

    if [[ ! -x "$ORION_BIN" ]]; then
        echo -e "${RED}orion binary not found at $ORION_BIN${RESET}" >&2
        exit 1
    fi
    if [[ ! -x "$ORION_CLI" ]]; then
        echo -e "${RED}orion-cli binary not found at $ORION_CLI${RESET}" >&2
        exit 1
    fi

    log_info "orion:     $ORION_BIN"
    log_info "orion-cli: $ORION_CLI"
}

# ── Discover and Run Suites ────────────────────────────────────
run_suites() {
    local suite_filter=("$@")
    local suites=()

    for suite_file in "$SCRIPT_DIR"/suites/*.sh; do
        [[ -f "$suite_file" ]] || continue
        local basename
        basename=$(basename "$suite_file" .sh)

        if [[ ${#suite_filter[@]} -gt 0 ]]; then
            local match=false
            for filter in "${suite_filter[@]}"; do
                if [[ "$basename" == "$filter"* ]]; then
                    match=true
                    break
                fi
            done
            [[ "$match" == "true" ]] || continue
        fi

        suites+=("$suite_file")
    done

    if [[ ${#suites[@]} -eq 0 ]]; then
        echo -e "${YELLOW}No matching test suites found.${RESET}"
        return 0
    fi

    log_info "Running ${#suites[@]} suite(s)"

    for suite_file in "${suites[@]}"; do
        source "$suite_file"
    done
}

# ── Main ────────────────────────────────────────────────────────
main() {
    local total_start
    total_start=$(date +%s)

    echo -e "${BOLD}${CYAN}"
    echo "    ____       _                _________   ______   ______          __      "
    echo "   / __ \_____(_)___  ____     / ____/__ \ / ____/  /_  __/__  _____/ /______"
    echo "  / / / / ___/ / __ \/ __ \   / __/  __/ // __/      / / / _ \/ ___/ __/ ___/"
    echo " / /_/ / /  / / /_/ / / / /  / /___ / __// /___     / / /  __(__  ) /_(__  ) "
    echo " \____/_/  /_/\____/_/ /_/  /_____//____/_____/    /_/  \___/____/\__/____/  "
    echo -e "${RESET}"

    check_prerequisites
    build_orion

    trap 'stop_server' EXIT INT TERM

    start_server

    run_suites "$@"

    if [[ -z "${E2E_KEEP_SERVER:-}" ]]; then
        stop_server
    fi

    trap - EXIT INT TERM

    local total_elapsed=$(( $(date +%s) - total_start ))
    echo -e "${DIM}Total time: ${total_elapsed}s${RESET}"

    print_summary
}

main "$@"

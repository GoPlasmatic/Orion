#!/usr/bin/env bash
# tests/e2e/run.sh — the workspace end-to-end suite: drives a real
# orion-server over HTTP with the orion-cli binary, both built from this
# tree. The one place the two crates' API contract is exercised end to end.
#
# Usage (from the repo root; `just e2e` is the same thing):
#   ./tests/e2e/run.sh                    # Run all suites
#   ./tests/e2e/run.sh 01_health          # Run a specific suite
#   ./tests/e2e/run.sh 01 07 08           # Run multiple suites (prefix match)
#
# Environment:
#   ORION_BIN=<path>     orion-server binary (default: target/debug build)
#   ORION_CLI=<path>     orion-cli binary (default: target/debug build)
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
build_binaries() {
    if [[ -n "${E2E_SKIP_BUILD:-}" ]]; then
        log_info "Skipping build (E2E_SKIP_BUILD=1)"
    else
        echo -e "${BOLD}Building orion-server and orion-cli...${RESET}"
        if ! cargo build --manifest-path "$REPO_ROOT/Cargo.toml" -p orion-server -p orion-cli 2>&1; then
            echo -e "${RED}Build failed${RESET}" >&2
            exit 1
        fi
    fi

    if [[ ! -x "$ORION_CLI" ]]; then
        echo -e "${RED}orion-cli binary not found at $ORION_CLI${RESET}" >&2
        echo "  Build it with: cargo build -p orion-cli (or set ORION_CLI)" >&2
        exit 1
    fi

    if [[ ! -x "$ORION_BIN" ]]; then
        echo -e "${RED}orion-server binary not found at $ORION_BIN${RESET}" >&2
        echo "  Build it with: cargo build -p orion-server (or set ORION_BIN)" >&2
        exit 1
    fi

    log_info "orion-server: $ORION_BIN"
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
    echo "    ____       _                  ________    ____   ______          __      "
    echo "   / __ \_____(_)___  ____       / ____/ /   /  _/  /_  __/__  _____/ /______"
    echo "  / / / / ___/ / __ \/ __ \     / /   / /    / /     / / / _ \/ ___/ __/ ___/"
    echo " / /_/ / /  / / /_/ / / / /    / /___/ /____/ /     / / /  __(__  ) /_(__  ) "
    echo " \____/_/  /_/\____/_/ /_/     \____/_____/___/    /_/  \___/____/\__/____/  "
    echo -e "${RESET}"

    check_prerequisites
    build_binaries

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

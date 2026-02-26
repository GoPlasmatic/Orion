#!/usr/bin/env bash
# Suite: Use Case Tests (data-driven)
#
# Discovers and runs all JSON test case files in tests/e2e/cases/.
# Adding a new use case = adding a .json file, no code changes needed.

CASES_DIR="$E2E_DIR/cases"

for case_file in "$CASES_DIR"/*.json; do
    [[ -f "$case_file" ]] || continue
    run_case_file "$case_file"
done

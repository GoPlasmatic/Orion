#!/usr/bin/env bash
# Suite: Use Case Tests (data-driven)
#
# Runs every JSON case file from two homes with distinct roles:
#   - examples/use-cases/ (CASES_DIR): scenario cases that deploy the shipped
#     example packages whole (`"packages"`, never copied) and assert live
#     responses — the examples proven against real traffic.
#   - tests/e2e/cases/: runtime-behaviour cases (definition test traces,
#     connection secret locators, connector error handling, body modes) —
#     contract tests, not examples.
# Each case is compiled into one definition set and deployed as the head.
# Adding a case = adding a .json file to either directory, no code changes.
# The format is documented in examples/use-cases/README.md.

for case_file in "$CASES_DIR"/*.json "$E2E_DIR"/cases/*.json; do
    [[ -f "$case_file" ]] || continue
    run_case_file "$case_file"
done

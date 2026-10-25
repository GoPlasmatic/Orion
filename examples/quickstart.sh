#!/usr/bin/env bash
#
# Orion quickstart — deploy your first service with one command.
#
#   curl -fsSL https://raw.githubusercontent.com/GoPlasmatic/Orion/main/examples/quickstart.sh | bash
#   # or, from a clone:
#   ./examples/quickstart.sh [base-url]
#
# Deploys the "quickstart-orders" service against a running Orion instance
# (default http://localhost:8080): a workflow that flags orders over $10,000,
# and a POST /orders channel that exposes it. The script writes the two
# definitions to a scratch directory, compiles them with `orion-server
# compile` and deploys the result with `orion-cli deployments apply -d` —
# the same path every Orion deployment takes.
#
# A deployment is the whole of what a node serves, so this replaces whatever
# the instance served before. Safe to re-run: an unchanged deployment applies
# nothing. Needs `orion-server` and `orion-cli` (on PATH, through
# ORION_BIN / ORION_CLI, or built in this clone's target/) and curl.
#
# The workflow id is quickstart-orders, not high-value-order, deliberately:
# examples/packages/high-value-order ships the same logic as a package of its
# own, and the two are told apart by id.
set -euo pipefail

BASE="${1:-${ORION_URL:-http://localhost:8080}}"

say()  { printf '==> %s\n' "$*"; }
have() { curl -fsS "$1" > /dev/null 2>&1; }

# The binaries: an explicit path, this clone's build (when run from one), or
# PATH. Piped into bash there is no clone, so PATH it is.
SCRIPT_DIR=""
[[ -n "${BASH_SOURCE[0]:-}" && -f "${BASH_SOURCE[0]}" ]] && SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
find_bin() {
  local var="$1" name="$2" candidate
  if [[ -n "${!var:-}" ]]; then echo "${!var}"; return; fi
  for candidate in ${SCRIPT_DIR:+"$SCRIPT_DIR/../target/debug/$name" "$SCRIPT_DIR/../target/release/$name"} \
                   "$(command -v "$name" || true)"; do
    [[ -n "$candidate" && -x "$candidate" ]] && { echo "$candidate"; return; }
  done
  echo "error: $name not found — install it, or set $var=<path to $name>" >&2
  exit 1
}
ORION_BIN="$(find_bin ORION_BIN orion-server)"
ORION_CLI="$(find_bin ORION_CLI orion-cli)"

# Wait for the server (a fresh `orion-server` needs a moment to come up).
for _ in $(seq 1 10); do
  have "$BASE/healthz" && break
  sleep 1
done
have "$BASE/healthz" || {
  echo "error: no Orion instance at $BASE" >&2
  echo "start one first:  orion-server   (or: docker run -p 8080:8080 ghcr.io/goplasmatic/orion:latest)" >&2
  exit 1
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/quickstart"

# --- Workflow: flag orders over $10,000 ------------------------------------
cat > "$WORK/quickstart/workflow.json" <<'JSON'
{
  "workflow_id": "quickstart-orders",
  "name": "High-Value Order",
  "condition": true,
  "tasks": [
    {
      "id": "parse",
      "name": "Parse payload",
      "function": { "name": "parse_json", "input": { "source": "payload", "target": "order" } }
    },
    {
      "id": "flag",
      "name": "Flag order",
      "condition": { ">": [{ "var": "data.order.total" }, 10000] },
      "function": {
        "name": "map",
        "input": {
          "mappings": [
            { "path": "data.order.flagged", "logic": true },
            {
              "path": "data.order.alert",
              "logic": { "cat": ["High-value order: $", { "var": "data.order.total" }] }
            }
          ]
        }
      }
    }
  ]
}
JSON

# --- Channel: POST /orders --------------------------------------------------
cat > "$WORK/quickstart/channel.json" <<'JSON'
{
  "channel_id": "orders",
  "name": "orders",
  "channel_type": "sync",
  "protocol": "rest",
  "route_pattern": "/orders",
  "methods": ["POST"],
  "workflow_id": "quickstart-orders"
}
JSON

say "Compile the definitions"
"$ORION_BIN" compile "$WORK/quickstart" -o "$WORK/deployment" > "$WORK/compile.log" 2>&1 || {
  cat "$WORK/compile.log" >&2
  exit 1
}

say "Deploy to $BASE"
REVISION="$("$ORION_CLI" --server "$BASE" --yes --quiet deployments apply -d "$WORK/deployment")"
say "Serving deployment revision $REVISION"

# --- First request ----------------------------------------------------------
say "Your service is live. Sending a test order:"
echo
curl -fsS -X POST "$BASE/api/v2/data/orders" \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "order_id": "ORD-9182", "total": 25000 } }'
echo
echo
say "Call it yourself:"
echo "    curl -X POST $BASE/api/v2/data/orders \\"
echo "      -H 'Content-Type: application/json' \\"
echo "      -d '{ \"data\": { \"order_id\": \"ORD-0001\", \"total\": 12500 } }'"

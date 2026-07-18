#!/usr/bin/env bash
#
# Orion quickstart — deploy your first service with one command.
#
#   curl -fsSL https://raw.githubusercontent.com/GoPlasmatic/Orion/main/examples/quickstart.sh | bash
#   # or, from a clone:
#   ./examples/quickstart.sh [base-url]
#
# Deploys the "high-value-order" service against a running Orion instance
# (default http://localhost:8080): a workflow that flags orders over $10,000,
# and a POST /orders channel that exposes it. Safe to re-run — existing
# objects are left in place. Requires only curl.
set -euo pipefail

BASE="${1:-${ORION_URL:-http://localhost:8080}}"
ADMIN="$BASE/api/v1/admin"

say()    { printf '==> %s\n' "$*"; }
have()   { curl -fsS "$1" > /dev/null 2>&1; }
active() { curl -fsS "$1" 2> /dev/null | grep -q '"status":"active"'; }

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

# --- Workflow: flag orders over $10,000 ------------------------------------
if have "$ADMIN/workflows/high-value-order"; then
  say "Workflow 'high-value-order' already exists"
else
  say "Create workflow 'high-value-order'"
  curl -fsS -X POST "$ADMIN/workflows" -H 'Content-Type: application/json' -d '{
    "workflow_id": "high-value-order",
    "name": "High-Value Order",
    "condition": true,
    "tasks": [
      { "id": "parse", "name": "Parse payload", "function": {
          "name": "parse_json",
          "input": { "source": "payload", "target": "order" }
      }},
      { "id": "flag", "name": "Flag order",
        "condition": { ">": [{ "var": "data.order.total" }, 10000] },
        "function": {
          "name": "map",
          "input": { "mappings": [
            { "path": "data.order.flagged", "logic": true },
            { "path": "data.order.alert", "logic": { "cat": ["High-value order: $", { "var": "data.order.total" }] } }
          ]}
      }}
    ]
  }' > /dev/null
fi

if active "$ADMIN/workflows/high-value-order"; then
  say "Workflow already active"
else
  say "Activate workflow"
  curl -fsS -X PATCH "$ADMIN/workflows/high-value-order/status" \
    -H 'Content-Type: application/json' -d '{"status":"active"}' > /dev/null
fi

# --- Channel: POST /orders --------------------------------------------------
if have "$ADMIN/channels/orders"; then
  say "Channel 'orders' already exists"
else
  say "Create channel 'orders'"
  curl -fsS -X POST "$ADMIN/channels" -H 'Content-Type: application/json' -d '{
    "channel_id": "orders", "name": "orders", "channel_type": "sync",
    "protocol": "rest", "route_pattern": "/orders",
    "methods": ["POST"], "workflow_id": "high-value-order"
  }' > /dev/null
fi

if active "$ADMIN/channels/orders"; then
  say "Channel already active"
else
  say "Activate channel"
  curl -fsS -X PATCH "$ADMIN/channels/orders/status" \
    -H 'Content-Type: application/json' -d '{"status":"active"}' > /dev/null
fi

# --- First request ----------------------------------------------------------
say "Your service is live. Sending a test order:"
echo
curl -fsS -X POST "$BASE/api/v1/data/orders" \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "order_id": "ORD-9182", "total": 25000 } }'
echo
echo
say "Call it yourself:"
echo "    curl -X POST $BASE/api/v1/data/orders \\"
echo "      -H 'Content-Type: application/json' \\"
echo "      -d '{ \"data\": { \"order_id\": \"ORD-0001\", \"total\": 12500 } }'"

#!/usr/bin/env bash
#
# Deploy and run one Orion example end-to-end.
#
#   ./deploy.sh <example-dir> [base-url]
#
# Creates + activates the workflow, creates + activates the channel (and
# creates the connector first, if the example ships a connector.json), then
# POSTs request.json and prints the response. Idempotent: re-running against
# the same instance skips objects that already exist. Requires curl and
# python3.
#
# Example:
#   ./deploy.sh high-value-order
#   ./deploy.sh order-classification http://localhost:8080
set -euo pipefail

DIR="${1:-}"
BASE="${2:-http://localhost:8080}"
ADMIN="$BASE/api/v1/admin"

if [[ -z "$DIR" || ! -d "$DIR" ]]; then
  echo "usage: ./deploy.sh <example-dir> [base-url]" >&2
  echo "example: ./deploy.sh high-value-order" >&2
  exit 1
fi

DIR="${DIR%/}"
WF_FILE="$DIR/workflow.json"
CH_FILE="$DIR/channel.json"
REQ_FILE="$DIR/request.json"
CONN_FILE="$DIR/connector.json"

for f in "$WF_FILE" "$CH_FILE" "$REQ_FILE"; do
  [[ -f "$f" ]] || { echo "missing $f" >&2; exit 1; }
done

# Read an id field out of a JSON file with the stdlib (no jq dependency).
json_field() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$1" "$2"; }

have()   { curl -fsS "$1" > /dev/null 2>&1; }
active() { curl -fsS "$1" 2> /dev/null | grep -q '"status":"active"'; }

WF_ID=$(json_field "$WF_FILE" workflow_id)
CH_ID=$(json_field "$CH_FILE" channel_id)
ROUTE=$(json_field "$CH_FILE" route_pattern)

if [[ -f "$CONN_FILE" ]]; then
  CONN_ID=$(json_field "$CONN_FILE" id)
  if have "$ADMIN/connectors/$CONN_ID"; then
    echo "==> Connector '$CONN_ID' already exists"
  else
    echo "==> Create connector '$CONN_ID'"
    curl -fsS -X POST "$ADMIN/connectors" \
      -H 'Content-Type: application/json' --data @"$CONN_FILE" > /dev/null
  fi
fi

if have "$ADMIN/workflows/$WF_ID"; then
  echo "==> Workflow '$WF_ID' already exists"
else
  echo "==> Create workflow '$WF_ID'"
  curl -fsS -X POST "$ADMIN/workflows" \
    -H 'Content-Type: application/json' --data @"$WF_FILE" > /dev/null
fi

if active "$ADMIN/workflows/$WF_ID"; then
  echo "==> Workflow '$WF_ID' already active"
else
  echo "==> Activate workflow '$WF_ID'"
  curl -fsS -X PATCH "$ADMIN/workflows/$WF_ID/status" \
    -H 'Content-Type: application/json' -d '{"status":"active"}' > /dev/null
fi

if have "$ADMIN/channels/$CH_ID"; then
  echo "==> Channel '$CH_ID' already exists"
else
  echo "==> Create channel '$CH_ID'"
  curl -fsS -X POST "$ADMIN/channels" \
    -H 'Content-Type: application/json' --data @"$CH_FILE" > /dev/null
fi

if active "$ADMIN/channels/$CH_ID"; then
  echo "==> Channel '$CH_ID' already active"
else
  echo "==> Activate channel '$CH_ID'"
  curl -fsS -X PATCH "$ADMIN/channels/$CH_ID/status" \
    -H 'Content-Type: application/json' -d '{"status":"active"}' > /dev/null
fi

echo "==> POST /api/v1/data$ROUTE"
curl -fsS -X POST "$BASE/api/v1/data$ROUTE" \
  -H 'Content-Type: application/json' --data @"$REQ_FILE"
echo

#!/usr/bin/env bash
#
# Deploy and run one Orion example end-to-end.
#
#   ./deploy.sh <example-dir> [base-url]
#
# Creates + activates the workflow, creates + activates the channel, then POSTs
# request.json and prints the response. Requires curl and python3.
#
# Example:
#   ./deploy.sh high-value-order
#   ./deploy.sh order-classification http://localhost:8080
set -euo pipefail

DIR="${1:-}"
BASE="${2:-http://localhost:8080}"

if [[ -z "$DIR" || ! -d "$DIR" ]]; then
  echo "usage: ./deploy.sh <example-dir> [base-url]" >&2
  echo "example: ./deploy.sh high-value-order" >&2
  exit 1
fi

DIR="${DIR%/}"
WF_FILE="$DIR/workflow.json"
CH_FILE="$DIR/channel.json"
REQ_FILE="$DIR/request.json"

for f in "$WF_FILE" "$CH_FILE" "$REQ_FILE"; do
  [[ -f "$f" ]] || { echo "missing $f" >&2; exit 1; }
done

# Read an id field out of a JSON file with the stdlib (no jq dependency).
json_field() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$1" "$2"; }

WF_ID=$(json_field "$WF_FILE" workflow_id)
CH_ID=$(json_field "$CH_FILE" channel_id)
ROUTE=$(json_field "$CH_FILE" route_pattern)

echo "==> Create workflow '$WF_ID'"
curl -fsS -X POST "$BASE/api/v1/admin/workflows" \
  -H 'Content-Type: application/json' --data @"$WF_FILE" > /dev/null

echo "==> Activate workflow '$WF_ID'"
curl -fsS -X PATCH "$BASE/api/v1/admin/workflows/$WF_ID/status" \
  -H 'Content-Type: application/json' -d '{"status":"active"}' > /dev/null

echo "==> Create channel '$CH_ID'"
curl -fsS -X POST "$BASE/api/v1/admin/channels" \
  -H 'Content-Type: application/json' --data @"$CH_FILE" > /dev/null

echo "==> Activate channel '$CH_ID'"
curl -fsS -X PATCH "$BASE/api/v1/admin/channels/$CH_ID/status" \
  -H 'Content-Type: application/json' -d '{"status":"active"}' > /dev/null

echo "==> POST /api/v1/data$ROUTE"
curl -fsS -X POST "$BASE/api/v1/data$ROUTE" \
  -H 'Content-Type: application/json' --data @"$REQ_FILE"
echo

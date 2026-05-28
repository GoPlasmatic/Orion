#!/usr/bin/env bash
# Quickstart hero: a live service from plain HTTP, no SDK and no redeploy.
# Assumes orion-server is already running on $ORION_PORT (record.sh starts it).
source "$(dirname "$0")/lib.sh"
cd "$EXAMPLES"
B="http://localhost:${ORION_PORT:-8080}"

sleep 0.4
note "Orion is running. Build a service with plain HTTP — no SDK, no redeploy."
step "curl -s $B/health | jq '{status, version}'"

note "1) Create the workflow — your business logic as JSON"
step "curl -s -X POST $B/api/v1/admin/workflows -H 'Content-Type: application/json' -d @workflow.json | jq '.data | {workflow_id, status}'"
step "curl -s -X PATCH $B/api/v1/admin/workflows/high-value-order/status -H 'Content-Type: application/json' -d '{\"status\":\"active\"}' | jq -r '.data.status'"

note "2) Create the channel — the service endpoint"
step "curl -s -X POST $B/api/v1/admin/channels -H 'Content-Type: application/json' -d @channel.json | jq '.data | {channel_id, status}'"
step "curl -s -X PATCH $B/api/v1/admin/channels/orders/status -H 'Content-Type: application/json' -d '{\"status\":\"active\"}' | jq -r '.data.status'"

note "3) Send a request — your service is live"
step "curl -s -X POST $B/api/v1/data/orders -H 'Content-Type: application/json' -d @request.json | jq '.data.order'"

note "Flagged in milliseconds — rate limiting, metrics and tracing already on."
sleep 0.6

#!/usr/bin/env bash
# Quickstart hero: a live service from two JSON files, called over plain HTTP.
# Assumes orion-server is already running on $ORION_PORT (record.sh starts it).
source "$(dirname "$0")/lib.sh"
# A scratch copy, so the compiled deployment lands outside the repository.
cd "$(mktemp -d)"
cp -R "$EXAMPLES" high-value-order
B="http://localhost:${ORION_PORT:-8080}"

sleep 0.4
note "Orion is running. A service is two JSON files — no SDK, no code to build."
step "curl -s $B/health | jq '{status, version}'"

note "1) The workflow is your business logic; the channel is its endpoint"
step "jq '{workflow_id, tasks: [.tasks[].name]}' high-value-order/workflow.json"
step "jq '{channel_id, methods, route_pattern, workflow_id}' high-value-order/channel.json"

note "2) Compile the set into a deployment and apply it"
step "orion-cli compile high-value-order -o dist"
step "orion-cli --yes deployments apply -d dist | jq '{committed_revision, converged: .evaluation.converged}'"

note "3) Send a request over plain HTTP — the service is live"
step "curl -s -X POST $B/api/v2/data/high-value-orders -H 'Content-Type: application/json' -d @high-value-order/request.json | jq '.data.order'"

note "Flagged in milliseconds — rate limiting, metrics and tracing already on."
sleep 0.6

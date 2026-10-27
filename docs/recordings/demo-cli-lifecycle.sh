#!/usr/bin/env bash
# Orion CLI lifecycle: lint -> dry-run -> compile -> deploy -> send -> trace.
# Uses $ORION_SERVER_URL (exported by record.sh) so no --server flag is shown.
source "$(dirname "$0")/lib.sh"
# A scratch copy, so the compiled deployment lands outside the repository.
cd "$(mktemp -d)"
cp -R "$EXAMPLES" high-value-order
# The payload alone: request.json is the HTTP body, `{"data": …}`.
jq .data high-value-order/request.json > order.json

sleep 0.4
note "Manage Orion from your terminal — offline checks first, then one deploy."
step "orion-cli health"

note "Check the definition set before anything reaches a server"
step "orion-cli lint high-value-order"

note "Dry-run the workflow against sample input, offline"
step "orion-cli dry-run -w high-value-order/workflow.json -i order.json --trace none | jq '{tasks, order: .output.order}'"

note "Compile and deploy — the server serves exactly what was applied"
step "orion-cli compile high-value-order -o dist"
step "orion-cli --yes deployments apply -d dist | jq '{committed_revision, converged: .evaluation.converged}'"

note "Send live data through the channel, then read its trace"
step "orion-cli send high-value-orders -d '{\"order_id\":\"ORD-9182\",\"total\":25000}'"
step "orion-cli traces list --limit 1"

note "The same files, checked offline and served live — one tool, full lifecycle."
sleep 0.6

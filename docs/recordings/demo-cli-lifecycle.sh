#!/usr/bin/env bash
# Orion CLI lifecycle: create -> activate -> dry-run -> wire channel -> send.
# Uses $ORION_SERVER_URL (exported by record.sh) so no --server flag is shown.
source "$(dirname "$0")/lib.sh"
cd "$EXAMPLES"

sleep 0.4
note "Manage Orion from your terminal — colored output, scriptable, fast."
step "orion-cli health"

note "Create a workflow from a file (it starts life as a draft)"
step "orion-cli --yes workflows create -f workflow.json"
step "orion-cli --yes workflows activate high-value-order"

note "Dry-run it against sample data before any real traffic hits it"
step "orion-cli workflows test high-value-order -d '{\"order_id\":\"ORD-9182\",\"total\":25000}'"

note "Wire up the channel, reload, then send live data"
step "orion-cli --yes channels create -f channel.json"
step "orion-cli --yes channels activate orders"
step "orion-cli --yes engine reload"
step "orion-cli send orders -d '{\"order_id\":\"ORD-9182\",\"total\":25000}' --verbose"

note "Dry-run and live traffic through the same workflow — one tool, full lifecycle."
sleep 0.6

#!/usr/bin/env bash
# MCP demo: a real stdio JSON-RPC session against `orion-cli mcp serve`, the
# same transport Claude Desktop / Cursor use. record.sh seeds the 'orders'
# channel first so the tool call has something live to hit.
source "$(dirname "$0")/lib.sh"

sleep 0.4
note "Claude Desktop / Cursor launch:  orion-cli mcp serve   (stdio, JSON-RPC)"
note "Below is a real session — handshake, tool discovery, then a live tool call."

REQ="$(mktemp)"
{
  echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"claude-desktop","version":"1.0"}}}'
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  echo '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
  echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"data_send_sync","arguments":{"channel":"orders","data":"{\"order_id\":\"ORD-9182\",\"total\":25000}"}}}'
} > "$REQ"

OUT="$(orion-cli mcp serve < "$REQ" 2>/dev/null)"
rm -f "$REQ"

note "-> initialize"
printf '%s\n' "$OUT" | sed -n '1p' | jq -r '"  connected to \(.result.serverInfo.name) \(.result.serverInfo.version)   (MCP protocol \(.result.protocolVersion))"'
sleep 0.7

note "-> tools/list"
N="$(printf '%s\n' "$OUT" | sed -n '2p' | jq '.result.tools | length')"
printf '  %s tools exposed to the AI, including:\n' "$N"
printf '%s\n' "$OUT" | sed -n '2p' | jq -r '.result.tools[].name' \
  | grep -E '^(health_check|engine_reload|workflows_create|workflows_test|channels_create|connectors_create|data_send_sync|data_send_async|traces_list|functions_list|circuit_breakers_list|get_metrics)$' \
  | awk '{ printf "    %-26s", $0; if (NR % 3 == 0) printf "\n" } END { if (NR % 3 != 0) printf "\n" }'
sleep 1.0

note "-> tools/call  data_send_sync(channel=orders, {order_id, total: 25000})"
printf '%s\n' "$OUT" | sed -n '3p' | jq -r '.result.content[0].text' | jq '.data.order' | sed 's/^/    /'
sleep 0.8

note "46 tools, one server. Drop the config into your AI client and just ask."
sleep 0.6

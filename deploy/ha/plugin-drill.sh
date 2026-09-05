#!/usr/bin/env bash
# Plugin drill for the HA reference topology.
#
# Two claims the plugin design makes about a cluster, exercised against the
# real compose stack through its load balancer:
#
#   1. A plugin activated on one node converges on every node through the
#      config epoch — each node compiles the component locally from the same
#      rows — so the function is served by whichever node the LB picks.
#   2. Activating a NEW version of a plugin under steady load produces zero
#      non-2xx responses: a request in flight finishes on the generation it
#      started with, and the next one starts on the new digest.
#
# Nodes are reached only through nginx (round-robin, two upstreams), so
# "every node" is asserted as a run of consecutive LB reads that all agree —
# with two upstreams, 20 in a row missing a node is a 2^-20 event.
#
# Usage:
#   deploy/ha/plugin-drill.sh              # assumes the stack is already up
#   START_STACK=1 deploy/ha/plugin-drill.sh
#
# Requires: docker compose, curl, jq, python3. Runs ~40 s.

set -euo pipefail

COMPOSE=(docker compose -f docker-compose.ha.yml)
LB_URL="${LB_URL:-http://localhost:8080}"
DURATION_SECS="${DURATION_SECS:-20}"
CONSECUTIVE="${CONSECUTIVE:-20}"

cd "$(dirname "$0")/../.."

# The HA compose enforces admin auth (proposal P1); the stack needs the key
# set for every command, and the admin calls below carry it.
export ORION_ADMIN_API_KEYS="${ORION_ADMIN_API_KEYS:-drill-only-throwaway-admin-key-padded-past-32}"
ADMIN_KEY="${ORION_ADMIN_API_KEYS%%,*}"
auth=(-H "Authorization: Bearer ${ADMIN_KEY}")
ADMIN="$LB_URL/api/v1/admin"

FIXTURES="crates/orion-server/tests/fixtures/plugins"

if [[ "${START_STACK:-0}" == "1" ]]; then
    echo "==> Starting the HA stack..."
    "${COMPOSE[@]}" up -d --wait
fi

echo "==> Baseline check via LB: $LB_URL/health"
curl -fsS -o /dev/null "$LB_URL/health" || {
    echo "LB is not serving; is the stack up? (START_STACK=1 $0)"
    exit 1
}

upload_body() {
    # The same JSON `orion-cli plugins create -f` sends: the manifest text and
    # the component as base64. The fixture's upload manifest names eight
    # functions the self-test can probe.
    python3 -c 'import base64, json, sys
manifest = open(sys.argv[1]).read()
component = base64.b64encode(open(sys.argv[2], "rb").read()).decode()
json.dump({"manifest": manifest, "component": component, "tags": ["drill"]}, sys.stdout)' \
        "$FIXTURES/fixture-upload.toml" "$FIXTURES/fixture.wasm"
}

# A clean slate, in dependency order: the workflow before the plugin, since
# a plugin delete is refused while an active workflow calls it.
echo "==> Clearing any previous drill state"
curl -fsS "${auth[@]}" -X DELETE "$ADMIN/channels/plugin-drill" >/dev/null 2>&1 || true
curl -fsS "${auth[@]}" -X DELETE "$ADMIN/workflows/plugin-drill" >/dev/null 2>&1 || true
curl -fsS "${auth[@]}" -X DELETE "$ADMIN/plugins/test.fixture" >/dev/null 2>&1 || true

echo "==> Uploading and activating the fixture plugin (one node takes the write)"
upload_body | curl -fsS "${auth[@]}" -X POST "$ADMIN/plugins" \
    -H 'Content-Type: application/json' --data @- >/dev/null
v1=$(curl -fsS "${auth[@]}" -X PATCH "$ADMIN/plugins/test.fixture/status" \
    -H 'Content-Type: application/json' -d '{"status":"active"}' | jq -r '.data.digest')
echo "    v1 digest: $v1"

converged_on() {
    # Every one of $CONSECUTIVE consecutive LB reads serves the function at
    # the given digest — the round-robin has visited every upstream by then.
    local digest="$1" i seen
    for ((i = 0; i < CONSECUTIVE; i++)); do
        seen=$(curl -fsS "${auth[@]}" "$ADMIN/functions" \
            | jq -r '.data[] | select(.name == "test.fixture.wrap") | .plugin.digest')
        if [[ "$seen" != "$digest" ]]; then
            return 1
        fi
    done
    return 0
}

echo "==> Waiting for every node to serve test.fixture.wrap at v1 (epoch poll is 2s)"
for attempt in $(seq 1 30); do
    if converged_on "$v1"; then
        echo "    converged after ${attempt} check(s)"
        break
    fi
    if [[ "$attempt" -eq 30 ]]; then
        echo "FAIL: the plugin did not converge on every node"
        exit 1
    fi
    sleep 1
done

echo "==> A workflow and channel that call the plugin"
curl -fsS "${auth[@]}" -X POST "$ADMIN/workflows" -H 'Content-Type: application/json' -d '{
  "workflow_id": "plugin-drill", "name": "plugin drill", "tags": ["drill"],
  "tasks": [
    {"id": "parse", "name": "parse", "function": {"name": "parse_json", "input": {"source": "payload", "target": "input"}}},
    {"id": "wrap", "name": "wrap", "function": {"name": "test.fixture.wrap", "input": {"message": {"var": "data.input.msg"}, "output": "data.result"}}}
  ]}' >/dev/null
curl -fsS "${auth[@]}" -X PATCH "$ADMIN/workflows/plugin-drill/status" \
    -H 'Content-Type: application/json' -d '{"status":"active"}' >/dev/null
curl -fsS "${auth[@]}" -X POST "$ADMIN/channels" -H 'Content-Type: application/json' -d '{
  "channel_id": "plugin-drill", "name": "plugin-drill", "channel_type": "sync", "protocol": "http",
  "methods": ["POST"], "route_pattern": "/plugin-drill", "workflow_id": "plugin-drill", "tags": ["drill"]}' >/dev/null
curl -fsS "${auth[@]}" -X PATCH "$ADMIN/channels/plugin-drill/status" \
    -H 'Content-Type: application/json' -d '{"status":"active"}' >/dev/null
# The channel converges by the same epoch; wait for a run of successes.
for attempt in $(seq 1 30); do
    ok=1
    for ((i = 0; i < CONSECUTIVE; i++)); do
        code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$LB_URL/api/v1/data/plugin-drill" \
            -H 'Content-Type: application/json' -d '{"data":{"msg":"hi"}}')
        [[ "$code" == "200" ]] || { ok=0; break; }
    done
    [[ "$ok" == "1" ]] && break
    [[ "$attempt" -eq 30 ]] && { echo "FAIL: the plugin-backed channel never served on every node"; exit 1; }
    sleep 1
done

# Version 2: the same component under a new draft — the digest is what a
# generation names, so a re-upload of the same bytes is a legitimate new
# version, and activating it is a full engine rebuild on every node.
echo "==> Driving traffic for ${DURATION_SECS}s while activating version 2..."
codes_file="$(mktemp)"
trap 'rm -f "$codes_file"' EXIT
end=$((SECONDS + DURATION_SECS))
(
    sleep 4
    curl -fsS "${auth[@]}" -X POST "$ADMIN/plugins/test.fixture/versions" >/dev/null
    curl -fsS "${auth[@]}" -X PATCH "$ADMIN/plugins/test.fixture/status" \
        -H 'Content-Type: application/json' -d '{"status":"active"}' >/dev/null
) &
activator=$!
total=0
while ((SECONDS < end)); do
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST "$LB_URL/api/v1/data/plugin-drill" \
        -H 'Content-Type: application/json' -d '{"data":{"msg":"hi"}}' || echo "000")
    echo "$code" >>"$codes_file"
    total=$((total + 1))
done
wait "$activator" || true

non2xx=$(grep -cv '^2' "$codes_file" || true)
echo "==> $total requests during the activation, $non2xx non-2xx"
sort "$codes_file" | uniq -c | sed 's/^/    /'

v2=$(curl -fsS "${auth[@]}" "$ADMIN/plugins/test.fixture" | jq -r '.data.version')
echo "==> Waiting for every node to serve version $v2"
for attempt in $(seq 1 30); do
    all=1
    for ((i = 0; i < CONSECUTIVE; i++)); do
        seen=$(curl -fsS "${auth[@]}" "$ADMIN/functions" \
            | jq -r '.data[] | select(.name == "test.fixture.wrap") | .plugin.version')
        [[ "$seen" == "$v2" ]] || { all=0; break; }
    done
    [[ "$all" == "1" ]] && { echo "    converged after ${attempt} check(s)"; break; }
    [[ "$attempt" -eq 30 ]] && { echo "FAIL: version $v2 did not converge on every node"; exit 1; }
    sleep 1
done

echo "==> Leaving the stack as found"
curl -fsS "${auth[@]}" -X DELETE "$ADMIN/channels/plugin-drill" >/dev/null
curl -fsS "${auth[@]}" -X DELETE "$ADMIN/workflows/plugin-drill" >/dev/null
curl -fsS "${auth[@]}" -X DELETE "$ADMIN/plugins/test.fixture" >/dev/null

if [[ "$non2xx" -ne 0 ]]; then
    echo "FAIL: $non2xx non-2xx response(s) while a plugin version activated under load"
    exit 1
fi
echo "PASS: converged on every node, and zero non-2xx across a version activation under load"

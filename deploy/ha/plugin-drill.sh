#!/usr/bin/env bash
# Plugin drill for the HA reference topology.
#
# Two claims the plugin design makes about a cluster, exercised against the
# real compose stack through its load balancer:
#
#   1. A deployment carrying a plugin, applied through one node, converges on
#      every node — each compiles the component locally from the same stored
#      definition — so the function is served by whichever node the LB picks.
#   2. Deploying a NEW revision of the plugin under steady load produces zero
#      non-2xx responses: a request in flight finishes on the generation it
#      started with, and the next one starts on the new revision.
#
# Nodes are reached only through nginx (round-robin, two upstreams), so
# "every node" is asserted as a run of consecutive LB reads that all agree —
# with two upstreams, 20 in a row missing a node is a 2^-20 event. That holds
# only while every node is in rotation, so the stack is waited for first
# (see `wait_for_nodes` in lib.sh): a returning node's catch-up window would
# otherwise land inside the measured traffic as refusals.
#
# Usage:
#   deploy/ha/plugin-drill.sh              # assumes the stack is already up
#   START_STACK=1 deploy/ha/plugin-drill.sh
#
# Requires: docker compose, curl, jq, orion-cli (see lib.sh). Runs ~40 s.

set -euo pipefail

cd "$(dirname "$0")/../.."
source deploy/ha/lib.sh

DURATION_SECS="${DURATION_SECS:-20}"
CONSECUTIVE="${CONSECUTIVE:-20}"
FIXTURES="crates/orion-server/tests/fixtures/plugins"

if [[ "${START_STACK:-0}" == "1" ]]; then
    echo "==> Starting the HA stack..."
    "${COMPOSE[@]}" up -d --wait
fi

echo "==> Waiting for every node to be healthy: ${NODES[*]}"
wait_for_nodes

echo "==> Baseline check via LB: $LB_URL/health"
curl -fsS -o /dev/null "$LB_URL/health" || {
    echo "LB is not serving; is the stack up? (START_STACK=1 $0)"
    exit 1
}

work="$(mktemp -d)"
codes_file="$work/codes"
# The refusals themselves, not just their count: a bare "36 non-2xx" says
# nothing about which refusal it was, and the body distinguishes every one
# this drill can provoke — an unknown channel from a quarantined one from a
# workflow that failed. Kept to the first few; they are all the same shape.
refusals_file="$work/refusals"
touch "$codes_file" "$refusals_file"
found_head=$(head_revision)
trap 'restore_head "$found_head" || echo "WARN: could not restore head revision $found_head"; rm -rf "$work"' EXIT

# The fixture plugin, and a workflow and channel that call it. The upload
# manifest names the functions the load-time self-test can probe.
set_dir="$work/plugin-drill"
mkdir -p "$set_dir/plugin"
cp "$FIXTURES/fixture-upload.toml" "$FIXTURES/fixture.wasm" "$set_dir/plugin/"
cat >"$set_dir/workflow.json" <<'EOF'
{
  "workflow_id": "plugin-drill",
  "name": "plugin drill",
  "condition": true,
  "tasks": [
    {"id": "parse", "name": "parse", "function": {"name": "parse_json", "input": {"source": "payload", "target": "input"}}},
    {"id": "wrap", "name": "wrap", "function": {"name": "test.fixture.wrap", "input": {"message": {"var": "data.input.msg"}, "output": "data.result"}}}
  ]
}
EOF
cat >"$set_dir/channel.json" <<'EOF'
{
  "channel_id": "plugin-drill", "name": "plugin-drill", "channel_type": "sync", "protocol": "http",
  "methods": ["POST"], "route_pattern": "/plugin-drill", "workflow_id": "plugin-drill"
}
EOF

# The plugin's member revision as one LB read sees it.
served_revision() {
    curl -fsS "${auth[@]}" "$ADMIN/functions" |
        jq -r '.data[] | select(.name == "test.fixture.wrap") | .plugin.version'
}

# Every one of $CONSECUTIVE consecutive LB reads serves the plugin at the
# given revision — the round-robin has visited every upstream by then.
wait_converged_on() {
    local want="$1" attempt i
    for attempt in $(seq 1 30); do
        for ((i = 0; i < CONSECUTIVE; i++)); do
            [[ "$(served_revision)" == "$want" ]] || continue 2
        done
        echo "    converged after ${attempt} check(s)"
        return 0
    done
    echo "FAIL: plugin revision $want did not converge on every node"
    exit 1
}

echo "==> Deploying the plugin, its workflow and channel (one node takes the write)"
deploy_set "$set_dir"
v1=$(served_revision)
echo "    plugin revision: $v1"
echo "==> Waiting for every node to serve test.fixture.wrap at revision $v1"
wait_converged_on "$v1"

# The channel converges with the same deployment; wait for a run of successes.
for attempt in $(seq 1 30); do
    ok=1
    for ((i = 0; i < CONSECUTIVE; i++)); do
        code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$LB_URL/api/v2/data/plugin-drill" \
            -H 'Content-Type: application/json' -d '{"data":{"msg":"hi"}}')
        [[ "$code" == "200" ]] || { ok=0; break; }
    done
    [[ "$ok" == "1" ]] && break
    [[ "$attempt" -eq 30 ]] && { echo "FAIL: the plugin-backed channel never served on every node"; exit 1; }
    sleep 1
done

# Revision 2: the same component under a new manifest version. The plugin
# definition's digest changes, so the apply is a new member revision and a
# full engine rebuild on every node.
sed 's/^version = .*/version = "0.0.1"/' "$FIXTURES/fixture-upload.toml" >"$set_dir/plugin/fixture-upload.toml"

echo "==> Driving traffic for ${DURATION_SECS}s while deploying plugin revision 2..."
end=$((SECONDS + DURATION_SECS))
(
    sleep 4
    deploy_set "$set_dir"
) &
deployer=$!
total=0
while ((SECONDS < end)); do
    response=$(curl -s -w '\n%{http_code}' --max-time 5 -X POST "$LB_URL/api/v2/data/plugin-drill" \
        -H 'Content-Type: application/json' -d '{"data":{"msg":"hi"}}' || printf '\n000')
    code=${response##*$'\n'}
    echo "$code" >>"$codes_file"
    if [[ "$code" != 2* ]] && [[ "$(wc -l <"$refusals_file")" -lt 3 ]]; then
        printf '%s %s\n' "$code" "$(printf '%s' "${response%$'\n'*}" | tr -d '\n' | cut -c1-300)" \
            >>"$refusals_file"
    fi
    total=$((total + 1))
done
wait "$deployer" || { echo "FAIL: deploying plugin revision 2 failed"; exit 1; }

non2xx=$(grep -cv '^2' "$codes_file" || true)
echo "==> $total requests during the deployment, $non2xx non-2xx"
sort "$codes_file" | uniq -c | sed 's/^/    /'
if [[ -s "$refusals_file" ]]; then
    sed 's/^/    refused: /' "$refusals_file"
fi

v2=$((v1 + 1))
echo "==> Waiting for every node to serve revision $v2"
wait_converged_on "$v2"

echo "==> Leaving the stack as found (head revision $found_head)"
if [[ "$non2xx" -ne 0 ]]; then
    echo "FAIL: $non2xx non-2xx response(s) while a plugin revision deployed under load"
    exit 1
fi
echo "PASS: converged on every node, and zero non-2xx across a plugin deployment under load"

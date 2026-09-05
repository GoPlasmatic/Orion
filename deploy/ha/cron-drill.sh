#!/usr/bin/env bash
# Cron drill for the HA reference topology.
#
# The scheduler's two cluster claims, exercised against the real compose stack
# rather than against two AppStates in one process:
#
#   1. **One occurrence per scheduled instant.** Both nodes run their own
#      reconciler against one database. `UNIQUE(channel_id, scheduled_for)` is
#      what makes that safe, and this asserts it on the rows the two of them
#      actually wrote — not on a simulation of the race.
#   2. **A `forbid` singleton holds across a rolling restart.** One node is
#      taken down and brought back while a schedule fires faster than its work
#      takes. At no sampled instant may two occurrences of one key be
#      `running`, including the window where the restarted node is catching up
#      and re-claiming leases the dead one left behind.
#
# The second is the one that cannot be tested anywhere else. The cluster test
# suite proves non-overlap between two live nodes; only a restart proves it
# across the lease-expiry path, where a peer takes over work whose owner is
# gone and the returning node comes back holding stale beliefs.
#
# Sampling, not proof: the drill reads the ledger repeatedly and asserts the
# invariant on every read. A violation is a state that persists for at least
# one poll interval, which is what a real overlap would be.
#
# Usage:
#   deploy/ha/cron-drill.sh              # assumes the stack is already up
#   START_STACK=1 deploy/ha/cron-drill.sh
#
# Requires: docker compose, curl, jq. Runs ~60 s.

set -euo pipefail

COMPOSE=(docker compose -f docker-compose.ha.yml)
LB_URL="${LB_URL:-http://localhost:8080}"
NODES=(${NODES:-orion-a orion-b})
RESTART_NODE="${RESTART_NODE:-orion-b}"
HEALTH_TIMEOUT_SECS="${HEALTH_TIMEOUT_SECS:-60}"
# Long enough to span the restart and several occurrences either side of it.
OBSERVE_SECS="${OBSERVE_SECS:-45}"

cd "$(dirname "$0")/../.."

export ORION_ADMIN_API_KEYS="${ORION_ADMIN_API_KEYS:-drill-only-throwaway-admin-key-padded-past-32}"
ADMIN_KEY="${ORION_ADMIN_API_KEYS%%,*}"
auth=(-H "Authorization: Bearer ${ADMIN_KEY}")
ADMIN="$LB_URL/api/v1/admin"

if [[ "${START_STACK:-0}" == "1" ]]; then
    echo "==> Starting the HA stack..."
    "${COMPOSE[@]}" up -d --wait
fi

wait_for_nodes() {
    for node in "${NODES[@]}"; do
        for ((i = 0; i < HEALTH_TIMEOUT_SECS; i++)); do
            id=$("${COMPOSE[@]}" ps -q "$node" 2>/dev/null || true)
            if [[ -n "$id" ]] &&
                [[ "$(docker inspect -f '{{.State.Health.Status}}' "$id" 2>/dev/null || true)" == "healthy" ]]; then
                continue 2
            fi
            sleep 1
        done
        echo "FAIL: $node did not become healthy within ${HEALTH_TIMEOUT_SECS}s"
        exit 1
    done
}

echo "==> Waiting for every node to be healthy: ${NODES[*]}"
wait_for_nodes

echo "==> Baseline check via LB: $LB_URL/health"
curl -fsS -o /dev/null "$LB_URL/health" || {
    echo "LB is not serving; is the stack up? (START_STACK=1 $0)"
    exit 1
}

cleanup() {
    curl -fsS -X PATCH "${auth[@]}" -H 'Content-Type: application/json' \
        -d '{"status":"archived"}' "$ADMIN/channels/ha-cron/status" >/dev/null 2>&1 || true
    curl -fsS -X DELETE "${auth[@]}" "$ADMIN/channels/ha-cron" >/dev/null 2>&1 || true
    curl -fsS -X PATCH "${auth[@]}" -H 'Content-Type: application/json' \
        -d '{"status":"archived"}' "$ADMIN/workflows/ha-cron-wf/status" >/dev/null 2>&1 || true
    curl -fsS -X DELETE "${auth[@]}" "$ADMIN/workflows/ha-cron-wf" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

echo "==> Creating a schedule that fires faster than its work takes"
# `sleep` is not a task function, so the work is made slow the honest way: a
# loop the engine has to walk. It only has to outlast one second.
curl -fsS -X POST "${auth[@]}" -H 'Content-Type: application/json' -d '{
  "workflow_id": "ha-cron-wf",
  "name": "HA cron drill",
  "condition": true,
  "tasks": [
    {
      "id": "spin",
      "name": "Take a moment",
      "loop": { "counter": "data.i", "init": 0, "max": 400, "increment": 1 },
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.spin", "logic": { "cat": ["tick-", { "var": "data.i" }] } }
      ] } }
    }
  ]
}' "$ADMIN/workflows" >/dev/null
curl -fsS -X PATCH "${auth[@]}" -H 'Content-Type: application/json' \
    -d '{"status":"active"}' "$ADMIN/workflows/ha-cron-wf/status" >/dev/null

curl -fsS -X POST "${auth[@]}" -H 'Content-Type: application/json' -d '{
  "channel_id": "ha-cron",
  "name": "ha-cron",
  "channel_type": "async",
  "protocol": "cron",
  "workflow_id": "ha-cron-wf",
  "transport_config": {
    "schedule": "* * * * * *",
    "concurrency": { "policy": "forbid" }
  }
}' "$ADMIN/channels" >/dev/null
curl -fsS -X PATCH "${auth[@]}" -H 'Content-Type: application/json' \
    -d '{"status":"active"}' "$ADMIN/channels/ha-cron/status" >/dev/null

# The ledger, as either node reports it — it is shared state, so whichever
# upstream the LB picks answers the same rows.
occurrences() {
    curl -fsS "${auth[@]}" "$ADMIN/cron/occurrences?channel_id=ha-cron&limit=200"
}

echo "==> Sampling the ledger while ${RESTART_NODE} restarts under load"
max_running=0
violations=0
restarted=0
deadline=$((SECONDS + OBSERVE_SECS))

while [ $SECONDS -lt $deadline ]; do
    body=$(occurrences || true)
    if [[ -n "$body" ]]; then
        running=$(jq -r '[.data[] | select(.status == "running")] | length' <<<"$body" 2>/dev/null || echo 0)
        [ "${running:-0}" -gt "$max_running" ] && max_running=$running
        if [ "${running:-0}" -gt 1 ]; then
            violations=$((violations + 1))
            echo "VIOLATION: $running occurrences of one singleton key running at once"
            jq -r '.data[] | select(.status == "running") | "  \(.id) scheduled_for=\(.scheduled_for)"' <<<"$body"
        fi
    fi

    # Roughly a third of the way in, take a node out and bring it back. Its
    # in-flight occurrence dies with it; the survivor must not start a second
    # copy before the lease expires, and must not run two once it does.
    if [ "$restarted" -eq 0 ] && [ $SECONDS -gt $((deadline - OBSERVE_SECS * 2 / 3)) ]; then
        echo "==> Restarting ${RESTART_NODE}"
        "${COMPOSE[@]}" restart "$RESTART_NODE" >/dev/null 2>&1 || true
        restarted=1
    fi
    sleep 1
done

echo "==> Waiting for the restarted node to rejoin"
wait_for_nodes

body=$(occurrences)
total=$(jq -r '.data | length' <<<"$body")
completed=$(jq -r '[.data[] | select(.status == "completed")] | length' <<<"$body")
skipped=$(jq -r '[.data[] | select(.status == "skipped_singleton")] | length' <<<"$body")

# Claim 1: one row per scheduled instant, however many reconcilers wrote them.
instants=$(jq -r '[.data[].scheduled_for] | length' <<<"$body")
unique=$(jq -r '[.data[].scheduled_for] | unique | length' <<<"$body")

echo
echo "==> Results"
echo "  occurrences:        $total ($completed completed, $skipped skipped_singleton)"
echo "  distinct instants:  $unique of $instants"
echo "  max concurrent run: $max_running"
echo "  violations:         $violations"

if [ "$instants" -ne "$unique" ]; then
    echo "FAIL: two reconcilers produced duplicate occurrences for one instant"
    exit 1
fi
if [ "$violations" -ne 0 ]; then
    echo "FAIL: a forbid singleton admitted overlapping occurrences across the cluster"
    exit 1
fi
if [ "$completed" -eq 0 ]; then
    echo "FAIL: nothing completed — the schedule never ran, so nothing was proven"
    exit 1
fi

echo
echo "PASS: one occurrence per instant, and never two of one key at once — across a restart."

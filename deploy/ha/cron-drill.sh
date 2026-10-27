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
# Requires: docker compose, curl, jq, orion-cli (see lib.sh). Runs ~60 s.

set -euo pipefail

cd "$(dirname "$0")/../.."
source deploy/ha/lib.sh

RESTART_NODE="${RESTART_NODE:-orion-b}"
# Long enough to span the restart and several occurrences either side of it.
OBSERVE_SECS="${OBSERVE_SECS:-45}"
# How much work one occurrence does: a 10,000-sweep loop summing this many
# numbers per sweep. The default takes a release build a few seconds — longer
# than the one-second schedule, so occurrences contend for the singleton.
SPIN_SIZE="${SPIN_SIZE:-50000}"

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
found_head=$(head_revision)
trap 'restore_head "$found_head" || echo "WARN: could not restore head revision $found_head"; rm -rf "$work"' EXIT

echo "==> Deploying a schedule that fires faster than its work takes"
# `sleep` is not a task function, so the work is made slow the honest way: a
# loop the engine has to walk, over numbers the schedule's payload carries (a
# literal array would be folded when the workflow compiles).
set_dir="$work/ha-cron"
mkdir -p "$set_dir"
jq -n '{
  workflow_id: "ha-cron-wf",
  name: "HA cron drill",
  condition: true,
  loop: {counter: "i", max: 10000},
  tasks: [{
    id: "spin",
    name: "Take a moment",
    function: {name: "map", input: {mappings: [{
      path: "temp_data.sum",
      logic: {reduce: [{var: "data.xs"}, {"+": [{var: "accumulator"}, {var: "current"}]}, 0]}
    }]}}
  }]
}' >"$set_dir/workflow.json"
jq -n --argjson n "$SPIN_SIZE" '{
  channel_id: "ha-cron",
  name: "ha-cron",
  channel_type: "async",
  protocol: "cron",
  workflow_id: "ha-cron-wf",
  transport_config: {
    schedule: "* * * * * *",
    concurrency: {policy: "forbid"},
    payload: {xs: [range($n)]}
  }
}' >"$set_dir/channel.json"
deploy_set "$set_dir"

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

#!/usr/bin/env bash
# Rolling-deploy drill for the HA reference topology (multi-instance-ha A9/B4).
#
# Sends steady traffic through the load balancer while SIGTERM-ing one node,
# then asserts ZERO non-2xx responses were observed: the node withdraws
# readiness, keeps serving through its drain window, and nginx retries the
# surviving node for anything that slips through.
#
# Usage:
#   deploy/ha/rolling-drill.sh            # assumes the stack is already up
#   START_STACK=1 deploy/ha/rolling-drill.sh   # up -d --wait first
#
# Requires: docker compose, curl. Runs ~30 s.

set -euo pipefail

COMPOSE=(docker compose -f docker-compose.ha.yml)
LB_URL="${LB_URL:-http://localhost:8080/health}"
VICTIM="${VICTIM:-orion-a}"
DURATION_SECS="${DURATION_SECS:-25}"

cd "$(dirname "$0")/../.."

if [[ "${START_STACK:-0}" == "1" ]]; then
    echo "==> Starting the HA stack..."
    "${COMPOSE[@]}" up -d --wait
fi

echo "==> Baseline check via LB: $LB_URL"
curl -fsS -o /dev/null "$LB_URL" || {
    echo "LB is not serving; is the stack up? (START_STACK=1 $0)"
    exit 1
}

codes_file="$(mktemp)"
trap 'rm -f "$codes_file"' EXIT

echo "==> Driving traffic for ${DURATION_SECS}s while SIGTERM-ing ${VICTIM}..."
end=$((SECONDS + DURATION_SECS))
(
    # SIGTERM the victim a few seconds into the load window. `stop` sends
    # SIGTERM and waits (stop_grace_period covers drain + force timeout).
    sleep 3
    "${COMPOSE[@]}" stop "$VICTIM" >/dev/null 2>&1
) &
killer_pid=$!

total=0
while ((SECONDS < end)); do
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$LB_URL" || echo "000")
    echo "$code" >>"$codes_file"
    total=$((total + 1))
done
wait "$killer_pid" || true

non_2xx=$(grep -cv '^2' "$codes_file" || true)
echo "==> $total requests, $non_2xx non-2xx"
sort "$codes_file" | uniq -c | sed 's/^/    /'

echo "==> Restarting ${VICTIM} for symmetry..."
"${COMPOSE[@]}" start "$VICTIM" >/dev/null

if ((non_2xx > 0)); then
    echo "FAIL: $non_2xx non-2xx responses observed at the LB during the roll"
    exit 1
fi
echo "PASS: zero non-2xx responses at the LB while ${VICTIM} was rolled"

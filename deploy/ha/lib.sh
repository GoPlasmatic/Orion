# Shared by the HA drills; sourced, not run. The drills reach the nodes only
# through the load balancer, and change what they serve the way an operator
# does: a definition set compiled with `orion-cli compile` and applied with
# `orion-cli deployments apply -d`. An apply replaces the whole head, so each
# drill records the head it found and puts it back on exit.
#
# The CLI comes from $ORION_CLI, else this tree's release or debug build, else
# PATH. The stack must run an image that speaks the v2 admin API — until a
# release ships one, the locally built image:
#   docker compose -f docker-compose.ha.yml -f docker-compose.ha.build.yml up -d --wait

COMPOSE=(docker compose -f docker-compose.ha.yml)
# Published defaults still target v1. Use this checkout until a v2 image is selected.
if [[ -z "${ORION_VERSION:-}" ]]; then
    COMPOSE+=(-f docker-compose.ha.build.yml)
fi
LB_URL="${LB_URL:-http://localhost:8080}"
NODES=(${NODES:-orion-a orion-b})
HEALTH_TIMEOUT_SECS="${HEALTH_TIMEOUT_SECS:-60}"

# The HA compose enforces admin auth; the stack needs the key set for every
# command, and the admin calls below carry it.
export ORION_ADMIN_API_KEYS="${ORION_ADMIN_API_KEYS:-drill-only-throwaway-admin-key-padded-past-32}"
ADMIN_KEY="${ORION_ADMIN_API_KEYS%%,*}"
auth=(-H "Authorization: Bearer ${ADMIN_KEY}")
ADMIN="$LB_URL/api/v2/admin"

if [[ -z "${ORION_CLI:-}" ]]; then
    for candidate in target/release/orion-cli target/debug/orion-cli; do
        [[ -x "$candidate" ]] && { ORION_CLI="$PWD/$candidate"; break; }
    done
fi
ORION_CLI="${ORION_CLI:-$(command -v orion-cli || true)}"
[[ -n "$ORION_CLI" ]] || {
    echo "orion-cli not found: build it (cargo build -p orion-cli) or set ORION_CLI"
    exit 1
}

cli() {
    "$ORION_CLI" --server "$LB_URL" --api-key "$ADMIN_KEY" --output json --yes "$@"
}

# Every node in rotation before anything is measured: nginx retries the
# survivor when an upstream refuses a connection, so a node still booting is
# one the LB hides rather than one a drill can see.
wait_for_nodes() {
    local node id i
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

head_revision() {
    curl -fsS "${auth[@]}" "$ADMIN/deployments" | jq -r '.head_revision'
}

# deploy_set <set_dir> — compile into <set_dir>.dist and apply it.
deploy_set() {
    local dir="$1"
    rm -rf "$dir.dist"
    "$ORION_CLI" compile "$dir" -o "$dir.dist" >/dev/null
    cli deployments apply -d "$dir.dist" >/dev/null
}

# restore_head <revision> — put back the head a drill found: a rollback to
# it, or an empty head when there was none. A no-op when the drill committed
# nothing.
restore_head() {
    local found="$1" now manifest digest
    now=$(head_revision)
    [[ -z "$now" || "$now" == "$found" ]] && return 0
    if [[ "$found" -gt 0 ]]; then
        cli deployments rollback --target-revision "$found" --expected-head-revision "$now" \
            --idempotency-key "ha-drill-restore-$found-$now" --reason "ha drill: restore" >/dev/null
        return
    fi
    # `compile` refuses an empty set, so the empty head is a manifest.
    manifest=$(mktemp)
    echo '{"schema_version": 2, "providers": [], "members": []}' >"$manifest"
    digest=$(cli deployments plan -f "$manifest" --base-revision "$now" | jq -r '.reviewed_manifest_digest')
    cli deployments apply -f "$manifest" --base-revision "$now" --reviewed-digest "$digest" \
        --idempotency-key "ha-drill-empty-$now" --reason "ha drill: restore" >/dev/null
    rm -f "$manifest"
}

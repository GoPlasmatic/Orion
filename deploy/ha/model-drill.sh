#!/usr/bin/env bash
# Model drill for the HA reference topology.
#
# The model design splits one claim in two, and a cluster is where the split
# becomes visible. This exercises both against the real compose stack through
# its load balancer:
#
#   1. The VERDICT is shared. Admission runs once, on the node that took the
#      registration; the verdict lands on the row, and every other node loads
#      the model on it through the `models` epoch scope without re-fetching,
#      re-probing or being asked. `admission.node` names one node forever.
#   2. The BYTES are not. Each node's artifact cache is its own, so every node
#      fetches the object through the storage connector and checks the digest
#      before it will run the graph. `health.state` going to `loaded` on every
#      node is that fetch having happened on each of them.
#
# Then the refusal that matters operationally: with the bucket stopped, a node
# that has not yet cached the artifact fails the *call*, not the activation —
# so an unreachable bucket degrades new nodes rather than the running estate.
#
# Nodes are reached only through nginx (round-robin, two upstreams), so
# "every node" is asserted as a run of consecutive LB reads that all agree —
# with two upstreams, 20 in a row missing a node is a 2^-20 event. And as in
# the plugin drill, that argument holds only while every node is in rotation,
# so the stack is waited for first.
#
# `GET /admin/models/{id}` reports the node that answered, not the cluster:
# `health` is per-node by construction, which is exactly what makes it the
# right probe here.
#
# Usage:
#   deploy/ha/model-drill.sh              # assumes the stack is already up
#   START_STACK=1 deploy/ha/model-drill.sh
#
# The stack needs the bucket overlay, which is what serves the artifact:
#   docker compose -f docker-compose.ha.yml \
#                  -f deploy/ha/docker-compose.bucket.yml up -d --wait
#
# Requires: docker compose, curl, jq, python3, sha256sum (or shasum).
# Runs ~60 s.

set -euo pipefail

COMPOSE=(docker compose -f docker-compose.ha.yml -f deploy/ha/docker-compose.bucket.yml)
LB_URL="${LB_URL:-http://localhost:8080}"
CONSECUTIVE="${CONSECUTIVE:-20}"
NODES=(${NODES:-orion-a orion-b})
HEALTH_TIMEOUT_SECS="${HEALTH_TIMEOUT_SECS:-60}"
ADMIT_TIMEOUT_SECS="${ADMIT_TIMEOUT_SECS:-60}"

cd "$(dirname "$0")/../.."

export ORION_ADMIN_API_KEYS="${ORION_ADMIN_API_KEYS:-drill-only-throwaway-admin-key-padded-past-32}"
ADMIN_KEY="${ORION_ADMIN_API_KEYS%%,*}"
auth=(-H "Authorization: Bearer ${ADMIN_KEY}")
ADMIN="$LB_URL/api/v1/admin"

FIXTURE="crates/orion-server/tests/fixtures/models/c4-tiny"
MODEL_ID="ada.c4-tiny"
# Path-style puts the bucket name in the path, so the connector requests
# {endpoint}/{bucket}/{key} = http://bucket/models/c4-tiny.onnx — which is
# where the overlay mounts the fixture. The key is the object within the
# bucket, not the whole path.
KEY="c4-tiny.onnx"

if [[ "${START_STACK:-0}" == "1" ]]; then
    echo "==> Starting the HA stack with the bucket overlay..."
    "${COMPOSE[@]}" up -d --wait
fi

echo "==> Waiting for every node to be healthy: ${NODES[*]}"
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

echo "==> Baseline check via LB: $LB_URL/health"
curl -fsS -o /dev/null "$LB_URL/health" || {
    echo "LB is not serving; is the stack up? (START_STACK=1 $0)"
    exit 1
}

# The bucket has to be in the stack — without the overlay every admission
# below fails at `head`, which is a confusing way to learn the file is missing.
if ! "${COMPOSE[@]}" ps --status running --services 2>/dev/null | grep -qx bucket; then
    echo "FAIL: no 'bucket' service running. Bring the stack up with the overlay:"
    echo "  docker compose -f docker-compose.ha.yml -f deploy/ha/docker-compose.bucket.yml up -d --wait"
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    DIGEST="sha256:$(sha256sum "$FIXTURE/c4-tiny.onnx" | cut -d' ' -f1)"
else
    DIGEST="sha256:$(shasum -a 256 "$FIXTURE/c4-tiny.onnx" | cut -d' ' -f1)"
fi
echo "    artifact digest: $DIGEST"

# Connectors are addressed by id, not by name, so a cleanup has to look the
# id up first — a `DELETE /connectors/drill-bucket` answers 404 and leaves the
# row standing, which then fails the create below with a 409.
delete_connector() {
    local id
    id=$(curl -fsS "${auth[@]}" "$ADMIN/connectors" 2>/dev/null \
        | jq -r '.data[]? | select(.name == "drill-bucket") | .id' || true)
    [[ -n "$id" ]] && curl -fsS "${auth[@]}" -X DELETE "$ADMIN/connectors/$id" >/dev/null 2>&1 || true
}

# A clean slate, in dependency order: the channel and workflow before the
# model, since a model delete is refused while an active workflow names it.
echo "==> Clearing any previous drill state"
curl -fsS "${auth[@]}" -X DELETE "$ADMIN/channels/model-drill" >/dev/null 2>&1 || true
curl -fsS "${auth[@]}" -X DELETE "$ADMIN/workflows/model-drill" >/dev/null 2>&1 || true
curl -fsS "${auth[@]}" -X DELETE "$ADMIN/models/$MODEL_ID" >/dev/null 2>&1 || true
delete_connector

echo "==> The storage connector the artifact is read through"
curl -fsS "${auth[@]}" -X POST "$ADMIN/connectors" -H 'Content-Type: application/json' -d '{
  "name": "drill-bucket", "connector_type": "storage", "tags": ["drill"],
  "config": {
    "endpoint": "http://bucket", "region": "us-east-1", "bucket": "models",
    "access_key": "AKIADRILL", "secret_key": "drill-secret",
    "force_path_style": true, "allow_private_urls": true
  }}' >/dev/null

# The write landed on one node; the others pick the connector up into their
# in-memory registry on their next epoch tick. A registration is validated by
# whichever node the LB hands it to — it HEADs the object through that node's
# registry before writing the row — so a registration sent too early fails
# with a 400 at the `head` stage on whichever node has not caught up.
#
# There is no read that settles this from outside: `GET /connectors` is
# served from the database, so it answers the instant the row is written and
# says nothing about any node's registry. The registration itself is the only
# probe of the thing that matters, so it is what gets retried.
echo "==> Registering the model (retried until the answering node has the connector)"
registration=$(python3 -c 'import json, sys
manifest = json.load(open(sys.argv[1]))
json.dump({"manifest": manifest,
           "artifact": {"connector": "drill-bucket", "key": sys.argv[2], "digest": sys.argv[3]},
           "tags": ["drill"]}, sys.stdout)' \
    "$FIXTURE/model.json" "$KEY" "$DIGEST")
registered=0
for attempt in $(seq 1 30); do
    body=$(curl -sS "${auth[@]}" -X POST "$ADMIN/models" \
        -H 'Content-Type: application/json' --data "$registration")
    if [[ "$(jq -r '.data.admission.state // empty' <<<"$body")" != "" ]]; then
        echo "    admission: $(jq -r '.data.admission.state' <<<"$body") (after ${attempt} attempt(s))"
        registered=1
        break
    fi
    sleep 1
done
if [[ "$registered" != "1" ]]; then
    echo "FAIL: the model could not be registered:"
    jq -c '.error // .' <<<"$body" | sed 's/^/    /'
    exit 1
fi

# The registration is answered by one node and admitted by its worker, which
# is asynchronous — so this polls the row rather than assuming.
echo "==> Waiting for admission (max ${ADMIT_TIMEOUT_SECS}s)"
for ((i = 0; i < ADMIT_TIMEOUT_SECS; i++)); do
    admission=$(curl -fsS "${auth[@]}" "$ADMIN/models/$MODEL_ID" | jq -c '.data.admission')
    state=$(jq -r '.state' <<<"$admission")
    [[ "$state" == "pending" ]] || break
    sleep 1
done
if [[ "$state" != "passed" ]]; then
    echo "FAIL: admission did not pass: $admission"
    exit 1
fi
ADMITTED_BY=$(jq -r '.node' <<<"$admission")
stats=$(curl -fsS "${auth[@]}" "$ADMIN/models/$MODEL_ID" | jq -c '.data.stats')
echo "    passed on $ADMITTED_BY: $stats"

echo "==> Activating"
curl -fsS "${auth[@]}" -X PATCH "$ADMIN/models/$MODEL_ID/status" \
    -H 'Content-Type: application/json' -d '{"status":"active"}' >/dev/null

# Claim 1: every node carries it, and every node agrees it was admitted by the
# one node that ran the sequence. A second node re-running admission would
# rewrite `admission.node`, so a run of consecutive reads all naming the same
# node is the assertion that it did not.
echo "==> Waiting for every node to carry the model (epoch poll is 2s)"
carried_everywhere() {
    local i view
    for ((i = 0; i < CONSECUTIVE; i++)); do
        view=$(curl -fsS "${auth[@]}" "$ADMIN/models/$MODEL_ID" \
            | jq -r '.data.health.state + " " + .data.admission.node')
        case "$view" in
            "admitted $ADMITTED_BY" | "loaded $ADMITTED_BY" | "evicted $ADMITTED_BY") ;;
            *) return 1 ;;
        esac
    done
    return 0
}
for attempt in $(seq 1 30); do
    if carried_everywhere; then
        echo "    every node carries it, all crediting $ADMITTED_BY"
        break
    fi
    if [[ "$attempt" -eq 30 ]]; then
        echo "FAIL: the model did not converge on every node"
        curl -fsS "${auth[@]}" "$ADMIN/models/$MODEL_ID" | jq -c '.data.health'
        exit 1
    fi
    sleep 1
done

echo "==> A workflow and channel that call the model"
curl -fsS "${auth[@]}" -X POST "$ADMIN/workflows" -H 'Content-Type: application/json' -d '{
  "workflow_id": "model-drill", "name": "model drill", "tags": ["drill"],
  "tasks": [
    {"id": "parse", "name": "parse", "function": {"name": "parse_json", "input": {"source": "payload", "target": "board"}}},
    {"id": "infer", "name": "infer", "function": {"name": "model_infer", "input": {
       "model": "'"$MODEL_ID"'", "input": {"var": ""}, "output": "data.policy",
       "stats_output": "data.inference"}}}
  ]}' >/dev/null
curl -fsS "${auth[@]}" -X PATCH "$ADMIN/workflows/model-drill/status" \
    -H 'Content-Type: application/json' -d '{"status":"active"}' >/dev/null
curl -fsS "${auth[@]}" -X POST "$ADMIN/channels" -H 'Content-Type: application/json' -d '{
  "channel_id": "model-drill", "name": "model-drill", "channel_type": "sync", "protocol": "http",
  "methods": ["POST"], "route_pattern": "/model-drill", "workflow_id": "model-drill", "tags": ["drill"]}' >/dev/null
curl -fsS "${auth[@]}" -X PATCH "$ADMIN/channels/model-drill/status" \
    -H 'Content-Type: application/json' -d '{"status":"active"}' >/dev/null

# A zero board of the manifest's [1, 2, 6, 7].
BOARD=$(python3 -c 'import json; print(json.dumps({"data": [[[[0.0]*7]*6]*2]}))')

echo "==> Every node serves an inference"
for attempt in $(seq 1 30); do
    ok=1
    for ((i = 0; i < CONSECUTIVE; i++)); do
        code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$LB_URL/api/v1/data/model-drill" \
            -H 'Content-Type: application/json' -d "$BOARD")
        [[ "$code" == "200" ]] || { ok=0; break; }
    done
    [[ "$ok" == "1" ]] && break
    [[ "$attempt" -eq 30 ]] && {
        echo "FAIL: the model-backed channel never served on every node"
        curl -s -X POST "$LB_URL/api/v1/data/model-drill" \
            -H 'Content-Type: application/json' -d "$BOARD" | head -c 400
        exit 1
    }
    sleep 1
done
curl -fsS -X POST "$LB_URL/api/v1/data/model-drill" \
    -H 'Content-Type: application/json' -d "$BOARD" \
    | jq -c '{policy: .data.policy.policy, inference: .data.inference}' \
    | sed 's/^/    /'

# Claim 2: every node fetched the bytes for itself. `loaded` means a runtime
# on the node that answered holds the session, which it can only have built
# from bytes it fetched and hashed — its cache started empty.
echo "==> Every node holds its own copy (health = loaded)"
for attempt in $(seq 1 30); do
    all=1
    for ((i = 0; i < CONSECUTIVE; i++)); do
        seen=$(curl -fsS "${auth[@]}" "$ADMIN/models/$MODEL_ID" | jq -r '.data.health.state')
        [[ "$seen" == "loaded" ]] || { all=0; break; }
    done
    [[ "$all" == "1" ]] && { echo "    every node has it resident"; break; }
    [[ "$attempt" -eq 30 ]] && {
        echo "FAIL: some node never loaded the artifact for itself"
        curl -fsS "${auth[@]}" "$ADMIN/models/$MODEL_ID" | jq -c '.data.health'
        exit 1
    }
    sleep 1
done

# The operational corollary: an unreachable bucket does not take the estate
# down, because every serving node already holds the bytes.
echo "==> Stopping the bucket; the running estate keeps serving"
"${COMPOSE[@]}" stop bucket >/dev/null 2>&1
served=0
for ((i = 0; i < CONSECUTIVE; i++)); do
    code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$LB_URL/api/v1/data/model-drill" \
        -H 'Content-Type: application/json' -d "$BOARD")
    [[ "$code" == "200" ]] && served=$((served + 1))
done
"${COMPOSE[@]}" start bucket >/dev/null 2>&1
echo "    $served/$CONSECUTIVE served with the bucket down"

echo "==> Leaving the stack as found"
curl -fsS "${auth[@]}" -X DELETE "$ADMIN/channels/model-drill" >/dev/null
curl -fsS "${auth[@]}" -X DELETE "$ADMIN/workflows/model-drill" >/dev/null
curl -fsS "${auth[@]}" -X DELETE "$ADMIN/models/$MODEL_ID" >/dev/null
delete_connector

if [[ "$served" -ne "$CONSECUTIVE" ]]; then
    echo "FAIL: $((CONSECUTIVE - served)) request(s) failed with the bucket down, but every"
    echo "      node had already loaded the artifact — the data path must not need the bucket"
    exit 1
fi
echo "PASS: one admission on $ADMITTED_BY, every node carrying it on that verdict,"
echo "      every node holding its own verified copy, and the data path independent of the bucket"

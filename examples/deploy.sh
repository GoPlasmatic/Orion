#!/usr/bin/env bash
#
# Deploy and run one example package end-to-end.
#
#   ./deploy.sh <package> [base-url]
#
# A package is the set of entities that ship together — its channels, their
# workflows, and the connector when the package needs one. The script creates
# and activates every workflow, then every channel (creating the connector
# first, if the package ships a connector.json), then POSTs request.json to the
# primary channel's route and prints the response. Idempotent: re-running
# against the same instance skips objects that already exist. Requires curl and
# python3.
#
# File conventions inside a package directory:
#   workflow.json          the primary workflow          (required)
#   workflow-<name>.json   additional workflows          (optional)
#   channel.json           the primary channel           (required)
#   channel-<name>.json    additional channels           (optional)
#   connector.json         a connector the package needs (optional)
#   connector-<name>.json  additional connectors         (optional)
#   plugin.toml            a plugin, with its component  (optional)
#   entrant/model.json     a model the workflows call    (optional)
#   request.json           a sample request              (optional)
#
# A package whose primary channel has no route_pattern — a Kafka or a cron
# channel — is deployed the same way; the script prints the topic it now
# consumes, or the schedule, instead of sending a request. A package that
# ships a model entrant is deployed whole, but its sample request is not
# sent: the model's bytes have to be in a bucket and admitted first, which is
# an organiser's step (the script prints it).
#
# Example:
#   ./deploy.sh high-value-order
#   ./deploy.sh channel-composition http://localhost:8080
set -euo pipefail

cd "$(dirname "$0")"

DIR="${1:-}"
BASE="${2:-http://localhost:8080}"
ADMIN="$BASE/api/v1/admin"

DIR="${DIR%/}"
# Accept a bare package name (`high-value-order`) or its path (`packages/high-value-order`).
if [[ -n "$DIR" && ! -d "$DIR" && -d "packages/$DIR" ]]; then
  DIR="packages/$DIR"
fi

if [[ -z "$DIR" || ! -d "$DIR" ]]; then
  echo "usage: ./deploy.sh <package> [base-url]" >&2
  echo "example: ./deploy.sh high-value-order" >&2
  exit 1
fi
# A package written in source form — `$sql`, `$from`, `$use`, `$each`, or a
# `shared.json` beside the definitions — is compiled before anything is sent.
# The admin API is canonical-only by design and refuses source form with
# `UNCOMPILED_SOURCE`, so this is the same `compile` step a promotion does,
# run into a scratch directory whose file names are the ones below.
if grep -qlE '"\$(sql|from|use|each|param)"' "$DIR"/*.json 2>/dev/null || [[ -f "$DIR/shared.json" ]]; then
  ORION_BIN="${ORION_BIN:-}"
  if [[ -z "$ORION_BIN" ]]; then
    for candidate in ../target/debug/orion-server ../target/release/orion-server "$(command -v orion-server || true)"; do
      [[ -x "$candidate" ]] && { ORION_BIN="$candidate"; break; }
    done
  fi
  if [[ -z "$ORION_BIN" ]]; then
    echo "$DIR is written in source form and needs 'orion-server compile' first." >&2
    echo "Build it (cargo build) or set ORION_BIN=<path to orion-server>." >&2
    exit 1
  fi
  COMPILED="$(mktemp -d)"
  trap 'rm -rf "$COMPILED"' EXIT
  echo "==> Compile '$DIR' (source form) -> canonical definitions"
  "$ORION_BIN" compile "$DIR" --name "$(basename "$DIR")" --version 0.0.0 \
    --format dir -o "$COMPILED" > /dev/null
  # Everything the deploy reads that compile does not emit: the sample
  # request, a plugin and its component, a model entrant.
  for extra in request.json plugin.toml; do
    [[ -e "$DIR/$extra" ]] && cp -R "$DIR/$extra" "$COMPILED/"
  done
  [[ -d "$DIR/entrant" ]] && cp -R "$DIR/entrant" "$COMPILED/"
  PLUGIN_COMPONENT_FILE="$(ls "$DIR"/*.wasm 2>/dev/null | head -1 || true)"
  [[ -n "$PLUGIN_COMPONENT_FILE" ]] && cp "$PLUGIN_COMPONENT_FILE" "$COMPILED/"
  DIR="$COMPILED"
fi

WF_FILE="$DIR/workflow.json"
CH_FILE="$DIR/channel.json"
REQ_FILE="$DIR/request.json"
ENTRANT_FILE="$DIR/entrant/model.json"

for f in "$WF_FILE" "$CH_FILE"; do
  [[ -f "$f" ]] || { echo "missing $f" >&2; exit 1; }
done

# Read a field out of a JSON file with the stdlib (no jq dependency). Prints
# nothing when the field is absent, so a caller can test for it.
json_field() {
  python3 -c 'import json,sys
d = json.load(open(sys.argv[1]))
v = d.get(sys.argv[2])
print("" if v is None else v)' "$1" "$2"
}

# One top-level string from a plugin manifest. Deliberately not `tomllib`:
# that is Python 3.11+, and this script promises to run on `python3` — which
# is still 3.10 on Ubuntu 22.04 LTS, and was the CI runner that caught it.
# The two keys it is ever asked for (`name`, `component`) are bare top-level
# assignments above the first `[[functions]]` table, so reading exactly that
# much of the grammar is enough and stays honest about its limits.
manifest_field() {
  python3 -c 'import re,sys
path, want = sys.argv[1], sys.argv[2]
for line in open(path, encoding="utf-8"):
    line = line.strip()
    if line.startswith("["):
        break            # a table header ends the top-level block
    m = re.match(r"([A-Za-z0-9_-]+)\s*=\s*\"([^\"]*)\"", line)
    if m and m.group(1) == want:
        print(m.group(2))
        sys.exit(0)
sys.exit(f"{path}: no top-level {want} = \"...\"")' "$1" "$2"
}

have()   { curl -fsS "$1" > /dev/null 2>&1; }
active() { curl -fsS "$1" 2> /dev/null | grep -q '"status":"active"'; }

# The primary entity first, then any siblings, so a package that composes
# channels deploys its callee alongside its caller in one run.
workflow_files()  { echo "$WF_FILE"; ls "$DIR"/workflow-*.json 2>/dev/null || true; }
channel_files()   { echo "$CH_FILE"; ls "$DIR"/channel-*.json 2>/dev/null || true; }
connector_files() { ls "$DIR"/connector.json "$DIR"/connector-*.json 2>/dev/null || true; }

# A package that ships a plugin (plugin.toml beside the component it names)
# installs and activates it first: a workflow calling one of its functions is
# refused at create until the plugin is active. The server must run with
# `plugins.enabled = true`. The body is the same JSON `orion-cli plugins
# create -f` sends — manifest text plus the component as base64.
PLUGIN_FILE="$DIR/plugin.toml"
if [[ -f "$PLUGIN_FILE" ]]; then
  PLUGIN_ID=$(manifest_field "$PLUGIN_FILE" name)
  PLUGIN_COMPONENT=$(manifest_field "$PLUGIN_FILE" component)
  if have "$ADMIN/plugins/$PLUGIN_ID"; then
    echo "==> Plugin '$PLUGIN_ID' already exists"
  else
    echo "==> Create plugin '$PLUGIN_ID'"
    python3 -c 'import base64, json, os, sys
manifest_path, component = sys.argv[1], sys.argv[2]
text = open(manifest_path, encoding="utf-8").read()
with open(os.path.join(os.path.dirname(manifest_path), component), "rb") as f:
    encoded = base64.b64encode(f.read()).decode()
json.dump({"manifest": text, "component": encoded}, sys.stdout)' \
        "$PLUGIN_FILE" "$PLUGIN_COMPONENT" \
      | curl --fail-with-body -sS -X POST "$ADMIN/plugins" \
          -H 'Content-Type: application/json' --data @- > /dev/null
  fi
  if active "$ADMIN/plugins/$PLUGIN_ID"; then
    echo "==> Plugin '$PLUGIN_ID' already active"
  else
    echo "==> Activate plugin '$PLUGIN_ID'"
    curl --fail-with-body -sS -X PATCH "$ADMIN/plugins/$PLUGIN_ID/status" \
      -H 'Content-Type: application/json' -d '{"status":"active"}' > /dev/null
  fi
fi

while IFS= read -r conn; do
  [[ -n "$conn" ]] || continue
  CONN_ID=$(json_field "$conn" id)
  if have "$ADMIN/connectors/$CONN_ID"; then
    echo "==> Connector '$CONN_ID' already exists"
  else
    echo "==> Create connector '$CONN_ID'"
    curl --fail-with-body -sS -X POST "$ADMIN/connectors" \
      -H 'Content-Type: application/json' --data @"$conn" > /dev/null
  fi
done < <(connector_files)

while IFS= read -r wf; do
  [[ -n "$wf" ]] || continue
  WF_ID=$(json_field "$wf" workflow_id)
  if have "$ADMIN/workflows/$WF_ID"; then
    echo "==> Workflow '$WF_ID' already exists"
  else
    echo "==> Create workflow '$WF_ID'"
    curl --fail-with-body -sS -X POST "$ADMIN/workflows" \
      -H 'Content-Type: application/json' --data @"$wf" > /dev/null
  fi
  if active "$ADMIN/workflows/$WF_ID"; then
    echo "==> Workflow '$WF_ID' already active"
  else
    echo "==> Activate workflow '$WF_ID'"
    curl --fail-with-body -sS -X PATCH "$ADMIN/workflows/$WF_ID/status" \
      -H 'Content-Type: application/json' -d '{"status":"active"}' > /dev/null
  fi
done < <(workflow_files)

while IFS= read -r ch; do
  [[ -n "$ch" ]] || continue
  CH_ID=$(json_field "$ch" channel_id)
  if have "$ADMIN/channels/$CH_ID"; then
    echo "==> Channel '$CH_ID' already exists"
  else
    echo "==> Create channel '$CH_ID'"
    curl --fail-with-body -sS -X POST "$ADMIN/channels" \
      -H 'Content-Type: application/json' --data @"$ch" > /dev/null
  fi
  if active "$ADMIN/channels/$CH_ID"; then
    echo "==> Channel '$CH_ID' already active"
  else
    echo "==> Activate channel '$CH_ID'"
    curl --fail-with-body -sS -X PATCH "$ADMIN/channels/$CH_ID/status" \
      -H 'Content-Type: application/json' -d '{"status":"active"}' > /dev/null
  fi
done < <(channel_files)

ROUTE=$(json_field "$CH_FILE" route_pattern)
TOPIC=$(json_field "$CH_FILE" topic)

if [[ -z "$ROUTE" ]]; then
  # A Kafka channel has no HTTP route; produce to its topic to exercise it.
  echo "==> Deployed. '$(json_field "$CH_FILE" channel_id)' consumes topic '${TOPIC:-?}'"
  echo "    Produce a record to that topic to run the workflow."
  exit 0
fi

if [[ ! -f "$REQ_FILE" ]]; then
  echo "==> Deployed. No request.json to send."
  exit 0
fi

# A model entrant is registered by reference to a bucket, then admitted and
# activated — steps that need a bucket this script does not have. The
# package is deployed; the request that would run the model is left to the
# organiser, with the commands to run it.
if [[ -f "$ENTRANT_FILE" ]]; then
  MODEL_ID=$(json_field "$ENTRANT_FILE" name)
  MODEL_ARTIFACT=$(json_field "$ENTRANT_FILE" artifact)
  echo "==> Deployed. The sample request calls model '$MODEL_ID', which is registered separately:"
  echo "    1. put $DIR/entrant/$MODEL_ARTIFACT in a bucket behind a 'storage' connector"
  echo "    2. orion-cli models create -f $DIR/entrant/model.json --connector <bucket> --key <key> \\"
  echo "           --digest sha256:\$(shasum -a 256 $DIR/entrant/$MODEL_ARTIFACT | cut -d' ' -f1) --wait"
  echo "    3. orion-cli models activate $MODEL_ID"
  echo "    4. curl -X POST $BASE/api/v1/data$ROUTE -H 'Content-Type: application/json' --data @$REQ_FILE"
  echo "    The server needs models.enabled = true; see $DIR/README.md."
  exit 0
fi

echo "==> POST /api/v1/data$ROUTE"
curl --fail-with-body -sS -X POST "$BASE/api/v1/data$ROUTE" \
  -H 'Content-Type: application/json' --data @"$REQ_FILE"
echo

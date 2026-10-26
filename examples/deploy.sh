#!/usr/bin/env bash
#
# Deploy one or more example packages and run them end-to-end.
#
#   ./deploy.sh <package>... [base-url]
#
# A package is a definition set: its channels, their workflows, and the
# connector or plugin it needs. The script compiles the packages with
# `orion-cli compile` into one deployment directory, deploys it with
# `orion-cli deployments apply -d`, then POSTs each package's request.json to
# its primary channel's route and prints the response.
#
# A deployment is the whole of what a node serves: applying one replaces the
# node's deployment head, so whatever it served before and these packages do
# not carry stops serving. To run several packages side by side, name them in
# one invocation — they are compiled into one set. Idempotent: re-running
# with the same packages applies nothing ("Up to date") and sends the
# requests again.
#
# Needs `orion-cli` to compile and deploy: set ORION_CLI, build it with
# `cargo build -p orion-cli` so it is found under target/, or put it on PATH.
# Also needs curl, python3 and a running Orion server.
#
# File conventions inside a package directory:
#   workflow.json          the primary workflow          (required)
#   workflow-<name>.json   additional workflows          (optional)
#   channel.json           the primary channel           (required)
#   channel-<name>.json    additional channels           (optional)
#   connector.json         a connector the package needs (optional)
#   connector-<name>.json  additional connectors         (optional)
#   plugin.toml            a plugin, with its component  (optional)
#   shared.json, sql/…     source-form authoring the compile resolves (optional)
#   request.json           a sample request              (optional)
#
# A connector's credential is an `env://` reference resolved on the server,
# so the server's environment must define it (postgres-orders needs
# ORDERS_DB_URL). A package with a plugin needs a server with
# `plugins.enabled = true`. A package whose primary channel has no
# route_pattern — a cron channel — is deployed the same way; the script
# prints the schedule instead of sending a request.
#
# Example:
#   ./deploy.sh high-value-order
#   ./deploy.sh channel-composition http://localhost:8080
#   ./deploy.sh order-classification webhook-transform
set -euo pipefail

CALLER_DIR="$(pwd)"
cd "$(dirname "$0")"
EXAMPLES_DIR="$(pwd)"

BASE="http://localhost:8080"
PACKAGES=()
for arg in "$@"; do
  case "$arg" in
    http://*|https://*) BASE="$arg" ;;
    *)
      arg="${arg%/}"
      # A path from the caller's directory, from examples/, or a bare
      # package name (`high-value-order` for `packages/high-value-order`).
      for candidate in "$CALLER_DIR/$arg" "$arg" "packages/$arg"; do
        if [[ -d "$candidate" ]]; then
          PACKAGES+=("$(cd "$candidate" && pwd)")
          continue 2
        fi
      done
      echo "no such package: $arg" >&2
      exit 1
      ;;
  esac
done

if [[ ${#PACKAGES[@]} -eq 0 ]]; then
  echo "usage: ./deploy.sh <package>... [base-url]" >&2
  echo "example: ./deploy.sh high-value-order" >&2
  exit 1
fi

for pkg in "${PACKAGES[@]}"; do
  [[ -f "$pkg/channel.json" ]] || { echo "missing $pkg/channel.json" >&2; exit 1; }
done

# The binaries: an explicit path, the workspace's own build, or PATH.
find_bin() {
  local var="$1" name="$2" candidate
  if [[ -n "${!var:-}" ]]; then echo "${!var}"; return; fi
  for candidate in "$EXAMPLES_DIR/../target/debug/$name" "$EXAMPLES_DIR/../target/release/$name" \
                   "$(command -v "$name" || true)"; do
    [[ -n "$candidate" && -x "$candidate" ]] && { echo "$candidate"; return; }
  done
  echo "$name not found: build it (cargo build -p $name) or set $var=<path>" >&2
  exit 1
}
ORION_CLI="$(find_bin ORION_CLI orion-cli)"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# One package compiles as itself; several are copied side by side into one
# set, which `compile` walks recursively.
if [[ ${#PACKAGES[@]} -eq 1 ]]; then
  SET="${PACKAGES[0]}"
else
  SET="$WORK/set"
  mkdir -p "$SET"
  for pkg in "${PACKAGES[@]}"; do
    cp -R "$pkg" "$SET/$(basename "$pkg")"
  done
fi

NAMES="$(for pkg in "${PACKAGES[@]}"; do basename "$pkg"; done | paste -sd ' ' -)"
echo "==> Compile $NAMES"
if ! "$ORION_CLI" compile "$SET" -o "$WORK/deployment" > "$WORK/compile.log" 2>&1; then
  cat "$WORK/compile.log" >&2
  exit 1
fi

# --quiet: an apply prints the revision the node now serves; its plan goes
# to stderr.
cli() { "$ORION_CLI" --server "$BASE" --yes --quiet "$@"; }

echo "==> Deploy to $BASE"
if ! REVISION="$(cli deployments apply -d "$WORK/deployment" 2> "$WORK/apply.log")"; then
  cat "$WORK/apply.log" >&2
  exit 1
fi
cat "$WORK/apply.log" >&2
echo "==> Serving deployment revision $REVISION"

# Read a field out of a JSON file with the stdlib (no jq dependency). Prints
# nothing when the field is absent, so a caller can test for it.
json_field() {
  python3 -c 'import json,sys
d = json.load(open(sys.argv[1]))
for key in sys.argv[2].split("."):
    d = d.get(key) if isinstance(d, dict) else None
print("" if d is None else d)' "$1" "$2"
}

for pkg in "${PACKAGES[@]}"; do
  CH_FILE="$pkg/channel.json"
  CH_ID="$(json_field "$CH_FILE" channel_id)"
  ROUTE="$(json_field "$CH_FILE" route_pattern)"
  if [[ -z "$ROUTE" ]]; then
    # A cron channel has no HTTP route; its schedule is what runs the workflow.
    echo "==> Deployed. '$CH_ID' runs on schedule '$(json_field "$CH_FILE" transport_config.schedule)'"
    continue
  fi
  if [[ ! -f "$pkg/request.json" ]]; then
    echo "==> Deployed. '$CH_ID' has no request.json to send."
    continue
  fi
  echo "==> POST /api/v2/data$ROUTE"
  curl --fail-with-body -sS -X POST "$BASE/api/v2/data$ROUTE" \
    -H 'Content-Type: application/json' --data @"$pkg/request.json"
  echo
done

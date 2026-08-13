#!/usr/bin/env bash
#
# Regenerate the documentation recordings.
#
# Single source of truth per flow: asciinema records a real terminal session
# driving a throwaway orion-server, then we emit BOTH artifacts from that cast:
#   - <name>.cast  -> docs/src/casts/   (interactive asciinema player in mdBook)
#   - <name>.gif   -> docs/media/       (animated GIF embedded in the READMEs)
# so the GIF and the cast can never drift apart.
#
# Prereqs: asciinema, agg, jq, curl, lsof   (brew install asciinema agg jq)
# Builds the Rust binaries if they are missing.
#
# Usage:
#   ./record.sh            # record everything
#   DRY=1 ./record.sh      # run the demo scripts live (no recording) to eyeball output
#   ORION_PORT=8090 ./...   # use a different port (note: quickstart GIF shows :8080)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"   # docs/recordings
ORION_DIR="$(cd "$HERE/../.." && pwd)"                 # .../Orion (workspace root; the CLI is the in-tree crate)

EXAMPLES="$ORION_DIR/examples/packages/high-value-order"
CAST_DIR="$ORION_DIR/docs/src/casts"
MEDIA_DIR="$ORION_DIR/docs/media"

PORT="${ORION_PORT:-8080}"
WINDOW="${WINDOW:-100x32}"
AGG_THEME="${AGG_THEME:-asciinema}"
DRY="${DRY:-0}"

export EXAMPLES ORION_PORT="$PORT"
export ORION_SERVER_URL="http://localhost:$PORT"

SERVER_BIN="$ORION_DIR/target/debug/orion-server"
CLI_BIN="$ORION_DIR/target/debug/orion-cli"
[ -x "$SERVER_BIN" ] || ( echo "building orion-server…"; cd "$ORION_DIR" && cargo build -p orion-server )
[ -x "$CLI_BIN" ]    || ( echo "building orion-cli…";    cd "$ORION_DIR" && cargo build -p orion-cli )

# Expose the binaries under clean names so the recorded prompt shows `orion-cli`.
BINDIR="$(mktemp -d)"
ln -sf "$SERVER_BIN" "$BINDIR/orion-server"
ln -sf "$CLI_BIN" "$BINDIR/orion-cli"
export PATH="$BINDIR:$PATH"

if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "Port $PORT is already in use. Free it or set ORION_PORT." >&2
  echo "(The quickstart GIF shows localhost:8080, so 8080 is preferred.)" >&2
  exit 1
fi

SERVER_PID=""
WORK=""
start_server() {
  WORK="$(mktemp -d)"
  ORION_SERVER__PORT="$PORT" \
  ORION_STORAGE__URL="sqlite:$WORK/orion.db?mode=rwc" \
  ORION_LOGGING__LEVEL=warn \
    "$SERVER_BIN" >"$WORK/server.log" 2>&1 &
  SERVER_PID=$!
  local i
  for i in $(seq 1 40); do
    curl -sf "$ORION_SERVER_URL/health" >/dev/null 2>&1 && return 0
    sleep 0.25
  done
  echo "orion-server did not become healthy:" >&2; cat "$WORK/server.log" >&2
  return 1
}
stop_server() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  [ -n "$SERVER_PID" ] && wait "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=""
  [ -n "$WORK" ] && rm -rf "$WORK" || true
}
cleanup() { stop_server; rm -rf "$BINDIR"; }
trap cleanup EXIT

seed_orders() {
  # The MCP demo focuses on tool calls, so the 'orders' service must already exist.
  orion-cli --yes workflows create -f "$EXAMPLES/workflow.json"  >/dev/null
  orion-cli --yes workflows activate high-value-order            >/dev/null
  orion-cli --yes channels create  -f "$EXAMPLES/channel.json"   >/dev/null
  orion-cli --yes channels activate orders                       >/dev/null
  orion-cli --yes engine reload                                  >/dev/null
}

record_one() {   # <name> <seed-fn|""> <gif-destination>
  local name="$1" seed="$2" gif="$3"
  start_server
  [ -n "$seed" ] && "$seed"
  if [ "$DRY" = "1" ]; then
    echo "==================== DRY RUN: $name ===================="
    bash "$HERE/demo-$name.sh"
    echo "==================== END: $name ===================="
  else
    local v3="$CAST_DIR/.$name.v3.cast" cast="$CAST_DIR/$name.cast"
    echo "▸ recording $name …"
    asciinema rec "$v3" --window-size "$WINDOW" --overwrite -q -c "bash $HERE/demo-$name.sh"
    asciinema convert -f asciicast-v2 "$v3" "$cast" --overwrite >/dev/null
    rm -f "$v3"
    agg --font-size 20 --theme "$AGG_THEME" --idle-time-limit 1.5 --last-frame-duration 3 "$cast" "$gif"
    echo "  cast -> $cast"
    echo "  gif  -> $gif  ($(du -h "$gif" | cut -f1))"
  fi
  stop_server
}

mkdir -p "$CAST_DIR" "$MEDIA_DIR"

record_one quickstart    ""           "$MEDIA_DIR/quickstart.gif"
record_one cli-lifecycle ""           "$MEDIA_DIR/cli-lifecycle.gif"
record_one mcp           seed_orders  "$MEDIA_DIR/mcp.gif"

echo "All recordings regenerated."

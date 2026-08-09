#!/usr/bin/env bash
#
# Regenerate the Orion console (UI) recordings and screenshots.
#
# A Playwright script (ui/demo-ui-quickstart.mjs) drives a real Orion-ui dev
# server against a throwaway orion-server and captures, per theme:
#   - docs/src/images/ui-{operations,system-map,workflow-dag,console}-<theme>.png
#   - docs/src/videos/ui-quickstart-<theme>.webm     (docs site + README demo link)
#   - ui/out/ui-quickstart-<theme>.gif               (local only, gitignored — upload as a release asset when an animated embed is wanted)
#
# Prereqs: node/npm, ffmpeg, curl, lsof, and a sibling Orion-ui checkout
# (override with ORION_UI_DIR). Builds orion-server if it is missing;
# downloads the Playwright Chromium build on first run.
#
# Usage:
#   ./record-ui.sh                 # record everything (light + dark)
#   THEMES=dark ./record-ui.sh     # a single theme
#   SPEED=1.0 ./record-ui.sh      # GIF playback speed multiplier (default 1.15)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"   # docs/recordings
ORION_DIR="$(cd "$HERE/../.." && pwd)"                 # .../Orion
UI_DIR="${ORION_UI_DIR:-$(cd "$ORION_DIR/.." && pwd)/Orion-ui}"

EXAMPLES="$ORION_DIR/examples/high-value-order"
DOCS_IMAGES="$ORION_DIR/docs/src/images"
DOCS_VIDEOS="$ORION_DIR/docs/src/videos"
OUT="$HERE/ui/out"

PORT="${ORION_PORT:-8080}"
UI_PORT="${ORION_UI_PORT:-5273}"   # not vite's default 5173, to avoid a running dev server
SPEED="${SPEED:-1.15}"
FPS="${FPS:-12}"
GIF_WIDTH="${GIF_WIDTH:-1000}"
THEMES="${THEMES:-light dark}"

for cmd in node npm ffmpeg curl lsof; do
  command -v "$cmd" >/dev/null || { echo "missing prerequisite: $cmd" >&2; exit 1; }
done
[ -d "$UI_DIR" ] || { echo "Orion-ui checkout not found at $UI_DIR (set ORION_UI_DIR)" >&2; exit 1; }

SERVER_BIN="$ORION_DIR/target/debug/orion-server"
[ -x "$SERVER_BIN" ] || ( echo "building orion-server…"; cd "$ORION_DIR" && cargo build --bin orion-server )

for p in "$PORT" "$UI_PORT"; do
  if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "Port $p is already in use. Free it or set ORION_PORT / ORION_UI_PORT." >&2
    exit 1
  fi
done

echo "▸ installing driver deps…"
( cd "$HERE/ui" && npm install --no-audit --no-fund --loglevel=error && npx playwright install chromium )
[ -d "$UI_DIR/node_modules" ] || ( echo "▸ npm ci in Orion-ui…"; cd "$UI_DIR" && npm ci --loglevel=error )

SERVER_PID=""
WORK=""
start_server() {
  WORK="$(mktemp -d)"
  ORION_SERVER__PORT="$PORT" \
  ORION_STORAGE__URL="sqlite:$WORK/orion.db?mode=rwc" \
  ORION_TRACING__DEBUG_PROFILE_ENABLED=true \
  ORION_METRICS__ENABLED=true \
  ORION_LOGGING__LEVEL=warn \
    "$SERVER_BIN" >"$WORK/server.log" 2>&1 &
  SERVER_PID=$!
  local i
  for i in $(seq 1 40); do
    curl -sf "http://localhost:$PORT/health" >/dev/null 2>&1 && return 0
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

UI_PID=""
UI_LOG="$(mktemp)"
start_ui() {
  ( cd "$UI_DIR" && ORION_URL="http://localhost:$PORT" npm run dev -- --port "$UI_PORT" --strictPort >"$UI_LOG" 2>&1 ) &
  UI_PID=$!
  local i
  for i in $(seq 1 60); do
    curl -sf "http://localhost:$UI_PORT" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  echo "Orion-ui dev server did not come up:" >&2; cat "$UI_LOG" >&2
  return 1
}
stop_ui() {
  # npm spawns vite as a child; kill the whole process group.
  [ -n "$UI_PID" ] && pkill -P "$UI_PID" 2>/dev/null || true
  [ -n "$UI_PID" ] && kill "$UI_PID" 2>/dev/null || true
  UI_PID=""
}
cleanup() { stop_server; stop_ui; rm -f "$UI_LOG"; }
trap cleanup EXIT

to_gif() {   # <webm> <gif>
  ffmpeg -y -loglevel error -i "$1" -vf "\
setpts=PTS/$SPEED,fps=$FPS,scale=$GIF_WIDTH:-1:flags=lanczos,\
split[a][b];[a]palettegen=stats_mode=diff[p];\
[b][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle" \
    -loop 0 "$2"
  echo "  gif -> $2  ($(du -h "$2" | cut -f1))"
}

mkdir -p "$OUT" "$DOCS_IMAGES" "$DOCS_VIDEOS"
echo "▸ starting Orion-ui dev server…"
start_ui

for THEME in $THEMES; do
  echo "▸ recording UI quickstart ($THEME)…"
  rm -rf "$OUT/$THEME"
  start_server
  node "$HERE/ui/demo-ui-quickstart.mjs" record \
    --theme "$THEME" --out "$OUT/$THEME" \
    --base "http://localhost:$UI_PORT" --orion "http://localhost:$PORT" \
    --examples "$EXAMPLES"
  node "$HERE/ui/demo-ui-quickstart.mjs" stills \
    --theme "$THEME" --out "$OUT/$THEME" \
    --base "http://localhost:$UI_PORT" --orion "http://localhost:$PORT" \
    --examples "$EXAMPLES"
  stop_server

  # The GIF stays a local artifact (ui/out is gitignored): 6+ MB per theme is
  # too heavy to track, so the README links the docs page's webm instead.
  to_gif "$OUT/$THEME/record.webm" "$OUT/ui-quickstart-$THEME.gif"
  # Docs embed the real video; trim the blank pre-navigation head so the first
  # frame is already themed.
  ffmpeg -y -loglevel error -ss 0.7 -i "$OUT/$THEME/record.webm" \
    -c:v libvpx-vp9 -crf 34 -b:v 0 -row-mt 1 -cpu-used 4 -an \
    "$DOCS_VIDEOS/ui-quickstart-$THEME.webm"
  echo "  webm -> $DOCS_VIDEOS/ui-quickstart-$THEME.webm  ($(du -h "$DOCS_VIDEOS/ui-quickstart-$THEME.webm" | cut -f1))"
  for shot in operations system-map workflow-dag console; do
    cp "$OUT/$THEME/$shot.png" "$DOCS_IMAGES/ui-$shot-$THEME.png"
    echo "  png -> $DOCS_IMAGES/ui-$shot-$THEME.png"
  done
done

echo "All UI recordings regenerated."

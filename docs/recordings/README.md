# Documentation recordings

The terminal demos in the READMEs and the mdBook are generated from this folder,
not captured by hand. Each flow has a `demo-*.sh` script that drives a real
`orion-server` + `orion-cli`; [`record.sh`](record.sh) boots a throwaway server,
records the session with [asciinema](https://asciinema.org), and produces **both**
artifacts from that single cast:

| Flow | Script | GIF (READMEs) | Cast (mdBook player) |
|------|--------|---------------|----------------------|
| Quickstart (curl/HTTP) | `demo-quickstart.sh` | `../media/quickstart.gif` | `../src/casts/quickstart.cast` |
| CLI lifecycle | `demo-cli-lifecycle.sh` | `../media/cli-lifecycle.gif` | `../src/casts/cli-lifecycle.cast` |

Recording the cast once and rendering the GIF from it (via `agg`) means the GIF and
the interactive player can never drift apart.

## Regenerate

```bash
brew install asciinema agg jq      # one-time: recorder, cast→gif renderer, JSON tool
./record.sh                        # rebuilds binaries if needed, records both
```

Useful overrides:

```bash
DRY=1 ./record.sh                  # run the demo scripts live (no recording) to eyeball output
ORION_PORT=8090 ./record.sh        # use a different port (the quickstart GIF shows :8080)
```

`record.sh` uses the workspace debug binaries at `../../target/debug/`
(`orion-server` and `orion-cli`, both built from this tree), building them
if they are missing. Both GIFs land in `docs/media/` — the single
media folder every README references.

## The Open Graph card

`../src/images/og-card.png` is the 1200x630 image every page's `og:image` and
`twitter:image` points at — what renders when a docs link is pasted into Slack,
X or LinkedIn. It is generated too, by
[`make-og-card.mjs`](make-og-card.mjs), for the same reason as everything
above: it carries the brand's real type and palette, embedded from the repo's
own vendored woff2 files, so it is re-rendered when those change rather than
redrawn from memory.

It has no per-page content, so it is **not** part of the build — run it when
the brand or the positioning line changes, and commit the PNG.

```bash
cd ui && npm install     # first time only; shares Playwright with the UI script
cd .. && node make-og-card.mjs
```

The palette is copied from `docs/css/plasmatic.css`'s dark theme: a static
image cannot read a CSS variable, so those values are duplicated in the script
and the comment there points back at their owner.

## UI recordings (console GIF + screenshots)

The README hero GIF and the console screenshots are also generated, not
hand-captured: [`record-ui.sh`](record-ui.sh) boots a throwaway `orion-server`
plus the Orion-ui dev server (sibling checkout, override with `ORION_UI_DIR`),
then a Playwright script ([`ui/demo-ui-quickstart.mjs`](ui/demo-ui-quickstart.mjs))
drives the console through the creation loop — import workflow (paste →
validate → dry-run → activate) → logic visualization → channel form → Data
Console request → System Map — and captures, per theme (light + dark):

| Artifact | Files |
|----------|-------|
| Hero GIF (creation loop, ~50 s) | `ui/out/ui-quickstart-{light,dark}.gif` — local only (gitignored); upload as a release asset when an animated embed is wanted |
| Demo video (same recording, webm, mdBook + README demo link) | `../src/videos/ui-quickstart-{light,dark}.webm` |
| Screenshots (README + mdBook) | `../src/images/ui-{operations,system-map,workflow-dag,console}-{light,dark}.png` |

The GIFs/screenshots in Orion-ui's README are copies of the same files — sync them
when regenerating.

```bash
brew install ffmpeg                # one-time (plus node/npm)
./record-ui.sh                     # installs Playwright + Chromium on first run
```

Useful overrides:

```bash
THEMES=dark ./record-ui.sh         # a single theme
SPEED=1.0 FPS=15 ./record-ui.sh    # GIF playback speed / frame rate
ORION_UI_DIR=~/src/Orion-ui ./record-ui.sh
```

The captions and the pointer are injected into the page by the driver, so the
recording stays in sync with the real UI — if the console changes, just re-run.

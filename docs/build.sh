#!/usr/bin/env bash
# Build the deployable documentation site into docs/book/.
#
# Everything the published site contains beyond plain `mdbook build` output
# lives here rather than in CI config, because the site is deployed by
# Cloudflare Workers Builds — Cloudflare clones this repo and, from the `docs`
# root directory, runs `bash build.sh` (see docs/wrangler.jsonc for the full
# deploy picture).
# Keeping the steps in the repo means the dashboard holds one command, and
# `just docs` locally produces byte-for-byte what Cloudflare serves.
# Same arrangement as docs/lint.sh — the logic is in the script, CI only calls it.

# Re-exec under bash when started by another shell. The dashboard's "Build
# command" field is the one deploy setting nothing in this repo can lint, and
# it has now been wrong twice: `bash docs/build.sh` (fixed in 6481c0b5, which
# resolved to docs/docs/build.sh) and `sh build.sh`, which reaches the line
# below and dies with "Illegal option -o pipefail" because /bin/sh is dash.
# `pipefail` is load-bearing here — this script pipes curl into tar — so the
# fix is to get bash, not to drop the option. Keep this guard POSIX-clean: it
# runs *before* we know we are in bash.
if [ -z "${BASH_VERSION:-}" ]; then
  exec bash "$0" "$@"
fi

set -euo pipefail

cd "$(dirname "$0")/.."

# The one mdBook version this project builds with. It lives here, not in a
# dashboard field, so the deploy and the PR check cannot use different ones —
# ci.yml's book job installs this same version and mdbook_version_pin_test
# asserts the two agree.
MDBOOK_VERSION=0.5.4

# CI (and most dev machines) already have mdbook on PATH, from
# taiki-e/install-action and `cargo install` respectively. Cloudflare's build
# image does not, so fetch the pinned release binary into a scratch dir there.
# Guarded on both presence *and* version: a stale mdbook on PATH would
# otherwise silently build the site with something other than the pin.
if ! command -v mdbook >/dev/null 2>&1 || ! mdbook --version | grep -qF "$MDBOOK_VERSION"; then
  case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *)
    echo "docs/build.sh: no mdBook $MDBOOK_VERSION build for $(uname -m)" >&2
    exit 1
    ;;
  esac
  # Only Cloudflare's image actually reaches this branch, but a Mac dev without
  # mdbook installed would too — and silently handing them a Linux binary is a
  # far worse first failure than one extra case arm costs.
  case "$(uname -s)" in
  Linux) target="${arch}-unknown-linux-musl" ;;
  Darwin) target="${arch}-apple-darwin" ;;
  *)
    echo "docs/build.sh: no mdBook $MDBOOK_VERSION build for $(uname -s); install mdbook $MDBOOK_VERSION manually" >&2
    exit 1
    ;;
  esac
  tarball="mdbook-v${MDBOOK_VERSION}-${target}.tar.gz"
  echo "docs/build.sh: fetching mdBook $MDBOOK_VERSION ($arch)"
  mkdir -p docs/.bin
  curl -fsSL "https://github.com/rust-lang/mdBook/releases/download/v${MDBOOK_VERSION}/${tarball}" |
    tar -xz -C docs/.bin mdbook
  PATH="$PWD/docs/.bin:$PATH"
  export PATH
fi

mdbook build docs

# llms-full.txt: the whole book as one markdown file, in SUMMARY order, served
# from the site root for LLM/agent consumption. (llms.txt is the curated index
# and is a static file in docs/src that mdbook build copies as-is.)
#
# Each page's `<!-- description: … -->` line — the one docs/seo.mjs turns into
# the page's meta description — is lifted into the front-matter block rather
# than passed through in the body. Same fact either way, but as a
# `description:` field a retrieval model can use it to pick a section, whereas
# an HTML comment mid-document is just noise it has to skip.
{
  echo "# Orion — complete documentation"
  echo "# Generated from the mdBook sources at https://github.com/GoPlasmatic/Orion/tree/main/docs/src"
  grep -oE '\(\./[A-Za-z0-9_./-]+\.md\)' docs/src/SUMMARY.md | tr -d '()' | while read -r p; do
    f="docs/src/${p#./}"
    desc=$(sed -n '1s/^<!-- description: \(.*\) -->$/\1/p' "$f")
    printf '\n\n---\nsource: %s\n' "$p"
    [ -n "$desc" ] && printf 'description: %s\n' "$desc"
    printf -- '---\n\n'
    sed '1{/^<!-- description: .* -->$/d;}' "$f"
  done
} >docs/book/llms-full.txt

# Per-page descriptions, canonicals, Open Graph/Twitter cards, JSON-LD and
# sitemap.xml. mdBook gives every page the book-level description and no
# canonical at all, so this pass is what makes the site legible to search and
# answer engines. Node rather than python3: `npx wrangler deploy` is the deploy
# command, so Node is guaranteed on Cloudflare's build image.
node docs/seo.mjs

# _redirects carries the "/" → /index.html proxy that wrangler.jsonc's
# html_handling:"none" requires; _headers carries the security and font-caching
# headers; robots.txt points crawlers at the sitemap and names the answer-engine
# agents explicitly. All three have to sit at the root of the deployed asset
# directory, which is why they are copied in from docs/cloudflare/ rather than
# kept in docs/src (mdBook would publish them as book content).
cp docs/cloudflare/_redirects docs/cloudflare/_headers docs/cloudflare/robots.txt docs/book/

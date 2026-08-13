#!/usr/bin/env bash
# Build the deployable documentation site into docs/book/.
#
# Everything the published site contains beyond plain `mdbook build` output
# lives here rather than in CI config, because the site is deployed by
# Cloudflare Workers Builds — Cloudflare clones this repo and runs
# `bash docs/build.sh` (see docs/wrangler.jsonc for the full deploy picture).
# Keeping the steps in the repo means the dashboard holds one command, and
# `just docs` locally produces byte-for-byte what Cloudflare serves.
# Same arrangement as docs/lint.sh — the logic is in the script, CI only calls it.
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
{
  echo "# Orion — complete documentation"
  echo "# Generated from the mdBook sources at https://github.com/GoPlasmatic/Orion/tree/main/docs/src"
  grep -oE '\(\./[A-Za-z0-9_./-]+\.md\)' docs/src/SUMMARY.md | tr -d '()' | while read -r p; do
    printf '\n\n---\nsource: %s\n---\n\n' "$p"
    cat "docs/src/${p#./}"
  done
} >docs/book/llms-full.txt

# _redirects carries the "/" → /index.html proxy that wrangler.jsonc's
# html_handling:"none" requires; _headers carries the security and font-caching
# headers. Both have to sit at the root of the deployed asset directory, which
# is why they are copied in from docs/cloudflare/ rather than kept in docs/src
# (mdBook would publish them as book content).
cp docs/cloudflare/_redirects docs/cloudflare/_headers docs/book/

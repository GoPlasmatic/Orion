#!/usr/bin/env bash
# docs-lint — enforcement arm of docs-implementation-plan.md T6.1.
#
# Some checks only hold once a later migration phase has landed; they are
# gated on DOCS2_PHASE. Bump it in the phase PR that makes the check true.
set -uo pipefail
cd "$(dirname "$0")/.."

DOCS2_PHASE=3

fail=0
err() { printf 'docs-lint: %s\n' "$*" >&2; fail=1; }

## 1. {{#include}} targets resolve (mdBook only warns on a missing include).
while IFS= read -r hit; do
  file=${hit%%:*}
  rest=${hit#*:}
  line=${rest%%:*}
  body=${rest#*:}
  for t in $(printf '%s' "$body" | grep -oE '\{\{#include [^}]+\}\}' | sed -E 's/\{\{#include +([^:}]+).*/\1/'); do
    [ -f "$(dirname "$file")/$t" ] || err "$file:$line: include target missing: $t"
  done
done < <(grep -rn '{{#include ' docs/src --include='*.md' 2>/dev/null)

## 2. Relative .md links point at files that exist.
while IFS= read -r hit; do
  file=${hit%%:*}
  rest=${hit#*:}
  line=${rest%%:*}
  body=${rest#*:}
  for l in $(printf '%s' "$body" | grep -oE '\]\([^)#[:space:]]+\.md' | sed 's/^](//'); do
    case "$l" in http://*|https://*|/*) continue ;; esac
    [ -f "$(dirname "$file")/$l" ] || err "$file:$line: dead link: $l"
  done
done < <(grep -rnE '\]\([^)]*\.md[#)]' docs/src --include='*.md' 2>/dev/null)

## 3. Hard site links (README, examples, llms.txt) resolve to a live page or a
##    redirect entry in book.toml — the don't-break-the-web invariant.
for f in README.md examples/README.md docs/src/llms.txt; do
  [ -f "$f" ] || continue
  while IFS= read -r url; do
    p=${url#goplasmatic.github.io/Orion/}
    p=${p%%#*}
    case "$p" in *.html) ;; *) continue ;; esac
    if [ ! -f "docs/src/${p%.html}.md" ] && ! grep -qF "\"/${p}\"" docs/book.toml; then
      err "$f: site link has no page and no redirect: $url"
    fi
  done < <(grep -oE 'goplasmatic\.github\.io/Orion/[A-Za-z0-9_./#-]+' "$f" | sort -u)
done

## 4. No hand-maintained magic numbers (casts are recorded sessions — exempt).
if git grep -nE '46 (MCP )?tools|6,000' -- docs/src README.md ':!docs/src/casts' >/dev/null 2>&1; then
  git grep -nE '46 (MCP )?tools|6,000' -- docs/src README.md ':!docs/src/casts' >&2
  err 'hand-maintained magic number (46 tools / 6,000+)'
fi

## 5. The function count is stated in exactly one place.
stray=$(git grep -lE '18 functions|18 built-in' -- docs/src 2>/dev/null | grep -v 'reference/functions.md' || true)
[ -z "$stray" ] || err "function count '18' stated outside reference/functions.md: $stray"

## 6. No internal review IDs in user docs. The 1.0 upgrade guide is exempt
##    until its Phase 2 restructure decides how to cite change IDs.
if git grep -nE '\((K|R|F|N|S)[0-9]+(, ?(K|R|F|N|S)[0-9]+)*\)' -- docs/src ':!docs/src/getting-started/upgrading.md' ':!docs/src/operate/upgrading-to-1.0.md' >/dev/null 2>&1; then
  git grep -nE '\((K|R|F|N|S)[0-9]+' -- docs/src ':!docs/src/getting-started/upgrading.md' ':!docs/src/operate/upgrading-to-1.0.md' >&2
  err 'internal review IDs in user docs'
fi

## 7. (Phase ≥3, after the features/* pages dissolve) no Rust internals
##    outside reference/design-notes.md.
if [ "$DOCS2_PHASE" -ge 3 ]; then
  if git grep -nE 'Arc<RwLock|tokio::sync::mpsc|apply_guards|CatchPanicLayer|arena-mode' -- docs/src ':!docs/src/reference/design-notes.md' >/dev/null 2>&1; then
    git grep -nE 'Arc<RwLock|tokio::sync::mpsc|apply_guards|CatchPanicLayer|arena-mode' -- docs/src ':!docs/src/reference/design-notes.md' >&2
    err 'Rust internals outside reference/design-notes.md'
  fi
fi

## 8. (Phase ≥4, once the ToC is final) every SUMMARY chapter has an llms.txt
##    entry, so the curated index cannot silently fall behind the book.
if [ "$DOCS2_PHASE" -ge 4 ]; then
  while IFS= read -r p; do
    html="${p%.md}.html"
    grep -qF "goplasmatic.github.io/Orion/${html}" docs/src/llms.txt \
      || err "llms.txt: no entry for SUMMARY chapter $p"
  done < <(grep -oE '\(\./[A-Za-z0-9_./-]+\.md\)' docs/src/SUMMARY.md | sed 's/^(\.\///; s/)$//')
fi

if [ "$fail" -eq 0 ]; then
  echo 'docs-lint: OK'
else
  exit 1
fi

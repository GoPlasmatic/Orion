#!/usr/bin/env bash
# docs-lint — the docs estate's structural guard: include and link integrity,
# redirect coverage for hard site links, and the one-owner-per-fact greps that
# stop a fact being restated in two places.
#
# DOCS2_PHASE gated the checks that only became true as the 2.0 restructure
# landed. The restructure is complete, so it sits at its final value; leave it
# there unless a check is deliberately being relaxed.
#
# docs/src/llms.txt is CURATED, NOT GENERATED — a settled decision, not an
# unfinished one. Its preamble, its per-entry descriptions and its two
# non-chapter entries are editorial: they are written for a retrieval model
# choosing one page, and no generator produces them from SUMMARY.md. Checks 3,
# 8, 9 and 10 are what keep a hand-maintained index honest — every chapter is
# listed, every listed page resolves, nothing lists a page that no longer has a
# chapter, and no entry describes a page by a title it has lost. Descriptions
# stay uncompared on purpose; that is the part a human is better at.
set -uo pipefail
cd "$(dirname "$0")/.."

DOCS2_PHASE=4

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

## 5. The function count is stated in exactly one place. Whether the number is
##    *correct* is asserted by functions_docs_drift_test against the schema
##    registry and the page's own summary table — this check only stops the
##    number being restated somewhere it would later drift.
stray=$(git grep -lE '18 functions|18 built-in' -- docs/src 2>/dev/null | grep -v 'reference/functions.md' || true)
[ -z "$stray" ] || err "function count '18' stated outside reference/functions.md: $stray"

## 6. No internal review IDs in user docs. The audit IDs (K…, R…, F…, N…, S…)
##    are repo-internal: they resolve to nothing a reader can open. Every page
##    is in scope — the 1.0 upgrade guide's old blanket exemption expired with
##    its restructure and its ten IDs are gone.
##
##    One occurrence survives, deliberately. The upgrade guide quotes
##    `config/retired_env.rs`'s refusal message verbatim, and that string
##    carries a `(K4)` the binary prints at operators. Quote the binary or
##    change the binary; do not paraphrase it into a doc that then disagrees
##    with what the terminal says. Drop this filter if that string ever loses
##    its ID.
hits=$(git grep -nE '\((K|R|F|N|S)[0-9]+(, ?(K|R|F|N|S)[0-9]+)*\)' -- docs/src \
       | grep -v 'removed in 1.0 (K4)' || true)
if [ -n "$hits" ]; then
  printf '%s\n' "$hits" >&2
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

## 9.  No llms.txt entry points at a page that has no chapter. Check 3 accepts
##     a redirect, so a retired page's entry would otherwise survive there
##     indefinitely; check 8 only ever walks SUMMARY → llms.txt, never back.
## 10. An entry's link text matches its chapter's SUMMARY title, so a rename
##     cannot leave the curated index naming a page something it is not.
##
##     Both skip anything that is not a book page: the raw OpenAPI spec, the
##     GitHub repo and llms-full.txt are deliberate non-chapter entries, and
##     none of them ends in .html.
if [ "$DOCS2_PHASE" -ge 4 ]; then
  while IFS= read -r entry; do
    title=${entry%%](*}; title=${title#*[}
    url=${entry#*](}; url=${url%)}
    p=${url#https://goplasmatic.github.io/Orion/}
    p=${p%%#*}
    case "$p" in *.html) ;; *) continue ;; esac
    chapter=$(grep -F "](./${p%.html}.md)" docs/src/SUMMARY.md || true)
    if [ -z "$chapter" ]; then
      err "llms.txt: entry for a page with no SUMMARY chapter: $p"
      continue
    fi
    chapter_title=${chapter%%](*}; chapter_title=${chapter_title#*[}
    [ "$title" = "$chapter_title" ] \
      || err "llms.txt: entry titled '$title' but SUMMARY calls $p '$chapter_title'"
  done < <(grep -oE '^- \[[^]]+\]\(https://goplasmatic\.github\.io/Orion/[A-Za-z0-9_./#-]+\)' docs/src/llms.txt)
fi

## 11. Mindmap node links resolve to a real page. An ```orion-mindmap block is
##     JSON inside a fenced code block carrying runtime .html hrefs, so checks 1
##     and 2 never see it — they look for markdown links. Without this, retiring a
##     page would leave the capability map pointing at a 404 that nothing notices.
##     Only the page is checked, matching check 2's depth; fragments are not.
##     Plain grep, not git grep: a page added but not yet committed must be
##     checked too, which is exactly when a bad link is cheapest to fix.
for f in $(grep -rl 'orion-mindmap' docs/src --include='*.md' 2>/dev/null); do
  while IFS= read -r hit; do
    line=${hit%%:*}
    body=${hit#*:}
    for l in $(printf '%s' "$body" | grep -oE '"link" *: *"[^"]+"' | sed -E 's/.*"link" *: *"([^"#]+).*/\1/'); do
      case "$l" in http://*|https://*) continue ;; esac
      [ -f "docs/src/${l%.html}.md" ] || err "$f:$line: mindmap link has no page: $l"
    done
  done < <(grep -n '"link" *: *"' "$f" 2>/dev/null)
done

## 12. Every comparison page carries the template's load-bearing parts. The
##     section's whole value is that its pages read side by side and that the
##     honest half is never quietly dropped, so both are checked rather than
##     trusted: a page that cannot fill "What Orion cannot do here" is a page
##     that is not honest yet, and an undated comparison is a stale one. The
##     review date is a claim about when a human last checked the other tools'
##     docs — nothing can verify that for us, but a missing line is catchable.
##
##     "What Orion does instead" is deliberately NOT required. dataflow-rs.md
##     says "What Orion adds on top" because it is a build decision, not a
##     competitive comparison, and the page opens by saying so. Forcing the
##     heading there would make it lie to satisfy a grep.
if [ -d docs/src/compare ]; then
  for f in docs/src/compare/*.md; do
    [ -e "$f" ] || continue
    for h in 'Side by side' 'Where they overlap' 'Running both' \
             'What Orion cannot do here' 'Related'; do
      grep -q "^## $h\$" "$f" || err "$f: missing required heading '## $h'"
    done
    grep -q '^\*\*How it relates:\*\* ' "$f" \
      || err "$f: missing a '**How it relates:**' line"
    ## The date alone was the weaker half of the contract: it says a human
    ## looked, not what they looked at. A version is what makes the claim
    ## falsifiable later, so the line has to name one.
    grep -q '^\*\*Last reviewed:\*\* .*, against ' "$f" \
      || err "$f: '**Last reviewed:**' needs a ', against <Tool> <version>' clause"
    ## An orphan page would otherwise sit outside checks 8 and 10, which only
    ## ever walk SUMMARY.md.
    rel=${f#docs/src/}
    grep -qF "](./$rel)" docs/src/SUMMARY.md \
      || err "$f: no SUMMARY.md chapter, so llms.txt is never checked for it"
  done
fi

if [ "$fail" -eq 0 ]; then
  echo 'docs-lint: OK'
else
  exit 1
fi

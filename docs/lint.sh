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
#
# TWO RULES FOR EVERY `git grep` BELOW, both learned from a CI failure this
# script could not reproduce on any developer's machine (run 31587839330).
#
# CI runs with no LANG set, so git's regexes execute in the C locale, where
# every byte is a valid single-byte character. A developer's shell is almost
# always UTF-8, where the same bytes are invalid sequences the engine skips.
# The two therefore disagree about the ~20 binaries under docs/src (fonts,
# screenshots, videos), and only the CI half of the disagreement is ever seen:
#
#   1. Pass -I to any `git grep` scanning a tree, so binaries are never
#      searched. Every check here is about prose; a woff2 cannot hold a review
#      ID, but its compressed bytes can spell one. That is precisely what broke
#      — check 6 matched inter-italic-latin-ext.woff2 in CI and nowhere else.
#   2. Start any -P pattern using \x{...} above FF with (*UTF), which puts
#      PCRE in UTF mode from inside the pattern rather than relying on the
#      locale. Without it, check 14's pattern does not merely mismatch in the C
#      locale: it fails to compile, and git exits 128. Since these checks read
#      "did grep find anything", a 128 is indistinguishable from a clean pass —
#      the guard reports success having tested nothing.
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
    p=${url#docs.goplasmatic.io/}
    p=${p%%#*}
    case "$p" in *.html) ;; *) continue ;; esac
    if [ ! -f "docs/src/${p%.html}.md" ] && ! grep -qF "\"/${p}\"" docs/book.toml; then
      err "$f: site link has no page and no redirect: $url"
    fi
  done < <(grep -oE 'docs\.goplasmatic\.io/[A-Za-z0-9_./#-]+' "$f" | sort -u)
done

## 4. No hand-maintained magic numbers (casts are recorded sessions — exempt).
if git grep -I -nE '46 tools|6,000' -- docs/src README.md ':!docs/src/casts' >/dev/null 2>&1; then
  git grep -I -nE '46 tools|6,000' -- docs/src README.md ':!docs/src/casts' >&2
  err 'hand-maintained magic number (46 tools / 6,000+)'
fi

## 5. The function count is stated in exactly one place. Whether the number is
##    *correct* is asserted by functions_docs_drift_test against the schema
##    registry and the page's own summary table — this check only stops the
##    number being restated somewhere it would later drift.
stray=$(git grep -I -lE '24 functions|24 built-in' -- docs/src 2>/dev/null | grep -v 'reference/functions.md' || true)
[ -z "$stray" ] || err "function count '24' stated outside reference/functions.md: $stray"

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
hits=$(git grep -I -nE '\((K|R|F|N|S)[0-9]+(, ?(K|R|F|N|S)[0-9]+)*\)' -- docs/src \
       | grep -v 'removed in 1.0 (K4)' || true)
if [ -n "$hits" ]; then
  printf '%s\n' "$hits" >&2
  err 'internal review IDs in user docs'
fi

## 7. (Phase ≥3, after the features/* pages dissolve) no Rust internals
##    outside reference/design-notes.md.
if [ "$DOCS2_PHASE" -ge 3 ]; then
  if git grep -I -nE 'Arc<RwLock|tokio::sync::mpsc|apply_guards|CatchPanicLayer|arena-mode' -- docs/src ':!docs/src/reference/design-notes.md' >/dev/null 2>&1; then
    git grep -I -nE 'Arc<RwLock|tokio::sync::mpsc|apply_guards|CatchPanicLayer|arena-mode' -- docs/src ':!docs/src/reference/design-notes.md' >&2
    err 'Rust internals outside reference/design-notes.md'
  fi
fi

## 8. (Phase ≥4, once the ToC is final) every SUMMARY chapter has an llms.txt
##    entry, so the curated index cannot silently fall behind the book.
if [ "$DOCS2_PHASE" -ge 4 ]; then
  while IFS= read -r p; do
    html="${p%.md}.html"
    grep -qF "docs.goplasmatic.io/${html}" docs/src/llms.txt \
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
    p=${url#https://docs.goplasmatic.io/}
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
  done < <(grep -oE '^- \[[^]]+\]\(https://docs\.goplasmatic\.io/[A-Za-z0-9_./#-]+\)' docs/src/llms.txt)
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

## 13. The Required column is lower case, everywhere.
##
##     Two field-table legends used to disagree — functions.md published
##     `yes`/`no` and channel-config.md published `Yes`/`No` — for the same
##     column, which meant no reader could tell whether the difference meant
##     anything. It does not. The Required column carries a controlled value
##     that sits beside `rest`, `hmac` and `one of …`, so it is lower case
##     like those.
##
##     A boolean MATRIX cell is a different thing: it answers a question in
##     prose ("does this guard apply on this ingress?"), so it stays `Yes` /
##     `No`. That is why this walks the tables and only looks at the column
##     whose header is Required, rather than grepping for the words.
req_case=$(python3 - <<'PY'
import pathlib
bad = []
for f in sorted(pathlib.Path('docs/src').rglob('*.md')):
    hdr = idx = None
    for n, l in enumerate(f.read_text().split('\n'), 1):
        if not l.startswith('|'):
            hdr = idx = None
            continue
        cells = [c.strip() for c in l.strip().strip('|').split('|')]
        if hdr is None:
            hdr = cells
            low = [c.lower().strip('`* ') for c in cells]
            idx = low.index('required') if 'required' in low else None
            continue
        if set(''.join(cells)) <= set('-: '):
            continue
        if idx is not None and idx < len(cells) and cells[idx] in ('Yes', 'No'):
            bad.append(f"{f}:{n}: Required column says '{cells[idx]}', want lower case")
print('\n'.join(bad))
PY
)
if [ -n "$req_case" ]; then
  printf '%s\n' "$req_case" >&2
  err 'capitalised Yes/No in a Required column'
fi

## 14. No emoji, and no character standing in for an icon.
##
##     A character borrowed from the text face cannot be sized, aligned or
##     stroked against the type around it, and an emoji renders differently
##     on every platform. Every mark in this book is an SVG — a <symbol> in
##     js/extra.js for the ones the script inserts, a `--p-icon-*` mask in
##     plasmatic.css §1 for the ones CSS draws.
##
##     The arrow characters are NOT listed here: `→` is real punctuation in
##     prose like "rate limit → auth", and this book uses it that way in nine
##     files. Only the ones whose sole use is decorative are refused.
##
##     Prose only. The casts are recorded terminal sessions and their `❯`
##     prompt is what the shell actually printed — the same reason check 4
##     exempts them — and the images, videos and webfonts are binaries that a
##     byte-range match hits by accident.
##     `(*UTF)` is load-bearing — see rule 2 in the header. Without it this
##     pattern does not compile in CI's C locale, and the failure is invisible:
##     git exits 128, which an `if git grep …; then` reads as "found nothing".
##
##     Run once, not twice, and separate the three outcomes git actually
##     returns (0 found / 1 clean / >1 error) instead of collapsing them into
##     a boolean. A guard that cannot tell "clean" from "broken" is how this
##     one shipped dead in the first place.
emoji_re='(*UTF)[\x{1F300}-\x{1FAFF}\x{2600}-\x{27BF}\x{2B00}-\x{2BFF}\x{FE0F}]'
emoji_hits=$(git grep -I -nP "$emoji_re" -- 'docs/src/**/*.md' 'docs/src/*.md' 2>&1)
case $? in
0)
  printf '%s\n' "$emoji_hits" >&2
  err 'emoji or dingbat in docs/src — use an SVG mark (js/extra.js sprite, or --p-icon-* in plasmatic.css §1)'
  ;;
1) ;; # clean
*)
  printf '%s\n' "$emoji_hits" >&2
  err 'the emoji check could not run (git grep failed) — it is not passing, it is broken'
  ;;
esac

## 15. Every SUMMARY chapter carries a `<!-- description: ... -->` on its first
##     line, and every description is distinct and the right length.
##
##     docs/seo.mjs reads these into each page's <meta name="description">,
##     og:description and JSON-LD. Without one a page falls back to the
##     book-wide description from book.toml — which is what all 90 pages used
##     to share, and is the condition this check exists to stop returning.
##
##     The bounds are Google's rendering, not a style rule: below ~110 chars a
##     description under-uses the snippet; above ~160 it is truncated mid-word.
##     Uniqueness matters for the same reason the shared fallback did not work
##     — two pages with one description give a ranker nothing to tell them
##     apart, and give an answer engine one summary for two subjects.
##
##     Nothing else can generate these: llms.txt's entries are index
##     annotations written to sit beside a title, not standalone sentences, so
##     reusing them here would make one line serve two jobs badly.
desc_report=$(python3 - <<'DESCPY'
import pathlib, re, collections

summary = pathlib.Path('docs/src/SUMMARY.md').read_text()
chapters = [m[1] for m in re.findall(r'\[([^\]]+)\]\(\./([A-Za-z0-9_./-]+\.md)\)', summary)]

problems, seen = [], collections.defaultdict(list)
for rel in chapters:
    p = pathlib.Path('docs/src') / rel
    if not p.exists():
        continue
    first = p.read_text().split('\n', 1)[0]
    m = re.match(r'^<!--\s*description:\s*(.*?)\s*-->$', first)
    if not m:
        problems.append(f"docs/src/{rel}: no '<!-- description: ... -->' on line 1")
        continue
    d = m.group(1)
    n = len(d)
    if not (110 <= n <= 160):
        problems.append(f"docs/src/{rel}: description is {n} chars, want 110-160")
    seen[d].append(rel)

for d, files in seen.items():
    if len(files) > 1:
        problems.append(f"duplicate description shared by {', '.join(files)}")

print('\n'.join(problems))
DESCPY
)
if [ -n "$desc_report" ]; then
  printf '%s\n' "$desc_report" >&2
  err 'page description missing, mis-sized or duplicated (docs/seo.mjs reads these)'
fi

## 16. Chapter titles are unique. Search results and browser tabs only carry
##     the H1, so two chapters with one title are indistinguishable even when
##     their sidebar labels happen to differ.
title_report=$(python3 - <<'TITLEPY'
import collections, pathlib, re

summary = pathlib.Path('docs/src/SUMMARY.md').read_text()
paths = [m[1] for m in re.findall(r'\[([^\]]+)\]\(\./([A-Za-z0-9_./-]+\.md)\)', summary)]
seen = collections.defaultdict(list)
for rel in paths:
    p = pathlib.Path('docs/src') / rel
    if not p.exists():
        continue
    h1 = next((line[2:].strip() for line in p.read_text().splitlines()
               if line.startswith('# ')), None)
    if h1:
        seen[h1.casefold()].append((rel, h1))
for entries in seen.values():
    if len(entries) > 1:
        print('duplicate H1:', ', '.join(f'{p} ({h})' for p, h in entries))
TITLEPY
)
if [ -n "$title_report" ]; then
  printf '%s\n' "$title_report" >&2
  err 'duplicate chapter titles'
fi

## 17. Relative Markdown links carrying a fragment resolve to a real heading.
##     mdBook preserves underscores and repeated hyphens in heading IDs; this
##     slugger mirrors that behavior for the syntax used in this book.
fragment_report=$(python3 - <<'FRAGPY'
import pathlib, re, urllib.parse

root = pathlib.Path('docs/src')

def anchors(path):
    found, counts, fenced = set(), {}, False
    for line in path.read_text().splitlines():
        if line.startswith('```'):
            fenced = not fenced
            continue
        if fenced:
            continue
        match = re.match(r'^(#{1,6})\s+(.+?)\s*#*$', line)
        if not match:
            continue
        slug = re.sub(r'<[^>]+>', '', match.group(2))
        slug = re.sub(r'[`*~]', '', slug).lower()
        slug = re.sub(r'[^\w\- ]', '', slug).replace(' ', '-')
        duplicate = counts.get(slug, 0)
        counts[slug] = duplicate + 1
        if duplicate:
            slug = f'{slug}-{duplicate}'
        found.add(slug)
    return found

for source in sorted(root.rglob('*.md')):
    for match in re.finditer(r'\]\(([^)\s]+\.md)#([^)\s]+)\)', source.read_text()):
        target = (source.parent / match.group(1)).resolve()
        fragment = urllib.parse.unquote(match.group(2))
        if target.exists() and fragment not in anchors(target):
            print(f'{source}: missing fragment #{fragment} in {target}')
FRAGPY
)
if [ -n "$fragment_report" ]; then
  printf '%s\n' "$fragment_report" >&2
  err 'broken relative fragment links'
fi

## 18. Executable tutorials state the version they were actually run with,
##     and that version follows the workspace rather than becoming stale prose.
tutorial_report=$(python3 - <<'TUTORIALPY'
import pathlib, re

cargo = pathlib.Path('Cargo.toml').read_text()
version = re.search(r'^version\s*=\s*"([^"]+)"', cargo, re.M).group(1)
paths = [
    'getting-started/quickstart.md', 'getting-started/install.md',
    'getting-started/first-service.md', 'getting-started/first-connector.md',
    'getting-started/test-and-promote.md', 'getting-started/examples.md',
    'getting-started/console.md', 'guides/orders-golden-path.md',
    'guides/worked-examples.md', 'guides/kafka-channels.md',
    'guides/ci-cd.md', 'ai/claude-code.md',
]
for rel in paths:
    path = pathlib.Path('docs/src') / rel
    text = path.read_text() if path.exists() else ''
    match = re.search(r'^\*\*Tested with:\*\* Orion ([^ ]+) · \*\*Last reviewed:\*\* (\d{4}-\d{2}-\d{2})$', text, re.M)
    if not match:
        print(f'{path}: missing Tested with / Last reviewed metadata')
    elif match.group(1) != version:
        print(f'{path}: tested with {match.group(1)}, workspace version is {version}')
    if rel.startswith('getting-started/') or rel == 'guides/orders-golden-path.md':
        if not re.search(r'^## Next steps$', text, re.M):
            print(f'{path}: executable tutorial has no ## Next steps')
TUTORIALPY
)
if [ -n "$tutorial_report" ]; then
  printf '%s\n' "$tutorial_report" >&2
  err 'tutorial test metadata missing or stale'
fi

## 19. Retired editorial terms and the unsafe quickstart shortcut do not
##     return. Product names inside URLs and historical upgrade notes are not
##     aliases and remain valid.
forbidden=$(grep -rnE 'CI-CD|The Console \(Orion UI\)|raw\.githubusercontent\.com/GoPlasmatic/Orion/main/examples/quickstart\.sh *\| *bash' \
  docs/src --include='*.md' 2>/dev/null || true)
if [ -n "$forbidden" ]; then
  printf '%s\n' "$forbidden" >&2
  err 'deprecated terminology or pipe-to-shell quickstart'
fi

## 20. Repository JSON examples and the published OpenAPI document parse.
##     Workflow semantics are covered by the existing server/example CI; this
##     catches malformed copyable assets before those deeper checks run.
json_report=$(python3 - <<'JSONPY'
import json, pathlib

paths = [pathlib.Path('docs/openapi.json')]
paths.extend(pathlib.Path('examples').rglob('*.json'))
for path in sorted(paths):
    try:
        json.loads(path.read_text())
    except Exception as exc:
        print(f'{path}: {exc}')
JSONPY
)
if [ -n "$json_report" ]; then
  printf '%s\n' "$json_report" >&2
  err 'invalid JSON in a copyable repository example'
fi

if [ "$fail" -eq 0 ]; then
  echo 'docs-lint: OK'
else
  exit 1
fi

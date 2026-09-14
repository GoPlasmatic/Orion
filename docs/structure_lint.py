#!/usr/bin/env python3
"""Structure guard for docs/src (DOCUMENTATION_STANDARD.md §11, §12).

docs/lint.sh checks *integrity* — that links resolve, that includes exist, that
the curated index matches the book. This checks *conformance*: that every page
declares one of the standard's types, carries that type's template sections in
order, and obeys the numeric rules (sentence length, callout count, hub size,
navigation depth).

Run from docs/lint.sh, which passes the enforcement level. The two levels exist
because §12 asks for a linter that starts advisory and tightens to failing:

    report   print findings, exit 0     (the default while the rewrite runs)
    enforce  print findings, exit 1     (from Phase 12)

Python rather than Vale because three of these rules need a page's declared
type, and two need the navigation tree. Vale sees neither. Sentence length is
here for a third reason: Vale segments a Markdown table row into one sentence,
which made a five-column reference table report a false positive per row.
"""

from __future__ import annotations

import collections
import datetime
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent
SRC = ROOT / "src"
SUMMARY = SRC / "SUMMARY.md"

# ── The content model (§2.1) ────────────────────────────────────────────────
#
# Six reader-facing types plus the four supporting kinds §2.1 names. A page
# declares exactly one on line 2.
TYPES = {
    "quickstart",
    "tutorial",
    "concept",
    "guide",
    "reference",
    "troubleshooting",
    "hub",
    "release-note",
    "migration",
    "glossary",
}

# Sections each type must carry, in this order (§5). A page may hold other
# headings between them; what is checked is presence and relative order, so a
# guide can number its steps however the task needs.
#
# Reference is the exception: §5.5 is one skeleton for every reference kind,
# and "sections that do not apply are omitted, never renamed". So its list is
# the *permitted order*, and every heading matching one of those names must
# appear in it — that is what stops a page inventing "Usage" between
# "Parameters" and "Returns".
REQUIRED = {
    "quickstart": ["Before you start", "Verify", "What just happened", "Next steps"],
    "tutorial": ["What you will learn", "Before you start", "Recap", "Next steps"],
    "guide": ["Before you start", "Verify", "Next steps"],
    "concept": ["Next steps|Related"],
    "troubleshooting": ["Related"],
    "reference": ["Related"],
    "migration": ["Before you start", "Related"],
    "glossary": [],
    "release-note": [],
    "hub": [],
}

REFERENCE_SKELETON = [
    "Synopsis",
    "Description",
    "Parameters",
    "Fields",
    "Options",
    "Properties",
    "Returns",
    "Outputs",
    "Response",
    "Errors",
    "Examples",
    "Caveats",
    "Compatibility",
    "Related",
]

# §8.1: four labels and only four. mdBook renders three of them as GFM alerts;
# `Deprecated` is a badge plus a prose sentence (see docs/CONTRIBUTING-DOCS.md,
# "Recorded deviations"), because mdBook has no alert keyword for it and a
# relabelled `[!CAUTION]` would read as CAUTION in the Markdown twin.
ALERT_LABELS = {"NOTE", "TIP", "WARNING"}

MAX_CALLOUTS = 3          # §8.2
MAX_HUB_CHILDREN = 7      # §3.3
# Hubs whose child count is a recorded deviation (docs/CONTRIBUTING-DOCS.md,
# "Recorded deviations", §3.3): one machine catalogue, one sidebar group.
HUB_CHILDREN_DEVIATIONS = {
    "reference/functions/index.md",
    "reference/cli/orion-server/index.md",
    "reference/cli/orion-cli/index.md",
    "reference/clippy/index.md",
}
MAX_NAV_DEPTH = 3         # §3.3, section -> group -> page
MAX_SENTENCE_WORDS = 25   # §6.2
MAX_AVERAGE_WORDS = 18    # §6.2
MAX_SCOPE_SENTENCES = 3   # §5.0
MAX_HEADING_LEVEL = 4     # §6.3

# A handoff is three to five described links (§4.5). Reference pages close on
# `## Related`, which is the same contract under another name.
MIN_HANDOFF_LINKS = 3
MAX_HANDOFF_LINKS = 5

# Contractions read as a contract when they are absent (§6.8). Reference
# descriptions, parameter tables, error text and warnings do without them.
NO_CONTRACTIONS = {"reference", "release-note"}
# `'s` is left out: a possessive ("the channel's guards") is not a contraction,
# and nothing short of parsing separates it from "it's". The pronoun forms that
# do contract with `'s` are listed by name instead.
CONTRACTION = re.compile(
    r"\b(?:\w+n(?:'|’)t|\w+(?:'|’)(?:re|ve|ll|d|m)|(?:it|that|what|there|here|let|who|he|she)(?:'|’)s)\b(?![^`]*`)",
    re.IGNORECASE,
)

# Pages outside the book's chapter list: the vendor notice beside the diagram
# assets, and SUMMARY itself.
NOT_A_CHAPTER = {"SUMMARY.md", "diagram-assets/VENDOR.md"}


def problems() -> list[str]:
    found: list[str] = []
    chapters = read_summary(found)
    pages = {rel for rel, _, _ in chapters}

    for path in sorted(SRC.rglob("*.md")):
        rel = str(path.relative_to(SRC))
        if rel in NOT_A_CHAPTER:
            continue
        if rel not in pages:
            found.append(f"{rel}: no SUMMARY chapter, so nothing navigates to it")
            continue
        found.extend(check_page(path, rel))

    found.extend(check_navigation(chapters))
    return found


# ── The navigation tree ─────────────────────────────────────────────────────


def read_summary(found: list[str]) -> list[tuple[str, str, int]]:
    """Every chapter as `(path, label, depth)`, depth 0 for a part's top level."""
    out: list[tuple[str, str, int]] = []
    for line in SUMMARY.read_text().splitlines():
        # A prefix chapter (the landing page) is a bare `[title](./path)` with
        # no list marker; everything else is a `- ` item whose indent is depth.
        match = re.match(r"^(\s*)(?:- )?\[([^\]]+)\]\(\./([A-Za-z0-9_./-]+\.md)\)", line)
        if not match:
            continue
        indent, label, rel = match.groups()
        if len(indent) % 2:
            found.append(f"SUMMARY.md: '{label}' is indented by {len(indent)}, want a multiple of 2")
        out.append((rel, label, len(indent) // 2))
    return out


def check_navigation(chapters: list[tuple[str, str, int]]) -> list[str]:
    found: list[str] = []

    for rel, label, depth in chapters:
        if depth + 1 > MAX_NAV_DEPTH:
            found.append(
                f"SUMMARY.md: '{label}' sits {depth + 1} levels below its part; "
                f"§3.3 allows {MAX_NAV_DEPTH} (section -> group -> page)"
            )

    # A hub's children are the chapters nested directly beneath it.
    children: dict[str, int] = collections.defaultdict(int)
    stack: list[tuple[int, str]] = []
    for rel, _, depth in chapters:
        while stack and stack[-1][0] >= depth:
            stack.pop()
        if stack:
            children[stack[-1][1]] += 1
        stack.append((depth, rel))

    for rel, count in sorted(children.items()):
        if count <= MAX_HUB_CHILDREN or rel in HUB_CHILDREN_DEVIATIONS:
            continue
        page = SRC / rel
        if not page.exists():
            continue
        if declared_type(page.read_text()) != "hub":
            continue
        found.append(
            f"{rel}: hub introduces {count} children; §3.3 allows {MAX_HUB_CHILDREN}. "
            "Group them, or record the deviation in docs/CONTRIBUTING-DOCS.md"
        )

    # §3.6: the sidebar label is the H1, or the H1 with a leading "How to"
    # dropped. Nothing else is dropped, so a reader arriving from search can
    # match the two.
    for rel, label, _ in chapters:
        page = SRC / rel
        if not page.exists():
            continue
        h1 = heading_one(page.read_text())
        if h1 is None:
            found.append(f"{rel}: no H1")
        elif label not in (h1, re.sub(r"^How to ", "", h1)):
            found.append(f"{rel}: sidebar label '{label}' is not the H1 '{h1}' (§3.6)")

    return found


# ── One page ────────────────────────────────────────────────────────────────


def check_page(path: pathlib.Path, rel: str) -> list[str]:
    text = path.read_text()
    lines = text.splitlines()
    found: list[str] = []

    kind = declared_type(text)
    if kind is None:
        return [f"{rel}: no '<!-- type: ... -->' on line 2 (§2.1)"]
    if kind not in TYPES:
        return [f"{rel}: unknown type '{kind}'; one of {sorted(TYPES)}"]

    found += check_stamp(rel, lines, kind)
    found += check_headings(rel, text, kind)
    found += check_scope(rel, text)
    found += check_callouts(rel, lines)
    found += check_prose(rel, text, kind)
    found += check_handoff(rel, text, kind)
    return found


def declared_type(text: str) -> str | None:
    match = re.search(r"^<!--\s*type:\s*([a-z-]+)\s*-->$", text, re.M)
    return match.group(1) if match else None


def heading_one(text: str) -> str | None:
    for line in text.splitlines():
        if line.startswith("# "):
            return line[2:].strip()
    return None


def check_stamp(rel: str, lines: list[str], kind: str) -> list[str]:
    """§10.2: one of two stamps, and no third."""
    verified = generated = None
    for line in lines[:6]:
        match = re.match(r"^<!--\s*last_verified:\s*(\d{4}-\d{2}-\d{2})\s*-->$", line)
        if match:
            verified = match.group(1)
        match = re.match(
            r"^<!--\s*generated_from:\s*(\S+) on (\d{4}-\d{2}-\d{2})\s*-->$", line
        )
        if match:
            generated = match.group(2)

    if verified and generated:
        return [f"{rel}: carries both freshness stamps; §10.2 allows one"]
    stamp = verified or generated
    if not stamp:
        return [
            f"{rel}: no freshness stamp. Authored pages carry "
            "'<!-- last_verified: YYYY-MM-DD -->', generated pages "
            "'<!-- generated_from: <version> on YYYY-MM-DD -->' (§10.2)"
        ]

    try:
        when = datetime.date.fromisoformat(stamp)
    except ValueError:
        return [f"{rel}: freshness stamp '{stamp}' is not a date"]
    if when > datetime.date.today():
        return [f"{rel}: freshness stamp '{stamp}' is in the future"]
    return []


def check_headings(rel: str, text: str, kind: str) -> list[str]:
    found: list[str] = []
    headings = [
        (len(m.group(1)), m.group(2).strip())
        for m in re.finditer(r"^(#{1,6})\s+(.+?)\s*$", strip_fences(text), re.M)
    ]

    previous = 0
    for level, title in headings:
        if level > MAX_HEADING_LEVEL:
            found.append(f"{rel}: '{title}' is H{level}; §3.3 stops at H{MAX_HEADING_LEVEL}")
        if previous and level > previous + 1:
            found.append(f"{rel}: '{title}' skips from H{previous} to H{level} (§6.3)")
        # §6.3 allows no end punctuation. The one exception is §5.6's
        # question-form troubleshooting heading ("How do I reset my root
        # password?"), which is the reader's own sentence.
        if title.endswith((".", ":", "!")):
            found.append(f"{rel}: heading '{title}' ends in punctuation (§6.3)")
        previous = level

    twos = [t for level, t in headings if level == 2]
    required = REQUIRED[kind]
    position = 0
    for want in required:
        # "A|B" means either heading satisfies the slot.
        options = [w.lower() for w in want.split("|")]
        matches = [i for i, t in enumerate(twos) if any(t.lower().startswith(o) for o in options)]
        later = [i for i in matches if i >= position]
        if not later:
            found.append(f"{rel}: a {kind} needs '## {want}' (§5)")
        else:
            position = later[0] + 1

    if kind == "reference":
        seen = [t for t in twos if t in REFERENCE_SKELETON]
        order = [REFERENCE_SKELETON.index(t) for t in seen]
        if order != sorted(order):
            found.append(
                f"{rel}: reference sections are out of §5.5 order: {seen}"
            )

    if kind == "hub" and len(twos) > 2:
        found.append(
            f"{rel}: a hub has {len(twos)} H2 sections. §5.7: one paragraph, its "
            "children, an optional routing table, and nothing else"
        )

    return found


def check_scope(rel: str, text: str) -> list[str]:
    """§5.0: one to three sentences between the H1 and the first H2 or block."""
    body = strip_fences(text)
    after = re.split(r"^# .+$", body, maxsplit=1, flags=re.M)
    if len(after) < 2:
        return []
    scope: list[str] = []
    for line in after[1].splitlines():
        stripped = line.strip()
        if not stripped:
            if scope:
                break
            continue
        if stripped.startswith(("#", "|", "<", ">")):
            break
        if re.match(r"^([-*+] |\d+\. )", stripped):
            break
        scope.append(stripped)
    if not scope:
        return [f"{rel}: no scope paragraph after the H1 (§4.4, §5.0)"]
    count = len(sentences(" ".join(scope)))
    if count > MAX_SCOPE_SENTENCES:
        return [
            f"{rel}: scope paragraph is {count} sentences; §5.0 allows "
            f"{MAX_SCOPE_SENTENCES}"
        ]
    return []


def check_callouts(rel: str, lines: list[str]) -> list[str]:
    found: list[str] = []
    count = 0
    previous_end = -99
    for index, line in enumerate(lines):
        match = re.match(r"^> \[!([A-Z]+)\]\s*$", line)
        if not match:
            continue
        label = match.group(1)
        count += 1
        if label not in ALERT_LABELS:
            found.append(
                f"{rel}:{index + 1}: callout '[!{label}]' is not one of "
                f"{sorted(ALERT_LABELS)} (§8.1)"
            )
        if index - previous_end <= 1:
            found.append(f"{rel}:{index + 1}: two callouts in a row (§8.2)")
        end = index
        while end + 1 < len(lines) and lines[end + 1].startswith(">"):
            end += 1
        previous_end = end
    if count > MAX_CALLOUTS:
        found.append(
            f"{rel}: {count} callouts; §8.2 allows {MAX_CALLOUTS}. The rest belong "
            "in a Caveats section or a troubleshooting entry"
        )
    return found


def check_prose(rel: str, text: str, kind: str) -> list[str]:
    found: list[str] = []
    paragraphs = prose(text)
    if not paragraphs:
        return found

    lengths = [len(s.split()) for s in sentences(paragraphs)]
    if not lengths:
        return found

    over = [n for n in lengths if n > MAX_SENTENCE_WORDS]
    if over:
        found.append(
            f"{rel}: {len(over)} sentences over {MAX_SENTENCE_WORDS} words "
            f"(longest {max(over)}) (§6.2)"
        )
    average = sum(lengths) / len(lengths)
    if average > MAX_AVERAGE_WORDS:
        found.append(
            f"{rel}: average sentence is {average:.1f} words; §6.2 targets "
            f"{MAX_AVERAGE_WORDS}"
        )

    if kind in NO_CONTRACTIONS:
        body = " ".join(paragraphs)
        hits = sorted({m.group(0) for m in CONTRACTION.finditer(body)})
        if hits:
            found.append(
                f"{rel}: a {kind} page uses contractions ({', '.join(hits[:5])}); "
                "§6.8 keeps the flat form here"
            )
    return found


def check_handoff(rel: str, text: str, kind: str) -> list[str]:
    """§4.5: three to five links, each with a one-line description."""
    if kind in {"hub", "glossary", "release-note"}:
        return []
    body = strip_fences(text)
    heading = None
    for candidate in ("Next steps", "Related"):
        if re.search(rf"^## {candidate}\s*$", body, re.M):
            heading = candidate
            break
    if heading is None:
        return []  # check_headings already reported the missing section
    parts = re.split(rf"^## {heading}\s*$", body, flags=re.M)
    tail = parts[-1]
    links = re.findall(r"^\s*[-*] \[([^\]]+)\]\(([^)]+)\)(.*)$", tail, re.M)
    if not (MIN_HANDOFF_LINKS <= len(links) <= MAX_HANDOFF_LINKS):
        return [
            f"{rel}: '## {heading}' has {len(links)} links; §4.5 wants "
            f"{MIN_HANDOFF_LINKS} to {MAX_HANDOFF_LINKS}"
        ]
    bare = [name for name, _, rest in links if len(rest.strip(" :.—-")) < 10]
    if bare:
        return [
            f"{rel}: '## {heading}' links have no description: {bare}. "
            "§4.5: a bare link list is not a handoff"
        ]
    return []


# ── Text handling ───────────────────────────────────────────────────────────


def strip_fences(text: str) -> str:
    out: list[str] = []
    fenced = False
    for line in text.splitlines():
        if line.lstrip().startswith("```"):
            fenced = not fenced
            out.append("")
            continue
        out.append("" if fenced else line)
    return "\n".join(out)


def prose(text: str) -> list[str]:
    """Body paragraphs only: no fences, tables, headings, HTML, callouts or lists.

    List items and table cells are excluded because neither is a sentence — a
    parameter description is a fragment by design, and counting it drags the
    average down while a long table row drags the maximum up.

    Paragraphs come back separately rather than joined, because a paragraph
    boundary is a sentence boundary even when the paragraph ends in a colon.
    The one-line lead-in before a code block ("Then start the server:") is
    the common case; joining it to the paragraph after the block invented a
    forty-word sentence out of two short ones.
    """
    out: list[str] = []
    current: list[str] = []
    in_list = False
    for line in strip_fences(text).splitlines():
        stripped = line.strip()
        if not stripped:
            if current:
                out.append(" ".join(current))
                current = []
            in_list = False
            continue
        if stripped.startswith(("#", "|", "<", ">")):
            continue
        if re.match(r"^[-*+] ", stripped) or re.match(r"^\d+\.\s", stripped):
            in_list = True
            continue
        if in_list:
            # A hard-wrapped list item continues on indented lines until the
            # next blank line; those lines are still the item, not a paragraph.
            continue
        if stripped.startswith("<!--"):
            continue
        current.append(stripped)
    if current:
        out.append(" ".join(current))
    return out


def sentences(paragraphs: list[str] | str) -> list[str]:
    """Split each paragraph on sentence-final punctuation, keeping abbreviations.

    `1.8.0`, `e.g.` and `orion-server fmt.` all end in a period that is not a
    sentence boundary, so a split is only taken when the period is followed by
    whitespace and a capital letter, an opening bracket, a code span or an
    emphasis marker. The last three matter because a sentence opening on
    `` `config_json` `` or `**What changed.**` is a sentence, and joining it to
    the one before invented a forty-word finding out of two short ones. A
    paragraph is always its own boundary (see `prose`).
    """
    if isinstance(paragraphs, str):
        paragraphs = [paragraphs]
    out: list[str] = []
    for text in paragraphs:
        text = re.sub(r"`[^`]*`", "CODE", text)
        text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
        text = re.sub(r"\*\*([^*]+)\*\*", r"\1", text)
        parts = re.split(r"(?<=[.!?])\s+(?=[A-Z(\"`*\[])", text)
        out.extend(p for p in parts if len(p.split()) > 2)
    return out


def main() -> int:
    level = sys.argv[1] if len(sys.argv) > 1 else "report"
    found = problems()
    if not found:
        print("docs-structure: OK")
        return 0

    for line in found:
        print(f"docs-structure: {line}", file=sys.stderr)
    print(f"\ndocs-structure: {len(found)} findings", file=sys.stderr)
    if level == "enforce":
        return 1
    print(
        "docs-structure: advisory — pass 'enforce' to fail the build (§12)",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

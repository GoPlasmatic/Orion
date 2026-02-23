#!/usr/bin/env bash
# Generate docs/project-tasks.md from the GitHub Project board.
# Requires: gh (GitHub CLI), python3
# Usage: ./scripts/generate-project-tasks.sh

set -euo pipefail

ORG="GoPlasmatic"
PROJECT_NUM=14
REPO="Orion"
OUTFILE="docs/project-tasks.md"

# Resolve script location so it works from any cwd
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPATH="$ROOT_DIR/$OUTFILE"

mkdir -p "$(dirname "$OUTPATH")"

echo "Fetching project items from $ORG project #$PROJECT_NUM ..."

# 1. Fetch project items (status lives here)
PROJECT_JSON=$(gh project item-list "$PROJECT_NUM" --owner "$ORG" --format json --limit 300)

# 2. Fetch issue metadata (milestone, labels, state)
ISSUES_JSON=$(gh issue list --repo "$ORG/$REPO" --state all --limit 300 \
  --json number,title,milestone,labels,state)

# 3. Merge and render with Python
python3 - "$PROJECT_JSON" "$ISSUES_JSON" "$OUTPATH" "$ORG" "$REPO" "$PROJECT_NUM" <<'PYEOF'
import json, sys
from datetime import date
from collections import Counter, OrderedDict

project_json = json.loads(sys.argv[1])
issues_json = json.loads(sys.argv[2])
outpath = sys.argv[3]
org = sys.argv[4]
repo = sys.argv[5]
project_num = sys.argv[6]

# Build lookup: issue number -> { milestone, labels, state }
issue_meta = {}
for iss in issues_json:
    num = iss["number"]
    ms = (iss.get("milestone") or {}).get("title", "")
    labels = sorted([l["name"] for l in iss.get("labels", [])])
    state = iss.get("state", "OPEN")
    issue_meta[num] = {"milestone": ms, "labels": labels, "state": state}

# Build items from project board
items = []
for pitem in project_json.get("items", []):
    content = pitem.get("content", {})
    num = content.get("number")
    title = content.get("title", "")
    status = pitem.get("status", "")
    if not num:
        continue

    # Determine type from title prefix
    if title.startswith("[Epic]"):
        item_type = "Epic"
        clean_title = title.replace("[Epic] ", "")
    elif title.startswith("[Story]"):
        item_type = "Story"
        clean_title = title.replace("[Story] ", "")
    elif title.startswith("[Task]"):
        item_type = "Task"
        clean_title = title.replace("[Task] ", "")
    else:
        item_type = "Task"
        clean_title = title

    meta = issue_meta.get(num, {"milestone": "", "labels": [], "state": "OPEN"})

    items.append({
        "num": num,
        "title": clean_title,
        "status": status,
        "type": item_type,
        "milestone": meta["milestone"],
        "labels": meta["labels"],
        "gh_state": meta["state"],
    })

items.sort(key=lambda x: x["num"])
total = len(items)

# --- Summaries ---
status_counts = Counter(i["status"] for i in items)
type_counts = Counter(i["type"] for i in items)
ms_counts = Counter(i["milestone"] for i in items if i["milestone"])

# Ordered status display
status_order = ["Todo", "Ready", "In Progress", "Done"]
ms_order = [
    "Phase 1: Foundation",
    "Phase 2: Core APIs",
    "Phase 3: Integration",
    "Phase 4: Kafka",
    "Phase 5: Production",
]

# --- Render ---
lines = []
w = lines.append

w(f"# {repo} Project Tasks")
w("")
w(f"> **Organization:** [{org}](https://github.com/{org})  ")
w(f"> **Project:** {repo} (Project #{project_num})  ")
w(f"> **Total Items:** {total}  ")
w(f"> **Last Updated:** {date.today().isoformat()}")
w("")
w("---")
w("")
w("## Summary")
w("")
w("### By Status")
w("")
w("| Status | Count |")
w("|--------|------:|")
for s in status_order:
    if status_counts.get(s):
        w(f"| {s} | {status_counts[s]} |")
w(f"| **Total** | **{total}** |")
w("")
w("### By Type")
w("")
w("| Type | Count |")
w("|------|------:|")
for t in ["Epic", "Story", "Task"]:
    if type_counts.get(t):
        w(f"| {t} | {type_counts[t]} |")
w(f"| **Total** | **{total}** |")
w("")
w("### By Milestone")
w("")
w("| Milestone | Count |")
w("|-----------|------:|")
for m in ms_order:
    if ms_counts.get(m):
        w(f"| {m} | {ms_counts[m]} |")
w(f"| **Total** | **{total}** |")

def render_table(item_type, item_list):
    w("")
    w("---")
    w("")
    plural = "ies" if item_type.endswith("y") else "s"
    display = item_type[:-1] + plural if item_type.endswith("y") else item_type + plural
    w(f"## {display} ({len(item_list)})")
    w("")
    w("| # | Title | Status | Labels | Milestone | Link |")
    w("|--:|-------|--------|--------|-----------|------|")
    for i in item_list:
        labels_str = ", ".join(f"`{l}`" for l in i["labels"])
        link = f"[#{i['num']}](https://github.com/{org}/{repo}/issues/{i['num']})"
        w(f"| {i['num']} | {i['title']} | {i['status']} | {labels_str} | {i['milestone']} | {link} |")

epics = [i for i in items if i["type"] == "Epic"]
stories = [i for i in items if i["type"] == "Story"]
tasks = [i for i in items if i["type"] == "Task"]

render_table("Epic", epics)
render_table("Story", stories)
render_table("Task", tasks)

w("")

with open(outpath, "w") as f:
    f.write("\n".join(lines))

print(f"Written {outpath}")
print(f"  {len(epics)} epics, {len(stories)} stories, {len(tasks)} tasks")
print(f"  Status: {dict(status_counts)}")
PYEOF

echo "Done."

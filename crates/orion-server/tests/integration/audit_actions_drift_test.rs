//! Drift guard for the audit-log `action` and `resource_type` vocabularies.
//!
//! `GET /audit-logs` filters on both fields by **exact match**, so an
//! operator's compliance query is only as good as the vocabulary the page
//! publishes. That made the page's list load-bearing, and nothing checked it:
//! the 1.0 documentation audit found it naming ten actions when the code wrote
//! fourteen forms. A filter on a missing action does not error — it returns an
//! empty page, which reads exactly like "nothing happened".
//!
//! The call sites are the authority. This module walks `src/`, reads the
//! action and resource type out of every `audit_log` / `audit_log_draft_only` /
//! `audit_and_reload` call, and asserts against the table in
//! `operate/audit-logs.md`:
//!
//! 1. Every emitted `(action, resource_type)` pair is documented.
//! 2. Every documented action is one some call site emits.
//! 3. Every documented resource type is one some call site emits.
//!
//! Directions 2 and 3 are what catch a *ghost* — an entry describing something
//! the server does not do. The page's original list had one: `status_draft`,
//! which cannot be written at all, because `StatusAction::parse` refuses a
//! transition to draft before the audit row is reached.
//!
//! The pair check is deliberately one-directional. A table row groups several
//! resource types under one action, so the cross product it implies is wider
//! than the set of real pairs, and asserting that every implied pair is emitted
//! would fail on rows that are perfectly accurate. What matters — and what
//! actually rotted — is that nothing the server writes is missing from the
//! page, and that the page invents nothing.

use std::collections::BTreeSet;

const SRC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
const AUDIT_LOGS_MD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/src/operate/audit-logs.md"
);

/// The helpers that write an audit row. Each takes the action as its third
/// argument and the resource type as its fourth.
const WRITERS: [&str; 3] = ["audit_log(", "audit_log_draft_only(", "audit_and_reload("];

/// Actions built with `format!`, and the values the interpolation can actually
/// take.
///
/// Only two call sites compute their action, and neither can be resolved by
/// reading the `format!` alone — the reachable set is decided by a gate
/// upstream of the call:
///
/// * `status_{}` — `StatusAction::parse` accepts only `active` and `archived`
///   and returns a `400` for `draft`, so a draft transition never reaches the
///   audit write. Expanding this over all three `EntityStatus` values would
///   invent `status_draft`, the ghost this module exists to catch.
/// * `package_{}` — `PUT /packages/{name}` accepts either `PackageState`, and
///   both are written.
const COMPUTED_ACTIONS: [(&str, &[&str]); 2] = [
    ("status_", &["active", "archived"]),
    ("package_", &["staged", "applied"]),
];

/// One audit row the server can write.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Emitted {
    action: String,
    resource_type: String,
}

/// Walk `src/`, skipping `#[cfg(test)]` modules, and collect every
/// `(action, resource_type)` an audit writer is called with.
///
/// Test modules are excluded for the same reason `field_codes_drift_test`
/// excludes them: a test may write a nonsense action to assert a query, and
/// that is not a vocabulary any operator can filter on.
fn emitted_pairs() -> BTreeSet<Emitted> {
    fn walk(dir: &std::path::Path, out: &mut BTreeSet<Emitted>) {
        for entry in std::fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let source = std::fs::read_to_string(&path).expect("read source file");
                let production = match source.find("#[cfg(test)]") {
                    Some(i) => &source[..i],
                    None => &source[..],
                };
                collect(production, out);
            }
        }
    }

    fn collect(source: &str, out: &mut BTreeSet<Emitted>) {
        for writer in WRITERS {
            for (index, _) in source.match_indices(writer) {
                let args = arguments(&source[index + writer.len()..]);
                // Every writer takes the action third and the resource type
                // fourth. A non-call — the helper definitions in
                // `routes/admin/mod.rs`, whose parameters are identifiers
                // rather than literals — resolves to neither and is skipped.
                let (Some(action), Some(resource_type)) = (args.get(2), args.get(3)) else {
                    continue;
                };
                let Some(resource_type) = literal(resource_type) else {
                    continue;
                };
                for action in actions(action) {
                    out.insert(Emitted {
                        action,
                        resource_type: resource_type.clone(),
                    });
                }
            }
        }
    }

    let mut out = BTreeSet::new();
    walk(std::path::Path::new(SRC_DIR), &mut out);
    out
}

/// Split an argument list at its top-level commas, stopping at the closing
/// paren. Nesting and string literals are tracked so a `format!("a, b", x)`
/// argument is not split down the middle.
fn arguments(rest: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;

    for ch in rest.chars() {
        if in_string {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                current.push(ch);
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ')' if depth == 0 => {
                args.push(current.trim().to_string());
                return args;
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                args.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    args
}

/// The contents of a bare `"..."` argument, or `None` for anything computed.
fn literal(arg: &str) -> Option<String> {
    let arg = arg.trim().trim_start_matches('&').trim();
    let inner = arg.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('"')).then(|| inner.to_string())
}

/// Every action one argument can produce: a single value for a literal, or the
/// declared expansion for a `format!` with a known prefix.
fn actions(arg: &str) -> Vec<String> {
    if let Some(literal) = literal(arg) {
        return vec![literal];
    }
    let arg = arg.trim().trim_start_matches('&').trim();
    if !arg.starts_with("format!(") {
        return Vec::new();
    }
    for (prefix, values) in COMPUTED_ACTIONS {
        if arg.contains(&format!("\"{prefix}{{}}\"")) {
            return values.iter().map(|v| format!("{prefix}{v}")).collect();
        }
    }
    panic!(
        "a computed audit action has no declared expansion: {arg}\n\
         Add it to COMPUTED_ACTIONS with the set of values it can take."
    );
}

/// The action and resource-type cells of the page's vocabulary table.
///
/// The page carries other tables, so this locks onto the one whose header
/// names both columns and stops at the first line that is not a table row.
fn documented() -> (BTreeSet<String>, BTreeSet<String>) {
    let page = std::fs::read_to_string(AUDIT_LOGS_MD).expect("read operate/audit-logs.md");
    let mut actions = BTreeSet::new();
    let mut resource_types = BTreeSet::new();
    let mut in_table = false;

    for line in page.lines() {
        let cells = row_cells(line);
        if !in_table {
            in_table = cells.len() >= 2
                && cells[0].contains("`action`")
                && cells[1].contains("`resource_type`");
            continue;
        }
        if cells.len() < 2 {
            break;
        }
        if cells[0].starts_with("---") {
            continue;
        }
        actions.extend(backticked(cells[0]));
        resource_types.extend(backticked(cells[1]));
    }

    assert!(
        !actions.is_empty() && !resource_types.is_empty(),
        "the vocabulary table in operate/audit-logs.md did not parse — its header \
         must name `action` and `resource_type` in the first two columns"
    );
    (actions, resource_types)
}

fn row_cells(line: &str) -> Vec<&str> {
    let line = line.trim();
    if !line.starts_with('|') {
        return Vec::new();
    }
    line.trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect::<Vec<_>>()
}

/// Every backticked token in one cell.
fn backticked(cell: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = cell;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else { break };
        out.push(after[..close].to_string());
        rest = &after[close + 1..];
    }
    out
}

/// Nothing the server records is missing from the page.
#[test]
fn every_emitted_audit_action_is_documented() {
    let (actions, resource_types) = documented();
    let missing: Vec<String> = emitted_pairs()
        .into_iter()
        .filter(|e| !actions.contains(&e.action) || !resource_types.contains(&e.resource_type))
        .map(|e| format!("{} on {}", e.action, e.resource_type))
        .collect();

    assert!(
        missing.is_empty(),
        "the server writes audit rows the vocabulary table in \
         docs/src/operate/audit-logs.md does not list:\n  {}\n\
         `action` and `resource_type` are exact-match filters, so an undocumented \
         value is a query that silently returns nothing.",
        missing.join("\n  ")
    );
}

/// The page invents nothing.
#[test]
fn no_documented_audit_action_is_a_ghost() {
    let (actions, resource_types) = documented();
    let emitted = emitted_pairs();
    let emitted_actions: BTreeSet<&str> = emitted.iter().map(|e| e.action.as_str()).collect();
    let emitted_types: BTreeSet<&str> = emitted.iter().map(|e| e.resource_type.as_str()).collect();

    let ghost_actions: Vec<&String> = actions
        .iter()
        .filter(|a| !emitted_actions.contains(a.as_str()))
        .collect();
    let ghost_types: Vec<&String> = resource_types
        .iter()
        .filter(|t| !emitted_types.contains(t.as_str()))
        .collect();

    assert!(
        ghost_actions.is_empty() && ghost_types.is_empty(),
        "docs/src/operate/audit-logs.md documents audit values no call site writes — \
         actions {ghost_actions:?}, resource types {ghost_types:?}. \
         An operator filtering on one of these waits forever for rows that never come."
    );
}

/// The extractor still finds the call sites.
///
/// Without this, a refactor that renames the writers would empty the emitted
/// set and turn both checks above into green no-ops.
#[test]
fn the_extractor_finds_the_audit_call_sites() {
    let emitted = emitted_pairs();
    assert!(
        emitted.len() > 15,
        "expected many audit call sites, found {} — the extractor has probably \
         stopped recognising a writer in WRITERS",
        emitted.len()
    );
    for expected in [
        "reload",
        "update_rollout",
        "status_active",
        "package_applied",
    ] {
        assert!(
            emitted.iter().any(|e| e.action == expected),
            "the extractor missed the `{expected}` call site"
        );
    }
}

//! Documentation drift guard for the metric surface (proposal O14).
//!
//! Metric names and label keys are a public contract: they are typed into
//! dashboards and alert rules that live outside this repository. O14 renamed
//! three of them (`orion_errors_total`'s `type` label, `kafka_consumer_lag`,
//! and the `orion_channel_executions_total` family it deleted outright), and
//! the documentation table listing them sat in a different file — the exact
//! shape of drift that leaves an operator reading a name the binary no longer
//! emits.
//!
//! `src/metrics.rs` is authoritative: every `counter!` / `gauge!` /
//! `histogram!` invocation in the crate lives there, so its macro arguments
//! are the complete metric surface. This module parses them and asserts:
//!
//! 1. Every emitted metric appears in the metrics reference page's tables.
//! 2. No table row invents a metric the binary does not emit — which is what
//!    a rename with a missed doc update looks like.
//! 3. The documented labels are exactly the labels the code passes.
//! 4. Every name carries the `orion_` prefix, so nothing can collide in a
//!    shared registry.

use std::collections::{BTreeMap, BTreeSet};

const METRICS_RS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/metrics.rs");
const OBSERVABILITY_MD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/src/reference/metrics.md"
);

/// `name -> label keys`, parsed from the `counter!` / `gauge!` / `histogram!`
/// invocations in `src/metrics.rs`.
///
/// The grammar is fixed by the `metrics` crate:
/// `macro!("name", "label" => value, …)`. Every invocation in this crate is a
/// literal name followed by literal label keys — there is no dynamic-name call
/// site, and this parser would not silently accept one (a non-literal first
/// argument produces no entry, which surfaces as a "documented but not
/// emitted" failure below).
fn emitted_metrics() -> BTreeMap<String, BTreeSet<String>> {
    let source = std::fs::read_to_string(METRICS_RS).expect("read src/metrics.rs");
    let mut out = BTreeMap::new();
    for macro_name in ["counter!", "gauge!", "histogram!"] {
        let mut rest = source.as_str();
        while let Some(at) = rest.find(macro_name) {
            rest = &rest[at + macro_name.len()..];
            // A real invocation opens its parenthesis immediately. Prose in
            // the module's doc comments mentions these macro names too.
            if !rest.starts_with('(') {
                continue;
            }
            let Some(args) = macro_args(rest) else {
                continue;
            };
            let mut literals = string_literals(args);
            if literals.is_empty() {
                continue;
            }
            let name = literals.remove(0);
            // A label key is the last literal before each `=>`. Taking it that
            // way rather than "every other literal" keeps literal *values*
            // (`"status" => status`, `"channel" => channel.to_owned()`) from
            // shifting the alignment.
            let chunks: Vec<&str> = args.split("=>").collect();
            let labels: BTreeSet<String> = chunks[..chunks.len().saturating_sub(1)]
                .iter()
                .filter_map(|chunk| string_literals(chunk).pop())
                .collect();
            out.entry(name).or_insert_with(BTreeSet::new).extend(labels);
        }
    }
    assert!(
        out.len() > 20,
        "the parser found only {} metrics — it has stopped matching the source",
        out.len()
    );
    out
}

/// The argument list of a macro invocation: `s` starts immediately after the
/// `name!`, so this takes everything between the opening parenthesis and its
/// balanced partner. Parentheses inside string literals do not count, and
/// nested ones do — `"version" => env!("CARGO_PKG_VERSION")` is one argument,
/// not a truncation point.
fn macro_args(s: &str) -> Option<&str> {
    let open = s.find('(')?;
    let mut depth = 0usize;
    let mut in_string = false;
    for (i, c) in s[open..].char_indices() {
        match c {
            '"' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Every `"…"` literal in `s`, in order. The metric call sites contain no
/// escapes, so a naive quote scan is exact here.
fn string_literals(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.char_indices();
    while let Some((start, c)) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut end = None;
        for (i, c) in chars.by_ref() {
            if c == '"' {
                end = Some(i);
                break;
            }
        }
        let Some(end) = end else { break };
        out.push(s[start + 1..end].to_string());
    }
    out
}

/// `name -> label keys` from every metrics table on the metrics reference page: a
/// row whose first cell is a backticked `orion_*` name, with the third cell
/// holding its backticked labels (or the em dash for none).
fn documented_metrics() -> BTreeMap<String, BTreeSet<String>> {
    let doc = std::fs::read_to_string(OBSERVABILITY_MD).expect("read metrics.md");
    let mut out = BTreeMap::new();
    for line in doc.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() < 3 {
            continue;
        }
        let Some(name) = backticked(cells[0]).into_iter().next() else {
            continue;
        };
        if !name.starts_with("orion_") {
            continue;
        }
        out.insert(name, backticked(cells[2]).into_iter().collect());
    }
    out
}

/// The backticked spans in one table cell.
fn backticked(cell: &str) -> Vec<String> {
    cell.split('`')
        .skip(1)
        .step_by(2)
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn every_emitted_metric_is_documented() {
    let emitted = emitted_metrics();
    let documented = documented_metrics();
    let missing: Vec<&String> = emitted
        .keys()
        .filter(|name| !documented.contains_key(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "these metrics are emitted but absent from docs/src/reference/metrics.md: {missing:?}"
    );
}

#[test]
fn no_documented_metric_is_a_ghost() {
    let emitted = emitted_metrics();
    let documented = documented_metrics();
    let ghosts: Vec<&String> = documented
        .keys()
        .filter(|name| !emitted.contains_key(*name))
        .collect();
    assert!(
        ghosts.is_empty(),
        "docs/src/reference/metrics.md documents metrics the binary does not emit \
         (a rename with a missed doc update looks exactly like this): {ghosts:?}"
    );
}

#[test]
fn documented_labels_match_the_code() {
    let emitted = emitted_metrics();
    let documented = documented_metrics();
    let mut mismatches = Vec::new();
    for (name, labels) in &emitted {
        let Some(doc_labels) = documented.get(name) else {
            continue; // reported by every_emitted_metric_is_documented
        };
        if labels != doc_labels {
            mismatches.push(format!("{name}: code {labels:?} vs docs {doc_labels:?}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "label drift between src/metrics.rs and the metrics reference page:\n  {}",
        mismatches.join("\n  ")
    );
}

/// O14: `kafka_consumer_lag` was the one family without the prefix, so it
/// could collide with any other exporter's gauge of the same name.
#[test]
fn every_metric_carries_the_orion_prefix() {
    let unprefixed: Vec<String> = emitted_metrics()
        .into_keys()
        .filter(|name| !name.starts_with("orion_"))
        .collect();
    assert!(
        unprefixed.is_empty(),
        "every metric must carry the `orion_` prefix so it cannot collide in a shared \
         registry: {unprefixed:?}"
    );
}

/// O14 spelled out: the exact names and labels the rename produced. Reverting
/// any one of them fails here with the old spelling named.
#[test]
fn o14_renames_are_in_place() {
    let emitted = emitted_metrics();
    assert!(
        !emitted.contains_key("orion_channel_executions_total"),
        "orion_channel_executions_total was merged into orion_messages_total \
         (`sum by (channel)`) — it must not come back"
    );
    assert_eq!(
        emitted.get("orion_errors_total"),
        Some(&BTreeSet::from(["reason".to_string()])),
        "orion_errors_total is labelled `reason`, not `type`"
    );
    assert!(
        emitted.contains_key("orion_kafka_consumer_lag_messages"),
        "the Kafka lag gauge is orion_kafka_consumer_lag_messages (prefix + unit)"
    );
    assert!(
        emitted.contains_key("orion_messages_total"),
        "orion_messages_total is the surviving per-channel counter"
    );
}

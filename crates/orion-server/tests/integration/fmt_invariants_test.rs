//! The three properties the formatter promises, checked over every JSON
//! file the repository ships rather than over hand-picked inputs:
//!
//! - **round-trip** — the output converts to the same `serde_json::Value`
//!   as the input, so formatting never changes what a document means;
//! - **idempotence** — formatting the output again changes nothing, so a
//!   second `fmt` run (or a `--check` after a write) is always clean;
//! - **width** — no output line exceeds the style width unless it holds a
//!   single token that is itself wider, which is the one thing a line
//!   printer cannot fix.

use std::path::{Path, PathBuf};

use orion::definitions::Document;
use orion::definitions::fmt::style::STYLE;
use orion::definitions::fmt::{Outcome, format_document, format_str};

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

fn corpus() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in ["../../examples", "../../tests/e2e", "tests/fixtures/fmt"] {
        out.extend(orion::definitions::json_files(&Path::new(MANIFEST).join(dir)).unwrap());
    }
    // Only the parseable half of the conformance set has a formatted form.
    out.extend(
        orion::definitions::json_files(&Path::new(MANIFEST).join("tests/fixtures/json"))
            .unwrap()
            .into_iter()
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("y_")),
    );
    out
}

fn format(text: &str, origin: &str) -> String {
    match format_str(text, origin).unwrap_or_else(|e| panic!("{origin}: {e}")) {
        Outcome::Unchanged => text.to_string(),
        Outcome::Changed(s) => s,
    }
}

#[test]
fn formatting_preserves_every_value_in_the_corpus() {
    for path in corpus() {
        let text = std::fs::read_to_string(&path).unwrap();
        let out = format(&text, &path.display().to_string());
        let before = Document::parse(&text).unwrap().to_value();
        let after = Document::parse(&out).unwrap().to_value();
        assert_eq!(before, after, "{}", path.display());
        // serde_json refuses a BOM that the front end strips; compare with
        // what serde_json sees once it is gone.
        assert_eq!(
            after,
            serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')).unwrap(),
            "{}",
            path.display()
        );
    }
}

#[test]
fn formatting_is_idempotent_over_the_corpus() {
    for path in corpus() {
        let text = std::fs::read_to_string(&path).unwrap();
        let once = format(&text, &path.display().to_string());
        assert_eq!(
            format_str(&once, &path.display().to_string()).unwrap(),
            Outcome::Unchanged,
            "{}: a second pass changed the output",
            path.display()
        );
        // And the unguarded entry point agrees with the guarded one.
        assert_eq!(format_document(&Document::parse(&text).unwrap()), once);
    }
}

#[test]
fn no_output_line_exceeds_the_width_unless_one_token_does() {
    for path in corpus() {
        let text = std::fs::read_to_string(&path).unwrap();
        let out = format(&text, &path.display().to_string());
        for (n, line) in out.lines().enumerate() {
            let width = line.chars().count();
            if width <= STYLE.width {
                continue;
            }
            // The line must consist of indentation plus one token — a
            // string, a number or a unary operator node — with at most a
            // key before it and a comma after. Everything else on it would
            // have been broken.
            let body = line.trim_start().trim_end_matches(',');
            let value = match body.split_once("\": ") {
                Some((key, value)) if key.starts_with('"') => value,
                _ => body,
            };
            assert!(
                is_one_token(value),
                "{}:{}: {width} columns and not a single token: {line}",
                path.display(),
                n + 1
            );
        }
    }
}

/// A single JSON string or number, or a unary operator node — the shapes
/// the style prints on one line regardless of width.
fn is_one_token(s: &str) -> bool {
    let s = s.trim_end_matches(',');
    // A bracket on its own line — past the width only when the nesting
    // itself is deeper than the width allows, as in the 127-level fixture.
    if matches!(s, "[" | "]" | "{" | "}" | "[]" | "{}") {
        return true;
    }
    if s.parse::<f64>().is_ok() {
        return true;
    }
    if s.starts_with('"') {
        return serde_json::from_str::<String>(s).is_ok();
    }
    // `{ "var": "…" }` and friends: a single-member object whose value is a
    // scalar or scalar array.
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(s)
        && map.len() == 1
    {
        return true;
    }
    false
}

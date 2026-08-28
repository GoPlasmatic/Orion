//! The order-preserving JSON front end (`definitions::json`) against
//! `serde_json`, which is the runtime's parser and therefore the authority
//! on what a document is.
//!
//! Two corpora. The **repository's own JSON** — examples, e2e fixtures, the
//! OpenAPI document — must parse to the same value through both. The
//! **conformance set** under `tests/fixtures/json/` is hand-written:
//! `y_*.json` must parse, `n_*.json` must not, and on every file the two
//! parsers must agree, so the front end can never accept a document the
//! admin API will refuse or refuse one it accepts. The two deliberate
//! disagreements — a duplicate key, which `serde_json` tolerates and the
//! front end refuses; a byte-order mark, which `serde_json` refuses and the
//! front end strips (`i_*.json`) — are asserted as such rather than hidden by
//! an exemption.

use std::path::{Path, PathBuf};

use orion::definitions::Document;

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

fn repo_json() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // Not `tests/fixtures/fmt`: those inputs include a BOM'd file on
    // purpose, and the formatter tests own them.
    for dir in ["../../examples", "../../tests/e2e", "../../docs"] {
        out.extend(
            orion::definitions::json_files(&Path::new(MANIFEST).join(dir))
                .unwrap_or_else(|e| panic!("{dir}: {e}")),
        );
    }
    assert!(out.len() > 40, "corpus looks too small: {}", out.len());
    out
}

fn conformance_set() -> Vec<PathBuf> {
    let files = orion::definitions::json_files(&Path::new(MANIFEST).join("tests/fixtures/json"))
        .expect("conformance fixtures");
    assert!(files.len() >= 40, "conformance set is missing files");
    files
}

#[test]
fn every_repository_document_parses_to_the_value_serde_json_sees() {
    for path in repo_json() {
        let text = std::fs::read_to_string(&path).unwrap();
        let ours = Document::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let theirs: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{}: serde_json: {e}", path.display()));
        assert_eq!(ours.to_value(), theirs, "{}", path.display());
    }
}

#[test]
fn the_conformance_set_agrees_with_serde_json_on_every_file() {
    for path in conformance_set() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let ours = Document::parse(&text);
        let theirs = serde_json::from_str::<serde_json::Value>(&text);
        if name.starts_with("y_") {
            assert!(ours.is_ok(), "{name}: {:?}", ours.err());
            assert!(theirs.is_ok(), "{name}: serde_json refuses a y_ case");
            assert_eq!(ours.unwrap().to_value(), theirs.unwrap(), "{name}");
        } else if name == "n_duplicate_key.json" {
            // The one place the front end is stricter, on purpose.
            assert!(ours.is_err(), "{name}");
            assert!(
                theirs.is_ok(),
                "{name}: serde_json is expected to tolerate this"
            );
        } else if name.starts_with("n_") {
            assert!(ours.is_err(), "{name}: accepted");
            assert!(theirs.is_err(), "{name}: serde_json accepts an n_ case");
        } else if name.starts_with("i_") {
            // Accepted here, refused by serde_json, and the formatted form
            // is one serde_json accepts — the repair is the point.
            let doc = ours.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(theirs.is_err(), "{name}: serde_json accepts an i_ case");
            let repaired = orion::definitions::fmt::format_document(&doc);
            assert!(
                serde_json::from_str::<serde_json::Value>(&repaired).is_ok(),
                "{name}"
            );
        } else {
            panic!("{name}: conformance files are named y_*, n_* or i_*");
        }
    }
}

#[test]
fn every_refusal_names_a_line_and_column() {
    for path in conformance_set() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with("n_") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let err = Document::parse(&text).unwrap_err();
        assert!(err.line >= 1 && err.column >= 1, "{name}: {err:?}");
        assert!(
            err.to_string()
                .starts_with(&format!("line {}, column {}: ", err.line, err.column)),
            "{name}: {err}"
        );
    }
}

#[test]
fn spans_address_the_source_by_the_same_paths_findings_use() {
    let path =
        Path::new(MANIFEST).join("../../examples/packages/order-classification/workflow.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let doc = Document::parse(&text).unwrap();
    let span = doc.locate("tasks[1].condition").unwrap();
    let slice = &doc.source[span.start..span.end];
    assert!(
        slice.starts_with('{') && slice.contains("data.order.amount"),
        "{slice}"
    );
    let (line, col) = doc.line_col(span.start);
    assert!(line > 1 && col > 1);
    assert!(doc.locate("tasks[99]").is_none());
}

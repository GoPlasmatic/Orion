//! Drift guard for the v2 admin API's `error.code` vocabulary.
//!
//! `orion_api::V2Code` is the closed registry: every v2 admin refusal is one
//! of its codes, answered with the status its kind names. This module
//! asserts:
//!
//! 1. Every code a lower layer raises under a code of its own — a package
//!    or lock finding, an admission policy's rejection, a build issue — is
//!    registered, so `DeploymentError::passed` keeps it rather than folding
//!    it into its fallback.
//! 2. Every registered code is reachable: the server names its variant, or a
//!    lower layer raises its literal.
//! 3. The errors reference page documents exactly the registered set, each
//!    with the status its kind answers.

use std::collections::{BTreeMap, BTreeSet};

use orion_api::V2Code;

/// The sources of every crate the runtime is built from. Not `orion-api`:
/// it defines the registry, so scanning it would find every code raised.
const SRC_DIRS: &[&str] = &[
    concat!(env!("CARGO_MANIFEST_DIR"), "/src"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/../orion-common/src"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/../orion-storage/src"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/../orion-engine/src"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/../orion-plugin-host/src"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/../orion-runtime/src"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/../orion-authoring/src"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/../orion-http/src"),
];

/// Where [`PRODUCERS`] live.
const PRODUCERS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../orion-runtime/src");
const ERRORS_MD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/src/reference/errors.md"
);

/// The files whose findings reach a v2 refusal under their own code.
const PRODUCERS: &[&str] = &[
    "extensions/schema_profile.rs",
    "extensions/package_identity.rs",
    "extensions/admission.rs",
    "extensions/reconciler.rs",
    "extensions/deployment_store/rules.rs",
    "runtime/reload.rs",
];

/// Upper-case literals in those files that are not v2 refusal codes: an
/// environment variable, and the finding `connector_call` raises on a
/// call's input, which never reaches the admin API.
const NOT_CODES: &[&str] = &["CARGO_MANIFEST_DIR", "CONNECTOR_INVALID_INPUT"];

/// `source` up to its first `#[cfg(test)]`: a test is free to spell any code.
fn production(source: &str) -> &str {
    match source.find("#[cfg(test)]") {
        Some(i) => &source[..i],
        None => source,
    }
}

/// Every `"SHOUTY_CASE"` literal in `source`.
fn code_literals(source: &str) -> BTreeSet<String> {
    source
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|s| {
            s.contains('_')
                && s.starts_with(|c: char| c.is_ascii_uppercase())
                && s.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        })
        .map(str::to_string)
        .collect()
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(PRODUCERS_DIR).join(relative))
        .unwrap_or_else(|e| panic!("read {relative}: {e}"))
}

/// All production source of the runtime's crates, concatenated.
fn all_production_source() -> String {
    fn walk(dir: &std::path::Path, out: &mut String) {
        for entry in std::fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") && !is_test_file(&path) {
                let source = std::fs::read_to_string(&path).expect("read source file");
                out.push_str(production(&source));
            }
        }
    }
    let mut out = String::new();
    for dir in SRC_DIRS {
        walk(std::path::Path::new(dir), &mut out);
    }
    out
}

fn registered() -> BTreeSet<String> {
    V2Code::ALL.iter().map(|c| c.as_str().to_string()).collect()
}

#[test]
fn every_code_a_lower_layer_raises_is_registered() {
    let mut raised = BTreeSet::new();
    for file in PRODUCERS {
        raised.extend(code_literals(production(&read(file))));
    }
    assert!(
        raised.len() > 20,
        "found only {} code literals in the producers — has a file moved?",
        raised.len()
    );
    let registered = registered();
    let unregistered: Vec<_> = raised
        .iter()
        .filter(|c| !NOT_CODES.contains(&c.as_str()) && !registered.contains(*c))
        .collect();
    assert!(
        unregistered.is_empty(),
        "codes a lower layer raises that orion_api::V2Code does not register: \
         {unregistered:?} — register each (and document it in errors.md), or a v2 \
         refusal answers it under its fallback code"
    );
}

#[test]
fn every_registered_code_is_reachable() {
    let source = all_production_source();
    let mut producers = BTreeSet::new();
    for file in PRODUCERS {
        producers.extend(code_literals(production(&read(file))));
    }
    let unreachable: Vec<_> = V2Code::ALL
        .iter()
        .filter(|code| {
            let variant = format!("::{code:?}");
            let named = source.match_indices(&variant).any(|(i, _)| {
                !source[i + variant.len()..].starts_with(|c: char| c.is_alphanumeric())
            });
            !named && !producers.contains(code.as_str())
        })
        .map(|code| code.as_str())
        .collect();
    assert!(
        unreachable.is_empty(),
        "v2 codes registered but never raised by src/: {unreachable:?} — a code \
         no call site produces is one clients would wait forever for"
    );
}

#[test]
fn the_errors_reference_documents_the_registry() {
    let md = std::fs::read_to_string(ERRORS_MD).expect("read docs/src/reference/errors.md");
    let heading = "## The v2 admin code registry";
    let start = md
        .find(heading)
        .expect("errors.md must carry a '## The v2 admin code registry' table");
    let section = &md[start + heading.len()..];
    let end = section.find("\n## ").unwrap_or(section.len());

    let mut documented = BTreeMap::new();
    for line in section[..end].lines() {
        let cells: Vec<&str> = line.trim().split('|').map(str::trim).collect();
        if cells.len() < 4 {
            continue;
        }
        let (Some(code), Ok(status)) = (
            cells[1].strip_prefix('`').and_then(|c| c.strip_suffix('`')),
            cells[2].parse::<u16>(),
        ) else {
            continue;
        };
        documented.insert(code.to_string(), status);
    }
    let expected: BTreeMap<String, u16> = V2Code::ALL
        .iter()
        .map(|c| (c.as_str().to_string(), c.kind().status()))
        .collect();
    assert_eq!(
        documented, expected,
        "docs/src/reference/errors.md's v2 admin code table and orion_api::V2Code \
         disagree (code or status)"
    );
}

/// A test module moved beside the code it tests (`tests.rs`, `foo_tests.rs`)
/// is test code throughout, with no `#[cfg(test)]` of its own to cut at.
fn is_test_file(path: &std::path::Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s == "tests" || s.ends_with("_tests"))
}

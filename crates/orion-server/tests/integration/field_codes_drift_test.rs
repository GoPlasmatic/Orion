//! Drift guard for the `error.details[].code` vocabulary.
//!
//! `FieldError::new` takes its code as `&'static str` precisely because the
//! vocabulary is meant to be closed — but "closed" was an intention with
//! nothing enforcing it. There was no registry, so every call site invented
//! its own literal, and the documentation named two codes (`ENUM_MISMATCH`,
//! `INVALID_FORMAT`) that only ever appeared in tests. A client branching on
//! either would have waited forever.
//!
//! `orion_api::error::field_codes` is now the registry. This module asserts:
//!
//! 1. Every code literal passed to a field-error constructor in `src/` is one
//!    the registry declares.
//! 2. Every registered code is actually reachable — a code no call site emits
//!    is a promise to clients that nothing keeps.
//! 3. The errors reference page documents exactly the registered set.

use std::collections::BTreeSet;

const SRC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
const ERRORS_MD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/src/reference/errors.md"
);

/// Walk `src/`, skipping `#[cfg(test)]` modules, and collect the code literal
/// from every `FieldError::new(path, "CODE", …)` and
/// `OrionError::invalid_field(path, "CODE", …)` call.
///
/// Test modules are excluded deliberately: a test is free to construct a
/// `FieldError` with a nonsense code to assert serialization, and that is not
/// a promise to any client. The exclusion is why `ENUM_MISMATCH` — which
/// appears only in tests — is correctly absent from the emitted set.
fn emitted_codes() -> BTreeSet<String> {
    fn walk(dir: &std::path::Path, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let source = std::fs::read_to_string(&path).expect("read source file");
                // Everything from the first `#[cfg(test)]` on is test code.
                let production = match source.find("#[cfg(test)]") {
                    Some(i) => &source[..i],
                    None => &source[..],
                };
                collect(production, out);
            }
        }
    }

    fn collect(source: &str, out: &mut BTreeSet<String>) {
        for ctor in ["FieldError::new(", "invalid_field("] {
            for (_, rest) in source.match_indices(ctor).map(|(i, _)| (i, &source[i..])) {
                // The code is the second argument: skip to the first comma
                // that is not inside the `path` expression's own parens.
                let Some(after) = rest.split_once(ctor) else {
                    continue;
                };
                let mut depth = 0i32;
                let mut second_arg = None;
                for (i, c) in after.1.char_indices() {
                    match c {
                        '(' | '[' => depth += 1,
                        ')' | ']' if depth == 0 => break,
                        ')' | ']' => depth -= 1,
                        ',' if depth == 0 => {
                            second_arg = Some(&after.1[i + 1..]);
                            break;
                        }
                        _ => {}
                    }
                }
                // Only a literal counts; a constant reference is already safe
                // by construction because the registry is where it comes from.
                if let Some(arg) = second_arg
                    && let Some(open) = arg.find('"')
                    && arg[..open].trim().is_empty()
                    && let Some(close) = arg[open + 1..].find('"')
                {
                    out.insert(arg[open + 1..open + 1 + close].to_string());
                }
            }
        }
    }

    let mut out = BTreeSet::new();
    walk(std::path::Path::new(SRC_DIR), &mut out);
    out
}

fn registered_codes() -> BTreeSet<String> {
    orion_api::error::field_codes::ALL
        .iter()
        .map(|c| (*c).to_string())
        .collect()
}

#[test]
fn field_code_literals_are_all_registered() {
    let emitted = emitted_codes();
    assert!(
        !emitted.is_empty(),
        "parsed no field-error codes at all — the constructor spelling probably changed"
    );

    let registered = registered_codes();
    let unregistered: Vec<_> = emitted.difference(&registered).collect();
    assert!(
        unregistered.is_empty(),
        "field-error codes emitted by src/ but absent from \
         orion_api::error::field_codes: {unregistered:?} — add them to the registry \
         (and to docs/src/reference/errors.md) or reuse an existing code"
    );
}

#[test]
fn every_registered_field_code_is_reachable() {
    let unreachable: Vec<_> = registered_codes()
        .difference(&emitted_codes())
        .cloned()
        .collect();
    assert!(
        unreachable.is_empty(),
        "field codes registered but never emitted by src/: {unreachable:?} — a code \
         no call site produces is a vocabulary entry clients would wait forever for"
    );
}

#[test]
fn the_errors_reference_documents_the_registered_vocabulary() {
    let md = std::fs::read_to_string(ERRORS_MD).expect("read docs/src/reference/errors.md");
    let heading = "### Field error codes";
    let start = md
        .find(heading)
        .expect("errors.md must carry a '### Field error codes' table");
    let section = &md[start + heading.len()..];
    let end = section.find("\n## ").unwrap_or(section.len());

    let mut documented = BTreeSet::new();
    for line in section[..end].lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        if let Some(cell) = line.split('|').nth(1) {
            for token in cell.split('`').skip(1).step_by(2) {
                let token = token.trim();
                if token.chars().all(|c| c.is_ascii_uppercase() || c == '_') && !token.is_empty() {
                    documented.insert(token.to_string());
                }
            }
        }
    }

    assert_eq!(
        documented,
        registered_codes(),
        "docs/src/reference/errors.md's field-code table and \
         orion_api::error::field_codes disagree"
    );
}

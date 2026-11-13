//! Tracker IDs stay out of what users read.
//!
//! Audit and spec IDs (`R12`, `K3`, `V2-07.4`, `TD-31`) mean something only
//! with the private tracker beside them. In a source comment they are a
//! record a maintainer can follow with `git log --grep`; in the OpenAPI
//! document or a CLI `--help` they are noise a user cannot resolve. The
//! OpenAPI document is generated from the server's doc comments
//! (`openapi_test` keeps the committed copy current), and `--help` from the
//! CLI's, so both are checked here.

use std::path::{Path, PathBuf};

/// The first tracker-ID token in `text`, if any: `(` then an optional
/// backtick, one to four capitals, an optional `-`, digits (optionally
/// `-digits` after them), an optional `.digits` or trailing lowercase
/// letter, then `)`, `,` or a backtick.
fn tracker_id(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for (start, _) in text.match_indices('(') {
        let mut i = start + 1;
        if bytes.get(i) == Some(&b'`') {
            i += 1;
        }
        let caps = bytes[i..]
            .iter()
            .take_while(|b| b.is_ascii_uppercase())
            .count();
        if !(1..=4).contains(&caps) {
            continue;
        }
        i += caps;
        if bytes.get(i) == Some(&b'-') {
            i += 1;
        }
        let digits = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            continue;
        }
        i += digits;
        // `V2-07`: a second number after a dash.
        if bytes.get(i) == Some(&b'-') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            i += 1 + bytes[i + 1..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
        }
        if bytes.get(i) == Some(&b'.') {
            i += 1 + bytes[i + 1..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
        } else if bytes.get(i).is_some_and(u8::is_ascii_lowercase) {
            i += 1;
        }
        if matches!(bytes.get(i), Some(b')' | b',' | b'`')) {
            return Some(text[start..=i.min(text.len() - 1)].to_string());
        }
    }
    None
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn public_sdk_and_api_docs_name_no_tracker_ids() {
    let mut found = Vec::new();
    let mut stack = vec![
        workspace().join("crates/orion-api/src"),
        workspace().join("crates/orion-client/src"),
        workspace().join("crates/orion-definitions/src"),
        workspace().join("crates/orion-extension-api/src"),
        workspace().join("crates/orion-plugin-sdk/src"),
    ];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (n, line) in text.lines().enumerate() {
                let t = line.trim_start();
                if (t.starts_with("///") || t.starts_with("//!"))
                    && let Some(id) = tracker_id(t)
                {
                    found.push(format!("{}:{}: {id}", path.display(), n + 1));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "public SDK/API rustdoc carries tracker IDs: {found:?}"
    );
}

#[test]
fn library_rustdoc_names_no_v2_tracker_ids() {
    let mut found = Vec::new();
    let mut stack = vec![
        workspace().join("crates/orion-common/src"),
        workspace().join("crates/orion-engine/src"),
        workspace().join("crates/orion-http/src"),
        workspace().join("crates/orion-runtime/src"),
        workspace().join("crates/orion-storage/src"),
    ];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (n, line) in text.lines().enumerate() {
                let t = line.trim_start();
                if !(t.starts_with("///") || t.starts_with("//!")) {
                    continue;
                }
                for prefix in ["V2-", "TD-", "MOD-", "DEC-"] {
                    if let Some(pos) = t.find(prefix)
                        && t.as_bytes()
                            .get(pos + prefix.len())
                            .is_some_and(u8::is_ascii_digit)
                    {
                        found.push(format!("{}:{}: {prefix}*", path.display(), n + 1));
                    }
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "library rustdoc carries v2 tracker IDs: {found:?}"
    );
}

#[test]
fn the_matcher_finds_the_shapes_it_is_for() {
    for hit in [
        "x (R12) y",
        "x (`V2-07.4`, `TD-31`)",
        "x (F22e).",
        "(O10), y",
    ] {
        assert!(tracker_id(hit).is_some(), "{hit}");
    }
    for miss in [
        "(SSRF protection)",
        "(500)",
        "(POST, PATCH)",
        "`HTTP/2`",
        "(v2)",
    ] {
        assert!(tracker_id(miss).is_none(), "{miss}");
    }
}

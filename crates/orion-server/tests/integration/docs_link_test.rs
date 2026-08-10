//! Drift guard for intra-book links in `docs/src` (C33).
//!
//! `mdbook build` (with `create-missing = false`) catches a SUMMARY entry
//! pointing at a missing file — and nothing else. A relative link or anchor
//! inside a page can rot silently: the 1.0 audit verified all ~250 by hand
//! and found them sound, but had to note that nothing keeps them that way.
//! This is that keeper, in the same spirit as `config_docs_drift_test`:
//! walk every page, resolve every relative link against the filesystem, and
//! check every fragment against the target's heading ids using mdBook's
//! normalisation rules.
//!
//! Deliberately scoped to what mdBook itself defines: relative links and
//! `#fragments` within `docs/src`. External URLs are not fetched (network in
//! a unit suite is flake, and the llms.txt URL sweep is an audit-time job).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const DOCS_SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/src");

fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read docs dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            markdown_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
}

/// The body with fenced code blocks blanked out, so a `](...)` inside a code
/// sample is not read as a link.
fn without_code_fences(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut in_fence = false;
    for line in src.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// mdBook's `normalize_id`: alphanumerics, `_` and `-` survive (lowercased),
/// whitespace becomes `-`, everything else is dropped.
fn normalize_id(text: &str) -> String {
    text.chars()
        .filter_map(|ch| {
            if ch.is_alphanumeric() || ch == '_' || ch == '-' {
                Some(ch.to_ascii_lowercase())
            } else if ch.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

/// Heading ids for one page: markdown formatting stripped the way mdBook
/// strips it (inline code markers and link syntax reduced to their text),
/// plus explicit `{#custom-id}` attributes. Duplicate headings get `-1`,
/// `-2`… suffixes, exactly as mdBook disambiguates them.
fn heading_ids(src: &str) -> BTreeSet<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = BTreeSet::new();
    for line in without_code_fences(src).lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            continue;
        }
        let text = trimmed.trim_start_matches('#');
        if !text.starts_with(' ') && !text.is_empty() {
            continue; // "#!/bin/sh"-style false positive
        }
        let text = text.trim();
        if text.ends_with('}')
            && let Some(open) = text.rfind("{#")
        {
            // mdBook emits ONLY the custom id for such a heading — also
            // registering the auto slug of the heading text would let a link
            // to an id the book never renders pass this guard.
            out.insert(text[open + 2..text.len() - 1].to_string());
            continue;
        }
        // Strip inline code markers and reduce [text](url) to text. Anchor
        // on "](" and find the '[' *before* it — locating each end
        // independently builds an inverted (panicking) slice range when a
        // heading contains "](" ahead of any '['.
        let mut stripped = text.replace('`', "");
        while let Some(mid) = stripped.find("](") {
            let Some(open) = stripped[..mid].rfind('[') else {
                break;
            };
            let Some(close_rel) = stripped[mid..].find(')') else {
                break;
            };
            let inner = stripped[open + 1..mid].to_string();
            stripped.replace_range(open..mid + close_rel + 1, &inner);
        }
        let base = normalize_id(stripped.trim());
        let n = counts.entry(base.clone()).or_insert(0);
        out.insert(if *n == 0 {
            base.clone()
        } else {
            format!("{base}-{n}")
        });
        *n += 1;
    }
    out
}

/// Every `](target)` in the page, code fences excluded.
fn link_targets(src: &str) -> Vec<String> {
    let body = without_code_fences(src);
    let mut out = Vec::new();
    for (i, _) in body.match_indices("](") {
        if let Some(close) = body[i + 2..].find(')') {
            out.push(body[i + 2..i + 2 + close].to_string());
        }
    }
    out
}

#[test]
fn every_relative_link_and_anchor_in_the_book_resolves() {
    let root = Path::new(DOCS_SRC);
    let mut files = Vec::new();
    markdown_files(root, &mut files);
    assert!(files.len() >= 25, "docs/src walk looks broken: {files:?}");
    // Canonicalized so the map lookups below hit: link targets resolve via
    // `canonicalize()`, which also expands symlinks in the checkout's own
    // path (macOS /tmp, a symlinked workspace) — raw walk paths would then
    // never match, silently disabling every anchor check.
    let files: Vec<PathBuf> = files
        .iter()
        .map(|f| f.canonicalize().expect("canonicalize docs page path"))
        .collect();

    let ids_by_file: BTreeMap<PathBuf, BTreeSet<String>> = files
        .iter()
        .map(|f| {
            let src = std::fs::read_to_string(f).expect("read page");
            (f.clone(), heading_ids(&src))
        })
        .collect();

    let mut broken = Vec::new();
    for file in &files {
        let src = std::fs::read_to_string(file).expect("read page");
        for target in link_targets(&src) {
            let target = target.trim();
            // External, protocol-relative, and in-page-only concerns of
            // other checkers; empty targets are reference-style noise.
            if target.is_empty()
                || target.starts_with("http://")
                || target.starts_with("https://")
                || target.starts_with("mailto:")
            {
                continue;
            }
            let (path_part, fragment) = match target.split_once('#') {
                Some((p, f)) => (p, Some(f)),
                None => (target, None),
            };
            let resolved = if path_part.is_empty() {
                file.clone()
            } else {
                let joined = file.parent().expect("page has a parent").join(path_part);
                match joined.canonicalize() {
                    Ok(p) => p,
                    Err(_) => {
                        broken.push(format!("{}: `{target}` (missing file)", file.display()));
                        continue;
                    }
                }
            };
            let Some(fragment) = fragment else { continue };
            // Anchors only exist in markdown pages this test indexed.
            let Some(ids) = ids_by_file.get(&resolved) else {
                continue;
            };
            if !ids.contains(fragment) {
                broken.push(format!(
                    "{}: `{target}` (no heading with id `{fragment}` in {})",
                    file.display(),
                    resolved.display()
                ));
            }
        }
    }
    assert!(
        broken.is_empty(),
        "broken intra-book links:\n  {}",
        broken.join("\n  ")
    );
}

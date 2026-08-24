//! Documentation drift guard for the task-function surface.
//!
//! `reference/functions.md` opens by stating how many functions Orion ships —
//! "**18 functions** … Eight are contributed by the dataflow-rs engine; ten are
//! Orion handlers". The docs 2.0 proposal called numbers like that out by name
//! (style rule 18: "no hand-maintained magic numbers … cite the generated
//! source or omit the number"), because the estate had already shipped a
//! 16-vs-18 disagreement between two pages.
//!
//! `docs/lint.sh` check 5 stops the number being *restated* elsewhere. It
//! cannot tell whether the number is *right*. This module can, and it checks
//! the thing that actually rots first: the names.
//!
//! `engine::functions::schema::REGISTRY` is authoritative for the ten functions
//! Orion implements itself — it is what `GET /api/v1/admin/functions` serves
//! and what workflow-create input validation runs against. The other eight come
//! from dataflow-rs and have no registry entry (they get no input-schema
//! check), so the page's own summary table is the only place both sets are
//! enumerated together. This module parses that table and asserts:
//!
//! 1. Every registry function has a row.
//! 2. No non-`Data` row invents a function the registry does not carry — which
//!    is what a renamed or deleted handler looks like.
//! 3. The stated counts agree with the table, and the handler count agrees with
//!    the registry. This is what makes "18" a checked number rather than a
//!    typed one.
//! 4. Every row has a section on the page documenting it.

use std::collections::BTreeSet;

use orion::engine::functions::schema::{catalogue, registry};

const FUNCTIONS_MD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/src/reference/functions.md"
);

/// One row of the page's summary table.
struct DocRow {
    name: String,
    /// `Data`, `Connector` or `Composition`.
    category: String,
}

fn doc() -> String {
    std::fs::read_to_string(FUNCTIONS_MD).expect("read reference/functions.md")
}

/// The summary table's rows, in page order.
///
/// The page carries several other tables (the Required legend, every
/// per-function field table), so this locks onto the one header that starts the
/// summary table and stops at the first line that is not a table row. Matching
/// rows by shape alone would sweep the field tables in too.
fn documented_functions(doc: &str) -> Vec<DocRow> {
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in doc.lines() {
        let cells = row_cells(line);
        if !in_table {
            in_table = cells.first() == Some(&"Function") && cells.get(1) == Some(&"Category");
            continue;
        }
        if cells.is_empty() {
            break; // the table ended
        }
        // The `|----|----|` separator under the header.
        if cells
            .iter()
            .all(|c| c.chars().all(|ch| ch == '-' || ch == ':'))
        {
            continue;
        }
        let Some(name) = cells.first().and_then(|c| backticked(c).into_iter().next()) else {
            continue;
        };
        let Some(category) = cells.get(1) else {
            continue;
        };
        rows.push(DocRow {
            name,
            category: category.to_string(),
        });
    }
    rows
}

/// A markdown table row split into trimmed cells, or empty for a non-row.
fn row_cells(line: &str) -> Vec<&str> {
    let line = line.trim();
    if !line.starts_with('|') {
        return Vec::new();
    }
    line.trim_matches('|').split('|').map(str::trim).collect()
}

/// The backticked spans in one table cell.
fn backticked(cell: &str) -> Vec<String> {
    cell.split('`')
        .skip(1)
        .step_by(2)
        .map(|s| s.to_string())
        .collect()
}

/// The number a spelled-out word denotes, for the counts the page writes as
/// words rather than digits.
fn number_word(n: usize) -> String {
    const WORDS: [&str; 21] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
        "twenty",
    ];
    WORDS
        .get(n)
        .map(|w| w.to_string())
        .unwrap_or_else(|| n.to_string())
}

/// The word immediately preceding `phrase` in the document, lowercased.
///
/// Used to read the counts the page spells out ("Eight **are contributed by**
/// …") without pinning the whole sentence, so a reword that keeps the claim
/// does not fail the test.
fn word_before(doc: &str, phrase: &str) -> String {
    let at = doc
        .find(phrase)
        .unwrap_or_else(|| panic!("reference/functions.md no longer contains {phrase:?}"));
    doc[..at]
        .split_whitespace()
        .next_back()
        .unwrap_or_default()
        .to_lowercase()
}

/// Functions the registry carries — the ones Orion implements and input-schema
/// validates.
fn registry_names() -> BTreeSet<String> {
    registry().iter().map(|s| s.name.to_string()).collect()
}

/// Every function a workflow may name — what `GET /admin/functions` serves.
///
/// Aliases are excluded: the summary table gives `validation` one row and
/// names `validate` in its heading, which is the right way round for a reader.
fn catalogue_names() -> BTreeSet<String> {
    catalogue()
        .into_iter()
        .map(|e| e.name.to_string())
        .collect()
}

/// Table rows that are not dataflow-rs contributions, so should correspond one
/// for one with the registry.
fn documented_orion_names(rows: &[DocRow]) -> BTreeSet<String> {
    rows.iter()
        .filter(|r| r.category != "Data")
        .map(|r| r.name.clone())
        .collect()
}

#[test]
fn the_summary_table_parses() {
    // A parser that silently yields nothing would satisfy every set comparison
    // below vacuously, so assert the shape before trusting it.
    let rows = documented_functions(&doc());
    assert!(
        rows.len() > 10,
        "parsed only {} rows from reference/functions.md's summary table — the \
         table's shape changed and the parser lost it",
        rows.len()
    );
    let categories: BTreeSet<&str> = rows.iter().map(|r| r.category.as_str()).collect();
    assert_eq!(
        categories,
        BTreeSet::from(["Composition", "Connector", "Data", "Utility"]),
        "unexpected Category values in the summary table"
    );
}

/// The summary table and the catalogue name the same functions.
///
/// This is the check the registry-based one could not make. `registry_names()`
/// covers 18 of the 27, so the eight dataflow-rs rows in the table were
/// asserted by nothing: `every_registry_function_is_documented` never looked at
/// them, and `no_documented_function_is_unknown` deliberately skipped every
/// `Data` row. A renamed or removed built-in would have sat in the page
/// indefinitely. Now the endpoint serves all 27, the page can be held to all 27.
#[test]
fn the_summary_table_matches_the_catalogue() {
    let rows = documented_functions(&doc());
    let documented: BTreeSet<String> = rows.iter().map(|r| r.name.clone()).collect();
    let catalogued = catalogue_names();

    let undocumented: Vec<&String> = catalogued.difference(&documented).collect();
    assert!(
        undocumented.is_empty(),
        "served by GET /admin/functions but absent from the summary table in \
         docs/src/reference/functions.md: {undocumented:?}"
    );
    let invented: Vec<&String> = documented.difference(&catalogued).collect();
    assert!(
        invented.is_empty(),
        "in the summary table but not served by GET /admin/functions — renamed \
         or removed?: {invented:?}"
    );
}

/// An engine built-in is served without an input schema, and an Orion handler
/// with one. A consumer branches on exactly that.
#[test]
fn the_catalogue_marks_which_entries_carry_a_schema() {
    use orion::engine::functions::schema::Source;
    for entry in catalogue() {
        match entry.source {
            Source::Orion => assert!(
                entry.input_fields.is_some(),
                "'{}' is an Orion handler and must serve its input schema",
                entry.name
            ),
            Source::Engine => assert!(
                entry.input_fields.is_none(),
                "'{}' is an engine built-in — Orion declares no schema for it, \
                 so serving one would claim a create-time check that does not run",
                entry.name
            ),
        }
    }
    // The alias is expressed on its function, not as a second entry.
    let names: BTreeSet<String> = catalogue()
        .into_iter()
        .map(|e| e.name.to_string())
        .collect();
    assert!(
        !names.contains("validate"),
        "an alias must not be its own entry"
    );
    assert!(
        catalogue()
            .iter()
            .any(|e| e.name == "validation" && e.aliases.contains(&"validate")),
        "'validation' must carry 'validate' as an alias"
    );
}

/// The catalogue is the accepted vocabulary, so it must agree with the gate
/// that accepts it — every served name loads, and every loadable name is
/// served (modulo the alias, which is served on its function).
#[test]
fn the_catalogue_matches_what_a_workflow_may_actually_name() {
    let mut served: BTreeSet<String> = BTreeSet::new();
    for entry in catalogue() {
        served.insert(entry.name.to_string());
        served.extend(entry.aliases.iter().map(|a| a.to_string()));
    }
    let accepted: BTreeSet<String> = orion::engine::known_functions()
        .map(|n| n.to_string())
        .collect();
    assert_eq!(
        served, accepted,
        "the catalogue and known_functions() disagree — one of them is lying to \
         an author about what they may write"
    );
}

#[test]
fn every_registry_function_is_documented() {
    let rows = documented_functions(&doc());
    let documented: BTreeSet<String> = rows.iter().map(|r| r.name.clone()).collect();
    let missing: Vec<String> = registry_names()
        .into_iter()
        .filter(|n| !documented.contains(n))
        .collect();
    assert!(
        missing.is_empty(),
        "these functions are in the schema registry but absent from \
         docs/src/reference/functions.md: {missing:?}"
    );
}

#[test]
fn no_documented_orion_function_is_a_ghost() {
    let rows = documented_functions(&doc());
    let registry = registry_names();
    let ghosts: Vec<String> = documented_orion_names(&rows)
        .into_iter()
        .filter(|n| !registry.contains(n))
        .collect();
    assert!(
        ghosts.is_empty(),
        "docs/src/reference/functions.md documents Connector/Composition \
         functions the schema registry does not carry (a rename with a missed \
         doc update looks exactly like this): {ghosts:?}"
    );
}

#[test]
fn the_stated_counts_match_the_table() {
    let doc = doc();
    let rows = documented_functions(&doc);
    let orion = documented_orion_names(&rows).len();
    let dataflow = rows.len() - orion;

    assert!(
        doc.contains(&format!("**{} functions**", rows.len())),
        "reference/functions.md states a total that its own summary table \
         contradicts: the table has {} rows",
        rows.len()
    );
    assert_eq!(
        word_before(&doc, "are contributed by"),
        number_word(dataflow),
        "the stated dataflow-rs function count disagrees with the summary table"
    );
    assert_eq!(
        word_before(&doc, "are Orion handlers"),
        number_word(orion),
        "the stated Orion handler count disagrees with the summary table"
    );
    assert_eq!(
        orion,
        registry().len(),
        "the summary table's Connector/Composition rows and the schema registry \
         are different sizes"
    );
}

#[test]
fn every_documented_function_has_a_section() {
    let doc = doc();
    let headings: Vec<&str> = doc
        .lines()
        .filter(|l| l.starts_with("### "))
        .collect::<Vec<_>>();
    let missing: Vec<String> = documented_functions(&doc)
        .iter()
        .map(|r| r.name.clone())
        .filter(|name| {
            // `validation` heads a shared section: `### `validation` / `validate``.
            let backticked = format!("`{name}`");
            !headings.iter().any(|h| h.contains(&backticked))
        })
        .collect();
    assert!(
        missing.is_empty(),
        "these functions have a summary-table row but no section on \
         docs/src/reference/functions.md: {missing:?}"
    );
}

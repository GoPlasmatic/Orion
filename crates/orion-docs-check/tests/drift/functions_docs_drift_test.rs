//! The code-side half of the task-function drift guard.
//!
//! `FunctionRegistry::builtin()` is authoritative — it is what
//! `GET /api/v2/admin/functions` serves. Its `orion` entries are the functions
//! Orion implements and input-validates; its `engine` entries are the
//! dataflow-rs built-ins, which get no input-schema check. This module holds
//! the catalogue to itself: every entry carries a schema exactly when Orion
//! validates its input, and the catalogue names exactly what a workflow may
//! load. The half that reads the function reference pages lives in
//! GoPlasmatic/Orion-docs (`functions_docs_drift_test`).

use std::collections::BTreeSet;

use orion_engine::functions::schema::Source;
use orion_engine::{CatalogueEntry, FunctionRegistry};

/// The built-in catalogue — what a binary with no plugins loaded serves. A
/// plugin entry, when a generation carries one, is filtered on every side of
/// these comparisons: the reference page documents what ships, not what an
/// operator installed.
fn catalogue() -> Vec<CatalogueEntry> {
    FunctionRegistry::builtin()
        .catalogue()
        .into_iter()
        .filter(|e| e.source != Source::Plugin)
        .collect()
}

/// An engine built-in is served without an input schema, and an Orion handler
/// with one. A consumer branches on exactly that.
#[test]
fn the_catalogue_marks_which_entries_carry_a_schema() {
    use orion_engine::functions::schema::Source;
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
            Source::Plugin => panic!(
                "'{}' is a plugin entry in the built-in catalogue",
                entry.name
            ),
        }
    }
    // `validate`, 1.12's spelling of `validation`, is not served.
    let names: BTreeSet<String> = catalogue()
        .into_iter()
        .map(|e| e.name.to_string())
        .collect();
    assert!(
        !names.contains("validate"),
        "1.12's spelling of `validation` names nothing"
    );
}

/// The catalogue is the accepted vocabulary, so it must agree with the gate
/// that accepts it — every served name loads, and every loadable name is
/// served.
#[test]
fn the_catalogue_matches_what_a_workflow_may_actually_name() {
    let mut served: BTreeSet<String> = BTreeSet::new();
    for entry in catalogue() {
        served.insert(entry.name.to_string());
    }
    let accepted: BTreeSet<String> = FunctionRegistry::builtin()
        .names()
        .map(|n| n.to_string())
        .collect();
    assert_eq!(
        served, accepted,
        "the catalogue and known_functions() disagree — one of them is lying to \
         an author about what they may write"
    );
}

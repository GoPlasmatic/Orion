//! Drift guard for the CI `#[ignore]` name filters.
//!
//! Container-gated tests are `#[ignore]`d so the default suite stays
//! Docker-free, which means the *only* place they ever execute is the
//! `integration-containers` job. That job selects them with substring filters
//! on the `libtest` name — `cargo test --test integration -- --ignored
//! data_parity_test postgres_test …` — so a module whose name is missing from
//! those lines runs nowhere at all. It does not fail; it is silently never
//! executed, and `tests/README.md` already has to warn about this in prose.
//!
//! This test replaces the warning with an assertion, in the same spirit as
//! `config_docs_drift_test`: parse the workflow, parse the test tree, and
//! refuse to let the two disagree.
//!
//! Only the `integration` binary needs this. The `cluster`, `storage_postgres`,
//! `storage_mysql` and `schema_parity` binaries get their own steps that run
//! `--ignored` with no name filter, so every ignored test in them is selected
//! wholesale.

use std::collections::BTreeSet;
use std::path::Path;

const WORKFLOW: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.github/workflows/ci.yml");
const INTEGRATION_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/integration");

/// Module names (file stems) under `tests/integration/` that declare at least
/// one `#[ignore]` test, and so depend on a CI filter to ever run.
fn modules_with_ignored_tests() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let dir = std::fs::read_dir(Path::new(INTEGRATION_DIR)).expect("read tests/integration");
    for entry in dir {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem == "main" {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read test module");
        // Only an attribute in attribute position counts: `#[ignore]` and
        // `#[ignore = "reason"]` at the start of a line. Substring-matching
        // the whole file instead would count every prose mention — the doc
        // comments in this very file, and `common/mod.rs`'s "(for #[ignore]
        // tests)" — as container-gated modules.
        if src.lines().any(|l| l.trim_start().starts_with("#[ignore")) {
            out.insert(stem.to_string());
        }
    }
    out
}

/// The union of the name filters on every `cargo test --test integration --
/// --ignored …` line in the workflow.
fn ci_integration_filters() -> BTreeSet<String> {
    let workflow = std::fs::read_to_string(Path::new(WORKFLOW)).expect("read ci.yml");
    let mut out = BTreeSet::new();
    for line in workflow.lines() {
        let line = line.trim();
        if !line.contains("--test integration") || !line.contains("--ignored") {
            continue;
        }
        let (_, filters) = line.split_once("--ignored").expect("checked above");
        out.extend(
            filters
                .split_whitespace()
                // Trailing flags such as `--test-threads=2` are not filters.
                .filter(|tok| !tok.starts_with('-'))
                .map(str::to_string),
        );
    }
    assert!(
        !out.is_empty(),
        "found no `cargo test --test integration -- --ignored …` line in ci.yml; if the \
         container job was restructured, this drift guard needs to be taught the new shape"
    );
    out
}

/// A module carrying `#[ignore]` tests but named by no CI filter is dead
/// weight: it compiles, it is skipped locally, and CI never selects it.
#[test]
fn every_container_gated_module_is_named_by_a_ci_filter() {
    let modules = modules_with_ignored_tests();
    let filters = ci_integration_filters();

    // Filters are substring matches on `module::test`, so a filter selects a
    // module when it is a substring of the module name (an exact name is the
    // normal case; a shared prefix would select several).
    let unreachable: Vec<&String> = modules
        .iter()
        .filter(|m| !filters.iter().any(|f| m.contains(f.as_str())))
        .collect();

    assert!(
        unreachable.is_empty(),
        "these modules declare #[ignore] tests that no CI filter selects, so they run \
         nowhere — not locally (ignored) and not in CI (unmatched): {unreachable:?}\n\
         Add each module name to a `cargo test --test integration -- --ignored …` line in \
         .github/workflows/ci.yml."
    );
}

/// The mirror image: a filter naming a module that no longer exists selects
/// nothing, and `libtest` exits 0 on an unmatched filter rather than
/// complaining. Left alone, the CI line reads as coverage it is not providing.
#[test]
fn every_ci_filter_still_matches_a_module() {
    let modules = modules_with_ignored_tests();
    let filters = ci_integration_filters();

    let stale: Vec<&String> = filters
        .iter()
        .filter(|f| !modules.iter().any(|m| m.contains(f.as_str())))
        .collect();

    assert!(
        stale.is_empty(),
        "these CI filters match no module with #[ignore] tests, so the step silently runs \
         nothing for them: {stale:?}\n\
         Either the module was renamed or deleted, or its #[ignore] tests were removed — \
         update .github/workflows/ci.yml."
    );
}

/// T40: the two guards above cover only the `integration` binary — the four
/// other binaries are hand-listed in ci.yml, and a NEW top-level test binary
/// carrying `#[ignore]` tests would get no CI step and no failure: it
/// compiles, is skipped locally, and never runs anywhere. This closes that
/// door the same way: enumerate the binaries from the filesystem, and refuse
/// to let one with ignored tests go unnamed by a `cargo test --test <name>`
/// line.
#[test]
fn every_test_binary_with_ignored_tests_has_a_ci_step() {
    let tests_dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests"));
    let workflow = std::fs::read_to_string(Path::new(WORKFLOW)).expect("read ci.yml");

    // Auto-discovered `tests/*.rs` binaries, plus explicit [[test]] targets
    // with a directory (integration is covered by the guards above; cluster
    // is the other one).
    let mut binaries: Vec<(String, Vec<std::path::PathBuf>)> = Vec::new();
    for entry in std::fs::read_dir(tests_dir).expect("read tests/") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            binaries.push((stem.to_string(), vec![path.clone()]));
        }
    }
    let cluster_sources: Vec<std::path::PathBuf> = std::fs::read_dir(tests_dir.join("cluster"))
        .expect("read tests/cluster")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"))
        .collect();
    binaries.push(("cluster".to_string(), cluster_sources));

    // The exact tokens following `--test` in ci.yml — not a raw substring
    // match, which would count `tests/metrics.rs` as covered because
    // `--test metrics_exposition` happens to start with `--test metrics`.
    let tokens: Vec<&str> = workflow.split_whitespace().collect();
    let tested: BTreeSet<&str> = tokens
        .windows(2)
        .filter(|pair| pair[0] == "--test")
        .map(|pair| pair[1])
        .collect();

    let unnamed: Vec<&String> = binaries
        .iter()
        .filter(|(_, sources)| {
            sources.iter().any(|path| {
                std::fs::read_to_string(path)
                    .expect("read test source")
                    .lines()
                    .any(|l| l.trim_start().starts_with("#[ignore"))
            })
        })
        .filter(|(name, _)| !tested.contains(name.as_str()))
        .map(|(name, _)| name)
        .collect();

    assert!(
        unnamed.is_empty(),
        "these test binaries declare #[ignore] tests but no `cargo test --test <name>` \
         line in ci.yml runs them — their ignored half executes nowhere: {unnamed:?}"
    );
}

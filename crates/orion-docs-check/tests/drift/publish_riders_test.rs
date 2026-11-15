//! M-17: the publish rider list is hand-written in `crates-publish.yml`, and
//! nothing checked it against the workspace. A new library crate, or a new
//! dependency edge between two, would break only at release time.
//!
//! This holds the list to `cargo metadata`: every publishable workspace crate
//! is named, and each appears after everything it depends on.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// The crates named in the rider step.
fn rider_list() -> Vec<String> {
    let workflow =
        std::fs::read_to_string(workspace_root().join(".github/workflows/crates-publish.yml"))
            .expect("crates-publish.yml");
    let line = workflow
        .lines()
        .find(|line| line.contains("run: .github/scripts/publish-crates.sh "))
        .expect("the rider publish step");
    line.split("publish-crates.sh ")
        .nth(1)
        .expect("arguments after the script")
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// `(name, workspace dependencies, publishable)` for every workspace member.
fn workspace_packages() -> BTreeMap<String, (BTreeSet<String>, bool)> {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(workspace_root())
        .output()
        .expect("cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata JSON");
    let members: BTreeSet<&str> = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|p| p["name"].as_str().expect("name"))
        .collect();

    metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|p| {
            let name = p["name"].as_str().expect("name").to_string();
            let deps: BTreeSet<String> = p["dependencies"]
                .as_array()
                .expect("dependencies")
                .iter()
                .filter_map(|d| d["name"].as_str())
                .filter(|d| members.contains(d))
                .map(str::to_string)
                .collect();
            // `publish: []` is the explicit opt-out.
            let publishable = p["publish"]
                .as_array()
                .is_none_or(|registries| !registries.is_empty());
            (name, (deps, publishable))
        })
        .collect()
}

#[test]
fn every_publishable_workspace_crate_is_a_rider_in_dependency_order() {
    let riders = rider_list();
    let packages = workspace_packages();

    // The binary crates are not riders: orion-server is published separately,
    // and orion-docs-check never is.
    let expected: Vec<&str> = packages
        .iter()
        .filter(|(name, (_, publishable))| {
            *publishable && name.as_str() != "orion-server" && name.as_str() != "orion-docs-check"
        })
        .map(|(name, _)| name.as_str())
        .collect();
    let listed: BTreeSet<&str> = riders.iter().map(String::as_str).collect();
    let expected_set: BTreeSet<&str> = expected.iter().copied().collect();

    assert_eq!(
        listed,
        expected_set,
        "crates-publish.yml's rider list is out of date: missing {:?}, unknown {:?}",
        expected_set.difference(&listed).collect::<Vec<_>>(),
        listed.difference(&expected_set).collect::<Vec<_>>()
    );

    let position: BTreeMap<&str, usize> = riders
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i))
        .collect();
    for rider in &riders {
        let (deps, _) = &packages[rider];
        for dep in deps {
            if let Some(dep_pos) = position.get(dep.as_str()) {
                assert!(
                    *dep_pos < position[rider.as_str()],
                    "'{rider}' is published before its dependency '{dep}'"
                );
            }
        }
    }
}

//! The release stamps that name the workspace version by hand.
//!
//! The Helm chart defaults its image tag to `appVersion`, so a chart left at an
//! old version installs an old image; the README states the current release in
//! prose. Both were left at 1.x after the workspace moved on, and nothing noticed
//! until a reader did. This holds them to `workspace.package.version`.

use std::path::Path;

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(repo_root().join(relative))
        .unwrap_or_else(|e| panic!("read {relative}: {e}"))
}

/// `version = "…"` under `[workspace.package]`.
fn workspace_version() -> String {
    let manifest = read("Cargo.toml");
    let section = manifest
        .split("[workspace.package]")
        .nth(1)
        .expect("a [workspace.package] section");
    section
        .lines()
        .find_map(|line| {
            let line = line.trim();
            line.strip_prefix("version")
                .and_then(|rest| rest.trim_start().strip_prefix('='))
                .map(|v| v.trim().trim_matches('"').to_string())
        })
        .expect("workspace.package.version")
}

fn chart_field(chart: &str, key: &str) -> String {
    chart
        .lines()
        .find_map(|line| {
            line.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(':'))
                .map(|v| v.trim().trim_matches('"').to_string())
        })
        .unwrap_or_else(|| panic!("Chart.yaml has no `{key}:`"))
}

#[test]
fn the_helm_chart_ships_the_workspace_version() {
    let version = workspace_version();
    let chart = read("deploy/helm/orion/Chart.yaml");
    assert_eq!(
        chart_field(&chart, "version"),
        version,
        "Chart.yaml `version` must match workspace.package.version"
    );
    assert_eq!(
        chart_field(&chart, "appVersion"),
        version,
        "Chart.yaml `appVersion` is the default image tag: it must name the release it deploys"
    );
}

#[test]
fn the_readme_names_the_current_release() {
    let version = workspace_version();
    let readme = read("README.md");
    assert!(
        readme.contains(&format!("The current workspace release is **{version}**")),
        "README.md must say `The current workspace release is **{version}**`"
    );
}

//! `orion-extension-api` stays the small crate an edition implements
//! against: it rides the workspace version, and it depends on nothing that
//! would link a runtime.

const MANIFEST: &str = include_str!("../../../orion-extension-api/Cargo.toml");

/// What a trait signature in the crate names. A runtime (tokio, sqlx, axum,
/// reqwest) belongs in orion-server, behind a trait there.
const ALLOWED: &[&str] = &[
    "async-trait",
    "orion-api",
    "serde",
    "serde_json",
    "thiserror",
];

#[test]
fn the_extension_api_shares_the_workspace_version() {
    assert!(
        MANIFEST.contains("version.workspace = true"),
        "orion-extension-api must inherit `workspace.package.version`"
    );
    assert!(
        !MANIFEST.contains("\nversion = \""),
        "orion-extension-api must not carry a version of its own"
    );
}

#[test]
fn the_extension_api_depends_only_on_what_its_signatures_name() {
    let manifest: toml::Table = toml::from_str(MANIFEST).expect("manifest parses");
    let mut deps: Vec<&str> = manifest
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .expect("[dependencies]")
        .keys()
        .map(String::as_str)
        .collect();
    deps.sort_unstable();
    assert_eq!(deps, ALLOWED, "orion-extension-api's dependencies");
    for section in ["dev-dependencies", "build-dependencies", "features"] {
        assert!(
            !manifest.contains_key(section),
            "orion-extension-api declares no [{section}]"
        );
    }
}

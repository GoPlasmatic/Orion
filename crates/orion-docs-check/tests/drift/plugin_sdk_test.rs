//! The guest SDK (`crates/orion-plugin-sdk`) and the host agree on the ABI.
//!
//! The SDK carries its own copy of the WIT file, because a crate on crates.io
//! cannot reach into the server's tree — so the two copies are one contract
//! written twice, and this is what keeps them the same file. The SDK's `ABI`
//! constant and the host's are pinned to each other the same way.
//!
//! Here rather than in the plugin host's own tests (P-29): this is a
//! cross-crate consistency check, the same shape as the other drift guards,
//! and it needs no Wasmtime to run.

const HOST_WIT: &str = include_str!("../../../orion-plugin-host/wit/orion-plugin.wit");
const SDK_WIT: &str = include_str!("../../../orion-plugin-sdk/wit/orion-plugin.wit");
const SDK_LIB: &str = include_str!("../../../orion-plugin-sdk/src/lib.rs");
const SDK_MANIFEST: &str = include_str!("../../../orion-plugin-sdk/Cargo.toml");
const HOST_MANIFEST: &str = include_str!("../../../orion-plugin-host/src/manifest.rs");

/// The host's ABI, read off its declaration.
///
/// Read as text rather than imported: depending on `orion-plugin-host` here
/// would build Wasmtime to check three strings.
fn host_abi() -> &'static str {
    HOST_MANIFEST
        .lines()
        .find_map(|line| line.strip_prefix("pub const ABI: &str = \""))
        .and_then(|rest| rest.strip_suffix("\";"))
        .expect("orion-plugin-host/src/manifest.rs declares `pub const ABI: &str = \"…\";`")
}

#[test]
fn the_sdk_ships_the_hosts_wit_file_byte_for_byte() {
    assert_eq!(
        HOST_WIT, SDK_WIT,
        "crates/orion-plugin-sdk/wit/orion-plugin.wit must be a copy of \
         crates/orion-plugin-host/wit/orion-plugin.wit — copy it over and commit both"
    );
}

#[test]
fn the_sdk_abi_constant_is_the_hosts() {
    let abi = host_abi();
    let expected = format!("pub const ABI: &str = \"{abi}\";");
    assert!(
        SDK_LIB.contains(&expected),
        "the SDK's ABI constant must read `{expected}`"
    );
    assert!(
        HOST_WIT.contains(&format!("package {abi};")),
        "the WIT package version is the ABI"
    );
}

/// The SDK rides the workspace version like the other two library crates: a
/// version of its own would need the hand-maintained bump the workspace
/// version exists to remove (see the root `Cargo.toml`).
#[test]
fn the_sdk_shares_the_workspace_version() {
    assert!(
        SDK_MANIFEST.contains("version.workspace = true"),
        "orion-plugin-sdk must inherit `workspace.package.version`"
    );
    assert!(
        !SDK_MANIFEST.contains("\nversion = \""),
        "orion-plugin-sdk must not carry a version of its own"
    );
}

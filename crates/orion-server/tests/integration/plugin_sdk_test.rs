//! The guest SDK (`crates/orion-plugin-sdk`) and the host agree on the ABI.
//!
//! The SDK carries its own copy of the WIT file, because a crate on crates.io
//! cannot reach into the server's tree — so the two copies are one contract
//! written twice, and this is what keeps them the same file. The SDK's `ABI`
//! constant and the host's are pinned to each other the same way.

const HOST_WIT: &str = include_str!("../../wit/orion-plugin.wit");
const SDK_WIT: &str = include_str!("../../../orion-plugin-sdk/wit/orion-plugin.wit");
const SDK_LIB: &str = include_str!("../../../orion-plugin-sdk/src/lib.rs");
const SDK_MANIFEST: &str = include_str!("../../../orion-plugin-sdk/Cargo.toml");

#[test]
fn the_sdk_ships_the_hosts_wit_file_byte_for_byte() {
    assert_eq!(
        HOST_WIT, SDK_WIT,
        "crates/orion-plugin-sdk/wit/orion-plugin.wit must be a copy of \
         crates/orion-server/wit/orion-plugin.wit — copy it over and commit both"
    );
}

#[test]
fn the_sdk_abi_constant_is_the_hosts() {
    let expected = format!("pub const ABI: &str = \"{}\";", orion::plugin::ABI);
    assert!(
        SDK_LIB.contains(&expected),
        "the SDK's ABI constant must read `{expected}`"
    );
    assert!(
        HOST_WIT.contains(&format!("package {};", orion::plugin::ABI)),
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

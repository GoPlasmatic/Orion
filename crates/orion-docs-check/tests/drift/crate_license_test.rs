//! Every crate that ships carries the workspace's LICENSE and NOTICE.
//!
//! Apache-2.0 §4 asks that a redistribution carry the licence text and the
//! NOTICE file. A crates.io tarball holds only what is under its own crate
//! directory (and, with an `include` list, only what that list names), so
//! each published crate keeps its own copy, byte-identical to the root's,
//! and names both in its `include` when it has one.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn every_shipped_crate_carries_the_workspace_license_and_notice() {
    let root = root();
    let license = std::fs::read(root.join("LICENSE")).expect("root LICENSE");
    let notice = std::fs::read(root.join("NOTICE")).expect("root NOTICE");
    let mut problems = Vec::new();
    for entry in std::fs::read_dir(root.join("crates")).expect("crates/") {
        let dir = entry.expect("entry").path();
        let manifest = dir.join("Cargo.toml");
        let Ok(toml) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        // Never shipped: the drift-guard crate itself.
        if name == "orion-docs-check" {
            continue;
        }
        for (file, want) in [("LICENSE", &license), ("NOTICE", &notice)] {
            match std::fs::read(dir.join(file)) {
                Ok(have) if &have == want => {}
                Ok(_) => problems.push(format!("{name}/{file} differs from the root {file}")),
                Err(_) => problems.push(format!("{name} has no {file}")),
            }
            let include = toml
                .lines()
                .find(|l| l.trim_start().starts_with("include"))
                .is_some();
            if include {
                let start = toml.find("include").unwrap();
                let list = &toml[start..start + toml[start..].find(']').unwrap()];
                if !list.contains(&format!("\"{file}\"")) && !list.contains(&format!("\"/{file}\""))
                {
                    problems.push(format!("{name}'s `include` does not name {file}"));
                }
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

//! V2-07.2: Connector provider authoring workflow (`scaffold`, `lint`, `pack`, `test`).
//!
//! Provides deterministic package scaffolding, manifest + schema profile linting,
//! canonical archive inventory packing, and local conformance smoke testing for
//! `rust`, `workflow`, and `wasm` connector packages.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::contract::ImplementationKind;
use super::package_identity::{
    PackageIdentityFinding, build_inventory, canonical_bytes, verify_inventory,
};
use super::schema_profile::{DIALECT_URI, SchemaProfileFinding, check_manifest_bundle};

/// Scaffolded provider package bundle (`V2-07.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaffoldedPackage {
    pub publisher: String,
    pub provider_id: String,
    pub version: String,
    pub implementation: ImplementationKind,
    pub files: BTreeMap<String, Vec<u8>>,
}

/// Lint report for a provider package (`V2-07.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageLintReport {
    pub valid: bool,
    pub provider_id: String,
    pub version: String,
    pub implementation: String,
    pub errors: Vec<String>,
}

/// Packed provider inventory and canonical package digest (`V2-07.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackedPackageInventory {
    pub package_digest: String,
    pub inventory: Value,
    pub inventory_bytes: Vec<u8>,
}

/// Generate a canonical scaffold for a `rust`, `workflow`, or `wasm` connector package (`V2-07.2`).
pub fn scaffold_provider_package(
    publisher: &str,
    provider_id: &str,
    version: &str,
    implementation: ImplementationKind,
) -> ScaffoldedPackage {
    let mut files = BTreeMap::new();

    let impl_block = match implementation {
        ImplementationKind::Rust => json!({
            "kind": "rust",
            "abi": "orion-rust-provider/1",
            "driver_id": provider_id
        }),
        ImplementationKind::Workflow => json!({
            "kind": "workflow",
            "entries": { "item.get": "entry" },
            "workflows": ["workflows/entry.json"]
        }),
        ImplementationKind::Wasm => json!({
            "kind": "wasm",
            "world": "orion:connector/connector@1.0.0",
            "component": "component.wasm"
        }),
    };

    let manifest = json!({
        "schema_version": 2,
        "kind": "connector-provider",
        "id": provider_id,
        "version": version,
        "contract": "orion:connector@1.0.0",
        "requires_orion": ">=2.0.0, <3.0.0",
        "schema_profile": "orion-json-schema/1",
        "implementation": impl_block,
        "configuration_schema": "schemas/config.public.json",
        "credential_schema": "schemas/config.secret.json",
        "operations": {
            "item.get": {
                "effect": "read",
                "retry": "safe",
                "input_schema": "schemas/item.get.input.json",
                "output_schema": "schemas/item.get.output.json"
            }
        },
        "dependencies": [],
        "capabilities": {},
        "payload_limits": {
            "configuration_bytes": 4096,
            "input_bytes": 4096,
            "output_bytes": 4096,
            "metadata_bytes": 1024
        }
    });

    let distribution = json!({
        "schema_version": 2,
        "publisher": publisher,
        "policy_requirements": []
    });

    let config_public = json!({
        "$schema": DIALECT_URI,
        "type": "object",
        "required": ["endpoint"],
        "additionalProperties": false,
        "properties": {
            "endpoint": { "type": "string", "minLength": 1 }
        }
    });
    let config_secret = json!({
        "$schema": DIALECT_URI,
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "api_key": { "type": "string", "x-orion-sensitive": true, "minLength": 1 }
        }
    });
    let in_schema = json!({
        "$schema": DIALECT_URI,
        "type": "object",
        "required": ["id"],
        "additionalProperties": false,
        "properties": {
            "id": { "type": "string", "minLength": 1 }
        }
    });
    let out_schema = json!({
        "$schema": DIALECT_URI,
        "type": "object",
        "required": ["id"],
        "additionalProperties": false,
        "properties": {
            "id": { "type": "string", "minLength": 1 }
        }
    });

    files.insert("LICENSE".to_string(), b"Apache-2.0\n".to_vec());
    files.insert(
        "connector.json".to_string(),
        serde_json::to_vec(&manifest).expect("scaffold manifest serializes"),
    );
    files.insert(
        "distribution.json".to_string(),
        serde_json::to_vec(&distribution).expect("scaffold distribution serializes"),
    );
    files.insert(
        "schemas/config.public.json".to_string(),
        serde_json::to_vec(&config_public).expect("scaffold public config schema serializes"),
    );
    files.insert(
        "schemas/config.secret.json".to_string(),
        serde_json::to_vec(&config_secret).expect("scaffold secret config schema serializes"),
    );
    files.insert(
        "schemas/item.get.input.json".to_string(),
        serde_json::to_vec(&in_schema).expect("scaffold input schema serializes"),
    );
    files.insert(
        "schemas/item.get.output.json".to_string(),
        serde_json::to_vec(&out_schema).expect("scaffold output schema serializes"),
    );

    match implementation {
        ImplementationKind::Rust => {}
        ImplementationKind::Workflow => {
            let wf = json!({
                "id": "entry",
                "name": "entry",
                "tasks": []
            });
            files.insert(
                "workflows/entry.json".to_string(),
                serde_json::to_vec(&wf).expect("scaffold workflow serializes"),
            );
        }
        ImplementationKind::Wasm => {
            files.insert("component.wasm".to_string(), b"\0asm\x01\0\0\0".to_vec());
        }
    }

    ScaffoldedPackage {
        publisher: publisher.to_string(),
        provider_id: provider_id.to_string(),
        version: version.to_string(),
        implementation,
        files,
    }
}

/// Lint a provider package's manifest and schema files (`V2-07.2`).
pub fn lint_provider_package(files: &BTreeMap<String, Vec<u8>>) -> PackageLintReport {
    let mut errors = Vec::new();
    let Some(manifest_bytes) = files.get("connector.json") else {
        return PackageLintReport {
            valid: false,
            provider_id: String::new(),
            version: String::new(),
            implementation: String::new(),
            errors: vec!["MANIFEST_MISSING: connector.json not found".to_string()],
        };
    };

    let manifest: Value = match serde_json::from_slice(manifest_bytes) {
        Ok(v) => v,
        Err(e) => {
            return PackageLintReport {
                valid: false,
                provider_id: String::new(),
                version: String::new(),
                implementation: String::new(),
                errors: vec![format!("MANIFEST_INVALID_JSON: {e}")],
            };
        }
    };

    let provider_id = manifest["id"].as_str().unwrap_or_default().to_string();
    let version = manifest["version"].as_str().unwrap_or_default().to_string();
    let implementation = manifest["implementation"]["kind"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let file_list: Vec<String> = files
        .keys()
        .filter(|k| *k != "inventory.json")
        .cloned()
        .collect();
    let mut schema_files = BTreeMap::new();
    for (path, bytes) in files {
        if path.starts_with("schemas/") && path.ends_with(".json") {
            match serde_json::from_slice::<Value>(bytes) {
                Ok(doc) => {
                    schema_files.insert(path.clone(), doc);
                }
                Err(e) => {
                    errors.push(format!("SCHEMA_INVALID_JSON ({path}): {e}"));
                }
            }
        }
    }

    if let Err(SchemaProfileFinding { code, location }) =
        check_manifest_bundle(&manifest, &file_list, &schema_files)
    {
        errors.push(format!("{code}: {location}"));
    }

    PackageLintReport {
        valid: errors.is_empty(),
        provider_id,
        version,
        implementation,
        errors,
    }
}

/// Pack and verify a provider package into a deterministic `PackedPackageInventory` (`V2-07.2`).
pub fn pack_provider_package(
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<PackedPackageInventory, PackageIdentityFinding> {
    let mut payloads = files.clone();
    payloads.remove("inventory.json");

    let lint = lint_provider_package(&payloads);
    if !lint.valid {
        return Err(PackageIdentityFinding::new("PACKAGE_LINT_FAILED"));
    }
    let inv = build_inventory(&payloads)?;
    let inv_bytes = canonical_bytes(&inv)?;
    let digest = verify_inventory(&inv, &payloads, Some(&inv_bytes))?;
    Ok(PackedPackageInventory {
        package_digest: digest,
        inventory: inv,
        inventory_bytes: inv_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_07_2_scaffold_lint_and_pack_all_three_implementation_kinds() {
        for kind in [
            ImplementationKind::Rust,
            ImplementationKind::Workflow,
            ImplementationKind::Wasm,
        ] {
            let pkg = scaffold_provider_package("org.orion", "org.orion.sample", "2.0.0", kind);
            let lint = lint_provider_package(&pkg.files);
            assert!(
                lint.valid,
                "scaffolded {kind:?} must pass lint: {:?}",
                lint.errors
            );

            let inv = pack_provider_package(&pkg.files)
                .expect("scaffolded package must pack deterministically");
            assert!(inv.package_digest.starts_with("sha256:"));
            assert_eq!(lint.provider_id, "org.orion.sample");
            assert_eq!(lint.version, "2.0.0");
        }
    }
}

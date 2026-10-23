//! V2-10.3 Content identity, canonical inventory encoder/verifier, signature
//! statement framing, and complete typed dependency lock verifier.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::schema_profile::MAX_SAFE_INTEGER;

pub const MAX_PACKAGE_FILE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PACKAGE_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PACKAGE_FILES: usize = 1_024;
pub const SIGNATURE_DOMAIN_PREFIX: &[u8] = b"orion-connector-package-signature-v1\0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}")]
pub struct PackageIdentityFinding {
    pub code: String,
}

impl PackageIdentityFinding {
    pub fn new(code: impl Into<String>) -> Self {
        Self { code: code.into() }
    }
}

/// Restricted ASCII/nonnegative safe-integer canonical JSON encoder.
///
/// Sorts object keys by ASCII byte order, preserves array order, emits no
/// whitespace or trailing newline, and rejects non-ASCII strings, negative or
/// unsafe integers, floats, booleans, and null.
pub fn canonical_bytes(value: &Value) -> Result<Vec<u8>, PackageIdentityFinding> {
    let mut out = Vec::new();
    encode_canonical_item(value, &mut out)?;
    Ok(out)
}

fn encode_canonical_item(value: &Value, out: &mut Vec<u8>) -> Result<(), PackageIdentityFinding> {
    match value {
        Value::String(s) => {
            if !s.is_ascii() {
                return Err(PackageIdentityFinding::new("CANONICAL_VALUE_UNSUPPORTED"));
            }
            let encoded = serde_json::to_string(s)
                .map_err(|_| PackageIdentityFinding::new("CANONICAL_VALUE_UNSUPPORTED"))?;
            out.extend_from_slice(encoded.as_bytes());
            Ok(())
        }
        Value::Number(n) => {
            if let Some(u) = n.as_u64()
                && u <= MAX_SAFE_INTEGER as u64
            {
                out.extend_from_slice(u.to_string().as_bytes());
                return Ok(());
            }
            Err(PackageIdentityFinding::new("CANONICAL_VALUE_UNSUPPORTED"))
        }
        Value::Array(items) => {
            out.push(b'[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(b',');
                }
                encode_canonical_item(item, out)?;
            }
            out.push(b']');
            Ok(())
        }
        Value::Object(map) => {
            let mut sorted_keys: Vec<&String> = map.keys().collect();
            sorted_keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push(b'{');
            for (idx, key) in sorted_keys.iter().enumerate() {
                if !key.is_ascii() {
                    return Err(PackageIdentityFinding::new("CANONICAL_VALUE_UNSUPPORTED"));
                }
                if idx > 0 {
                    out.push(b',');
                }
                let encoded_key = serde_json::to_string(key)
                    .map_err(|_| PackageIdentityFinding::new("CANONICAL_VALUE_UNSUPPORTED"))?;
                out.extend_from_slice(encoded_key.as_bytes());
                out.push(b':');
                encode_canonical_item(&map[*key], out)?;
            }
            out.push(b'}');
            Ok(())
        }
        _ => Err(PackageIdentityFinding::new("CANONICAL_VALUE_UNSUPPORTED")),
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn sha256_digest(data: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(data))
}

pub fn validate_payload_path(path: &str) -> Result<(), PackageIdentityFinding> {
    if path.is_empty()
        || path.len() > 256
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'-'))
    {
        return Err(PackageIdentityFinding::new("INVENTORY_PATH_INVALID"));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(PackageIdentityFinding::new("INVENTORY_PATH_INVALID"));
    }
    if path == "inventory.json" || path == "signatures" || path.starts_with("signatures/") {
        return Err(PackageIdentityFinding::new("INVENTORY_SELF_REFERENCE"));
    }
    Ok(())
}

pub fn build_inventory(
    payloads: &BTreeMap<String, Vec<u8>>,
) -> Result<Value, PackageIdentityFinding> {
    let mut files = Vec::with_capacity(payloads.len());
    for (path, bytes) in payloads {
        validate_payload_path(path)?;
        files.push(json!({
            "path": path,
            "sha256": sha256_hex(bytes),
            "size": bytes.len() as u64,
        }));
    }
    Ok(json!({
        "inventory_version": 1,
        "files": files,
    }))
}

pub fn verify_inventory(
    inventory: &Value,
    payloads: &BTreeMap<String, Vec<u8>>,
    raw_inventory: Option<&[u8]>,
) -> Result<String, PackageIdentityFinding> {
    validate_inventory_shape(inventory)?;

    let files = inventory["files"]
        .as_array()
        .ok_or_else(|| PackageIdentityFinding::new("INVENTORY_SHAPE_INVALID"))?;
    if files.len() > MAX_PACKAGE_FILES {
        return Err(PackageIdentityFinding::new("INVENTORY_LIMIT_EXCEEDED"));
    }

    let paths: Vec<String> = files
        .iter()
        .map(|entry| entry["path"].as_str().unwrap_or_default().to_string())
        .collect();

    for path in paths.iter().chain(payloads.keys()) {
        validate_payload_path(path)?;
    }

    let casefolded: HashSet<String> = paths.iter().map(|p| p.to_ascii_lowercase()).collect();
    if casefolded.len() != paths.len() {
        return Err(PackageIdentityFinding::new("INVENTORY_PATH_COLLISION"));
    }

    let mut sorted_paths = paths.clone();
    sorted_paths.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    if paths != sorted_paths {
        return Err(PackageIdentityFinding::new("INVENTORY_ORDER_INVALID"));
    }

    let path_set: BTreeSet<&String> = paths.iter().collect();
    let payload_set: BTreeSet<&String> = payloads.keys().collect();
    if path_set != payload_set {
        return Err(PackageIdentityFinding::new("INVENTORY_PAYLOAD_MISMATCH"));
    }

    for req in ["connector.json", "distribution.json", "LICENSE"] {
        if !payloads.contains_key(req) {
            return Err(PackageIdentityFinding::new(
                "INVENTORY_REQUIRED_FILE_MISSING",
            ));
        }
    }

    let mut total_bytes = 0usize;
    for entry in files {
        let path = entry["path"].as_str().expect("inventory entry path string");
        let data = &payloads[path];
        total_bytes += data.len();
        if data.len() > MAX_PACKAGE_FILE_BYTES || total_bytes > MAX_PACKAGE_TOTAL_BYTES {
            return Err(PackageIdentityFinding::new("INVENTORY_LIMIT_EXCEEDED"));
        }
        let expected_size = entry["size"].as_u64().expect("inventory entry size u64") as usize;
        let expected_sha = entry["sha256"]
            .as_str()
            .expect("inventory entry sha256 string");
        if expected_size != data.len() || expected_sha != sha256_hex(data) {
            return Err(PackageIdentityFinding::new("INVENTORY_CONTENT_MISMATCH"));
        }
    }

    let encoded = canonical_bytes(inventory)?;
    if let Some(raw) = raw_inventory
        && raw != encoded.as_slice()
    {
        return Err(PackageIdentityFinding::new("INVENTORY_NOT_CANONICAL"));
    }

    Ok(sha256_digest(&encoded))
}

fn validate_inventory_shape(inventory: &Value) -> Result<(), PackageIdentityFinding> {
    let Some(obj) = inventory.as_object() else {
        return Err(PackageIdentityFinding::new("INVENTORY_SHAPE_INVALID"));
    };
    if obj.len() != 2
        || obj.get("inventory_version").and_then(Value::as_u64) != Some(1)
        || !obj.get("files").is_some_and(Value::is_array)
    {
        return Err(PackageIdentityFinding::new("INVENTORY_SHAPE_INVALID"));
    }
    let files = obj["files"].as_array().expect("inventory files array");
    if files.is_empty() {
        return Err(PackageIdentityFinding::new("INVENTORY_SHAPE_INVALID"));
    }
    for entry in files {
        let Some(e_obj) = entry.as_object() else {
            return Err(PackageIdentityFinding::new("INVENTORY_SHAPE_INVALID"));
        };
        if e_obj.len() != 3 {
            return Err(PackageIdentityFinding::new("INVENTORY_SHAPE_INVALID"));
        }
        let Some(p) = e_obj.get("path").and_then(Value::as_str) else {
            return Err(PackageIdentityFinding::new("INVENTORY_SHAPE_INVALID"));
        };
        if p.is_empty() {
            return Err(PackageIdentityFinding::new("INVENTORY_SHAPE_INVALID"));
        }
        let Some(sha) = e_obj.get("sha256").and_then(Value::as_str) else {
            return Err(PackageIdentityFinding::new("INVENTORY_SHAPE_INVALID"));
        };
        if sha.len() != 64 || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(PackageIdentityFinding::new("INVENTORY_SHAPE_INVALID"));
        }
        if e_obj.get("size").and_then(Value::as_u64).is_none() {
            return Err(PackageIdentityFinding::new("INVENTORY_SHAPE_INVALID"));
        }
    }
    Ok(())
}

/// Build the domain-separated signing message for a detached signature statement.
pub fn signature_signing_bytes(statement: &Value) -> Result<Vec<u8>, PackageIdentityFinding> {
    let encoded = canonical_bytes(statement)?;
    let mut out = Vec::with_capacity(SIGNATURE_DOMAIN_PREFIX.len() + encoded.len());
    out.extend_from_slice(SIGNATURE_DOMAIN_PREFIX);
    out.extend_from_slice(&encoded);
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredLockDependency {
    pub kind: String,
    pub alias: String,
    pub id: String,
    pub requires: String,
    pub operations: Vec<String>,
    pub functions: Vec<String>,
    pub binding_slot: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedArtifactMetadata {
    pub publisher: String,
    pub id: String,
    pub version: String,
    pub contract: String,
    pub schema_profile: Option<String>,
    pub implementation: Option<String>,
    pub import_profile: Option<String>,
    pub operations: Vec<String>,
    pub functions: Vec<String>,
    pub dependencies: Vec<DeclaredLockDependency>,
}

/// Validate a complete typed dependency lock (`schema_version: 2`) against
/// verified package/plugin artifact metadata and return the canonical lock digest.
pub fn check_dependency_lock(
    lock: &Value,
    artifacts: &BTreeMap<(String, String), VerifiedArtifactMetadata>,
) -> Result<String, PackageIdentityFinding> {
    validate_lock_shape(lock)?;

    let packages = lock["packages"].as_array().expect("lock packages array");
    let mut nodes: HashMap<(String, String), &Value> = HashMap::new();
    let mut node_order: Vec<(String, String)> = Vec::with_capacity(packages.len());
    let mut identities: HashMap<(String, String, String, String), (String, String)> =
        HashMap::new();

    for node in packages {
        let key = (
            node["kind"].as_str().expect("node kind string").to_string(),
            node["digest"]
                .as_str()
                .expect("node digest string")
                .to_string(),
        );
        if nodes.contains_key(&key) {
            return Err(PackageIdentityFinding::new("LOCK_DUPLICATE_NODE"));
        }
        nodes.insert(key.clone(), node);
        node_order.push(key.clone());

        let identity = (
            node["kind"].as_str().expect("node kind string").to_string(),
            node["publisher"]
                .as_str()
                .expect("node publisher string")
                .to_string(),
            node["id"].as_str().expect("node id string").to_string(),
            node["version"]
                .as_str()
                .expect("node version string")
                .to_string(),
        );
        if let Some(existing) = identities.get(&identity)
            && existing != &key
        {
            return Err(PackageIdentityFinding::new("LOCK_IDENTITY_CONFLICT"));
        }
        identities.insert(identity, key);
    }

    let roots: Vec<(String, String)> = lock["roots"]
        .as_array()
        .expect("lock roots array")
        .iter()
        .map(|r| {
            (
                r["kind"].as_str().expect("root kind string").to_string(),
                r["digest"]
                    .as_str()
                    .expect("root digest string")
                    .to_string(),
            )
        })
        .collect();

    if roots.iter().any(|r| !nodes.contains_key(r)) {
        return Err(PackageIdentityFinding::new("LOCK_CLOSURE_MISSING"));
    }

    let mut edges: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
    for (key, node) in &nodes {
        let mut node_edges = Vec::new();
        let mut aliases = HashSet::new();
        for edge in node["dependencies"]
            .as_array()
            .expect("node dependencies array")
        {
            let alias = edge["alias"].as_str().expect("edge alias string");
            if !aliases.insert(alias) {
                return Err(PackageIdentityFinding::new("LOCK_ALIAS_DUPLICATE"));
            }
            let target = (
                edge["kind"].as_str().expect("edge kind string").to_string(),
                edge["digest"]
                    .as_str()
                    .expect("edge digest string")
                    .to_string(),
            );
            if !nodes.contains_key(&target) {
                return Err(PackageIdentityFinding::new("LOCK_CLOSURE_MISSING"));
            }
            node_edges.push(target);
        }
        edges.insert(key.clone(), node_edges);
    }

    let mut colors: HashMap<(String, String), u8> = HashMap::new();
    let mut reachable: HashSet<(String, String)> = HashSet::new();

    fn visit(
        key: &(String, String),
        edges: &HashMap<(String, String), Vec<(String, String)>>,
        colors: &mut HashMap<(String, String), u8>,
        reachable: &mut HashSet<(String, String)>,
    ) -> Result<(), PackageIdentityFinding> {
        if colors.get(key) == Some(&1) {
            return Err(PackageIdentityFinding::new("LOCK_CYCLE"));
        }
        if colors.get(key) == Some(&2) {
            return Ok(());
        }
        colors.insert(key.clone(), 1);
        reachable.insert(key.clone());
        if let Some(targets) = edges.get(key) {
            for target in targets {
                visit(target, edges, colors, reachable)?;
            }
        }
        colors.insert(key.clone(), 2);
        Ok(())
    }

    for root in &roots {
        visit(root, &edges, &mut colors, &mut reachable)?;
    }

    if reachable.len() != nodes.len() {
        return Err(PackageIdentityFinding::new("LOCK_UNREACHABLE_NODE"));
    }

    let mut sorted_nodes = node_order.clone();
    sorted_nodes.sort();
    let mut sorted_roots = roots.clone();
    sorted_roots.sort();
    if node_order != sorted_nodes || roots != sorted_roots {
        return Err(PackageIdentityFinding::new("LOCK_ORDER_INVALID"));
    }

    for key in &node_order {
        let node = nodes[key];
        let Some(artifact) = artifacts.get(key) else {
            return Err(PackageIdentityFinding::new("LOCK_ARTIFACT_UNAVAILABLE"));
        };
        if node["publisher"].as_str() != Some(&artifact.publisher)
            || node["id"].as_str() != Some(&artifact.id)
            || node["version"].as_str() != Some(&artifact.version)
            || node["contract"].as_str() != Some(&artifact.contract)
        {
            return Err(PackageIdentityFinding::new("LOCK_METADATA_MISMATCH"));
        }

        if node["kind"].as_str() == Some("provider") {
            let node_ops: Vec<String> = node["operations"]
                .as_array()
                .expect("node operations array")
                .iter()
                .map(|v| v.as_str().expect("operation string").to_string())
                .collect();
            let mut expected_ops = artifact.operations.clone();
            expected_ops.sort();
            if node_ops != expected_ops {
                return Err(PackageIdentityFinding::new("LOCK_METADATA_MISMATCH"));
            }
            if node["schema_profile"].as_str() != artifact.schema_profile.as_deref()
                || node["implementation"].as_str() != artifact.implementation.as_deref()
                || node.get("import_profile").and_then(Value::as_str)
                    != artifact.import_profile.as_deref()
            {
                return Err(PackageIdentityFinding::new("LOCK_METADATA_MISMATCH"));
            }
        } else {
            let node_fns: Vec<String> = node["functions"]
                .as_array()
                .expect("node functions array")
                .iter()
                .map(|v| v.as_str().expect("function string").to_string())
                .collect();
            let mut expected_fns = artifact.functions.clone();
            expected_fns.sort();
            if node_fns != expected_fns {
                return Err(PackageIdentityFinding::new("LOCK_METADATA_MISMATCH"));
            }
        }

        let declarations: BTreeMap<String, &DeclaredLockDependency> = artifact
            .dependencies
            .iter()
            .map(|d| (d.alias.clone(), d))
            .collect();
        let node_deps = node["dependencies"]
            .as_array()
            .expect("node dependencies array");
        let edge_aliases: Vec<String> = node_deps
            .iter()
            .map(|e| e["alias"].as_str().expect("edge alias string").to_string())
            .collect();
        let edge_alias_set: BTreeSet<String> = edge_aliases.iter().cloned().collect();
        let decl_alias_set: BTreeSet<String> = declarations.keys().cloned().collect();
        if edge_alias_set != decl_alias_set {
            return Err(PackageIdentityFinding::new("LOCK_DECLARATION_MISMATCH"));
        }
        let sorted_decl_aliases: Vec<String> = declarations.keys().cloned().collect();
        if edge_aliases != sorted_decl_aliases {
            return Err(PackageIdentityFinding::new("LOCK_ORDER_INVALID"));
        }

        for edge in node_deps {
            let alias = edge["alias"].as_str().expect("edge alias string");
            let declared = declarations[alias];
            let target_key = (
                edge["kind"].as_str().expect("edge kind string").to_string(),
                edge["digest"]
                    .as_str()
                    .expect("edge digest string")
                    .to_string(),
            );
            let target = nodes[&target_key];
            if edge["kind"].as_str() != Some(&declared.kind)
                || target["id"].as_str() != Some(&declared.id)
            {
                return Err(PackageIdentityFinding::new("LOCK_DECLARATION_MISMATCH"));
            }
            let expected_req = format!(
                "={}",
                target["version"].as_str().expect("target version string")
            );
            if declared.requires != expected_req {
                return Err(PackageIdentityFinding::new(
                    "LOCK_VERSION_REQUIREMENT_UNQUALIFIED",
                ));
            }
            if edge["kind"].as_str() == Some("provider") {
                let edge_ops: Vec<String> = edge["operations"]
                    .as_array()
                    .expect("edge operations array")
                    .iter()
                    .map(|v| v.as_str().expect("operation string").to_string())
                    .collect();
                let mut decl_ops = declared.operations.clone();
                decl_ops.sort();
                let target_ops: HashSet<String> = target["operations"]
                    .as_array()
                    .expect("target operations array")
                    .iter()
                    .map(|v| v.as_str().expect("operation string").to_string())
                    .collect();
                if edge_ops != decl_ops || !edge_ops.iter().all(|op| target_ops.contains(op)) {
                    return Err(PackageIdentityFinding::new("LOCK_GRANT_MISMATCH"));
                }
                if edge["binding_slot"].as_str() != declared.binding_slot.as_deref() {
                    return Err(PackageIdentityFinding::new("LOCK_BINDING_SLOT_MISMATCH"));
                }
            } else {
                let edge_fns: Vec<String> = edge["functions"]
                    .as_array()
                    .expect("edge functions array")
                    .iter()
                    .map(|v| v.as_str().expect("function string").to_string())
                    .collect();
                let mut decl_fns = declared.functions.clone();
                decl_fns.sort();
                let target_fns: HashSet<String> = target["functions"]
                    .as_array()
                    .expect("target functions array")
                    .iter()
                    .map(|v| v.as_str().expect("function string").to_string())
                    .collect();
                if edge_fns != decl_fns || !edge_fns.iter().all(|f| target_fns.contains(f)) {
                    return Err(PackageIdentityFinding::new("LOCK_GRANT_MISMATCH"));
                }
            }
        }
    }

    let encoded = canonical_bytes(lock)?;
    Ok(sha256_digest(&encoded))
}

fn is_sha256_digest(s: &str) -> bool {
    let Some(hex_part) = s.strip_prefix("sha256:") else {
        return false;
    };
    hex_part.len() == 64
        && hex_part
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn validate_lock_shape(lock: &Value) -> Result<(), PackageIdentityFinding> {
    let Some(obj) = lock.as_object() else {
        return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
    };
    if obj.len() != 3
        || obj.get("schema_version").and_then(Value::as_u64) != Some(2)
        || !obj.get("roots").is_some_and(Value::is_array)
        || !obj.get("packages").is_some_and(Value::is_array)
    {
        return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
    }
    let roots = obj["roots"].as_array().expect("lock roots array");
    if roots.is_empty() {
        return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
    }
    for root in roots {
        let Some(r_obj) = root.as_object() else {
            return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
        };
        if r_obj.len() != 2
            || r_obj.get("kind").and_then(Value::as_str) != Some("provider")
            || !r_obj
                .get("digest")
                .and_then(Value::as_str)
                .is_some_and(is_sha256_digest)
        {
            return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
        }
    }
    let packages = obj["packages"].as_array().expect("lock packages array");
    if packages.is_empty() {
        return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
    }
    for pkg in packages {
        let Some(p_obj) = pkg.as_object() else {
            return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
        };
        let Some(digest) = p_obj.get("digest").and_then(Value::as_str) else {
            return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
        };
        if !is_sha256_digest(digest) {
            return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
        }
        match p_obj.get("kind").and_then(Value::as_str) {
            Some("provider") => {
                if p_obj.get("contract").and_then(Value::as_str) != Some("orion:connector@1.0.0")
                    || p_obj.get("schema_profile").and_then(Value::as_str)
                        != Some("orion-json-schema/1")
                    || !p_obj.get("operations").is_some_and(Value::is_array)
                    || !p_obj.get("dependencies").is_some_and(Value::is_array)
                {
                    return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
                }
                let impl_kind = p_obj.get("implementation").and_then(Value::as_str);
                if !matches!(impl_kind, Some("workflow" | "wasm" | "rust")) {
                    return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
                }
                if impl_kind == Some("wasm")
                    && p_obj
                        .get("import_profile")
                        .and_then(Value::as_str)
                        .is_none()
                {
                    return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
                }
            }
            Some("function_plugin") => {
                if p_obj.get("contract").and_then(Value::as_str) != Some("orion:plugin@1.0.0")
                    || !p_obj.get("functions").is_some_and(Value::is_array)
                    || !p_obj
                        .get("dependencies")
                        .and_then(Value::as_array)
                        .is_some_and(Vec::is_empty)
                {
                    return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
                }
            }
            _ => return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID")),
        }
        for dep in p_obj["dependencies"]
            .as_array()
            .expect("package dependencies array")
        {
            let Some(d_obj) = dep.as_object() else {
                return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
            };
            if !d_obj
                .get("digest")
                .and_then(Value::as_str)
                .is_some_and(is_sha256_digest)
                || d_obj.get("alias").and_then(Value::as_str).is_none()
            {
                return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
            }
            match d_obj.get("kind").and_then(Value::as_str) {
                Some("provider") => {
                    if !d_obj.get("operations").is_some_and(Value::is_array)
                        || d_obj.get("binding_slot").and_then(Value::as_str).is_none()
                    {
                        return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
                    }
                }
                Some("function_plugin") => {
                    if !d_obj.get("functions").is_some_and(Value::is_array) {
                        return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID"));
                    }
                }
                _ => return Err(PackageIdentityFinding::new("LOCK_SHAPE_INVALID")),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::extensions::schema_profile::parse_json_pointer;
    use std::path::PathBuf;

    fn apply_patch(root: &mut Value, patches: &[Value]) {
        for patch in patches {
            let path = patch["path"].as_str().unwrap();
            let op = patch["op"].as_str().unwrap();
            let parts = parse_json_pointer(path).unwrap();
            let (parent_parts, last) = parts.split_at(parts.len() - 1);
            let key = &last[0];
            let mut cur = &mut *root;
            for p in parent_parts {
                cur = match cur {
                    Value::Array(arr) => &mut arr[p.parse::<usize>().unwrap()],
                    Value::Object(map) => map.get_mut(p).unwrap(),
                    _ => panic!("invalid patch parent"),
                };
            }
            match cur {
                Value::Array(arr) => {
                    let idx: usize = key.parse().unwrap();
                    match op {
                        "remove" => {
                            arr.remove(idx);
                        }
                        "replace" => {
                            arr[idx] = patch["value"].clone();
                        }
                        _ => {
                            arr.insert(idx, patch["value"].clone());
                        }
                    }
                }
                Value::Object(map) => match op {
                    "remove" => {
                        map.remove(key);
                    }
                    "replace" | "add" => {
                        map.insert(key.clone(), patch["value"].clone());
                    }
                    _ => panic!("unsupported patch op"),
                },
                _ => panic!("invalid patch target"),
            }
        }
    }

    #[test]
    fn v2_10_3_passes_all_package_identity_and_dependency_lock_fixture_cases() {
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2/package-identity");
        let bundle_raw = std::fs::read_to_string(fixture_dir.join("bundle.json")).unwrap();
        let fixture: Value = serde_json::from_str(&bundle_raw).unwrap();
        let cases_raw = std::fs::read_to_string(fixture_dir.join("cases.json")).unwrap();
        let cases_doc: Value = serde_json::from_str(&cases_raw).unwrap();

        let mut artifacts: BTreeMap<(String, String), VerifiedArtifactMetadata> = BTreeMap::new();
        for pkg in fixture["providers"].as_array().unwrap() {
            let payloads: BTreeMap<String, Vec<u8>> = pkg["payloads"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().as_bytes().to_vec()))
                .collect();
            let actual_digest = verify_inventory(&pkg["inventory"], &payloads, None).unwrap();
            assert_eq!(actual_digest, pkg["digest"].as_str().unwrap());

            let manifest: Value = serde_json::from_slice(&payloads["connector.json"]).unwrap();
            let distribution: Value =
                serde_json::from_slice(&payloads["distribution.json"]).unwrap();
            let impl_kind = manifest["implementation"]["kind"]
                .as_str()
                .unwrap()
                .to_string();
            let import_profile = if impl_kind == "wasm" {
                match manifest["implementation"]["world"].as_str().unwrap() {
                    "orion:connector/connector@1.0.0" => Some("core-v1".to_string()),
                    "orion:connector/connector-http@1.0.0" => Some("http-v1".to_string()),
                    "orion:connector/connector-composition@1.0.0" => {
                        Some("composition-v1".to_string())
                    }
                    "orion:connector/connector-sessions@1.0.0" => Some("sessions-v1".to_string()),
                    _ => None,
                }
            } else {
                None
            };
            let mut ops: Vec<String> = manifest["operations"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            ops.sort();
            let deps: Vec<DeclaredLockDependency> = manifest["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| DeclaredLockDependency {
                    kind: d["kind"].as_str().unwrap().to_string(),
                    alias: d["alias"].as_str().unwrap().to_string(),
                    id: d["id"].as_str().unwrap().to_string(),
                    requires: d["requires"].as_str().unwrap().to_string(),
                    operations: d
                        .get("operations")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
                        .unwrap_or_default(),
                    functions: d
                        .get("functions")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
                        .unwrap_or_default(),
                    binding_slot: d
                        .get("binding_slot")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
                .collect();

            artifacts.insert(
                ("provider".to_string(), actual_digest),
                VerifiedArtifactMetadata {
                    publisher: distribution["publisher"].as_str().unwrap().to_string(),
                    id: manifest["id"].as_str().unwrap().to_string(),
                    version: manifest["version"].as_str().unwrap().to_string(),
                    contract: manifest["contract"].as_str().unwrap().to_string(),
                    schema_profile: Some(manifest["schema_profile"].as_str().unwrap().to_string()),
                    implementation: Some(impl_kind),
                    import_profile,
                    operations: ops,
                    functions: Vec::new(),
                    dependencies: deps,
                },
            );
        }

        let lock_digest = check_dependency_lock(&fixture["lock"], &artifacts).unwrap();
        assert_eq!(lock_digest, fixture["lock_digest"].as_str().unwrap());

        for case in cases_doc["cases"].as_array().unwrap() {
            let id = case["id"].as_str().unwrap();
            let kind = case["kind"].as_str().unwrap();
            let expected = case["expect"].as_str().unwrap();
            let patches = case
                .get("patch")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);

            let observed = if kind == "identity" {
                let mut pkg = fixture["providers"][0].clone();
                apply_patch(&mut pkg, patches);
                let payloads: BTreeMap<String, Vec<u8>> = pkg["payloads"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().as_bytes().to_vec()))
                    .collect();
                let pretty_bytes =
                    if case.get("pretty_inventory").and_then(Value::as_bool) == Some(true) {
                        Some(serde_json::to_vec_pretty(&pkg["inventory"]).unwrap())
                    } else {
                        None
                    };
                match verify_inventory(&pkg["inventory"], &payloads, pretty_bytes.as_deref()) {
                    Ok(digest) => {
                        if digest != pkg["digest"].as_str().unwrap() {
                            "PACKAGE_DIGEST_MISMATCH".to_string()
                        } else {
                            if case.get("reorder_payloads").and_then(Value::as_bool) == Some(true) {
                                let built = build_inventory(&payloads).unwrap();
                                assert_eq!(built, pkg["inventory"]);
                            }
                            "OK".to_string()
                        }
                    }
                    Err(f) => f.code,
                }
            } else {
                let mut lock = fixture["lock"].clone();
                apply_patch(&mut lock, patches);
                let mut avail = artifacts.clone();
                if case.get("artifact_unavailable").and_then(Value::as_bool) == Some(true) {
                    let root = &fixture["lock"]["roots"][0];
                    avail.remove(&(
                        root["kind"].as_str().unwrap().to_string(),
                        root["digest"].as_str().unwrap().to_string(),
                    ));
                }
                match check_dependency_lock(&lock, &avail) {
                    Ok(_) => "OK".to_string(),
                    Err(f) => f.code,
                }
            };
            assert_eq!(observed, expected, "case {id} mismatch");
        }
    }
}

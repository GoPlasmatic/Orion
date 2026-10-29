//! Connector package manifest validator (`check_manifest_bundle`).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::{Map, Value};

use super::{
    CompiledSchemaBundle, PROFILE_ID, SchemaProfileFinding, SchemaProfileLimits,
    validate_safe_package_path,
};

/// Validate a connector manifest bundle (`manifest`, `files`, `schemas`) against
/// the profile's shape and relationship rules, returning a compiled schema bundle.
pub fn check_manifest_bundle(
    manifest: &Value,
    files: &[String],
    schemas: &BTreeMap<String, Value>,
) -> Result<CompiledSchemaBundle, SchemaProfileFinding> {
    let limits = SchemaProfileLimits::default();
    let Some(obj) = manifest.as_object() else {
        return Err(SchemaProfileFinding::new(
            "MANIFEST_SHAPE_INVALID",
            "manifest root",
        ));
    };

    if let Some(ver) = obj.get("schema_version") {
        if ver.as_i64() != Some(2) {
            return Err(SchemaProfileFinding::new(
                "MANIFEST_SCHEMA_VERSION_UNSUPPORTED",
                "schema_version",
            ));
        }
    } else {
        return Err(SchemaProfileFinding::new(
            "MANIFEST_SHAPE_INVALID",
            "missing schema_version",
        ));
    }

    validate_manifest_shape(obj)?;

    let operations = obj
        .get("operations")
        .and_then(Value::as_object)
        .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "operations"))?;
    if operations
        .keys()
        .any(|name| name.starts_with("orion.internal."))
    {
        return Err(SchemaProfileFinding::new(
            "MANIFEST_OPERATION_RESERVED",
            "operations",
        ));
    }

    let implementation = obj
        .get("implementation")
        .and_then(Value::as_object)
        .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "implementation"))?;
    let kind = implementation
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "implementation.kind")
        })?;
    let is_workflow = kind == "workflow";

    let capabilities = obj
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "capabilities"))?;

    if kind == "wasm" {
        let world = implementation
            .get("world")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let req_caps: &[&str] = match world {
            "orion:connector/connector@1.0.0" => &[],
            "orion:connector/connector-http@1.0.0" => &["http", "credentials", "logging"],
            "orion:connector/connector-composition@1.0.0" => {
                &["http", "credentials", "logging", "dependencies"]
            }
            "orion:connector/connector-sessions@1.0.0" => {
                &["http", "credentials", "logging", "sessions"]
            }
            _ => {
                return Err(SchemaProfileFinding::new(
                    "MANIFEST_SHAPE_INVALID",
                    "implementation.world",
                ));
            }
        };
        for req in req_caps {
            let present = match capabilities.get(*req) {
                Some(Value::Bool(b)) => *b,
                Some(Value::Array(a)) => !a.is_empty(),
                Some(Value::Object(o)) => !o.is_empty(),
                _ => false,
            };
            if !present {
                return Err(SchemaProfileFinding::new(
                    "MANIFEST_IMPORT_PROFILE_CAPABILITY_MISSING",
                    *req,
                ));
            }
        }
    }

    if is_workflow {
        let entries = implementation
            .get("entries")
            .and_then(Value::as_object)
            .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "entries"))?;
        let entry_keys: BTreeSet<&String> = entries.keys().collect();
        let op_keys: BTreeSet<&String> = operations.keys().collect();
        if entry_keys != op_keys {
            return Err(SchemaProfileFinding::new(
                "MANIFEST_ENTRY_MISMATCH",
                "entries",
            ));
        }
    }

    let dependencies = obj
        .get("dependencies")
        .and_then(Value::as_array)
        .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "dependencies"))?;
    let mut aliases = HashSet::new();
    for dep in dependencies {
        let alias = dep.get("alias").and_then(Value::as_str).ok_or_else(|| {
            SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "dependency.alias")
        })?;
        if !aliases.insert(alias) {
            return Err(SchemaProfileFinding::new(
                "MANIFEST_DEPENDENCY_ALIAS_DUPLICATE",
                alias,
            ));
        }
    }

    let payload_limits = obj
        .get("payload_limits")
        .and_then(Value::as_object)
        .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "payload_limits"))?;
    for (name, max_allowed) in [
        ("configuration_bytes", limits.configuration_bytes),
        ("input_bytes", limits.input_bytes),
        ("output_bytes", limits.output_bytes),
        ("metadata_bytes", limits.metadata_bytes),
    ] {
        let val = payload_limits
            .get(name)
            .and_then(Value::as_u64)
            .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", name))?;
        if val as usize > max_allowed {
            return Err(SchemaProfileFinding::new(
                "MANIFEST_LIMIT_EXCEEDS_PROFILE",
                name,
            ));
        }
    }

    for (op_name, op_val) in operations {
        let op_obj = op_val
            .as_object()
            .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", op_name))?;
        let effect = op_obj.get("effect").and_then(Value::as_str).unwrap_or("");
        let retry = op_obj.get("retry").and_then(Value::as_str).unwrap_or("");
        if matches!(effect, "write" | "mixed")
            && retry != "never"
            && !op_obj.contains_key("idempotency")
        {
            return Err(SchemaProfileFinding::new("MANIFEST_RETRY_UNSAFE", op_name));
        }
    }

    let mut facets: Vec<&Map<String, Value>> = Vec::new();
    if let Some(lifecycle) = obj.get("lifecycle").and_then(Value::as_object) {
        for v in lifecycle.values() {
            if let Some(f) = v.as_object() {
                facets.push(f);
            }
        }
    }
    if let Some(sources) = obj.get("sources").and_then(Value::as_object) {
        for source in sources.values() {
            if let Some(s_obj) = source.as_object() {
                for v in s_obj.values() {
                    if let Some(f) = v.as_object() {
                        facets.push(f);
                    }
                }
            }
        }
    }
    for facet in &facets {
        if is_workflow != facet.contains_key("entry") {
            return Err(SchemaProfileFinding::new(
                "MANIFEST_ENTRY_MISMATCH",
                "facet entry kind",
            ));
        }
    }

    let mut schema_paths: Vec<String> = vec![
        obj.get("configuration_schema")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    ];
    if let Some(cred) = obj.get("credential_schema").and_then(Value::as_str) {
        schema_paths.push(cred.to_string());
    }
    for op_val in operations.values() {
        if let Some(i) = op_val.get("input_schema").and_then(Value::as_str) {
            schema_paths.push(i.to_string());
        }
        if let Some(o) = op_val.get("output_schema").and_then(Value::as_str) {
            schema_paths.push(o.to_string());
        }
    }
    for facet in &facets {
        if let Some(i) = facet.get("input_schema").and_then(Value::as_str) {
            schema_paths.push(i.to_string());
        }
        if let Some(o) = facet.get("output_schema").and_then(Value::as_str) {
            schema_paths.push(o.to_string());
        }
    }

    let mut all_paths = schema_paths.clone();
    if is_workflow {
        if let Some(wfs) = implementation.get("workflows").and_then(Value::as_array) {
            for wf in wfs {
                if let Some(s) = wf.as_str() {
                    all_paths.push(s.to_string());
                }
            }
        }
        if let Some(chs) = implementation.get("channels").and_then(Value::as_array) {
            for ch in chs {
                if let Some(s) = ch.as_str() {
                    all_paths.push(s.to_string());
                }
            }
        }
    } else if kind == "wasm"
        && let Some(comp) = implementation.get("component").and_then(Value::as_str)
    {
        all_paths.push(comp.to_string());
    }

    for path in files.iter().chain(schemas.keys()).chain(all_paths.iter()) {
        validate_safe_package_path(path, "MANIFEST_PATH_INVALID")?;
    }

    let file_set: HashSet<&String> = files.iter().collect();
    if all_paths.iter().any(|p| !file_set.contains(p)) {
        return Err(SchemaProfileFinding::new(
            "MANIFEST_ARTIFACT_MISSING",
            "artifact",
        ));
    }
    if schema_paths.iter().any(|p| !schemas.contains_key(p)) {
        return Err(SchemaProfileFinding::new(
            "MANIFEST_ARTIFACT_MISSING",
            "schema document",
        ));
    }
    if schemas.keys().any(|p| !file_set.contains(p)) {
        return Err(SchemaProfileFinding::new(
            "MANIFEST_ARTIFACT_MISSING",
            "unlisted schema",
        ));
    }

    CompiledSchemaBundle::compile(schemas.clone(), limits)
}

fn validate_manifest_shape(obj: &Map<String, Value>) -> Result<(), SchemaProfileFinding> {
    let required = [
        "schema_version",
        "kind",
        "id",
        "version",
        "contract",
        "requires_orion",
        "schema_profile",
        "implementation",
        "configuration_schema",
        "operations",
        "dependencies",
        "capabilities",
        "payload_limits",
    ];
    let allowed: HashSet<&str> = required
        .iter()
        .copied()
        .chain(["credential_schema", "lifecycle", "sources"])
        .collect();
    for r in required {
        if !obj.contains_key(r) {
            return Err(SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", r));
        }
    }
    for k in obj.keys() {
        if !allowed.contains(k.as_str()) {
            return Err(SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", k));
        }
    }
    if obj.get("kind").and_then(Value::as_str) != Some("connector-provider")
        || obj.get("contract").and_then(Value::as_str) != Some("orion:connector@1.0.0")
        || obj.get("schema_profile").and_then(Value::as_str) != Some(PROFILE_ID)
    {
        return Err(SchemaProfileFinding::new(
            "MANIFEST_SHAPE_INVALID",
            "header constants",
        ));
    }
    let impl_obj = obj
        .get("implementation")
        .and_then(Value::as_object)
        .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "implementation"))?;
    match impl_obj.get("kind").and_then(Value::as_str) {
        Some("workflow") => {
            if !impl_obj.contains_key("entries") || !impl_obj.contains_key("workflows") {
                return Err(SchemaProfileFinding::new(
                    "MANIFEST_SHAPE_INVALID",
                    "workflow implementation",
                ));
            }
            for k in impl_obj.keys() {
                if !matches!(k.as_str(), "kind" | "entries" | "workflows" | "channels") {
                    return Err(SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", k));
                }
            }
        }
        Some("wasm") => {
            if !impl_obj.contains_key("component")
                || !impl_obj.contains_key("world")
                || impl_obj.len() != 3
            {
                return Err(SchemaProfileFinding::new(
                    "MANIFEST_SHAPE_INVALID",
                    "wasm implementation",
                ));
            }
            let world = impl_obj.get("world").and_then(Value::as_str).unwrap_or("");
            if !matches!(
                world,
                "orion:connector/connector@1.0.0"
                    | "orion:connector/connector-http@1.0.0"
                    | "orion:connector/connector-composition@1.0.0"
                    | "orion:connector/connector-sessions@1.0.0"
            ) {
                return Err(SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "world"));
            }
        }
        Some("rust") => {
            if impl_obj.get("abi").and_then(Value::as_str) != Some("orion-rust-provider/1")
                || impl_obj.get("driver_id").and_then(Value::as_str).is_none()
                || impl_obj.len() != 3
            {
                return Err(SchemaProfileFinding::new(
                    "MANIFEST_SHAPE_INVALID",
                    "rust implementation",
                ));
            }
        }
        _ => {
            return Err(SchemaProfileFinding::new(
                "MANIFEST_SHAPE_INVALID",
                "implementation.kind",
            ));
        }
    }

    let caps = obj
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "capabilities"))?;
    let allowed_caps = [
        "http",
        "credentials",
        "byte_streams",
        "sqlite",
        "clock",
        "random",
        "logging",
        "source_checkpoint",
        "dependencies",
        "sessions",
    ];
    for k in caps.keys() {
        if !allowed_caps.contains(&k.as_str()) {
            return Err(SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", k));
        }
    }

    if let Some(sources) = obj.get("sources") {
        let Some(s_map) = sources.as_object() else {
            return Err(SchemaProfileFinding::new(
                "MANIFEST_SHAPE_INVALID",
                "sources",
            ));
        };
        if s_map.is_empty() {
            return Err(SchemaProfileFinding::new(
                "MANIFEST_SHAPE_INVALID",
                "sources",
            ));
        }
        for s_val in s_map.values() {
            let Some(facet) = s_val.as_object() else {
                return Err(SchemaProfileFinding::new(
                    "MANIFEST_SHAPE_INVALID",
                    "source",
                ));
            };
            for req in ["open", "poll", "settle", "close"] {
                if !facet.contains_key(req) {
                    return Err(SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", req));
                }
            }
            if facet.len() != 4 {
                return Err(SchemaProfileFinding::new(
                    "MANIFEST_SHAPE_INVALID",
                    "source keys",
                ));
            }
        }
    }

    let limits = obj
        .get("payload_limits")
        .and_then(Value::as_object)
        .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "payload_limits"))?;
    for req in [
        "configuration_bytes",
        "input_bytes",
        "output_bytes",
        "metadata_bytes",
    ] {
        if limits.get(req).and_then(Value::as_u64).unwrap_or(0) < 1 {
            return Err(SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", req));
        }
    }
    if limits.len() != 4 {
        return Err(SchemaProfileFinding::new(
            "MANIFEST_SHAPE_INVALID",
            "payload_limits",
        ));
    }

    Ok(())
}

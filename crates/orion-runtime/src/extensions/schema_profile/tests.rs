#![expect(clippy::unwrap_used, clippy::panic)]

use super::*;
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
fn passes_all_36_schema_and_manifest_fixture_cases() {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../orion-server/tests/fixtures/v2/schema-validation");
    let cases_raw = std::fs::read_to_string(fixture_dir.join("cases.json")).unwrap();
    let catalog: Value = serde_json::from_str(&cases_raw).unwrap();
    let base_raw = std::fs::read_to_string(fixture_dir.join("bundle.json")).unwrap();
    let base: Value = serde_json::from_str(&base_raw).unwrap();

    let cases = catalog["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 38);

    for case in cases {
        let id = case["id"].as_str().unwrap();
        let expected = case["expect"].as_str().unwrap();
        let observed = {
            let mut bundle = base.clone();
            if let Some(patches) = case.get("patch").and_then(Value::as_array) {
                apply_patch(&mut bundle, patches);
            }
            let files: Vec<String> = bundle["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            let schemas: BTreeMap<String, Value> = bundle["schemas"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            match check_manifest_bundle(&bundle["manifest"], &files, &schemas) {
                Ok(compiled) => {
                    if let Some(instances) = case.get("instances").and_then(Value::as_array) {
                        for inst in instances {
                            let schema_path = inst["schema"].as_str().unwrap();
                            let expected_valid = inst["valid"].as_bool().unwrap();
                            let actual_valid = compiled
                                .validate_instance(schema_path, &inst["value"])
                                .is_ok();
                            assert_eq!(
                                actual_valid, expected_valid,
                                "case {id}: instance validation mismatch"
                            );
                        }
                    }
                    "OK".to_string()
                }
                Err(f) => f.code,
            }
        };
        assert_eq!(observed, expected, "case {id} mismatch");
    }
}

#[test]
fn enforces_step_metering() {
    let mut schemas = BTreeMap::new();
    schemas.insert(
        "schemas/config.json".to_string(),
        serde_json::json!({
            "$schema": DIALECT_URI,
            "type": "object",
            "properties": {
                "base_url": { "type": "string" },
                "client_secret": { "type": "string", "x-orion-sensitive": true }
            }
        }),
    );
    let limits = SchemaProfileLimits {
        validation_steps: 2,
        ..Default::default()
    };
    let bundle = CompiledSchemaBundle::compile(schemas, limits).unwrap();
    let err = bundle
        .validate_instance(
            "schemas/config.json",
            &serde_json::json!({
                "base_url": "https://api.example.com",
                "client_secret": "super-secret"
            }),
        )
        .unwrap_err();
    assert_eq!(err.code, "SCHEMA_PROFILE_LIMIT");
}

#[test]
fn a_refusal_names_the_value_it_refused() {
    let mut schemas = BTreeMap::new();
    schemas.insert(
        "schemas/config.json".to_string(),
        serde_json::json!({
            "$schema": DIALECT_URI,
            "type": "object",
            "required": ["auth"],
            "properties": {
                "auth": {
                    "type": "object",
                    "required": ["token"],
                    "properties": { "scopes": { "type": "array", "items": { "type": "string" } } }
                }
            }
        }),
    );
    let bundle = CompiledSchemaBundle::compile(schemas, SchemaProfileLimits::default()).unwrap();
    let refused = |instance: Value| {
        bundle
            .validate_instance("schemas/config.json", &instance)
            .unwrap_err()
    };
    let err = refused(serde_json::json!({}));
    assert_eq!(
        (err.location.as_str(), err.instance_field()),
        ("required", "auth".into())
    );
    let err = refused(serde_json::json!({"auth": {}}));
    assert_eq!(err.instance_field(), "auth.token");
    let err = refused(serde_json::json!({"auth": {"token": "t", "scopes": ["a", 2]}}));
    assert_eq!(
        (err.location.as_str(), err.instance_path.as_str()),
        ("type", "/auth/scopes/1")
    );
}

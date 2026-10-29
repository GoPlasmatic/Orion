//! Constrained JSON Schema profile (`orion-json-schema/1`): the
//! manifest validator and the step-metered instance validator.

mod manifest;

pub use manifest::check_manifest_bundle;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const PROFILE_ID: &str = "orion-json-schema/1";
pub const DIALECT_URI: &str = "https://json-schema.org/draft/2020-12/schema";
pub const MIN_SAFE_INTEGER: i64 = -9_007_199_254_740_991;
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

const ALLOWED_KEYWORDS: &[&str] = &[
    "$schema",
    "$defs",
    "$ref",
    "title",
    "description",
    "x-orion-sensitive",
    "type",
    "enum",
    "const",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "properties",
    "required",
    "additionalProperties",
    "minProperties",
    "maxProperties",
    "items",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "allOf",
    "anyOf",
    "oneOf",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaProfileLimits {
    pub schema_file_bytes: usize,
    pub schema_bundle_bytes: usize,
    pub schema_files: usize,
    pub schema_nodes: usize,
    pub schema_depth: usize,
    pub reference_edges: usize,
    pub composition_branches: usize,
    pub configuration_bytes: usize,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub metadata_bytes: usize,
    pub instance_nodes: usize,
    pub instance_depth: usize,
    pub validation_steps: u64,
    pub diagnostics: usize,
}

impl Default for SchemaProfileLimits {
    fn default() -> Self {
        Self {
            schema_file_bytes: 65_536,
            schema_bundle_bytes: 262_144,
            schema_files: 32,
            schema_nodes: 1_024,
            schema_depth: 32,
            reference_edges: 128,
            composition_branches: 4,
            configuration_bytes: 65_536,
            input_bytes: 1_048_576,
            output_bytes: 1_048_576,
            metadata_bytes: 16_384,
            instance_nodes: 16_384,
            instance_depth: 64,
            validation_steps: 100_000,
            diagnostics: 16,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {location}")]
pub struct SchemaProfileFinding {
    pub code: String,
    pub location: String,
    /// For an instance a schema refused, the JSON pointer to the value it
    /// refused (`/auth/token`); empty for the instance's root, and for a
    /// finding about a schema or a manifest.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instance_path: String,
}

impl SchemaProfileFinding {
    pub fn new(code: impl Into<String>, location: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            location: location.into(),
            instance_path: String::new(),
        }
    }

    /// This finding, about the value under `segment` of the instance the
    /// caller was checking.
    fn within(mut self, segment: &str) -> Self {
        let escaped = segment.replace('~', "~0").replace('/', "~1");
        self.instance_path = format!("/{escaped}{}", self.instance_path);
        self
    }

    /// The refused value's path as dotted keys (`auth.token`, `items.0`),
    /// the way a field error names a field; empty for the root.
    pub fn instance_field(&self) -> String {
        self.instance_path
            .split('/')
            .skip(1)
            .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
            .collect::<Vec<_>>()
            .join(".")
    }
}

pub fn validate_safe_package_path(path: &str, code: &str) -> Result<(), SchemaProfileFinding> {
    if path.is_empty()
        || path.len() > 256
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'-'))
    {
        return Err(SchemaProfileFinding::new(code, "nonportable package path"));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(SchemaProfileFinding::new(code, "unsafe package path"));
    }
    Ok(())
}

pub fn parse_json_pointer(pointer: &str) -> Result<Vec<String>, SchemaProfileFinding> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    if !pointer.starts_with('/') {
        return Err(SchemaProfileFinding::new(
            "SCHEMA_REFERENCE_DENIED",
            "invalid JSON Pointer",
        ));
    }
    let bytes = pointer.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'~'
            && (i + 1 >= bytes.len() || (bytes[i + 1] != b'0' && bytes[i + 1] != b'1'))
        {
            return Err(SchemaProfileFinding::new(
                "SCHEMA_REFERENCE_DENIED",
                "invalid JSON Pointer",
            ));
        }
    }
    Ok(pointer[1..]
        .split('/')
        .map(|part| part.replace("~1", "/").replace("~0", "~"))
        .collect())
}

pub fn resolve_pointer<'a>(value: &'a Value, parts: &[String]) -> Option<&'a Value> {
    let mut cur = value;
    for part in parts {
        match cur {
            Value::Array(arr) => {
                if part != "0"
                    && (part.starts_with('0') || !part.bytes().all(|b| b.is_ascii_digit()))
                {
                    return None;
                }
                let index: usize = part.parse().ok()?;
                cur = arr.get(index)?;
            }
            Value::Object(map) => {
                cur = map.get(part)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct NodeKey {
    path: String,
    pointer: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CompiledSchemaBundle {
    schemas: BTreeMap<String, Value>,
    limits: SchemaProfileLimits,
}

/// The walk `CompiledSchemaBundle::compile` makes over each schema file: every
/// node it reaches, the edges between them and the `$ref`s to resolve, held
/// against the profile's limits.
struct SchemaWalk<'l> {
    limits: &'l SchemaProfileLimits,
    allowed: HashSet<&'static str>,
    nodes: HashMap<NodeKey, Value>,
    edges: HashMap<NodeKey, Vec<NodeKey>>,
    references: Vec<(NodeKey, Value)>,
}

impl SchemaWalk<'_> {
    /// Record `node` (at `pointer` in `path`) and everything under it.
    fn walk(
        &mut self,
        path: &str,
        node: &Value,
        pointer: Vec<String>,
        depth: usize,
    ) -> Result<NodeKey, SchemaProfileFinding> {
        if !node.is_object() && !node.is_boolean() {
            return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
        }
        let key = NodeKey {
            path: path.to_string(),
            pointer: pointer.clone(),
        };
        self.nodes.insert(key.clone(), node.clone());
        self.edges.entry(key.clone()).or_default();

        if self.nodes.len() > self.limits.schema_nodes || depth > self.limits.schema_depth {
            return Err(SchemaProfileFinding::new(
                "SCHEMA_PROFILE_LIMIT",
                "schema node/depth count",
            ));
        }
        let Some(map) = node.as_object() else {
            return Ok(key);
        };

        for k in map.keys() {
            if !self.allowed.contains(k.as_str()) {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_UNSUPPORTED_KEYWORD",
                    path,
                ));
            }
        }
        if map.contains_key("$schema") && !pointer.is_empty() {
            return Err(SchemaProfileFinding::new(
                "SCHEMA_INVALID",
                "nested dialect declaration",
            ));
        }
        if let Some(sens) = map.get("x-orion-sensitive")
            && !sens.is_boolean()
        {
            return Err(SchemaProfileFinding::new(
                "SCHEMA_INVALID",
                "sensitivity annotation",
            ));
        }
        validate_keyword_shapes(map, path)?;

        if let Some(r) = map.get("$ref") {
            self.references.push((key.clone(), r.clone()));
            if self.references.len() > self.limits.reference_edges {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_PROFILE_LIMIT",
                    "reference count",
                ));
            }
        }

        for keyword in ["$defs", "properties"] {
            if let Some(sub) = map.get(keyword) {
                let Some(sub_map) = sub.as_object() else {
                    return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
                };
                for (name, child) in sub_map {
                    let mut next_ptr = pointer.clone();
                    next_ptr.push(keyword.to_string());
                    next_ptr.push(name.clone());
                    let child_key = self.walk(path, child, next_ptr, depth + 1)?;
                    self.edges
                        .get_mut(&key)
                        .ok_or_else(|| SchemaProfileFinding::new("SCHEMA_INVALID", path))?
                        .push(child_key);
                }
            }
        }

        for keyword in ["items", "additionalProperties"] {
            if let Some(sub) = map.get(keyword) {
                let mut next_ptr = pointer.clone();
                next_ptr.push(keyword.to_string());
                let child_key = self.walk(path, sub, next_ptr, depth + 1)?;
                self.edges
                    .get_mut(&key)
                    .ok_or_else(|| SchemaProfileFinding::new("SCHEMA_INVALID", path))?
                    .push(child_key);
            }
        }

        for keyword in ["allOf", "anyOf", "oneOf"] {
            if let Some(sub) = map.get(keyword) {
                let Some(children) = sub.as_array() else {
                    return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
                };
                if children.is_empty() {
                    return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
                }
                if children.len() > self.limits.composition_branches {
                    return Err(SchemaProfileFinding::new(
                        "SCHEMA_PROFILE_LIMIT",
                        "composition branches",
                    ));
                }
                for (idx, child) in children.iter().enumerate() {
                    let mut next_ptr = pointer.clone();
                    next_ptr.push(keyword.to_string());
                    next_ptr.push(idx.to_string());
                    let child_key = self.walk(path, child, next_ptr, depth + 1)?;
                    self.edges
                        .get_mut(&key)
                        .ok_or_else(|| SchemaProfileFinding::new("SCHEMA_INVALID", path))?
                        .push(child_key);
                }
            }
        }

        Ok(key)
    }
}

impl CompiledSchemaBundle {
    pub fn compile(
        schemas: BTreeMap<String, Value>,
        limits: SchemaProfileLimits,
    ) -> Result<Self, SchemaProfileFinding> {
        if schemas.len() > limits.schema_files {
            return Err(SchemaProfileFinding::new(
                "SCHEMA_PROFILE_LIMIT",
                "schema file count",
            ));
        }

        let mut walk = SchemaWalk {
            limits: &limits,
            allowed: ALLOWED_KEYWORDS.iter().copied().collect(),
            nodes: HashMap::new(),
            edges: HashMap::new(),
            references: Vec::new(),
        };
        let mut bundle_bytes = 0usize;

        for (path, schema) in &schemas {
            validate_safe_package_path(path, "SCHEMA_REFERENCE_DENIED")?;
            let Some(map) = schema.as_object() else {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_INVALID",
                    "missing/unsupported root dialect",
                ));
            };
            if map.get("$schema").and_then(Value::as_str) != Some(DIALECT_URI) {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_INVALID",
                    "missing/unsupported root dialect",
                ));
            }
            let encoded = serde_json::to_vec(schema)
                .map_err(|_| SchemaProfileFinding::new("SCHEMA_INVALID", "serialization error"))?;
            bundle_bytes += encoded.len();
            if encoded.len() > limits.schema_file_bytes || bundle_bytes > limits.schema_bundle_bytes
            {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_PROFILE_LIMIT",
                    "serialized example schema size",
                ));
            }
            walk.walk(path, schema, Vec::new(), 1)?;
        }
        let SchemaWalk {
            nodes,
            mut edges,
            references,
            ..
        } = walk;

        for (key, ref_val) in references {
            let Some(ref_str) = ref_val.as_str() else {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_REFERENCE_DENIED",
                    "reference URI",
                ));
            };
            if ref_str.is_empty()
                || ref_str.matches('#').count() > 1
                || ref_str.contains(['%', '?', '\\', ':'])
            {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_REFERENCE_DENIED",
                    "reference URI",
                ));
            }
            let (relative, fragment) = match ref_str.split_once('#') {
                Some((rel, frag)) => (rel, frag),
                None => (ref_str, ""),
            };
            if !relative.is_empty() {
                validate_safe_package_path(relative, "SCHEMA_REFERENCE_DENIED")?;
            }
            let target_path = if relative.is_empty() {
                key.path.clone()
            } else {
                match Path::new(&key.path).parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => {
                        format!("{}/{}", parent.to_string_lossy(), relative)
                    }
                    _ => relative.to_string(),
                }
            };
            let Some(target_doc) = schemas.get(&target_path) else {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_REFERENCE_MISSING",
                    target_path,
                ));
            };
            let pointer = parse_json_pointer(fragment)?;
            if resolve_pointer(target_doc, &pointer).is_none() {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_REFERENCE_MISSING",
                    target_path,
                ));
            }
            let target_key = NodeKey {
                path: target_path.clone(),
                pointer,
            };
            if !nodes.contains_key(&target_key) {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_REFERENCE_NOT_SCHEMA",
                    target_path,
                ));
            }
            edges
                .get_mut(&key)
                .ok_or_else(|| SchemaProfileFinding::new("SCHEMA_INVALID", key.path.clone()))?
                .push(target_key);
        }

        // Cycle check
        let mut colors: HashMap<NodeKey, u8> = HashMap::new();
        for start in nodes.keys() {
            let mut stack = vec![(start.clone(), false)];
            while let Some((curr, leaving)) = stack.pop() {
                if leaving {
                    colors.insert(curr, 2);
                } else if colors.get(&curr) == Some(&1) {
                    return Err(SchemaProfileFinding::new(
                        "SCHEMA_REFERENCE_CYCLE",
                        curr.path,
                    ));
                } else if colors.get(&curr) != Some(&2) {
                    colors.insert(curr.clone(), 1);
                    stack.push((curr.clone(), true));
                    if let Some(children) = edges.get(&curr) {
                        for child in children.iter().rev() {
                            stack.push((child.clone(), false));
                        }
                    }
                }
            }
        }

        Ok(Self { schemas, limits })
    }

    /// Validate `instance` against `schema_path`, metering validation steps
    /// against `max_steps` (`self.limits.validation_steps`).
    pub fn validate_instance(
        &self,
        schema_path: &str,
        instance: &Value,
    ) -> Result<(), SchemaProfileFinding> {
        let mut steps = 0u64;
        self.validate_instance_metered(schema_path, instance, &mut steps)
    }

    pub fn validate_instance_metered(
        &self,
        schema_path: &str,
        instance: &Value,
        steps: &mut u64,
    ) -> Result<(), SchemaProfileFinding> {
        let mut node_count = 0usize;
        check_instance_bounds(instance, 1, &self.limits, &mut node_count)?;
        self.eval_node(schema_path, &[], instance, steps)
    }

    fn eval_node(
        &self,
        schema_path: &str,
        pointer: &[String],
        instance: &Value,
        steps: &mut u64,
    ) -> Result<(), SchemaProfileFinding> {
        *steps += 1;
        if *steps > self.limits.validation_steps {
            return Err(SchemaProfileFinding::new(
                "SCHEMA_PROFILE_LIMIT",
                "validation steps exhausted",
            ));
        }

        let doc = self
            .schemas
            .get(schema_path)
            .ok_or_else(|| SchemaProfileFinding::new("SCHEMA_REFERENCE_MISSING", schema_path))?;
        let node = resolve_pointer(doc, pointer)
            .ok_or_else(|| SchemaProfileFinding::new("SCHEMA_REFERENCE_MISSING", schema_path))?;

        match node {
            Value::Bool(true) => Ok(()),
            Value::Bool(false) => Err(SchemaProfileFinding::new(
                "CONNECTOR_INVALID_INPUT",
                "boolean false schema",
            )),
            Value::Object(map) => {
                if let Some(ref_str) = map.get("$ref").and_then(Value::as_str) {
                    let (relative, fragment) = match ref_str.split_once('#') {
                        Some((rel, frag)) => (rel, frag),
                        None => (ref_str, ""),
                    };
                    let target_path = if relative.is_empty() {
                        schema_path.to_string()
                    } else {
                        match Path::new(schema_path).parent() {
                            Some(parent) if !parent.as_os_str().is_empty() => {
                                format!("{}/{}", parent.to_string_lossy(), relative)
                            }
                            _ => relative.to_string(),
                        }
                    };
                    let target_ptr = parse_json_pointer(fragment)?;
                    self.eval_node(&target_path, &target_ptr, instance, steps)?;
                }

                if let Some(ty) = map.get("type")
                    && !matches_type(ty, instance)
                {
                    return Err(SchemaProfileFinding::new("CONNECTOR_INVALID_INPUT", "type"));
                }
                if let Some(const_val) = map.get("const") {
                    *steps += 1;
                    if instance != const_val {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "const",
                        ));
                    }
                }
                if let Some(Value::Array(enum_vals)) = map.get("enum") {
                    let mut matched = false;
                    for item in enum_vals {
                        *steps += 1;
                        if instance == item {
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        return Err(SchemaProfileFinding::new("CONNECTOR_INVALID_INPUT", "enum"));
                    }
                }

                if let Some(num) = instance.as_f64() {
                    if let Some(min) = map.get("minimum").and_then(Value::as_f64)
                        && num < min
                    {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "minimum",
                        ));
                    }
                    if let Some(max) = map.get("maximum").and_then(Value::as_f64)
                        && num > max
                    {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "maximum",
                        ));
                    }
                    if let Some(ex_min) = map.get("exclusiveMinimum").and_then(Value::as_f64)
                        && num <= ex_min
                    {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "exclusiveMinimum",
                        ));
                    }
                    if let Some(ex_max) = map.get("exclusiveMaximum").and_then(Value::as_f64)
                        && num >= ex_max
                    {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "exclusiveMaximum",
                        ));
                    }
                }

                if let Some(s) = instance.as_str() {
                    let char_len = s.chars().count() as u64;
                    if let Some(min_l) = map.get("minLength").and_then(Value::as_u64)
                        && char_len < min_l
                    {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "minLength",
                        ));
                    }
                    if let Some(max_l) = map.get("maxLength").and_then(Value::as_u64)
                        && char_len > max_l
                    {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "maxLength",
                        ));
                    }
                }

                if let Some(arr) = instance.as_array() {
                    let len = arr.len() as u64;
                    if let Some(min_i) = map.get("minItems").and_then(Value::as_u64)
                        && len < min_i
                    {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "minItems",
                        ));
                    }
                    if let Some(max_i) = map.get("maxItems").and_then(Value::as_u64)
                        && len > max_i
                    {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "maxItems",
                        ));
                    }
                    if map.contains_key("items") {
                        let mut item_ptr = pointer.to_vec();
                        item_ptr.push("items".to_string());
                        for (index, elem) in arr.iter().enumerate() {
                            self.eval_node(schema_path, &item_ptr, elem, steps)
                                .map_err(|f| f.within(&index.to_string()))?;
                        }
                    }
                }

                if let Some(obj) = instance.as_object() {
                    let len = obj.len() as u64;
                    if let Some(min_p) = map.get("minProperties").and_then(Value::as_u64)
                        && len < min_p
                    {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "minProperties",
                        ));
                    }
                    if let Some(max_p) = map.get("maxProperties").and_then(Value::as_u64)
                        && len > max_p
                    {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "maxProperties",
                        ));
                    }
                    if let Some(Value::Array(req)) = map.get("required") {
                        for r in req {
                            if let Some(r_str) = r.as_str()
                                && !obj.contains_key(r_str)
                            {
                                return Err(SchemaProfileFinding::new(
                                    "CONNECTOR_INVALID_INPUT",
                                    "required",
                                )
                                .within(r_str));
                            }
                        }
                    }
                    let props = map.get("properties").and_then(Value::as_object);
                    for (k, v) in obj {
                        if let Some(p_map) = props
                            && p_map.contains_key(k)
                        {
                            let mut prop_ptr = pointer.to_vec();
                            prop_ptr.push("properties".to_string());
                            prop_ptr.push(k.clone());
                            self.eval_node(schema_path, &prop_ptr, v, steps)
                                .map_err(|f| f.within(k))?;
                        } else if map.contains_key("additionalProperties") {
                            let mut add_ptr = pointer.to_vec();
                            add_ptr.push("additionalProperties".to_string());
                            self.eval_node(schema_path, &add_ptr, v, steps)
                                .map_err(|f| f.within(k))?;
                        }
                    }
                }

                if let Some(Value::Array(branches)) = map.get("allOf") {
                    for idx in 0..branches.len() {
                        let mut b_ptr = pointer.to_vec();
                        b_ptr.push("allOf".to_string());
                        b_ptr.push(idx.to_string());
                        self.eval_node(schema_path, &b_ptr, instance, steps)?;
                    }
                }
                if let Some(Value::Array(branches)) = map.get("anyOf") {
                    let mut any_ok = false;
                    for idx in 0..branches.len() {
                        let mut b_ptr = pointer.to_vec();
                        b_ptr.push("anyOf".to_string());
                        b_ptr.push(idx.to_string());
                        if self.eval_node(schema_path, &b_ptr, instance, steps).is_ok() {
                            any_ok = true;
                        }
                    }
                    if !any_ok {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "anyOf",
                        ));
                    }
                }
                if let Some(Value::Array(branches)) = map.get("oneOf") {
                    let mut matches = 0usize;
                    for idx in 0..branches.len() {
                        let mut b_ptr = pointer.to_vec();
                        b_ptr.push("oneOf".to_string());
                        b_ptr.push(idx.to_string());
                        if self.eval_node(schema_path, &b_ptr, instance, steps).is_ok() {
                            matches += 1;
                        }
                    }
                    if matches != 1 {
                        return Err(SchemaProfileFinding::new(
                            "CONNECTOR_INVALID_INPUT",
                            "oneOf",
                        ));
                    }
                }

                Ok(())
            }
            _ => Err(SchemaProfileFinding::new("SCHEMA_INVALID", schema_path)),
        }
    }
}

fn check_instance_bounds(
    instance: &Value,
    depth: usize,
    limits: &SchemaProfileLimits,
    nodes: &mut usize,
) -> Result<(), SchemaProfileFinding> {
    *nodes += 1;
    if *nodes > limits.instance_nodes || depth > limits.instance_depth {
        return Err(SchemaProfileFinding::new(
            "DOCUMENT_LIMIT_EXCEEDED",
            "instance node/depth limit",
        ));
    }
    match instance {
        Value::Number(n) => {
            if let Some(i) = n.as_i64()
                && !(MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&i)
            {
                return Err(SchemaProfileFinding::new(
                    "DOCUMENT_INVALID_JSON",
                    "integer outside RFC 7493 safe range",
                ));
            }
            if let Some(u) = n.as_u64()
                && u > MAX_SAFE_INTEGER as u64
            {
                return Err(SchemaProfileFinding::new(
                    "DOCUMENT_INVALID_JSON",
                    "integer outside RFC 7493 safe range",
                ));
            }
        }
        Value::Array(items) => {
            for item in items {
                check_instance_bounds(item, depth + 1, limits, nodes)?;
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                check_instance_bounds(v, depth + 1, limits, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_keyword_shapes(
    map: &Map<String, Value>,
    path: &str,
) -> Result<(), SchemaProfileFinding> {
    let valid_types = [
        "null", "boolean", "object", "array", "number", "string", "integer",
    ];
    if let Some(t) = map.get("type") {
        let ok = match t {
            Value::String(s) => valid_types.contains(&s.as_str()),
            Value::Array(arr) => {
                !arr.is_empty()
                    && arr
                        .iter()
                        .all(|item| item.as_str().is_some_and(|s| valid_types.contains(&s)))
            }
            _ => false,
        };
        if !ok {
            return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
        }
    }
    for kw in ["title", "description"] {
        if let Some(v) = map.get(kw)
            && !v.is_string()
        {
            return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
        }
    }
    for kw in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        if let Some(v) = map.get(kw)
            && !v.is_number()
        {
            return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
        }
    }
    for kw in [
        "minProperties",
        "maxProperties",
        "minItems",
        "maxItems",
        "minLength",
        "maxLength",
    ] {
        if let Some(v) = map.get(kw)
            && v.as_u64().is_none()
        {
            return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
        }
    }
    if let Some(req) = map.get("required") {
        let Some(arr) = req.as_array() else {
            return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
        };
        let mut seen = HashSet::new();
        for item in arr {
            let Some(s) = item.as_str() else {
                return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
            };
            if !seen.insert(s) {
                return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
            }
        }
    }
    if let Some(en) = map.get("enum")
        && !en.is_array()
    {
        return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
    }
    Ok(())
}

fn matches_type(ty: &Value, instance: &Value) -> bool {
    let check_single = |name: &str| match name {
        "null" => instance.is_null(),
        "boolean" => instance.is_boolean(),
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "number" => instance.is_number(),
        "integer" => {
            instance.as_i64().is_some()
                || instance.as_u64().is_some()
                || instance
                    .as_f64()
                    .is_some_and(|f| f.fract() == 0.0 && f.is_finite())
        }
        _ => false,
    };
    match ty {
        Value::String(s) => check_single(s),
        Value::Array(arr) => arr.iter().filter_map(Value::as_str).any(check_single),
        _ => false,
    }
}

#[cfg(test)]
mod tests;

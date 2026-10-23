//! V2-10.2 Constrained JSON Schema profile (`orion-json-schema/1`), RFC 7493
//! I-JSON parser, manifest validator, and step-metered instance validator.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
}

impl SchemaProfileFinding {
    pub fn new(code: impl Into<String>, location: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            location: location.into(),
        }
    }
}

/// Strict RFC 7493 (I-JSON) parser that rejects duplicate object keys,
/// non-finite numbers, out-of-range exponents, and integers outside
/// `[-(2^53 - 1), 2^53 - 1]` when `enforce_safe_int` is enabled.
pub fn parse_strict_json(raw: &str, enforce_safe_int: bool) -> Result<Value, SchemaProfileFinding> {
    let bytes = raw.as_bytes();
    let mut idx = 0;
    skip_ws(bytes, &mut idx);
    let val = parse_json_value(bytes, &mut idx, enforce_safe_int, 1)?;
    skip_ws(bytes, &mut idx);
    if idx != bytes.len() {
        return Err(SchemaProfileFinding::new(
            "DOCUMENT_INVALID_JSON",
            "trailing tokens",
        ));
    }
    Ok(val)
}

fn skip_ws(bytes: &[u8], idx: &mut usize) {
    while *idx < bytes.len() && matches!(bytes[*idx], b' ' | b'\t' | b'\r' | b'\n') {
        *idx += 1;
    }
}

fn parse_json_value(
    bytes: &[u8],
    idx: &mut usize,
    enforce_safe_int: bool,
    depth: usize,
) -> Result<Value, SchemaProfileFinding> {
    if depth > 64 {
        return Err(SchemaProfileFinding::new(
            "DOCUMENT_INVALID_JSON",
            "nesting limit exceeded",
        ));
    }
    skip_ws(bytes, idx);
    if *idx >= bytes.len() {
        return Err(SchemaProfileFinding::new(
            "DOCUMENT_INVALID_JSON",
            "unexpected EOF",
        ));
    }
    match bytes[*idx] {
        b'{' => {
            *idx += 1;
            skip_ws(bytes, idx);
            let mut map = Map::new();
            if *idx < bytes.len() && bytes[*idx] == b'}' {
                *idx += 1;
                return Ok(Value::Object(map));
            }
            loop {
                skip_ws(bytes, idx);
                if *idx >= bytes.len() || bytes[*idx] != b'"' {
                    return Err(SchemaProfileFinding::new(
                        "DOCUMENT_INVALID_JSON",
                        "expected object key",
                    ));
                }
                let key = parse_json_string(bytes, idx)?;
                if map.contains_key(&key) {
                    return Err(SchemaProfileFinding::new(
                        "DOCUMENT_INVALID_JSON",
                        "duplicate key",
                    ));
                }
                skip_ws(bytes, idx);
                if *idx >= bytes.len() || bytes[*idx] != b':' {
                    return Err(SchemaProfileFinding::new(
                        "DOCUMENT_INVALID_JSON",
                        "expected colon",
                    ));
                }
                *idx += 1;
                let value = parse_json_value(bytes, idx, enforce_safe_int, depth + 1)?;
                map.insert(key, value);
                skip_ws(bytes, idx);
                if *idx >= bytes.len() {
                    return Err(SchemaProfileFinding::new(
                        "DOCUMENT_INVALID_JSON",
                        "unclosed object",
                    ));
                }
                match bytes[*idx] {
                    b',' => {
                        *idx += 1;
                    }
                    b'}' => {
                        *idx += 1;
                        return Ok(Value::Object(map));
                    }
                    _ => {
                        return Err(SchemaProfileFinding::new(
                            "DOCUMENT_INVALID_JSON",
                            "invalid object separator",
                        ));
                    }
                }
            }
        }
        b'[' => {
            *idx += 1;
            skip_ws(bytes, idx);
            let mut items = Vec::new();
            if *idx < bytes.len() && bytes[*idx] == b']' {
                *idx += 1;
                return Ok(Value::Array(items));
            }
            loop {
                let value = parse_json_value(bytes, idx, enforce_safe_int, depth + 1)?;
                items.push(value);
                skip_ws(bytes, idx);
                if *idx >= bytes.len() {
                    return Err(SchemaProfileFinding::new(
                        "DOCUMENT_INVALID_JSON",
                        "unclosed array",
                    ));
                }
                match bytes[*idx] {
                    b',' => {
                        *idx += 1;
                    }
                    b']' => {
                        *idx += 1;
                        return Ok(Value::Array(items));
                    }
                    _ => {
                        return Err(SchemaProfileFinding::new(
                            "DOCUMENT_INVALID_JSON",
                            "invalid array separator",
                        ));
                    }
                }
            }
        }
        b'"' => Ok(Value::String(parse_json_string(bytes, idx)?)),
        b't' if bytes[*idx..].starts_with(b"true") => {
            *idx += 4;
            Ok(Value::Bool(true))
        }
        b'f' if bytes[*idx..].starts_with(b"false") => {
            *idx += 5;
            Ok(Value::Bool(false))
        }
        b'n' if bytes[*idx..].starts_with(b"null") => {
            *idx += 4;
            Ok(Value::Null)
        }
        b'-' | b'0'..=b'9' => parse_json_number(bytes, idx, enforce_safe_int),
        _ => Err(SchemaProfileFinding::new(
            "DOCUMENT_INVALID_JSON",
            "invalid token",
        )),
    }
}

fn parse_json_string(bytes: &[u8], idx: &mut usize) -> Result<String, SchemaProfileFinding> {
    let start = *idx;
    *idx += 1; // skip opening quote
    while *idx < bytes.len() {
        match bytes[*idx] {
            b'"' => {
                *idx += 1;
                let slice = std::str::from_utf8(&bytes[start..*idx]).map_err(|_| {
                    SchemaProfileFinding::new("DOCUMENT_INVALID_JSON", "invalid utf8")
                })?;
                return serde_json::from_str(slice).map_err(|_| {
                    SchemaProfileFinding::new("DOCUMENT_INVALID_JSON", "invalid string escape")
                });
            }
            b'\\' => {
                *idx += 2;
            }
            c if c < 0x20 => {
                return Err(SchemaProfileFinding::new(
                    "DOCUMENT_INVALID_JSON",
                    "unescaped control character",
                ));
            }
            _ => {
                *idx += 1;
            }
        }
    }
    Err(SchemaProfileFinding::new(
        "DOCUMENT_INVALID_JSON",
        "unterminated string",
    ))
}

fn parse_json_number(
    bytes: &[u8],
    idx: &mut usize,
    enforce_safe_int: bool,
) -> Result<Value, SchemaProfileFinding> {
    let start = *idx;
    while *idx < bytes.len()
        && matches!(
            bytes[*idx],
            b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'
        )
    {
        *idx += 1;
    }
    let token = std::str::from_utf8(&bytes[start..*idx])
        .map_err(|_| SchemaProfileFinding::new("DOCUMENT_INVALID_JSON", "invalid number utf8"))?;
    let is_float = token.contains('.') || token.contains('e') || token.contains('E');
    if is_float {
        let parsed: f64 = token.parse().map_err(|_| {
            SchemaProfileFinding::new("DOCUMENT_INVALID_JSON", "invalid float literal")
        })?;
        if !parsed.is_finite() {
            return Err(SchemaProfileFinding::new(
                "DOCUMENT_INVALID_JSON",
                "unsupported numeric range",
            ));
        }
        let num = serde_json::Number::from_f64(parsed).ok_or_else(|| {
            SchemaProfileFinding::new("DOCUMENT_INVALID_JSON", "non-finite number")
        })?;
        let check: Value = serde_json::from_str(token).map_err(|_| {
            SchemaProfileFinding::new("DOCUMENT_INVALID_JSON", "invalid float syntax")
        })?;
        if !check.is_number() {
            return Err(SchemaProfileFinding::new(
                "DOCUMENT_INVALID_JSON",
                "invalid number",
            ));
        }
        Ok(Value::Number(num))
    } else {
        let check: Value = serde_json::from_str(token).map_err(|_| {
            SchemaProfileFinding::new("DOCUMENT_INVALID_JSON", "integer outside representable range")
        })?;
        let parsed: i128 = token.parse().map_err(|_| {
            SchemaProfileFinding::new("DOCUMENT_INVALID_JSON", "integer outside RFC 7493 safe range")
        })?;
        if enforce_safe_int
            && !(i128::from(MIN_SAFE_INTEGER)..=i128::from(MAX_SAFE_INTEGER)).contains(&parsed)
        {
            return Err(SchemaProfileFinding::new(
                "DOCUMENT_INVALID_JSON",
                "integer outside RFC 7493 safe range",
            ));
        }
        Ok(check)
    }
}

pub fn validate_safe_package_path(
    path: &str,
    code: &str,
) -> Result<(), SchemaProfileFinding> {
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
        if bytes[i] == b'~' {
            if i + 1 >= bytes.len() || (bytes[i + 1] != b'0' && bytes[i + 1] != b'1') {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_REFERENCE_DENIED",
                    "invalid JSON Pointer",
                ));
            }
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
                if part != "0" && (part.starts_with('0') || !part.bytes().all(|b| b.is_ascii_digit()))
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

        let allowed: HashSet<&str> = ALLOWED_KEYWORDS.iter().copied().collect();
        let mut nodes: HashMap<NodeKey, Value> = HashMap::new();
        let mut edges: HashMap<NodeKey, Vec<NodeKey>> = HashMap::new();
        let mut references: Vec<(NodeKey, Value)> = Vec::new();
        let mut bundle_bytes = 0usize;

        #[allow(clippy::too_many_arguments)]
        fn walk(
            path: &str,
            node: &Value,
            pointer: Vec<String>,
            depth: usize,
            limits: &SchemaProfileLimits,
            allowed: &HashSet<&str>,
            nodes: &mut HashMap<NodeKey, Value>,
            edges: &mut HashMap<NodeKey, Vec<NodeKey>>,
            references: &mut Vec<(NodeKey, Value)>,
        ) -> Result<NodeKey, SchemaProfileFinding> {
            if !node.is_object() && !node.is_boolean() {
                return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
            }
            let key = NodeKey {
                path: path.to_string(),
                pointer: pointer.clone(),
            };
            nodes.insert(key.clone(), node.clone());
            edges.entry(key.clone()).or_default();

            if nodes.len() > limits.schema_nodes || depth > limits.schema_depth {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_PROFILE_LIMIT",
                    "schema node/depth count",
                ));
            }
            let Some(map) = node.as_object() else {
                return Ok(key);
            };

            for k in map.keys() {
                if !allowed.contains(k.as_str()) {
                    return Err(SchemaProfileFinding::new("SCHEMA_UNSUPPORTED_KEYWORD", path));
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
                references.push((key.clone(), r.clone()));
                if references.len() > limits.reference_edges {
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
                        let child_key = walk(
                            path,
                            child,
                            next_ptr,
                            depth + 1,
                            limits,
                            allowed,
                            nodes,
                            edges,
                            references,
                        )?;
                        edges.get_mut(&key).unwrap().push(child_key);
                    }
                }
            }

            for keyword in ["items", "additionalProperties"] {
                if let Some(sub) = map.get(keyword) {
                    let mut next_ptr = pointer.clone();
                    next_ptr.push(keyword.to_string());
                    let child_key = walk(
                        path,
                        sub,
                        next_ptr,
                        depth + 1,
                        limits,
                        allowed,
                        nodes,
                        edges,
                        references,
                    )?;
                    edges.get_mut(&key).unwrap().push(child_key);
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
                    if children.len() > limits.composition_branches {
                        return Err(SchemaProfileFinding::new(
                            "SCHEMA_PROFILE_LIMIT",
                            "composition branches",
                        ));
                    }
                    for (idx, child) in children.iter().enumerate() {
                        let mut next_ptr = pointer.clone();
                        next_ptr.push(keyword.to_string());
                        next_ptr.push(idx.to_string());
                        let child_key = walk(
                            path,
                            child,
                            next_ptr,
                            depth + 1,
                            limits,
                            allowed,
                            nodes,
                            edges,
                            references,
                        )?;
                        edges.get_mut(&key).unwrap().push(child_key);
                    }
                }
            }

            Ok(key)
        }

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
            let encoded = serde_json::to_vec(schema).map_err(|_| {
                SchemaProfileFinding::new("SCHEMA_INVALID", "serialization error")
            })?;
            bundle_bytes += encoded.len();
            if encoded.len() > limits.schema_file_bytes || bundle_bytes > limits.schema_bundle_bytes
            {
                return Err(SchemaProfileFinding::new(
                    "SCHEMA_PROFILE_LIMIT",
                    "serialized example schema size",
                ));
            }
            walk(
                path,
                schema,
                Vec::new(),
                1,
                &limits,
                &allowed,
                &mut nodes,
                &mut edges,
                &mut references,
            )?;
        }

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
            edges.get_mut(&key).unwrap().push(target_key);
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
    pub fn validate_instance(&self, schema_path: &str, instance: &Value) -> Result<(), SchemaProfileFinding> {
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

        let doc = self.schemas.get(schema_path).ok_or_else(|| {
            SchemaProfileFinding::new("SCHEMA_REFERENCE_MISSING", schema_path)
        })?;
        let node = resolve_pointer(doc, pointer).ok_or_else(|| {
            SchemaProfileFinding::new("SCHEMA_REFERENCE_MISSING", schema_path)
        })?;

        match node {
            Value::Bool(true) => return Ok(()),
            Value::Bool(false) => {
                return Err(SchemaProfileFinding::new(
                    "CONNECTOR_INVALID_INPUT",
                    "boolean false schema",
                ));
            }
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

                if let Some(ty) = map.get("type") && !matches_type(ty, instance) {
                    return Err(SchemaProfileFinding::new("CONNECTOR_INVALID_INPUT", "type"));
                }
                if let Some(const_val) = map.get("const") {
                    *steps += 1;
                    if instance != const_val {
                        return Err(SchemaProfileFinding::new("CONNECTOR_INVALID_INPUT", "const"));
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
                    if let Some(min) = map.get("minimum").and_then(Value::as_f64) && num < min {
                        return Err(SchemaProfileFinding::new("CONNECTOR_INVALID_INPUT", "minimum"));
                    }
                    if let Some(max) = map.get("maximum").and_then(Value::as_f64) && num > max {
                        return Err(SchemaProfileFinding::new("CONNECTOR_INVALID_INPUT", "maximum"));
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
                        for elem in arr {
                            self.eval_node(schema_path, &item_ptr, elem, steps)?;
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
                            if let Some(r_str) = r.as_str() && !obj.contains_key(r_str) {
                                return Err(SchemaProfileFinding::new(
                                    "CONNECTOR_INVALID_INPUT",
                                    "required",
                                ));
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
                            self.eval_node(schema_path, &prop_ptr, v, steps)?;
                        } else if map.contains_key("additionalProperties") {
                            let mut add_ptr = pointer.to_vec();
                            add_ptr.push("additionalProperties".to_string());
                            self.eval_node(schema_path, &add_ptr, v, steps)?;
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
                        return Err(SchemaProfileFinding::new("CONNECTOR_INVALID_INPUT", "anyOf"));
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
                        return Err(SchemaProfileFinding::new("CONNECTOR_INVALID_INPUT", "oneOf"));
                    }
                }

                Ok(())
            }
            _ => Err(SchemaProfileFinding::new("SCHEMA_INVALID", schema_path)),
        }
    }

    /// Omit configuration fields marked `x-orion-sensitive: true` (and their
    /// descendants) when building public projections.
    pub fn project_public_configuration(
        &self,
        schema_path: &str,
        config: &Value,
    ) -> Value {
        let Some(doc) = self.schemas.get(schema_path) else {
            return config.clone();
        };
        redact_sensitive_node(doc, config)
    }
}

fn redact_sensitive_node(schema: &Value, value: &Value) -> Value {
    let Some(map) = schema.as_object() else {
        return value.clone();
    };
    if map.get("x-orion-sensitive").and_then(Value::as_bool) == Some(true) {
        return Value::String("[REDACTED]".to_string());
    }
    if let (Some(props), Some(obj)) = (
        map.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) {
        let mut out = Map::new();
        for (k, v) in obj {
            if let Some(prop_schema) = props.get(k) {
                if prop_schema
                    .get("x-orion-sensitive")
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    continue;
                }
                out.insert(k.clone(), redact_sensitive_node(prop_schema, v));
            } else {
                out.insert(k.clone(), v.clone());
            }
        }
        return Value::Object(out);
    }
    value.clone()
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

fn validate_keyword_shapes(map: &Map<String, Value>, path: &str) -> Result<(), SchemaProfileFinding> {
    let valid_types = ["null", "boolean", "object", "array", "number", "string", "integer"];
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
        if let Some(v) = map.get(kw) && !v.is_string() {
            return Err(SchemaProfileFinding::new("SCHEMA_INVALID", path));
        }
    }
    for kw in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        if let Some(v) = map.get(kw) && !v.is_number() {
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
        if let Some(v) = map.get(kw) && v.as_u64().is_none() {
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
    if let Some(en) = map.get("enum") && !en.is_array() {
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
        Value::Array(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .any(check_single),
        _ => false,
    }
}

/// Validate a connector manifest bundle (`manifest`, `files`, `schemas`) against
/// the V2-10.2 shape and relationship rules, returning a compiled schema bundle.
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
    if operations.keys().any(|name| name.starts_with("orion.internal.")) {
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
        .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "implementation.kind"))?;
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
        let alias = dep
            .get("alias")
            .and_then(Value::as_str)
            .ok_or_else(|| SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "dependency.alias"))?;
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
            return Err(SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "sources"));
        };
        if s_map.is_empty() {
            return Err(SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "sources"));
        }
        for s_val in s_map.values() {
            let Some(facet) = s_val.as_object() else {
                return Err(SchemaProfileFinding::new("MANIFEST_SHAPE_INVALID", "source"));
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

#[cfg(test)]
mod tests {
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
    fn v2_10_2_passes_all_36_schema_and_manifest_fixture_cases() {
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../design-docs/v2/fixtures/schema-validation");
        let cases_raw = std::fs::read_to_string(fixture_dir.join("cases.json")).unwrap();
        let catalog: Value = serde_json::from_str(&cases_raw).unwrap();
        let base_raw = std::fs::read_to_string(fixture_dir.join("bundle.json")).unwrap();
        let base: Value = serde_json::from_str(&base_raw).unwrap();

        let cases = catalog["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 42);

        for case in cases {
            let id = case["id"].as_str().unwrap();
            let expected = case["expect"].as_str().unwrap();
            let observed = if let Some(raw_json) = case.get("raw_json").and_then(Value::as_str) {
                match parse_strict_json(raw_json, true) {
                    Ok(_) => "OK".to_string(),
                    Err(f) => f.code,
                }
            } else {
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
    fn v2_10_2_enforces_step_metering_and_sensitive_projection() {
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
        let mut limits = SchemaProfileLimits::default();
        limits.validation_steps = 2;
        let bundle = CompiledSchemaBundle::compile(schemas, limits).unwrap();
        let projected = bundle.project_public_configuration(
            "schemas/config.json",
            &serde_json::json!({
                "base_url": "https://api.example.com",
                "client_secret": "super-secret"
            }),
        );
        assert_eq!(
            projected,
            serde_json::json!({ "base_url": "https://api.example.com" })
        );

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
}

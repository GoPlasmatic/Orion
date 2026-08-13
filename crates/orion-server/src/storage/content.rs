//! Canonical importable-content projections and their hash (K2, K10).
//!
//! One definition of "an entity's content" per kind, shared by three
//! consumers that must never disagree:
//!
//! - the upsert import's `unchanged` detection (K2) compares a stored row's
//!   projection against an incoming create-shaped item's;
//! - every entity response carries `content_hash` over the same projection
//!   (K10), so tooling can compare estates without fetching bodies;
//! - the package CLI hashes artifact entries with the same canonical form,
//!   so its `content_hash` and `diff` agree with the server.
//!
//! The projection is the **importable content**: exactly the fields the
//! create-shaped import consumes, spelled in the request vocabulary. The
//! DB-owned fields — `version`, `status`, timestamps, `rollout_percentage`,
//! connector `id` — are excluded, which is what makes a re-import of an
//! unmodified export hash equal.
//!
//! Hashes are computed over **stored** values. A connector holding literal
//! secrets therefore hashes over the secrets it stores, which a masked
//! export can never reproduce — the same boundary the round-trip contract
//! already draws: only `env://`/`vault://`-authored entities promote, and
//! for those stored == artifact.

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::errors::OrionError;
use crate::storage::models::{Channel, Connector, Workflow};
use crate::storage::repositories::channels::CreateChannelRequest;
use crate::storage::repositories::connectors::CreateConnectorRequest;
use crate::storage::repositories::workflows::CreateWorkflowRequest;

// ============================================================
// Per-kind projections: stored row and create-shaped request
// ============================================================

/// A workflow row's importable content, mirroring
/// [`workflow_request_content`].
/// `loop` is projected **only when present**, on both sides. Adding a key
/// unconditionally would have changed the hash of every workflow that predates
/// the column, which is not a cosmetic problem: applied package versions are
/// content-immutable, so a re-`apply` of an unmodified artifact would stop
/// being a no-op and start returning `409`. Absent in, absent out.
pub fn workflow_content(w: &Workflow) -> Result<Value, OrionError> {
    let mut content = serde_json::json!({
        "name": w.name,
        "description": w.description,
        "priority": w.priority,
        "condition": serde_json::from_str::<Value>(&w.condition_json)?,
        "tasks": serde_json::from_str::<Value>(&w.tasks_json)?,
        "tags": serde_json::from_str::<Value>(&w.tags_json)?,
        "continue_on_error": w.continue_on_error,
    });
    if let Some(loop_json) = w.loop_json.as_deref() {
        content["loop"] = serde_json::from_str::<Value>(loop_json)?;
    }
    Ok(content)
}

pub fn workflow_request_content(r: &CreateWorkflowRequest) -> Value {
    let mut content = serde_json::json!({
        "name": r.name,
        "description": r.description,
        "priority": r.priority,
        "condition": r.condition,
        "tasks": r.tasks,
        "tags": r.tags,
        "continue_on_error": r.continue_on_error,
    });
    if let Some(loop_config) = &r.loop_config {
        content["loop"] = loop_config.clone();
    }
    content
}

/// A channel row's importable content, mirroring
/// [`channel_request_content`].
pub fn channel_content(c: &Channel) -> Result<Value, OrionError> {
    let methods = c
        .methods_json
        .as_deref()
        .map(serde_json::from_str::<Value>)
        .transpose()?;
    Ok(serde_json::json!({
        "name": c.name,
        "description": c.description,
        "channel_type": c.channel_type,
        "protocol": c.protocol,
        "methods": methods,
        "route_pattern": c.route_pattern,
        "topic": c.topic,
        "consumer_group": c.consumer_group,
        "transport_config": serde_json::from_str::<Value>(&c.transport_config_json)?,
        "workflow_id": c.workflow_id,
        "config": serde_json::from_str::<Value>(&c.config_json)?,
        "priority": c.priority,
        "tags": serde_json::from_str::<Value>(&c.tags_json)?,
    }))
}

pub fn channel_request_content(r: &CreateChannelRequest) -> Value {
    serde_json::json!({
        "name": r.name,
        "description": r.description,
        "channel_type": r.channel_type.as_str(),
        "protocol": r.protocol.as_str(),
        "methods": r.methods,
        "route_pattern": r.route_pattern,
        "topic": r.topic,
        "consumer_group": r.consumer_group,
        "transport_config": r.transport_config,
        "workflow_id": r.workflow_id,
        "config": r.config,
        "priority": r.priority,
        "tags": r.tags,
    })
}

/// A connector row's importable content, mirroring
/// [`connector_request_content`]. `id` is excluded — the upsert matches on
/// `name` and keeps the stored id, so it is not part of the artifact
/// contract.
pub fn connector_content(c: &Connector) -> Result<Value, OrionError> {
    Ok(serde_json::json!({
        "name": c.name,
        "connector_type": c.connector_type,
        "config": serde_json::from_str::<Value>(&c.config_json)?,
        "enabled": c.enabled,
        "tags": serde_json::from_str::<Value>(&c.tags_json)?,
    }))
}

pub fn connector_request_content(r: &CreateConnectorRequest) -> Value {
    serde_json::json!({
        "name": r.name,
        "connector_type": r.connector_type.as_str(),
        "config": r.config,
        "enabled": r.enabled.unwrap_or(true),
        "tags": r.tags,
    })
}

// ============================================================
// Canonical form and hash
// ============================================================

/// Serialize with object keys sorted at every depth, compactly.
///
/// Explicit rather than relying on `serde_json`'s map ordering: the hash's
/// stability across builds must not hinge on whether `preserve_order` is
/// enabled somewhere in the dependency graph.
pub fn canonical_json(value: &Value) -> String {
    fn write(value: &Value, out: &mut String) {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort_unstable();
                out.push('{');
                for (i, key) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(key).expect("string serializes"));
                    out.push(':');
                    write(&map[key.as_str()], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            leaf => out.push_str(&serde_json::to_string(leaf).expect("scalar serializes")),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

/// `sha256:<hex>` over [`canonical_json`] — the spelling the package
/// receipts (K14) store and the CLI computes.
pub fn content_hash(value: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(value).as_bytes());
    // `hex::encode`, not `{:x}`: sha2 0.11 returns crypto-common's `Array`,
    // which — unlike the `GenericArray` it replaced — implements no LowerHex.
    // Same lowercase hex output, and this hash is the package-receipt
    // identity (K14) that the CLI recomputes, so the spelling cannot drift.
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The exact `sha256:<hex>` spelling, pinned against an outside authority.
    ///
    /// Every other test here is self-consistent — they compare two hashes this
    /// function produced, so a change in the *encoding* moves both and passes.
    /// That matters because a package receipt is content-immutable: the same
    /// version arriving with a different content_hash is a 409 (K14), so a
    /// re-spelled hash would reject every receipt already stored in an estate
    /// rather than fail anything here.
    ///
    /// The literal is not copied from this implementation's output. It is
    /// `shasum -a 256` over the canonical form asserted below, so this test
    /// answers to plain SHA-256 and lowercase hex rather than to whatever the
    /// digest crate currently returns:
    ///
    ///     printf '%s' '{"a":null,"b":[1,{"x":3,"y":2}]}' | shasum -a 256
    #[test]
    fn content_hash_spelling_is_pinned_to_plain_sha256_lowercase_hex() {
        let v: Value =
            serde_json::from_str(r#"{"b": [1, {"y": 2, "x": 3}], "a": null}"#).expect("test");
        assert_eq!(canonical_json(&v), r#"{"a":null,"b":[1,{"x":3,"y":2}]}"#);
        assert_eq!(
            content_hash(&v),
            "sha256:fd5905a59ba4aec9fd37e5214d395b1f9ac0db9d3a7addf85ee5c31e89e8a5bc"
        );
    }

    /// Key order must not change the hash; value changes must.
    #[test]
    fn canonical_hash_is_order_insensitive_and_value_sensitive() {
        let a: Value =
            serde_json::from_str(r#"{"b": [1, {"y": 2, "x": 3}], "a": null}"#).expect("test");
        let b: Value =
            serde_json::from_str(r#"{"a": null, "b": [1, {"x": 3, "y": 2}]}"#).expect("test");
        assert_eq!(canonical_json(&a), r#"{"a":null,"b":[1,{"x":3,"y":2}]}"#);
        assert_eq!(content_hash(&a), content_hash(&b));

        let c = json!({"a": null, "b": [1, {"x": 3, "y": 999}]});
        assert_ne!(content_hash(&a), content_hash(&c));
    }

    /// The channel and connector pairs get the same agreement proof as the
    /// workflow pair below: a field added to a `Create*Request` but forgotten
    /// in its projection would otherwise compile clean and make the upsert
    /// report `unchanged` for imports that differ in that field.
    #[test]
    fn channel_and_connector_projections_agree() {
        let now = chrono::NaiveDateTime::default();

        let req: crate::storage::repositories::channels::CreateChannelRequest =
            serde_json::from_value(json!({
                "channel_id": "ch-1",
                "name": "Hash Ch",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/hash",
                "workflow_id": "wf-1",
                "config": {"a": 1},
                "priority": 2,
                "tags": ["t"],
            }))
            .expect("channel request");
        let row = Channel {
            channel_id: "ch-1".to_string(),
            version: 4, // DB-owned: must not affect the projection
            name: "Hash Ch".to_string(),
            description: None,
            channel_type: "sync".to_string(),
            protocol: "rest".to_string(),
            methods_json: Some(r#"["POST"]"#.to_string()),
            route_pattern: Some("/hash".to_string()),
            topic: None,
            consumer_group: None,
            transport_config_json: "{}".to_string(),
            workflow_id: Some("wf-1".to_string()),
            config_json: r#"{"a":1}"#.to_string(),
            status: "active".to_string(),
            priority: 2,
            tags_json: r#"["t"]"#.to_string(),
            created_at: now,
            updated_at: now,
        };
        assert_eq!(
            channel_content(&row).expect("channel content"),
            channel_request_content(&req)
        );

        let req: crate::storage::repositories::connectors::CreateConnectorRequest =
            serde_json::from_value(json!({
                "name": "hash-conn",
                "connector_type": "http",
                "config": {"url": "https://example.com"},
                "enabled": false,
                "tags": ["t"],
            }))
            .expect("connector request");
        let row = Connector {
            id: "generated".to_string(), // excluded: upsert matches on name
            name: "hash-conn".to_string(),
            connector_type: "http".to_string(),
            config_json: r#"{"url":"https://example.com"}"#.to_string(),
            enabled: false,
            tags_json: r#"["t"]"#.to_string(),
            created_at: now,
            updated_at: now,
        };
        assert_eq!(
            connector_content(&row).expect("connector content"),
            connector_request_content(&req)
        );
    }

    /// A row and the create request that produced it project identically —
    /// the invariant the K2 `unchanged` detection and K10 hashes ride on.
    #[test]
    fn row_and_request_projections_agree() {
        let req: CreateWorkflowRequest = serde_json::from_value(json!({
            "workflow_id": "wf-1",
            "name": "Hash Me",
            "priority": 5,
            "tags": ["a"],
            "tasks": [{"id": "t", "name": "T",
                       "function": {"name": "log", "input": {"message": "x"}}}],
        }))
        .expect("request");
        let now = chrono::NaiveDateTime::default();
        let row = Workflow {
            workflow_id: "wf-1".to_string(),
            version: 3, // DB-owned: must not affect the projection
            name: "Hash Me".to_string(),
            description: None,
            priority: 5,
            status: "active".to_string(),
            rollout_percentage: 50,
            condition_json: "true".to_string(),
            tasks_json: serde_json::to_string(&req.tasks).expect("test"),
            tags_json: r#"["a"]"#.to_string(),
            loop_json: None,
            continue_on_error: false,
            created_at: now,
            updated_at: now,
        };
        let row_content = workflow_content(&row).expect("content");
        assert_eq!(row_content, workflow_request_content(&req));
        assert_eq!(
            content_hash(&row_content),
            content_hash(&workflow_request_content(&req))
        );
    }
}

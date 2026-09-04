//! The importable-content projection: what "an entity's content" is.
//!
//! One definition per kind, shared by everything that has to agree on whether
//! two entities are the same:
//!
//! - the server's upsert import, deciding `unchanged`;
//! - the `content_hash` on every entity response;
//! - the package CLI's artifact hashing;
//! - `orion-cli workflows diff`, on the no-hash path a hand-authored file
//!   takes.
//!
//! The last of those used to carry its own copy, which had already drifted on
//! `loop`. It lives here because it is a statement about the wire shape and
//! nothing else — no rows, no I/O, no server types.
//!
//! **The hash is not here.** `canonical_json` and `content_hash` stay in
//! `orion-server`: nothing outside it computes one (the CLI only *compares*
//! the server's), moving them would pull `sha2` and `hex` into this crate, and
//! the hash spelling answers to a test pinned against `shasum` output because
//! a re-spelling would 409 every stored package receipt. If the package
//! tooling ever moves into a client, the hash moves with it and that pin
//! travels too.
//!
//! # The input
//!
//! A document in either the **create shape** (what an import consumes, what a
//! hand-authored file looks like) or the **response shape**. The response
//! shape's extra keys — `workflow_id`, `version`, `status`,
//! `rollout_percentage`, `content_hash`, timestamps — are simply not selected.
//! That is what lets both sides of a `diff` go through one function.
//!
//! # The rules
//!
//! Not "copy these keys". Each field's missing-key behaviour mirrors what
//! serde does with the matching field on the server's `Create*Request`,
//! because the document being projected is create-shaped:
//!
//! | Request field | Rule |
//! |---|---|
//! | `Option<T>` | missing **or** explicit `null` ⇒ `null` — serde collapses both to `None` |
//! | `T` with a serde `default` | missing ⇒ that default; explicit `null` ⇒ `null` |
//! | `T`, required | missing ⇒ `null` |
//!
//! # `loop` is the only field ever omitted
//!
//! Every other optional is projected as an explicit `null` — channel
//! `methods`, `topic`, `consumer_group`, `description`. That is not tidiness
//! waiting to happen: it is what every stored channel hash already is, so
//! dropping those keys when absent would move every one of them and turn a
//! re-`apply` of an unmodified package into a `409`.
//!
//! `loop` is omitted because it was added after the fact. Projecting it
//! unconditionally would have changed the hash of every workflow predating the
//! column, with the same consequence. Absent in, absent out — and an explicit
//! `null` counts as absent, because that is what `Option<Value>` does with a
//! JSON `null`.

use serde_json::{Value, json};

/// A field serde would read into an `Option<T>`: missing and explicit `null`
/// are the same absence, and both project as `null`.
fn optional(doc: &Value, key: &str) -> Value {
    doc.get(key).cloned().unwrap_or(Value::Null)
}

/// A field serde would read with a `default`: missing takes the default, an
/// explicit `null` is a null the document actually carried.
fn defaulted(doc: &Value, key: &str, default: Value) -> Value {
    doc.get(key).cloned().unwrap_or(default)
}

/// A plugin's importable content: the manifest, the digest and the tags.
///
/// The component bytes are not content — the digest *is* the component, and
/// a response never carries the bytes — so an export with the artifact
/// inlined and one without hash the same.
pub fn plugin_content(doc: &Value) -> Value {
    json!({
        "manifest": optional(doc, "manifest"),
        "digest": optional(doc, "digest"),
        "tags": defaulted(doc, "tags", json!([])),
    })
}

/// A workflow's importable content.
pub fn workflow_content(doc: &Value) -> Value {
    let mut content = json!({
        "name": optional(doc, "name"),
        "description": optional(doc, "description"),
        "priority": defaulted(doc, "priority", json!(0)),
        "condition": defaulted(doc, "condition", Value::Bool(true)),
        "tasks": optional(doc, "tasks"),
        "tags": defaulted(doc, "tags", json!([])),
        "continue_on_error": defaulted(doc, "continue_on_error", Value::Bool(false)),
    });
    // The one conditional key — see the module doc.
    if let Some(loop_config) = doc.get("loop").filter(|v| !v.is_null()) {
        content["loop"] = loop_config.clone();
    }
    content
}

/// A channel's importable content.
pub fn channel_content(doc: &Value) -> Value {
    json!({
        "name": optional(doc, "name"),
        "description": optional(doc, "description"),
        "channel_type": optional(doc, "channel_type"),
        "protocol": optional(doc, "protocol"),
        "methods": optional(doc, "methods"),
        "route_pattern": optional(doc, "route_pattern"),
        "topic": optional(doc, "topic"),
        "consumer_group": optional(doc, "consumer_group"),
        "transport_config": defaulted(doc, "transport_config", json!({})),
        "workflow_id": optional(doc, "workflow_id"),
        "config": defaulted(doc, "config", json!({})),
        "priority": defaulted(doc, "priority", json!(0)),
        "tags": defaulted(doc, "tags", json!([])),
    })
}

/// A connector's importable content.
///
/// `id` is excluded — the upsert matches on `name` and keeps the stored id, so
/// it is not part of the artifact contract. `enabled` defaults to `true`, not
/// to `null`: `/export` has always emitted it, and until that default existed
/// a *disabled* connector promoted through export → import came back enabled.
pub fn connector_content(doc: &Value) -> Value {
    json!({
        "name": optional(doc, "name"),
        "connector_type": optional(doc, "connector_type"),
        "config": defaulted(doc, "config", json!({})),
        "enabled": match doc.get("enabled") {
            Some(Value::Bool(b)) => json!(b),
            // Missing or null: an `Option<bool>` that came back `None`.
            _ => json!(true),
        },
        "tags": defaulted(doc, "tags", json!([])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default table, exercised where the server's typed tests structurally
    /// cannot reach it: by the time a `Create*Request` exists, serde has
    /// already applied these.
    #[test]
    fn a_workflow_document_with_nothing_set_takes_the_request_defaults() {
        let c = workflow_content(&json!({}));
        assert_eq!(c["name"], Value::Null);
        assert_eq!(c["description"], Value::Null);
        assert_eq!(c["tasks"], Value::Null);
        assert_eq!(c["priority"], json!(0));
        assert_eq!(c["condition"], json!(true));
        assert_eq!(c["tags"], json!([]));
        assert_eq!(c["continue_on_error"], json!(false));
        assert!(
            c.get("loop").is_none(),
            "loop is the one key that stays absent"
        );
    }

    /// The rule that decides whether `workflows diff` calls two workflows the
    /// same. An explicit `null` is what `Option<Value>` turns a JSON null
    /// into — `None` — so it must project the same as no key at all.
    #[test]
    fn loop_is_projected_only_when_the_document_really_carries_one() {
        let absent = workflow_content(&json!({"name": "w"}));
        let explicit_null = workflow_content(&json!({"name": "w", "loop": null}));
        let present = workflow_content(&json!({"name": "w", "loop": {"over": "$.items"}}));

        assert!(absent.get("loop").is_none());
        assert!(explicit_null.get("loop").is_none());
        assert_eq!(
            absent, explicit_null,
            "a null loop and no loop import identically, so they must project identically"
        );
        assert_eq!(present["loop"], json!({"over": "$.items"}));
        assert_ne!(absent, present);
    }

    /// Every other optional keeps its explicit `null`. Dropping these would
    /// move every stored channel hash — see the module doc.
    #[test]
    fn a_channels_absent_optionals_are_null_and_not_missing() {
        let c = channel_content(&json!({"name": "c"}));
        for key in [
            "description",
            "methods",
            "route_pattern",
            "topic",
            "consumer_group",
            "workflow_id",
        ] {
            assert_eq!(c[key], Value::Null, "{key}");
            assert!(
                c.as_object().is_some_and(|o| o.contains_key(key)),
                "{key} must be present as null, not omitted"
            );
        }
        assert_eq!(c["transport_config"], json!({}));
        assert_eq!(c["config"], json!({}));
        assert_eq!(c["priority"], json!(0));
        assert_eq!(c["tags"], json!([]));
    }

    /// K1: a disabled connector promoted through export → import must stay
    /// disabled, and one that never mentions `enabled` is enabled.
    #[test]
    fn a_connectors_enabled_defaults_to_true_not_null() {
        assert_eq!(connector_content(&json!({}))["enabled"], json!(true));
        assert_eq!(
            connector_content(&json!({"enabled": null}))["enabled"],
            json!(true)
        );
        assert_eq!(
            connector_content(&json!({"enabled": false}))["enabled"],
            json!(false)
        );
    }

    /// The invariant `workflows diff` rides on: a response document and the
    /// create-shaped document that produced it project identically, so the
    /// two sides of a comparison are commensurable.
    #[test]
    fn a_response_projects_the_same_as_the_request_that_made_it() {
        let request = json!({
            "workflow_id": "wf-1",
            "name": "Same",
            "tasks": [{"id": "t"}],
            "priority": 5,
        });
        let response = json!({
            "workflow_id": "wf-1",
            "name": "Same",
            "tasks": [{"id": "t"}],
            "priority": 5,
            // DB-owned, and none of it is content.
            "version": 7,
            "status": "active",
            "rollout_percentage": 100,
            "content_hash": "sha256:whatever",
            "created_at": "2026-01-01T00:00:00",
            "updated_at": "2026-01-01T00:00:00",
            // Defaults the server filled in and the request left out.
            "condition": true,
            "tags": [],
            "continue_on_error": false,
            "description": null,
        });
        assert_eq!(workflow_content(&request), workflow_content(&response));
    }

    /// A non-object document must not panic — the CLI reads hand-authored
    /// files, and an array or a string reaching here is a user error, not a
    /// crash.
    #[test]
    fn a_document_that_is_not_an_object_projects_the_defaults() {
        for doc in [json!([]), json!("nope"), Value::Null, json!(3)] {
            assert_eq!(workflow_content(&doc), workflow_content(&json!({})));
            assert_eq!(channel_content(&doc), channel_content(&json!({})));
            assert_eq!(connector_content(&doc), connector_content(&json!({})));
        }
    }
}

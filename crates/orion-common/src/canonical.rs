//! The canonical JSON form: object keys sorted at every depth, compact.
//!
//! One encoder for every content-addressed document Orion stores — a v2
//! definition, a connection revision's configuration — so two spellings of
//! the same document are one digest. A leaf beside `crypto`.

use serde_json::Value;
use sha2::{Digest, Sha256};

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

/// `sha256:<hex>` over [`canonical_json`].
pub fn content_hash(value: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(value).as_bytes());
    // `hex::encode`, not `{:x}`: sha2 0.11 returns crypto-common's `Array`,
    // which — unlike the `GenericArray` it replaced — implements no LowerHex.
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
}

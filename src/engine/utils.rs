use dataflow_rs::engine::utils::set_nested_value;
use datavalue::OwnedDataValue;
use serde_json::Value;

/// Merge metadata key-value pairs into a message's metadata.
pub fn merge_metadata(message: &mut dataflow_rs::Message, metadata: &Value) {
    if let Some(meta_obj) = metadata.as_object() {
        for (k, v) in meta_obj {
            set_nested_value(
                &mut message.context,
                &format!("metadata.{k}"),
                OwnedDataValue::from(v),
            );
        }
    }
}

/// FNV-1a 64-bit hash. Unkeyed and deterministic — the same identity maps to
/// the same rollout bucket on every replica and across restarts.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Compute the rollout bucket (0–99) for a caller.
///
/// With a stable identity (configured sticky header, else forwarded client
/// IP) the bucket is a hash — the same caller lands on the same canary
/// version on every request and every replica. Without one (direct
/// connection, no forwarding headers) it falls back to per-request random,
/// which still honors the rollout percentages in aggregate.
pub fn rollout_bucket_for_identity(identity: Option<&str>) -> i64 {
    match identity {
        Some(id) if !id.is_empty() => (fnv1a64(id.as_bytes()) % 100) as i64,
        _ => (rand::random::<u32>() % 100) as i64,
    }
}

/// Inject `_rollout_bucket` (0–99) into the message data for rollout routing.
pub fn inject_rollout_bucket(message: &mut dataflow_rs::Message, identity: Option<&str>) {
    let bucket = rollout_bucket_for_identity(identity);
    set_nested_value(
        &mut message.context,
        "data._rollout_bucket",
        OwnedDataValue::from_i64(bucket),
    );
}

/// Remove the `_rollout_bucket` field from message data after processing.
/// v3 has no `unset`; we write `Null` which downstream callers treat as absent.
pub fn remove_rollout_bucket(message: &mut dataflow_rs::Message) {
    set_nested_value(
        &mut message.context,
        "data._rollout_bucket",
        OwnedDataValue::Null,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_message(data: Value) -> dataflow_rs::Message {
        dataflow_rs::Message::from_value(&data)
    }

    #[test]
    fn test_merge_metadata() {
        let mut msg = make_message(json!({}));
        let metadata = json!({"source": "test", "version": 2});
        merge_metadata(&mut msg, &metadata);

        assert_eq!(
            msg.metadata().get("source").and_then(|v| v.as_str()),
            Some("test")
        );
        assert_eq!(
            msg.metadata().get("version").and_then(|v| v.as_i64()),
            Some(2)
        );
    }

    #[test]
    fn test_inject_rollout_bucket_in_range() {
        let mut msg = make_message(json!({}));
        inject_rollout_bucket(&mut msg, None);

        let bucket = msg
            .data()
            .get("_rollout_bucket")
            .and_then(|v| v.as_i64())
            .expect("test");
        assert!(
            (0..100).contains(&bucket),
            "bucket should be 0–99, got {bucket}"
        );
    }

    #[test]
    fn test_rollout_bucket_is_sticky_per_identity() {
        let a1 = rollout_bucket_for_identity(Some("10.0.0.7"));
        let a2 = rollout_bucket_for_identity(Some("10.0.0.7"));
        assert_eq!(a1, a2, "same identity must map to the same bucket");
        assert!((0..100).contains(&a1));

        // Distinct identities distribute (spot-check that not everything
        // collapses onto one bucket).
        let buckets: std::collections::HashSet<i64> = (0..50)
            .map(|i| rollout_bucket_for_identity(Some(&format!("user-{i}"))))
            .collect();
        assert!(buckets.len() > 10, "expected spread, got {buckets:?}");
    }

    #[test]
    fn test_rollout_bucket_empty_identity_falls_back_to_random() {
        // Empty identity must not pin every caller to one bucket.
        let buckets: std::collections::HashSet<i64> = (0..100)
            .map(|_| rollout_bucket_for_identity(Some("")))
            .collect();
        assert!(buckets.len() > 1, "empty identity should randomize");
    }

    #[test]
    fn test_remove_rollout_bucket() {
        let mut msg = make_message(json!({"_rollout_bucket": 42}));
        remove_rollout_bucket(&mut msg);

        let is_absent = msg
            .data()
            .get("_rollout_bucket")
            .map(|v| v.is_null())
            .unwrap_or(true);
        assert!(is_absent, "bucket should be removed or null");
    }
}

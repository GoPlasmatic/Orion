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

/// Inject a random `_rollout_bucket` (0–99) into the message data for rollout routing.
pub fn inject_rollout_bucket(message: &mut dataflow_rs::Message) {
    let bucket = rand::random::<u32>() % 100;
    set_nested_value(
        &mut message.context,
        "data._rollout_bucket",
        OwnedDataValue::from_i64(bucket as i64),
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
        inject_rollout_bucket(&mut msg);

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

use serde_json::Value;

/// Merge metadata key-value pairs into a message's metadata.
pub fn merge_metadata(message: &mut dataflow_rs::Message, metadata: &Value) {
    if let Some(meta_obj) = metadata.as_object() {
        for (k, v) in meta_obj {
            message.metadata_mut()[k] = v.clone();
        }
    }
}

/// Inject a random `_rollout_bucket` (0–99) into the message data for rollout routing.
pub fn inject_rollout_bucket(message: &mut dataflow_rs::Message) {
    let bucket = rand::random::<u32>() % 100;
    message.data_mut()["_rollout_bucket"] = Value::from(bucket);
}

/// Remove the `_rollout_bucket` field from message data after processing.
pub fn remove_rollout_bucket(message: &mut dataflow_rs::Message) {
    if let Some(obj) = message.data_mut().as_object_mut() {
        obj.remove("_rollout_bucket");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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

        assert_eq!(msg.metadata()["source"], "test");
        assert_eq!(msg.metadata()["version"], 2);
    }

    #[test]
    fn test_merge_metadata_non_object_is_noop() {
        let mut msg = make_message(json!({}));
        let original_meta = msg.metadata().clone();
        merge_metadata(&mut msg, &json!("not an object"));
        assert_eq!(msg.metadata(), &original_meta);
    }

    #[test]
    fn test_inject_rollout_bucket_in_range() {
        let mut msg = make_message(json!({}));
        inject_rollout_bucket(&mut msg);

        let bucket = msg.data()["_rollout_bucket"].as_u64().unwrap();
        assert!(bucket < 100, "bucket should be 0–99, got {}", bucket);
    }

    #[test]
    fn test_remove_rollout_bucket() {
        let mut msg = make_message(json!({"_rollout_bucket": 42}));
        remove_rollout_bucket(&mut msg);

        assert!(
            msg.data().get("_rollout_bucket").is_none() || msg.data()["_rollout_bucket"].is_null(),
            "bucket should be removed"
        );
    }
}

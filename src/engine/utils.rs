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

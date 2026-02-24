use serde_json::Value;

/// Merge metadata key-value pairs into a message's metadata.
pub fn merge_metadata(message: &mut dataflow_rs::Message, metadata: &Value) {
    if let Some(meta_obj) = metadata.as_object() {
        for (k, v) in meta_obj {
            message.metadata_mut()[k] = v.clone();
        }
    }
}

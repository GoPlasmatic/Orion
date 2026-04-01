pub mod functions;
pub mod utils;

use std::collections::HashMap;
use std::sync::Arc;

use dataflow_rs::engine::functions::AsyncFunctionHandler;

use crate::connector::ConnectorRegistry;
use crate::storage::models::{Channel, Workflow};
use crate::storage::repositories::workflows::{
    workflow_to_dataflow, workflow_to_dataflow_with_rollout,
};

/// Known function names supported by the engine.
pub const KNOWN_FUNCTIONS: &[&str] = &[
    "map",
    "validation",
    "validate",
    "parse_json",
    "parse_xml",
    "publish_json",
    "publish_xml",
    "filter",
    "log",
    "http_call",
    "publish_kafka",
    "db_read",
    "db_write",
    "channel_call",
];

/// Function names that require a connector reference.
pub const CONNECTOR_FUNCTIONS: &[&str] = &["http_call", "publish_kafka", "db_read", "db_write"];

/// Build the custom function handlers for the dataflow-rs engine.
///
/// Registers http_call, channel_call, and (when kafka feature is disabled) a
/// stub publish_kafka handler. Use [`register_kafka_publisher`] to register the
/// real Kafka-backed handler when the feature is enabled.
pub fn build_custom_functions(
    registry: Arc<ConnectorRegistry>,
    client: reqwest::Client,
    engine: Arc<tokio::sync::RwLock<Arc<dataflow_rs::Engine>>>,
) -> HashMap<String, Box<dyn AsyncFunctionHandler + Send + Sync>> {
    let mut fns: HashMap<String, Box<dyn AsyncFunctionHandler + Send + Sync>> = HashMap::new();

    fns.insert(
        "http_call".to_string(),
        Box::new(functions::http_call::HttpCallHandler {
            registry: registry.clone(),
            client: client.clone(),
        }),
    );

    fns.insert(
        "channel_call".to_string(),
        Box::new(functions::channel_call::ChannelCallHandler { engine }),
    );

    // Register stub publish_kafka when kafka feature is not available
    #[cfg(not(feature = "kafka"))]
    fns.insert(
        "publish_kafka".to_string(),
        Box::new(functions::publish_kafka::PublishKafkaHandler {
            registry: registry.clone(),
        }),
    );

    fns
}

/// Register the real Kafka-backed publish_kafka handler.
///
/// Replaces the stub handler (or adds the handler if not yet registered).
#[cfg(feature = "kafka")]
pub fn register_kafka_publisher(
    fns: &mut HashMap<String, Box<dyn AsyncFunctionHandler + Send + Sync>>,
    registry: Arc<ConnectorRegistry>,
    producer: Arc<crate::kafka::producer::KafkaProducer>,
) {
    fns.insert(
        "publish_kafka".to_string(),
        Box::new(functions::publish_kafka::PublishKafkaHandler { registry, producer }),
    );
}

/// Convert active channels and their workflows to dataflow-rs workflows for the engine.
///
/// For each active channel, finds the associated workflow(s) and builds
/// dataflow-rs Workflow objects with the channel name injected as the channel field.
pub fn build_engine_workflows(
    channels: &[Channel],
    workflows: &[Workflow],
) -> Vec<dataflow_rs::Workflow> {
    // Index workflows by workflow_id for fast lookup
    let mut workflow_map: HashMap<String, Vec<&Workflow>> = HashMap::new();
    for workflow in workflows {
        workflow_map
            .entry(workflow.workflow_id.clone())
            .or_default()
            .push(workflow);
    }

    let mut result = Vec::new();

    for channel in channels {
        let Some(ref wf_id) = channel.workflow_id else {
            tracing::warn!(
                channel_id = %channel.channel_id,
                channel_name = %channel.name,
                "Channel has no workflow_id, skipping"
            );
            continue;
        };

        let Some(wf_versions) = workflow_map.get(wf_id) else {
            tracing::warn!(
                channel_id = %channel.channel_id,
                workflow_id = %wf_id,
                "Workflow not found for channel, skipping"
            );
            continue;
        };

        if wf_versions.len() == 1 && wf_versions[0].rollout_percentage == 100 {
            // Single version at 100% — convert normally
            match workflow_to_dataflow(wf_versions[0], &channel.name) {
                Ok(w) => result.push(w),
                Err(e) => {
                    tracing::warn!(
                        workflow_id = %wf_id,
                        channel = %channel.name,
                        error = %e,
                        "Failed to convert workflow to dataflow, skipping"
                    );
                }
            }
        } else {
            // Multiple versions or partial rollout — wrap with bucket ranges
            let mut sorted: Vec<&&Workflow> = wf_versions.iter().collect();
            sorted.sort_by(|a, b| b.version.cmp(&a.version));

            let mut bucket_offset = 0i64;
            for wf in &sorted {
                let bucket_min = bucket_offset;
                let bucket_max = bucket_offset + wf.rollout_percentage;
                match workflow_to_dataflow_with_rollout(wf, &channel.name, bucket_min, bucket_max) {
                    Ok(w) => result.push(w),
                    Err(e) => {
                        tracing::warn!(
                            workflow_id = %wf.workflow_id,
                            version = wf.version,
                            channel = %channel.name,
                            error = %e,
                            "Failed to convert workflow version to dataflow, skipping"
                        );
                    }
                }
                bucket_offset = bucket_max;
            }
        }
    }

    result
}

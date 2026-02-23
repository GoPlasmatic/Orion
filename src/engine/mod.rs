pub mod functions;

use std::collections::HashMap;
use std::sync::Arc;

use dataflow_rs::engine::functions::AsyncFunctionHandler;

use crate::connector::ConnectorRegistry;

/// Build the custom function handlers for the dataflow-rs engine.
///
/// These handlers implement the integration functions (http_call, enrich,
/// publish_kafka) that the engine dispatches to by name.
pub fn build_custom_functions(
    registry: Arc<ConnectorRegistry>,
) -> HashMap<String, Box<dyn AsyncFunctionHandler + Send + Sync>> {
    let client = reqwest::Client::new();

    let mut fns: HashMap<String, Box<dyn AsyncFunctionHandler + Send + Sync>> = HashMap::new();

    fns.insert(
        "http_call".to_string(),
        Box::new(functions::http_call::HttpCallHandler {
            registry: registry.clone(),
            client: client.clone(),
        }),
    );

    fns.insert(
        "enrich".to_string(),
        Box::new(functions::enrich::EnrichHandler {
            registry: registry.clone(),
            client,
        }),
    );

    fns.insert(
        "publish_kafka".to_string(),
        Box::new(functions::publish_kafka::PublishKafkaHandler {
            registry: registry.clone(),
        }),
    );

    fns
}

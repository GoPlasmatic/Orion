use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;

use crate::connector::ConnectorRegistry;

/// Publishes messages to Kafka topics.
///
/// Requires the `kafka` feature to be enabled. When disabled, returns an error
/// explaining that Kafka support is not compiled in.
pub struct PublishKafkaHandler {
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl AsyncFunctionHandler for PublishKafkaHandler {
    async fn execute(
        &self,
        _message: &mut Message,
        config: &FunctionConfig,
        _datalogic: Arc<DataLogic>,
    ) -> dataflow_rs::Result<(usize, Vec<Change>)> {
        let input = match config {
            FunctionConfig::PublishKafka { input, .. } => input,
            _ => {
                return Err(DataflowError::Validation(
                    "Expected PublishKafka config".into(),
                ));
            }
        };

        // Verify connector exists
        let _connector = self.registry.get(&input.connector).await.ok_or_else(|| {
            DataflowError::function_execution(
                format!("Connector '{}' not found", input.connector),
                None,
            )
        })?;

        // Kafka publishing is not yet implemented — requires rdkafka dependency
        // and will be fully implemented when the kafka feature is added.
        Err(DataflowError::FunctionExecution {
            context: format!(
                "Kafka publishing to topic '{}' is not available. \
                 Enable the 'kafka' feature to use publish_kafka.",
                input.topic
            ),
            source: None,
        })
    }
}

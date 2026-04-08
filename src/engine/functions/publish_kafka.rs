use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;

use crate::connector::ConnectorRegistry;

/// Kafka publish handler.
///
/// When `producer` is `Some`, publishes messages to Kafka topics using the
/// shared producer. When `producer` is `None`, returns an error explaining
/// that Kafka support is not enabled.
pub struct PublishKafkaHandler {
    pub registry: Arc<ConnectorRegistry>,
    pub producer: Option<Arc<crate::kafka::producer::KafkaProducer>>,
}

#[async_trait]
impl AsyncFunctionHandler for PublishKafkaHandler {
    async fn execute(
        &self,
        message: &mut Message,
        config: &FunctionConfig,
        datalogic: Arc<DataLogic>,
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

        // If no producer is available, return an error
        let producer = match &self.producer {
            Some(p) => p,
            None => {
                return Err(DataflowError::FunctionExecution {
                    context: format!(
                        "Kafka publishing to topic '{}' is not available. \
                         Enable the 'kafka' feature to use publish_kafka.",
                        input.topic
                    ),
                    source: None,
                });
            }
        };

        // Evaluate key from JSONLogic if provided
        let key = if let Some(key_logic) = &input.key_logic {
            let context = message.get_context_arc();
            let compiled = datalogic
                .compile(key_logic)
                .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
            let result = datalogic
                .evaluate(&compiled, context)
                .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
            let key_str = if let Some(s) = result.as_str() {
                s.to_string()
            } else {
                serde_json::to_string(&result).map_err(|e| {
                    DataflowError::function_execution(
                        format!("Failed to serialize Kafka message key: {}", e),
                        None,
                    )
                })?
            };
            Some(key_str)
        } else {
            None
        };

        // Evaluate value from JSONLogic or default to message data
        let value = if let Some(value_logic) = &input.value_logic {
            let context = message.get_context_arc();
            let compiled = datalogic
                .compile(value_logic)
                .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
            let result = datalogic
                .evaluate(&compiled, context)
                .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
            serde_json::to_string(&result).map_err(|e| {
                DataflowError::function_execution(
                    format!("Failed to serialize Kafka message value: {}", e),
                    None,
                )
            })?
        } else {
            serde_json::to_string(message.data()).map_err(|e| {
                DataflowError::function_execution(
                    format!("Failed to serialize Kafka message value: {}", e),
                    None,
                )
            })?
        };

        // Publish to Kafka
        producer
            .send(&input.topic, key.as_deref(), value.as_bytes())
            .await
            .map_err(|e| {
                DataflowError::function_execution(
                    format!("Kafka publish to '{}' failed: {}", input.topic, e),
                    None,
                )
            })?;

        tracing::debug!(
            topic = %input.topic,
            "Published message to Kafka"
        );

        Ok((200, vec![]))
    }
}

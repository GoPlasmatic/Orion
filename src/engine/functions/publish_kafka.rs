use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::PublishKafkaConfig;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use crate::connector::ConnectorRegistry;

/// Kafka publish handler.
pub struct PublishKafkaHandler {
    pub registry: Arc<ConnectorRegistry>,
    pub producer: Option<Arc<crate::kafka::producer::KafkaProducer>>,
}

#[async_trait]
impl AsyncFunctionHandler for PublishKafkaHandler {
    type Input = PublishKafkaConfig;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &PublishKafkaConfig,
    ) -> dataflow_rs::Result<TaskOutcome> {
        crate::engine::profile::record("publish_kafka", Some(&input.connector), async move {
            // Verify connector exists.
            let _connector = self.registry.get(&input.connector).await.ok_or_else(|| {
                DataflowError::function_execution(
                    format!("Connector '{}' not found", input.connector),
                    None,
                )
            })?;

            let producer = match &self.producer {
                Some(p) => p,
                None => {
                    return Err(DataflowError::FunctionExecution {
                        context: format!(
                            "Kafka publishing to topic '{}' is not available. \
                         Enable Kafka in configuration to use publish_kafka.",
                            input.topic
                        ),
                        source: None,
                    });
                }
            };

            let key = if let Some(compiled) = input.compiled_key_logic.as_deref() {
                let result: Value = ctx
                    .datalogic()
                    .session()
                    .eval_into(compiled, &ctx.message().context)
                    .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
                let key_str = if let Some(s) = result.as_str() {
                    s.to_string()
                } else {
                    serde_json::to_string(&result).map_err(|e| {
                        DataflowError::function_execution(
                            format!("Failed to serialize Kafka message key: {e}"),
                            None,
                        )
                    })?
                };
                Some(key_str)
            } else {
                None
            };

            let value = if let Some(compiled) = input.compiled_value_logic.as_deref() {
                let result: Value = ctx
                    .datalogic()
                    .session()
                    .eval_into(compiled, &ctx.message().context)
                    .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
                serde_json::to_string(&result).map_err(|e| {
                    DataflowError::function_execution(
                        format!("Failed to serialize Kafka message value: {e}"),
                        None,
                    )
                })?
            } else {
                let data_json: Value = ctx.data().into();
                serde_json::to_string(&data_json).map_err(|e| {
                    DataflowError::function_execution(
                        format!("Failed to serialize Kafka message value: {e}"),
                        None,
                    )
                })?
            };

            producer
                .send(&input.topic, key.as_deref(), value.as_bytes())
                .await
                .map_err(|e| {
                    DataflowError::function_execution(
                        format!("Kafka publish to '{}' failed: {e}", input.topic),
                        None,
                    )
                })?;

            tracing::debug!(
                topic = %input.topic,
                "Published message to Kafka"
            );

            Ok(TaskOutcome::Success)
        })
        .await
    }
}

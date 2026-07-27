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
    /// Producer cache keyed by the connector's broker list (F13). `None`
    /// when Kafka is disabled.
    pub producers: Option<Arc<crate::kafka::producer::KafkaProducerCache>>,
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
            let connector =
                super::connector_helpers::resolve_connector(&self.registry, &input.connector)
                    .await?;
            let kafka_config = super::connector_helpers::require_kafka_connector(
                connector.as_ref(),
                &input.connector,
            )?;

            let producers = match &self.producers {
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
            // F13: publish to the cluster the *connector* names, not the one
            // globally configured. Empty brokers keep the previous meaning:
            // the global cluster.
            let producer = producers
                .for_brokers(&kafka_config.brokers)
                .await
                .map_err(|e| {
                    DataflowError::function_execution(
                        format!(
                            "Failed to create Kafka producer for connector '{}': {e}",
                            input.connector
                        ),
                        None,
                    )
                })?;

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

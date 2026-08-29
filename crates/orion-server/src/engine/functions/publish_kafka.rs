use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::PublishKafkaConfig;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::schema::{FieldKind, FieldSchema};
use crate::connector::ConnectorRegistry;

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "publish_kafka";

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
        // F40: read the channel before the body borrows `ctx` mutably.
        let channel = super::extract_channel(ctx.message()).to_string();

        super::connector_helpers::guarded_handler(
            NAME,
            &self.registry,
            &input.connector,
            &channel,
            async move {
                let connector =
                    super::connector_helpers::resolve_connector(&self.registry, &input.connector)
                        .await?;
                let kafka_config = super::connector_helpers::require_connector::<
                    crate::connector::kind::Kafka,
                >(connector.as_ref(), &input.connector)?;
                // F22e: gate before anything else — whether the deployment has
                // Kafka enabled says nothing about whether this connector is
                // allowed to publish, and the refusal must be the same either
                // way.
                super::connector_helpers::require_op(
                    kafka_config.operations.publish,
                    "publish",
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
                // S6: the broker list is connector data, so judge it against
                // the private-address guard before dialling. An empty list
                // means the globally configured cluster — operator config,
                // not connector data — and is deliberately not re-judged.
                if !kafka_config.brokers.is_empty() {
                    crate::validation::check_broker_endpoints(
                        &input.connector,
                        &kafka_config.brokers,
                        kafka_config.allow_private_urls,
                    )
                    .await
                    .map_err(crate::errors::connector_detail_error)?;
                }

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

                // `resolve_key` applies the same string coercion this handler
                // used to spell out — a JSON string yields its contents,
                // anything else its compact form.
                let key = input.resolve_key(ctx)?;

                // No `value_logic` still means "publish the message data": that
                // is a transport decision the config cannot make, so upstream
                // returns `None` and the fallback stays here.
                let value_json: Value = match input.resolve_value(ctx)? {
                    Some(value) => value,
                    None => ctx.data().into(),
                };
                let value = serde_json::to_string(&value_json).map_err(|e| {
                    DataflowError::function_execution(
                        format!("Failed to serialize Kafka message value: {e}"),
                        None,
                    )
                })?;

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
            },
        )
        .await
    }
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const PUBLISH_KAFKA_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the Kafka connector to publish through.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "topic",
        description: "Target topic name.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key_logic",
        description: "JSONLogic expression to derive the message key.",
        kind: FieldKind::Any,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "value_logic",
        description: "JSONLogic expression to derive the message value.",
        kind: FieldKind::Any,
        ..FieldSchema::DEFAULT
    },
];

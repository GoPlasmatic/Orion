use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::PublishKafkaConfig;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{ConnectorCall, require_op};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::ConnectorRegistry;
use crate::engine::HandlerError;

/// Kafka publish handler.
pub struct PublishKafkaHandler {
    pub registry: Arc<ConnectorRegistry>,
    /// Producer cache keyed by the connector's broker list (F13). `None`
    /// when Kafka is disabled.
    pub producers: Option<Arc<crate::kafka::producer::KafkaProducerCache>>,
}

#[async_trait]
impl ConnectorHandler for PublishKafkaHandler {
    const NAME: &'static str = "publish_kafka";
    type Kind = crate::connector::kind::Kafka;
    type Input = PublishKafkaConfig;
    /// The resolved topic. Since dataflow-rs 3.9 it is a `Template` like the
    /// key and the value, so one task can route by message content; it is
    /// resolved here rather than in `run` because it names the destination in
    /// the "Kafka is not enabled" refusal, which precedes the producer.
    type Parsed = String;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        _call: &ConnectorCall<'_>,
        input: &PublishKafkaConfig,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, HandlerError> {
        Ok(input.resolve_topic(ctx)?)
    }

    fn gate(
        _parsed: &Self::Parsed,
        conn: &crate::connector::KafkaConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        // F22e: gate before anything else — whether the deployment has Kafka
        // enabled says nothing about whether this connector is allowed to
        // publish, and the refusal must be the same either way.
        Ok(require_op(conn.operations.publish, "publish", connector)?)
    }

    async fn run(
        &self,
        topic: Self::Parsed,
        kafka_config: &crate::connector::KafkaConnectorConfig,
        call: &ConnectorCall<'_>,
        input: &PublishKafkaConfig,
        ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        let producers = match &self.producers {
            Some(p) => p,
            None => {
                return Err(DataflowError::FunctionExecution {
                    context: format!(
                        "Kafka publishing to topic '{topic}' is not available. Enable Kafka in \
                         configuration to use publish_kafka."
                    ),
                    source: None,
                }
                .into());
            }
        };

        // S6: the broker list is connector data, so judge it against the
        // private-address guard before dialling. An empty list means the
        // globally configured cluster — operator config, not connector data —
        // and is deliberately not re-judged.
        if !kafka_config.brokers.is_empty() {
            crate::validation::check_broker_endpoints(
                call.connector,
                &kafka_config.brokers,
                kafka_config.allow_private_urls,
            )
            .await
            .map_err(crate::errors::connector_detail_error)?;
        }

        // F13: publish to the cluster the *connector* names, not the one
        // globally configured. Empty brokers keep the previous meaning: the
        // global cluster.
        let producer = producers
            .for_brokers(&kafka_config.brokers)
            .await
            .map_err(|e| {
                DataflowError::function_execution(
                    format!(
                        "Failed to create Kafka producer for connector '{}': {e}",
                        call.connector
                    ),
                    None,
                )
            })?;

        // `resolve_key` applies the same string coercion this handler used to
        // spell out — a JSON string yields its contents, anything else its
        // compact form.
        let key = input.resolve_key(ctx)?;

        // No `value_logic` still means "publish the message data": that is a
        // transport decision the config cannot make, so upstream returns `None`
        // and the fallback stays here.
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
            .send(&topic, key.as_deref(), value.as_bytes())
            .await
            .map_err(|e| {
                DataflowError::function_execution(
                    format!("Kafka publish to '{topic}' failed: {e}"),
                    None,
                )
            })?;

        tracing::debug!(topic = %topic, "Published message to Kafka");

        // A publish records nothing at `output`; the send is the whole effect.
        Ok(Produced::nothing())
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
        description: "Name of the Kafka connector to publish through (JSONLogic; a \
                      computed name is not yet supported).",
        kind: FieldKind::String,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "topic",
        description: "Target topic name (JSONLogic), so one task can route by message \
                      content.",
        kind: FieldKind::String,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key",
        description: "Message key (JSONLogic). \
                      (Was `key_logic`; still accepted, but not alongside `key`.)",
        kind: FieldKind::Any,
        template_at: &[""],
        alias: Some("key_logic"),
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "value",
        description: "Message value (JSONLogic). Defaults to the message data. \
                      (Was `value_logic`; still accepted, but not alongside `value`.)",
        kind: FieldKind::Any,
        template_at: &[""],
        alias: Some("value_logic"),
        ..FieldSchema::DEFAULT
    },
];

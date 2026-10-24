//! `publish_kafka` — one record to a Kafka topic through a Kafka connector.
//!
//! [`PublishKafkaFunction`] is the function, over Orion's own
//! [`PublishKafkaConfig`] (break 10).

use std::sync::Arc;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use serde_json::Value;

use super::schema::{FieldKind, FieldSchema};
use crate::connector::ConnectorRegistry;
use crate::engine::kit::{self, read};
use crate::engine::{ErrorClass, HandlerError};

/// This function's name, for the free functions below (F48).
pub(crate) const NAME: &str = "publish_kafka";

/// The refusal of a publish on a node with Kafka disabled, naming the topic.
fn unavailable(topic: &str) -> HandlerError {
    HandlerError::new(
        ErrorClass::Backend,
        format!(
            "Kafka publishing to topic '{topic}' is not available. Enable Kafka in \
             configuration to use publish_kafka."
        ),
    )
}

/// The failure of the publish itself, the backend's: dataflow-rs 3's
/// `FunctionExecution` with no source.
fn failed(msg: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(ErrorClass::Backend, msg)
}

/// `publish_kafka`'s input on dataflow-rs 4: Orion's own config, in place of
/// dataflow-rs 3's `PublishKafkaConfig` (break 10), each field read as
/// 3.15's read it (I183): `topic` and `key` by `Template::resolve_string`'s
/// reading ([`read::plain_str`]), `value` as the JSON it evaluates to, and a
/// `key` or `value` written as `null` unset ([`read::present`]), as serde
/// read an `Option<Template>`; an unset `value` publishes all of `data`.
///
/// The keys are the canonical ones: `key_logic` and `value_logic`, 3.15's
/// aliases, are rewritten by the loader (I186). A computed `connector` is
/// refused here, where 3.15 refused it on every call.
#[derive(Debug, FunctionConfig)]
pub struct PublishKafkaConfig {
    /// The Kafka connector published through.
    connector: String,
    /// The topic.
    topic: Expr<OwnedDataValue>,
    /// The record's key.
    key: Option<Expr<OwnedDataValue>>,
    /// The record's value; unset publishes all of `data`.
    value: Option<Expr<OwnedDataValue>>,
}

impl PublishKafkaConfig {
    /// The record's key for this call: `resolve_key`'s reading, `None` when
    /// unset, which Kafka partitions round-robin.
    fn record_key(&self, ctx: &Ctx<'_>) -> dataflow_rs::Result<Option<String>> {
        let Some(expr) = read::present(&self.key) else {
            return Ok(None);
        };
        let key = ctx.eval(expr)?;
        Ok(Some(read::plain_str(&key).into_owned()))
    }

    /// The record's value for this call, as JSON: `resolve_value`'s reading,
    /// and all of `data` when unset, the handler's fallback.
    fn record_value(&self, ctx: &Ctx<'_>) -> dataflow_rs::Result<Value> {
        Ok(match read::present(&self.value) {
            Some(expr) => Value::from(&*ctx.eval(expr)?),
            None => ctx
                .get(&kit::DATA)
                .map_or(Value::Null, |data| Value::from(&*data)),
        })
    }
}

/// `publish_kafka` as a dataflow-rs 4 function: the record
/// `PublishKafkaHandler` publishes, read in the same order. It writes
/// nothing; the send is the whole effect.
pub struct PublishKafkaFunction {
    pub registry: Arc<ConnectorRegistry>,
    /// Producer cache keyed by the connector's broker list (F13). `None`
    /// when Kafka is disabled.
    pub producers: Option<Arc<crate::kafka::producer::KafkaProducerCache>>,
}

impl Function for PublishKafkaFunction {
    type Config = PublishKafkaConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &PublishKafkaConfig,
    ) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // `parse`'s half: the topic names the destination in the refusal
        // below, which precedes the producer.
        let topic = ctx.eval(&cfg.topic)?;
        let topic = read::plain_str(&topic).into_owned();
        let ctx = &*ctx;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let kafka =
                kit::require_connector::<crate::connector::kind::Kafka>(&config, &cfg.connector)?;
            // F22e: the gate before anything else, so the refusal is the same
            // whether or not the deployment has Kafka enabled.
            kit::require_op(kafka.operations.publish, "publish", &cfg.connector)?;
            let producers = self.producers.as_ref().ok_or_else(|| unavailable(&topic))?;

            // S6: the broker list is connector data, judged before dialling;
            // an empty list means the globally configured cluster.
            if !kafka.brokers.is_empty() {
                crate::validation::check_broker_endpoints(
                    &cfg.connector,
                    &kafka.brokers,
                    kafka.allow_private_urls,
                )
                .await
                .map_err(crate::errors::service::connector_detail)?;
            }

            // F13: the cluster the *connector* names.
            let producer = producers.for_brokers(&kafka.brokers).await.map_err(|e| {
                failed(format!(
                    "Failed to create Kafka producer for connector '{}': {e}",
                    cfg.connector
                ))
            })?;

            let key = cfg.record_key(ctx)?;
            let value = serde_json::to_string(&cfg.record_value(ctx)?)
                .map_err(|e| failed(format!("Failed to serialize Kafka message value: {e}")))?;

            producer
                .send(&topic, key.as_deref(), value.as_bytes())
                .await
                .map_err(|e| failed(format!("Kafka publish to '{topic}' failed: {e}")))?;
            tracing::debug!(topic = %topic, "Published message to Kafka");
            Ok(Outcome::Ok)
        })
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
        description: "Message key (JSONLogic).",
        kind: FieldKind::Any,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "value",
        description: "Message value (JSONLogic). Defaults to the message data.",
        kind: FieldKind::Any,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::json;

    fn config(input: Value) -> Result<PublishKafkaConfig, Vec<dataflow_rs::Issue>> {
        testing::config_with(&kit::test_exprs(), input)
    }

    fn rejected(input: Value) -> Vec<(String, IssueCode)> {
        config(input)
            .expect_err("refused")
            .into_iter()
            .filter(|i| i.severity == Severity::Rejected)
            .map(|i| (i.path.to_string(), i.code))
            .collect()
    }

    /// Declaration order is the field table's (I179); `connector` and
    /// `topic` are required (I183) and `connector` a literal; an unknown key
    /// is refused (break 2a), 1.12's aliases among them: no alias is read.
    #[test]
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = PublishKafkaConfig::fields()
            .iter()
            .map(|f| &*f.name)
            .collect();
        let table: Vec<&str> = PUBLISH_KAFKA_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

        let cfg = config(
            json!({"connector": "events", "topic": {"cat": ["orders.", {"var": "data.region"}]},
                                "key": {"var": "data.id"}, "value": {"var": "data.order"}}),
        )
        .expect("compiles");
        assert!(cfg.topic.constant().is_none() && cfg.key.is_some() && cfg.value.is_some());
        assert!(cfg.primary_output().is_none(), "a publish writes nothing");

        assert_eq!(
            rejected(json!({"connector": "events"})),
            [("topic".to_string(), IssueCode::InputParse)]
        );
        assert_eq!(
            rejected(json!({"topic": "t"})),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        assert_eq!(
            rejected(json!({"connector": {"var": "data.c"}, "topic": "t"})),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        for key in ["key_logic", "value_logic", "partition"] {
            let issues = rejected(json!({"connector": "events", "topic": "t", key: "x"}));
            assert_eq!(issues.len(), 1, "{key}: {issues:?}");
            assert_eq!(issues[0].0, key);
        }
    }

    /// The config loads what 3.15's `PublishKafkaConfig` deserialized and
    /// refuses what it refused, but for the named breaks, each refused here
    /// where 3.15 loaded it: a computed `connector` (the shapes' note) and an
    /// alias on its own (I186).
    #[test]
    fn the_load_verdict_is_1_12s_but_for_the_named_breaks() {
        let loads = [
            json!({"connector": "events", "topic": "t"}),
            json!({"connector": "events", "topic": null, "key": null, "value": null}),
            json!({"connector": "events", "topic": 7, "key": {"var": "data.id"},
                   "value": {"cat": ["a"]}}),
        ];
        let refused_alike = [
            json!({}),
            json!({"connector": "events"}),
            json!({"topic": "t"}),
            json!({"connector": "events", "topic": "t", "partition": 1}),
            json!({"connector": "events", "topic": "t", "key": "k", "key_logic": "j"}),
        ];
        let breaks = [
            json!({"connector": 7, "topic": "t"}),
            json!({"connector": {"var": "data.c"}, "topic": "t"}),
            json!({"connector": "events", "topic": "t", "key_logic": "k"}),
            json!({"connector": "events", "topic": "t", "value_logic": {"var": "data"}}),
        ];
        for input in loads {
            assert!(config(input.clone()).is_ok(), "{input}");
        }
        for input in refused_alike.into_iter().chain(breaks) {
            assert!(config(input.clone()).is_err(), "{input}");
        }
    }

    /// Writes the function's readings of its config at `data.out`: the
    /// topic, the key, and the value with its fallback to all of `data`.
    struct Reads;

    impl Function for Reads {
        type Config = PublishKafkaConfig;

        async fn call(
            &self,
            ctx: &mut Ctx<'_>,
            cfg: &PublishKafkaConfig,
        ) -> dataflow_rs::Result<Outcome> {
            let topic = ctx.eval(&cfg.topic)?;
            let read = json!({
                "topic": read::plain_str(&topic),
                "key": cfg.record_key(ctx)?,
                "value": cfg.record_value(ctx)?,
            });
            ctx.set(&"data.out".parse()?, OwnedDataValue::from(&read))?;
            Ok(Outcome::Ok)
        }
    }

    /// `topic`, `key` and `value` read as 3.15's `PublishKafkaConfig` read
    /// them, over each kind of value: the plain-string coercion, a `null`
    /// unset, and the fallback to all of `data`.
    #[tokio::test]
    async fn the_fields_read_as_3_15s_config_read_them() {
        let data = json!({"id": 7, "region": "eu", "order": {"sku": "A1"}, "list": [1, "a"]});
        for input in [
            json!({"connector": "events", "topic": "orders"}),
            json!({"connector": "events", "topic": {"cat": ["orders.", {"var": "data.region"}]},
                   "key": {"var": "data.id"}, "value": {"var": "data.order"}}),
            json!({"connector": "events", "topic": 7, "key": null, "value": null}),
            json!({"connector": "events", "topic": {"var": "data.list"}, "key": {"var": "data.order"},
                   "value": "text"}),
            json!({"connector": "events", "topic": null, "key": true, "value": [1, {"var": "data.id"}]}),
            json!({"connector": "events", "topic": "t", "key": {"var": "data.none"},
                   "value": {"var": "data.none"}}),
        ] {
            let v4 = twin::v4(NAME, Reads, input.clone(), data.clone()).await;
            twin::pinned(&v4, format!("fields_read/{input}"));
        }
    }

    /// One task with no producer configured, as the function is tested in
    /// `connector_ops_test`.
    async fn run(input: Value) -> Seen {
        let connectors = [
            (
                "events",
                json!({"type": "kafka", "brokers": [], "topic": "t"}),
            ),
            (
                "closed",
                json!({"type": "kafka", "brokers": [], "topic": "t",
                              "operations": {"publish": false}}),
            ),
            ("api", json!({"type": "http", "url": "http://127.0.0.1:1"})),
        ];
        twin::v4(
            NAME,
            PublishKafkaFunction {
                registry: twin::connectors(&connectors).await,
                producers: None,
            },
            input,
            json!({"region": "eu"}),
        )
        .await
    }

    /// The gate first, so a closed connector refuses alike with Kafka on or
    /// off; past an open gate the disabled producer refuses, naming the
    /// topic as computed; a missing or a wrong connector refuses as 3.15's
    /// lookups did.
    #[tokio::test]
    async fn with_no_producer_the_refusals_are_3_15s() {
        use crate::errors::kind::CONNECTOR_DETAIL;
        for (input, expected) in [
            (
                json!({"connector": "closed", "topic": "t"}),
                CONNECTOR_DETAIL,
            ),
            (
                json!({"connector": "events", "topic": {"cat": ["orders.", {"var": "data.region"}]}}),
                "FUNCTION_ERROR",
            ),
            (
                json!({"connector": "absent", "topic": "t"}),
                "FUNCTION_ERROR",
            ),
            (json!({"connector": "api", "topic": "t"}), CONNECTOR_DETAIL),
        ] {
            let v4 = run(input.clone()).await;
            twin::pinned(&v4, format!("no_producer/{input}"));
            assert_eq!(v4.expect_err("refused").code, expected, "{input}");
        }
        let v4 = run(json!({"connector": "events",
                                   "topic": {"cat": ["orders.", {"var": "data.region"}]}}))
        .await;
        let failure = v4.expect_err("Kafka is disabled");
        assert!(
            failure
                .message
                .contains("topic 'orders.eu' is not available"),
            "{failure:?}"
        );
    }
}

//! Consumer-lag observability: periodic polling of committed offsets vs
//! high watermarks, exported per topic-partition as a Prometheus gauge.

use std::sync::Arc;

use rdkafka::TopicPartitionList;
use rdkafka::consumer::{Consumer, StreamConsumer};
use tokio::sync::watch;

use crate::metrics;

/// Periodically poll committed offsets and high watermarks to compute consumer lag.
pub(super) async fn poll_consumer_lag(
    consumer: Arc<StreamConsumer>,
    mut shutdown_rx: watch::Receiver<bool>,
    interval_secs: u64,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    // Skip the first immediate tick — let the consumer establish itself first
    interval.tick().await;

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { break; }
            }
            _ = interval.tick() => {
                let consumer = consumer.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let committed = match consumer.committed(std::time::Duration::from_secs(5)) {
                        Ok(tpl) => tpl,
                        Err(e) => {
                            tracing::debug!(error = %e, "Failed to fetch committed offsets for lag metric");
                            return;
                        }
                    };
                    report_lag_for_partitions(&consumer, &committed);
                }).await;
            }
        }
    }
}

/// Compute and report lag for each topic-partition in the committed offsets list.
fn report_lag_for_partitions(consumer: &StreamConsumer, committed: &TopicPartitionList) {
    for elem in committed.elements() {
        let topic = elem.topic();
        let partition = elem.partition();

        let committed_offset = match elem.offset() {
            rdkafka::Offset::Offset(n) => n,
            rdkafka::Offset::Invalid | rdkafka::Offset::Beginning => 0,
            _ => continue, // Stored, End, etc. — skip
        };

        match consumer.fetch_watermarks(topic, partition, std::time::Duration::from_secs(5)) {
            Ok((_low, high)) => {
                let lag = (high - committed_offset).max(0);
                metrics::set_kafka_consumer_lag(topic, partition, lag as f64);
            }
            Err(e) => {
                tracing::debug!(
                    topic = %topic,
                    partition = partition,
                    error = %e,
                    "Failed to fetch watermarks for lag metric"
                );
            }
        }
    }
}

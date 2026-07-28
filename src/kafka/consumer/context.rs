//! Rebalance-aware consumer context (K8).
//!
//! The consumer used to run with rdkafka's default context: no
//! `pre_rebalance`, no `post_rebalance`, no `commit_callback`. With
//! `CommitMode::Async` that left two holes in the at-least-once contract's
//! edges. First, an async commit still in flight when a partition was
//! revoked was silently lost, so the new owner reprocessed work this
//! consumer had already completed. Second, the in-place retry loop kept
//! working a message on a partition this consumer no longer owned,
//! duplicating the new owner's work for as long as the retries lasted.
//!
//! [`KafkaConsumerContext`] closes the first hole directly: `pre_rebalance`
//! synchronously flushes the not-yet-confirmed commits for the partitions
//! being revoked before the revocation proceeds. For the second it records
//! the revoked partitions in [`RebalanceState`] — but note the dispatch
//! semantics: rdkafka runs these callbacks only on the thread polling the
//! consumer queue, and the consume loop awaits processing inline, so the
//! flag can flip only between messages, never while one is being retried.
//! The mid-retry case is covered instead by *bounding* the in-place retry
//! and rewinding the partition when the budget expires, which returns
//! control to the poll loop where these callbacks (and the group's
//! liveness protocol) actually run — see
//! `process::process_until_committed`.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use rdkafka::client::ClientContext;
use rdkafka::consumer::{BaseConsumer, CommitMode, Consumer, ConsumerContext, Rebalance};
use rdkafka::error::KafkaResult;
use rdkafka::{Offset, TopicPartitionList};

/// Commit bookkeeping shared between the consume loop and the rebalance
/// callbacks. All methods take `&self`; the interior mutex is held only for
/// map operations, never across a broker call.
pub(crate) struct RebalanceState {
    inner: Mutex<RebalanceStateInner>,
}

#[derive(Default)]
struct RebalanceStateInner {
    /// Partitions revoked from this consumer and not (yet) re-assigned.
    /// The retry loop checks membership before working a message further.
    revoked: HashSet<(String, i32)>,
    /// Per partition: the next-to-consume offset most recently handed to an
    /// async commit and not yet confirmed by `commit_callback`. These are
    /// exactly the commits a revocation could lose, so `pre_rebalance`
    /// re-commits them synchronously.
    committable: HashMap<(String, i32), i64>,
}

impl RebalanceState {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(RebalanceStateInner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RebalanceStateInner> {
        // A poisoned lock means a panic mid-map-operation; the maps hold
        // plain values, so continuing with them is safe.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record that `next_offset` (the offset *after* the processed message)
    /// was handed to an async commit for `topic`/`partition`.
    pub(crate) fn record_committable(&self, topic: &str, partition: i32, next_offset: i64) {
        self.lock()
            .committable
            .insert((topic.to_string(), partition), next_offset);
    }

    /// Drop bookkeeping for commits the broker has confirmed at or beyond
    /// the recorded offset — those no longer need a synchronous flush on
    /// revocation.
    fn confirm_committed(&self, topic: &str, partition: i32, confirmed_offset: i64) {
        let mut inner = self.lock();
        let key = (topic.to_string(), partition);
        if inner
            .committable
            .get(&key)
            .is_some_and(|stored| *stored <= confirmed_offset)
        {
            inner.committable.remove(&key);
        }
    }

    /// Remove and return the unconfirmed commits for `partitions` as a
    /// commit-ready list. `None` when there is nothing to flush.
    fn take_committable(&self, partitions: &[(String, i32)]) -> Option<TopicPartitionList> {
        let mut taken = Vec::new();
        {
            let mut inner = self.lock();
            for key in partitions {
                if let Some(offset) = inner.committable.remove(key) {
                    taken.push((key.clone(), offset));
                }
            }
        }
        if taken.is_empty() {
            return None;
        }
        let mut tpl = TopicPartitionList::new();
        for ((topic, partition), offset) in taken {
            if let Err(e) = tpl.add_partition_offset(&topic, partition, Offset::Offset(offset)) {
                tracing::error!(
                    topic = %topic,
                    partition,
                    offset,
                    error = %e,
                    "Failed to stage a revoked partition's offset for commit"
                );
            }
        }
        Some(tpl)
    }

    fn mark_revoked(&self, partitions: &[(String, i32)]) {
        self.lock().revoked.extend(partitions.iter().cloned());
    }

    fn mark_assigned(&self, partitions: &[(String, i32)]) {
        let mut inner = self.lock();
        for key in partitions {
            inner.revoked.remove(key);
            // A fresh assignment resumes from the broker's committed offset;
            // any stale committable entry is at most that same offset.
            inner.committable.remove(key);
        }
    }

    /// Whether `topic`/`partition` was revoked from this consumer and not
    /// re-assigned. While true, no message on it may be processed further or
    /// committed — it belongs to another group member now.
    pub(crate) fn is_revoked(&self, topic: &str, partition: i32) -> bool {
        self.lock()
            .revoked
            .contains(&(topic.to_string(), partition))
    }
}

/// The ingest consumer's rdkafka context: synchronous commit flush on
/// revocation, revoked-partition tracking, and async-commit failure
/// reporting. See the module doc for why each hook exists.
pub(crate) struct KafkaConsumerContext {
    rebalance: std::sync::Arc<RebalanceState>,
}

impl KafkaConsumerContext {
    pub(crate) fn new(rebalance: std::sync::Arc<RebalanceState>) -> Self {
        Self { rebalance }
    }
}

impl ClientContext for KafkaConsumerContext {}

impl ConsumerContext for KafkaConsumerContext {
    fn pre_rebalance(&self, base_consumer: &BaseConsumer<Self>, rebalance: &Rebalance<'_>) {
        match rebalance {
            Rebalance::Revoke(tpl) => {
                let partitions = partition_keys(tpl);
                // Flush unconfirmed async commits for the partitions we are
                // losing, synchronously, while this consumer still owns them.
                // Re-committing an already-confirmed offset is idempotent, so
                // this can only narrow the new owner's replay window.
                if let Some(commit_tpl) = self.rebalance.take_committable(&partitions) {
                    match base_consumer.commit(&commit_tpl, CommitMode::Sync) {
                        Ok(()) => tracing::info!(
                            partitions = partitions.len(),
                            "Flushed in-flight offset commits before partition revocation"
                        ),
                        Err(e) => {
                            // The offsets stay uncommitted: the new owner
                            // redelivers from the last confirmed commit —
                            // duplicated work, never loss.
                            crate::metrics::record_error("kafka_commit");
                            tracing::error!(
                                error = %e,
                                "Failed to flush offset commits before partition revocation"
                            );
                        }
                    }
                }
                self.rebalance.mark_revoked(&partitions);
                tracing::info!(?partitions, "Kafka partitions revoked");
            }
            Rebalance::Assign(_) => {}
            Rebalance::Error(e) => {
                crate::metrics::record_error("kafka_rebalance");
                tracing::error!(error = %e, "Kafka rebalance error");
            }
        }
    }

    fn post_rebalance(&self, _base_consumer: &BaseConsumer<Self>, rebalance: &Rebalance<'_>) {
        if let Rebalance::Assign(tpl) = rebalance {
            let partitions = partition_keys(tpl);
            self.rebalance.mark_assigned(&partitions);
            tracing::info!(?partitions, "Kafka partitions assigned");
        }
    }

    fn commit_callback(&self, result: KafkaResult<()>, offsets: &TopicPartitionList) {
        match result {
            Ok(()) => {
                for elem in offsets.elements() {
                    if let Offset::Offset(offset) = elem.offset() {
                        self.rebalance
                            .confirm_committed(elem.topic(), elem.partition(), offset);
                    }
                }
            }
            Err(e) => {
                // Async commit failures used to be invisible — the enqueue
                // site logged only enqueue errors. A failed commit risks
                // redelivery, not loss, but operators need the signal.
                crate::metrics::record_error("kafka_commit");
                tracing::warn!(error = %e, "Kafka offset commit failed; affected messages may be redelivered");
            }
        }
    }
}

/// `(topic, partition)` keys of a partition list.
fn partition_keys(tpl: &TopicPartitionList) -> Vec<(String, i32)> {
    tpl.elements()
        .iter()
        .map(|e| (e.topic().to_string(), e.partition()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rdkafka::ClientConfig;

    use super::*;

    fn keys(pairs: &[(&str, i32)]) -> Vec<(String, i32)> {
        pairs.iter().map(|(t, p)| (t.to_string(), *p)).collect()
    }

    #[test]
    fn take_committable_drains_only_the_requested_partitions() {
        let state = RebalanceState::new();
        state.record_committable("orders", 0, 10);
        state.record_committable("orders", 1, 20);

        let tpl = state
            .take_committable(&keys(&[("orders", 0)]))
            .expect("partition 0 has an unconfirmed commit");
        let elems = tpl.elements();
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].topic(), "orders");
        assert_eq!(elems[0].partition(), 0);
        assert_eq!(elems[0].offset(), Offset::Offset(10));

        // Drained: a second take finds nothing for partition 0, while
        // partition 1 is untouched.
        assert!(state.take_committable(&keys(&[("orders", 0)])).is_none());
        assert!(state.take_committable(&keys(&[("orders", 1)])).is_some());
    }

    #[test]
    fn confirmed_commits_no_longer_need_a_revocation_flush() {
        let state = RebalanceState::new();
        state.record_committable("orders", 0, 10);

        // A confirmation below the recorded offset is stale — the newer
        // commit is still in flight and must survive.
        state.confirm_committed("orders", 0, 9);
        assert!(state.take_committable(&keys(&[("orders", 0)])).is_some());

        state.record_committable("orders", 0, 10);
        state.confirm_committed("orders", 0, 10);
        assert!(
            state.take_committable(&keys(&[("orders", 0)])).is_none(),
            "a confirmed commit must not be re-flushed on revocation"
        );
    }

    #[test]
    fn revocation_tracking_clears_on_reassignment() {
        let state = RebalanceState::new();
        assert!(!state.is_revoked("orders", 0));

        state.mark_revoked(&keys(&[("orders", 0), ("orders", 1)]));
        assert!(state.is_revoked("orders", 0));
        assert!(state.is_revoked("orders", 1));
        assert!(!state.is_revoked("orders", 2));

        state.mark_assigned(&keys(&[("orders", 0)]));
        assert!(!state.is_revoked("orders", 0), "re-assigned partition");
        assert!(state.is_revoked("orders", 1), "still someone else's");
    }

    /// The hooks themselves, driven directly: a revoke marks partitions in
    /// the shared state and an assign clears them. No committable offsets
    /// are recorded, so no broker commit is attempted — the consumer client
    /// exists but never contacts the (dead) address.
    #[test]
    fn rebalance_hooks_update_the_shared_state() {
        let state = Arc::new(RebalanceState::new());
        let consumer: BaseConsumer<KafkaConsumerContext> = ClientConfig::new()
            .set("bootstrap.servers", "127.0.0.1:1")
            .set("group.id", "context-hook-test")
            .create_with_context(KafkaConsumerContext::new(state.clone()))
            .expect("client creation is local; no broker contact");

        let mut tpl = TopicPartitionList::new();
        tpl.add_partition("orders", 0);

        consumer
            .context()
            .pre_rebalance(&consumer, &Rebalance::Revoke(&tpl));
        assert!(state.is_revoked("orders", 0));

        consumer
            .context()
            .post_rebalance(&consumer, &Rebalance::Assign(&tpl));
        assert!(!state.is_revoked("orders", 0));
    }
}

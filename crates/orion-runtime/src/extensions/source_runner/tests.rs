#![expect(clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::extensions::AttemptContext;
use crate::extensions::CompositionRoot;
use crate::extensions::context::{AdmittedRun, AuthorityGrant, BudgetLimits};
use crate::extensions::{ConnectorOutput, Digest, ProviderRef};
use orion_extension_api::{ProviderBackend, SourceConnectorFacet};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

const TEST_SOURCE: &str = "test.source";

/// A source-capable provider that echoes its checkpoint and never yields
/// an item: enough to drive the runner's privileged facet calls.
struct EchoSource;

#[async_trait::async_trait]
impl ProviderBackend for EchoSource {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Rust
    }

    fn driver_id(&self) -> &str {
        TEST_SOURCE
    }

    fn validate_config(&self, _config: &Value) -> Result<(), ConnectorError> {
        Ok(())
    }

    async fn invoke(
        &self,
        _ctx: &AttemptContext,
        _operation: &str,
        _input: Value,
    ) -> Result<ConnectorOutput, ConnectorError> {
        Ok(ConnectorOutput::new(json!({})))
    }

    fn as_source(&self) -> Option<&dyn SourceConnectorFacet> {
        Some(self)
    }
}

#[async_trait::async_trait]
impl SourceConnectorFacet for EchoSource {
    async fn open(
        &self,
        _ctx: &AttemptContext,
        checkpoint: Option<Value>,
    ) -> Result<Value, ConnectorError> {
        Ok(json!({ "checkpoint": checkpoint }))
    }

    async fn poll(
        &self,
        _ctx: &AttemptContext,
        cursor: &Value,
        _max_items: u32,
    ) -> Result<SourcePollBatch, ConnectorError> {
        Ok(SourcePollBatch::new(
            Vec::new(),
            cursor.get("checkpoint").cloned(),
        ))
    }

    async fn settle(
        &self,
        _ctx: &AttemptContext,
        _delivery_id: &str,
        _settlement: SourceSettlement,
    ) -> Result<(), ConnectorError> {
        Ok(())
    }

    async fn close(&self, _ctx: &AttemptContext, _cursor: Value) -> Result<(), ConnectorError> {
        Ok(())
    }
}

async fn coordinator() -> (SourceLeaseCoordinator, crate::storage::DbPool) {
    let pool = crate::storage::test_sqlite_pool().await;
    (SourceLeaseCoordinator::new(pool.clone()), pool)
}

/// Move a partition's lease into the database's past, as time would.
async fn expire(pool: &crate::storage::DbPool, key: &SourcePartitionKey) {
    let crate::storage::DbPool::Sqlite(p) = pool else {
        panic!("sqlite expected");
    };
    sqlx::query(
        "UPDATE source_claims SET lease_until = datetime('now', '-1 seconds') \
         WHERE channel_id = ?1 AND partition_id = ?2",
    )
    .bind(&key.channel_id)
    .bind(&key.partition_id)
    .execute(p)
    .await
    .unwrap();
}

#[tokio::test]
async fn fenced_leases_contiguous_checkpoints_dlq_guard_and_revision_handoff() {
    let (coord, pool) = coordinator().await;
    let ledger = ContiguousCheckpointLedger::new(coord.clone());
    let key = SourcePartitionKey::new("orders-source", "part-0");

    // 1. Acquire initial lease on revision 1 (fence_token = 1).
    let claim_r1 = coord
        .acquire_or_takeover(key.clone(), "node-a", 1, "pg-lsn-v1", 60)
        .await
        .expect("acquired rev 1");
    assert_eq!(claim_r1.fence_token, 1);
    // Another owner cannot take a live lease.
    let held = coord
        .acquire_or_takeover(key.clone(), "node-b", 1, "pg-lsn-v1", 60)
        .await
        .expect_err("live lease");
    assert_eq!(held.code, "SOURCE_PARTITION_LEASE_HELD");

    // Enqueue 3 polled items (seq 1, 2, 3).
    let mut seqs = Vec::new();
    for (i, lsn) in [101, 102, 103].into_iter().enumerate() {
        seqs.push(
            ledger
                .enqueue_polled_item(&key, 1, format!("deliv-{i}"), json!({"lsn": lsn}))
                .await
                .expect("enqueue"),
        );
    }
    assert_eq!(seqs, vec![1, 2, 3]);
    let (seq1, seq2, seq3) = (1, 2, 3);

    // 2. Out-of-order completion: seq 2 settles first while seq 1 is
    // still Pending. The gap at seq 1 blocks checkpoint advancement.
    ledger
        .record_settlement_intent(&key, 1, seq2, SourceSettlement::Ack)
        .await
        .expect("intent seq2");
    let cp = ledger
        .confirm_settled_and_advance(&key, 1, seq2, false)
        .await
        .expect("confirm seq2");
    assert!(
        cp.is_none(),
        "gap at seq 1 must block checkpoint advancement"
    );

    // 3. Seq 1 settles: contiguous advancement runs through 1 and 2, and
    // the checkpoint is in the database under fence 1.
    ledger
        .record_settlement_intent(&key, 1, seq1, SourceSettlement::Ack)
        .await
        .expect("intent seq1");
    let cp = ledger
        .confirm_settled_and_advance(&key, 1, seq1, false)
        .await
        .expect("confirm seq1");
    assert_eq!(cp, Some(json!({"lsn": 102})));
    assert_eq!(
        coord.load_checkpoint(&key, "pg-lsn-v1").await.unwrap(),
        Some((2, json!({"lsn": 102}), 1))
    );

    // 4. Seq 3 is DeadLetter but DLQ persistence failed: no advancement.
    ledger
        .record_settlement_intent(&key, 1, seq3, SourceSettlement::DeadLetter)
        .await
        .expect("intent seq3 dlq");
    let dlq_err = ledger
        .confirm_settled_and_advance(&key, 1, seq3, false)
        .await
        .expect_err("unpersisted DLQ must not advance checkpoint");
    assert_eq!(dlq_err.code, "SOURCE_DLQ_PERSISTENCE_REQUIRED");
    assert_eq!(
        ledger.committed_checkpoint(&key).await,
        Some((2, json!({"lsn": 102}), 1))
    );

    // 5. Revision handoff: an incompatible namespace is
    // refused without the migration flag; a compatible one moves the
    // fence to 2 and fences out the old runner.
    let incompat = coord
        .handoff_revision(&key, "node-b", 2, "pg-lsn-v2-incompatible", false, 60)
        .await
        .expect_err("incompatible namespace must fail");
    assert_eq!(incompat.code, "SOURCE_CHECKPOINT_NAMESPACE_MISMATCH");
    let claim_r2 = coord
        .handoff_revision(&key, "node-b", 2, "pg-lsn-v1", false, 60)
        .await
        .expect("handoff to rev 2 succeeds");
    assert_eq!(claim_r2.fence_token, 2);

    let stale_err = ledger
        .confirm_settled_and_advance(&key, 1, seq3, true)
        .await
        .expect_err("stale fence 1 must be rejected after handoff");
    assert_eq!(stale_err.code, "SOURCE_FENCE_STALE");
    let stale_commit = coord
        .commit_checkpoint(&key, 1, 9, &json!({"lsn": 999}))
        .await
        .expect_err("a stale fence cannot write a checkpoint");
    assert_eq!(stale_commit.code, "SOURCE_FENCE_STALE");

    // Fence 2 settles seq 3 once the DLQ write succeeded.
    let cp_final = ledger
        .confirm_settled_and_advance(&key, 2, seq3, true)
        .await
        .expect("fence 2 settles seq 3");
    assert_eq!(cp_final, Some(json!({"lsn": 103})));
    assert_eq!(
        coord.load_checkpoint(&key, "pg-lsn-v1").await.unwrap(),
        Some((3, json!({"lsn": 103}), 2))
    );

    // A fresh ledger (a restarted runner) resumes from the database.
    let resumed = ContiguousCheckpointLedger::new(coord.clone());
    resumed.resume(&key, "pg-lsn-v1").await.unwrap();
    assert_eq!(
        resumed.committed_checkpoint(&key).await,
        Some((3, json!({"lsn": 103}), 2))
    );
    let next = resumed
        .enqueue_polled_item(&key, 2, "deliv-4", json!({"lsn": 104}))
        .await
        .unwrap();
    assert_eq!(next, 4, "sequence numbers continue after the checkpoint");
    drop(pool);
}

/// The lease is judged on the database clock: renewal works while the
/// row's `lease_until` is ahead of the database's `now`, and once it is
/// behind, the holder can neither renew nor act, and another node takes
/// the partition over with a greater fence.
#[tokio::test]
async fn leases_expire_on_the_database_clock() {
    let (coord, pool) = coordinator().await;
    let key = SourcePartitionKey::new("orders-source", "part-1");
    let claim = coord
        .acquire_or_takeover(key.clone(), "node-a", 1, "ns", 60)
        .await
        .unwrap();
    coord
        .renew(&key, "node-a", claim.fence_token, 60)
        .await
        .unwrap();
    let stale = coord.renew(&key, "node-a", 99, 60).await.unwrap_err();
    assert_eq!(stale.code, "SOURCE_FENCE_STALE");
    // The holder re-acquiring its own live lease takes a new fence.
    let again = coord
        .acquire_or_takeover(key.clone(), "node-a", 1, "ns", 60)
        .await
        .unwrap();
    assert_eq!(again.fence_token, 2);

    expire(&pool, &key).await;
    assert_eq!(
        coord.renew(&key, "node-a", 2, 60).await.unwrap_err().code,
        "SOURCE_LEASE_EXPIRED"
    );
    assert_eq!(
        coord.validate_fence(&key, 2).await.unwrap_err().code,
        "SOURCE_LEASE_EXPIRED"
    );
    let takeover = coord
        .acquire_or_takeover(key.clone(), "node-b", 1, "ns", 60)
        .await
        .expect("an expired lease is taken over");
    assert_eq!(takeover.fence_token, 3);
    assert_eq!(takeover.owner_id, "node-b");
    // A takeover cannot change the checkpoint namespace.
    expire(&pool, &key).await;
    assert_eq!(
        coord
            .acquire_or_takeover(key.clone(), "node-c", 1, "other-ns", 60)
            .await
            .unwrap_err()
            .code,
        "SOURCE_CHECKPOINT_NAMESPACE_MISMATCH"
    );
    // Fences survive the process: a second coordinator over the same
    // database continues them.
    expire(&pool, &key).await;
    let other_process = SourceLeaseCoordinator::new(pool.clone());
    let next = other_process
        .acquire_or_takeover(key, "node-c", 1, "ns", 60)
        .await
        .unwrap();
    assert_eq!(next.fence_token, 4);
}

#[tokio::test]
async fn source_runner_executes_privileged_facet_while_app_boundary_blocks_it() {
    let root = CompositionRoot::community()
        .with_provider_backend(Arc::new(EchoSource))
        .build_in_memory()
        .expect("composes");
    let runner = SourceChannelRunner::new(root.providers().clone(), SourceRunnerLimits::default());

    let provider = ProviderRef {
        publisher: "test".to_string(),
        provider_id: TEST_SOURCE.into(),
        version: "2.0.0".to_string(),
        digest: Digest::parse(&format!("sha256:{}", "ef".repeat(32))).unwrap(),
    };
    let conn = ConnectionReference {
        connection_id: "pg-outbox".into(),
        revision: crate::extensions::Revision::FIRST,
        provider,
    };
    let eff_id = EffectiveConnectionIdentity {
        provider_digest: conn.provider.digest.clone(),
        connection_id: "pg-outbox".into(),
        revision: crate::extensions::Revision::FIRST,
        secret_hash: "sha256:5555".to_string(),
    };

    let run = AdmittedRun::admit(
        "run-source",
        1,
        Instant::now() + Duration::from_secs(30),
        BudgetLimits::default(),
        AuthorityGrant {
            allowed_operations: BTreeMap::from([(
                "pg-outbox".to_string(),
                BTreeSet::from(["query".to_string()]),
            )]),
            declared_dependencies: BTreeMap::new(),
            allowed_channels: BTreeSet::new(),
        },
    );

    // Application boundary must reject reserved `orion.internal.source.poll`.
    let app_err = run
        .context()
        .start_invocation(
            conn.clone(),
            eff_id.clone(),
            "orion.internal.source.poll",
            None,
            1,
        )
        .expect_err("app boundary must block reserved source operation");
    assert_eq!(app_err.code, codes::OPERATION_FORBIDDEN);

    // Privileged SourceChannelRunner succeeds in opening and polling the source facet.
    let cursor = runner
        .open_cursor(
            &run.context(),
            SourceTarget {
                connection: conn.clone(),
                effective_identity: eff_id.clone(),
                kind: ImplementationKind::Rust,
                driver_id: TEST_SOURCE.to_string(),
            },
            Some(json!({"lsn": 42})),
        )
        .await
        .expect("source open succeeds");
    assert_eq!(cursor["checkpoint"], json!({"lsn": 42}));

    let batch = runner
        .poll_batch(
            &run.context(),
            SourceTarget {
                connection: conn,
                effective_identity: eff_id,
                kind: ImplementationKind::Rust,
                driver_id: TEST_SOURCE.to_string(),
            },
            &cursor,
            25,
        )
        .await
        .expect("source poll succeeds");
    assert!(batch.items.is_empty());
    assert_eq!(batch.next_checkpoint, Some(json!({"lsn": 42})));
}

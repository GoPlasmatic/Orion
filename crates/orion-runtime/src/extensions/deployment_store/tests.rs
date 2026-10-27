use super::manifest::{RolloutInterval, validate_rollout_partition};
use super::*;
use crate::extensions::Admission;
use crate::extensions::package_identity::canonical_bytes;
use crate::storage::DbPool;
use orion_api::ErrorKind as ErrorClass;
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

fn sample_archive_entries() -> Vec<ArchiveEntryInput> {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../orion-server/tests/fixtures/v2/package-identity");
    let bundle: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture_dir.join("bundle.json")).unwrap())
            .unwrap();
    let pkg = &bundle["providers"][1]; // helper wasm package with no dependencies
    let mut entries = Vec::new();
    for (path, text) in pkg["payloads"].as_object().unwrap() {
        let bytes = text.as_str().unwrap().as_bytes().to_vec();
        entries.push(ArchiveEntryInput {
            path: path.clone(),
            compressed_size: bytes.len(),
            bytes,
            is_symlink: false,
        });
    }
    let inv_bytes = canonical_bytes(&pkg["inventory"]).unwrap();
    entries.push(ArchiveEntryInput {
        path: "inventory.json".to_string(),
        compressed_size: inv_bytes.len(),
        bytes: inv_bytes,
        is_symlink: false,
    });
    entries
}

async fn pool() -> DbPool {
    crate::storage::test_sqlite_pool().await
}

fn sqlite(pool: &DbPool) -> &sqlx::SqlitePool {
    let DbPool::Sqlite(p) = pool else {
        unreachable!("the store's unit tests run on SQLite")
    };
    p
}

fn provider_ref(pkg: &ProviderPackageRow) -> ProviderRef {
    ProviderRef {
        publisher: pkg.publisher.clone(),
        provider_id: pkg.provider_id.clone(),
        version: pkg.version.clone(),
        digest: pkg.package_digest.clone(),
    }
}

fn workflow_manifest(id: &str, revision: u64, digest: &str) -> CanonicalManifest {
    CanonicalManifest {
        schema_version: 2,
        providers: Vec::new(),
        provider_lock_digest: None,
        members: vec![ManifestMember {
            entity: EntityRef {
                kind: EntityKind::Workflow,
                id: id.to_string(),
                revision: crate::extensions::Revision::new(revision).expect("a revision"),
            },
            definition_digest: digest.to_string(),
            owning_package: None,
            requires: Vec::new(),
            rollouts: Vec::new(),
        }],
    }
}

fn empty_manifest() -> CanonicalManifest {
    CanonicalManifest {
        schema_version: 2,
        providers: Vec::new(),
        provider_lock_digest: None,
        members: Vec::new(),
    }
}

async fn definition(store: &ImmutableDeploymentStore, pool: &DbPool, bytes: &[u8]) -> String {
    store
        .store_definition_artifact_in_db(pool, bytes.to_vec())
        .await
        .unwrap()
        .digest
}

/// Move every artifact's `created_at` two hours into the past, out of any
/// grace window a test passes.
async fn backdate_all(pool: &DbPool) {
    sqlx::query("UPDATE artifact_objects SET created_at = datetime('now', '-2 hours')")
        .execute(sqlite(pool))
        .await
        .unwrap();
}

async fn state_of(store: &ImmutableDeploymentStore, pool: &DbPool, digest: &str) -> ArtifactState {
    store
        .fetch_artifact(pool, digest)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{digest} is stored"))
        .state
}

/// A manifest read at the API names a malformed reference as the reference
/// it is, though the types refuse it while reading; the wire form of a
/// typed manifest is the one it was read from.
#[test]
fn a_manifest_is_read_once_and_names_a_malformed_reference() {
    let digest = format!("sha256:{}", "ab".repeat(32));
    let manifest = |provider_digest: &str, revision: u64| {
        json!({
            "schema_version": 2,
            "providers": [{
                "publisher": "acme",
                "provider_id": "acme.crm",
                "version": "1.0.0",
                "digest": provider_digest,
            }],
            "provider_lock_digest": null,
            "members": [{
                "entity": {"kind": "workflow", "id": "orders", "revision": revision},
                "definition_digest": digest,
            }],
        })
    };

    let read = CanonicalManifest::from_json(&manifest(&digest, 1)).expect("reads");
    assert_eq!(read.providers[0].digest, digest);
    assert_eq!(read.members[0].entity.revision.get(), 1);
    let written = serde_json::to_value(&read).unwrap();
    assert_eq!(
        written["providers"][0],
        manifest(&digest, 1)["providers"][0]
    );
    assert_eq!(written["members"][0]["entity"]["revision"], 1);

    let short = CanonicalManifest::from_json(&manifest("sha256:abc", 1)).unwrap_err();
    assert_eq!(short.code(), "PROVIDER_REF_INVALID");
    let upper = CanonicalManifest::from_json(&manifest(&digest.to_uppercase(), 1)).unwrap_err();
    assert_eq!(upper.code(), "PROVIDER_REF_INVALID");
    let zero = CanonicalManifest::from_json(&manifest(&digest, 0)).unwrap_err();
    assert_eq!(zero.code(), "ENTITY_REF_INVALID");
    let garbage = CanonicalManifest::from_json(&json!({"providers": "none"})).unwrap_err();
    assert_eq!(garbage.code(), "INVALID_MANIFEST_JSON");
}

#[test]
fn typed_refs_disjoint_rollouts_and_canonical_manifest_verification() {
    // Reject integer/digest confusion on ProviderRef
    let bad_provider = ProviderRef {
        publisher: "org.orion".to_string(),
        provider_id: "org.orion.http".into(),
        version: "42".to_string(),
        digest: crate::crypto::Digest::from_sha256([0; 32]),
    };
    assert_eq!(
        super::manifest::validate_provider_ref(&bad_provider)
            .unwrap_err()
            .code(),
        "PROVIDER_REF_INVALID"
    );

    // Reject overlapping and gapped rollout intervals
    let gap_rollout = vec![
        RolloutInterval {
            workflow_revision: 1,
            start_bucket: 0,
            end_bucket: 50,
        },
        RolloutInterval {
            workflow_revision: 2,
            start_bucket: 60,
            end_bucket: 100,
        },
    ];
    assert_eq!(
        validate_rollout_partition(&gap_rollout).unwrap_err().code(),
        "ROLLOUT_PARTITION_INVALID"
    );

    // Valid partition [0, 20) + [20, 100)
    let valid_rollout = vec![
        RolloutInterval {
            workflow_revision: 2,
            start_bucket: 0,
            end_bucket: 20,
        },
        RolloutInterval {
            workflow_revision: 1,
            start_bucket: 20,
            end_bucket: 100,
        },
    ];
    validate_rollout_partition(&valid_rollout).expect("disjoint full partition");
}

#[tokio::test]
async fn end_to_end_archive_admit_plan_apply_rollback_and_gc() {
    let pool = pool().await;
    let store = ImmutableDeploymentStore::new("node-test-1");
    let policy = Admission::community();

    // 1. Reject symlink in archive (V2-04.1)
    let mut bad_entries = sample_archive_entries();
    bad_entries[0].is_symlink = true;
    let err = store
        .ingest_verified_archive_in_db(&pool, &bad_entries)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "ARCHIVE_SYMLINK_DENIED");
    assert_eq!(err.kind, DeploymentErrorKind::ArchiveSymlinkDenied);

    // 2. Ingest verified archive and admit package (V2-04.1, V2-04.2)
    let pkg = store
        .ingest_verified_archive_in_db(&pool, &sample_archive_entries())
        .await
        .expect("ingested");
    let adm = store
        .admit_package_in_db(&pool, &pkg.package_digest, &policy, None)
        .await
        .expect("admitted");
    assert!(!adm.revoked);

    // Upload/install alone does not advance deployment_head
    assert_eq!(store.current_head(), (0, 0));

    // 3. Plan & commit revision 1 (V2-04.3, V2-04.4)
    let mut manifest_v1 =
        workflow_manifest("orders-wf", 1, &definition(&store, &pool, b"wf-v1").await);
    manifest_v1.providers.push(provider_ref(&pkg));
    let plan_v1 = store
        .plan_candidate_in_db(&pool, 0, manifest_v1)
        .await
        .expect("planned v1");
    let receipt_v1 = store
        .apply_candidate_in_db(
            &pool,
            &Apply {
                principal: "alice",
                idempotency_key: "idem-1",
                plan: &plan_v1,
                reason: "initial deploy",
            },
            &policy,
        )
        .await
        .expect("applied v1");
    assert_eq!(receipt_v1.committed_revision, 1);
    assert!(
        receipt_v1.created_at.is_some(),
        "the receipt reads back its time"
    );
    assert_eq!(store.current_head(), (1, 1));
    assert!(store.get_revision(1).unwrap().created_at.is_some());

    // Lost-response retry with same principal + idempotency_key + request returns original receipt
    let retry_v1 = store
        .apply_candidate_in_db(
            &pool,
            &Apply {
                principal: "alice",
                idempotency_key: "idem-1",
                plan: &plan_v1,
                reason: "initial deploy",
            },
            &policy,
        )
        .await
        .expect("idempotent retry");
    assert_eq!(retry_v1, receipt_v1);
    assert_eq!(store.current_head(), (1, 1));

    // Reusing idempotency_key with different reason/request fails
    let conflict = store
        .apply_candidate_in_db(
            &pool,
            &Apply {
                principal: "alice",
                idempotency_key: "idem-1",
                plan: &plan_v1,
                reason: "changed reason",
            },
            &policy,
        )
        .await
        .unwrap_err();
    assert_eq!(conflict.code(), "IDEMPOTENCY_KEY_CONFLICT");

    // 4. Plan & commit revision 2
    let manifest_v2 = workflow_manifest("orders-wf", 2, &definition(&store, &pool, b"wf-v2").await);
    let plan_v2 = store
        .plan_candidate_in_db(&pool, 1, manifest_v2)
        .await
        .expect("planned v2");
    let receipt_v2 = store
        .apply_candidate_in_db(
            &pool,
            &Apply {
                principal: "alice",
                idempotency_key: "idem-2",
                plan: &plan_v2,
                reason: "upgrade wf",
            },
            &policy,
        )
        .await
        .expect("applied v2");
    assert_eq!(receipt_v2.committed_revision, 2);

    // 5. Rollback to revision 1 creates revision 3 (V2-04.5)
    let (rollback_receipt, warnings) = store
        .rollback_to_revision_in_db(
            &pool,
            &Rollback {
                principal: "alice",
                idempotency_key: "idem-3",
                expected_head_revision: 2,
                target_revision: 1,
                reason: "rollback to v1",
            },
            &policy,
        )
        .await
        .expect("rolled back to v1");
    assert_eq!(rollback_receipt.committed_revision, 3);
    assert_eq!(rollback_receipt.manifest_digest, receipt_v1.manifest_digest);
    assert!(!warnings.is_empty());

    // 6. Two-phase GC marks unreferenced artifacts DRAINING then DELETED (V2-04.6)
    let plan_v4 = store
        .plan_candidate_in_db(&pool, 3, empty_manifest())
        .await
        .unwrap();
    store
        .apply_candidate_in_db(
            &pool,
            &Apply {
                principal: "alice",
                idempotency_key: "idem-4",
                plan: &plan_v4,
                reason: "clear estate",
            },
            &policy,
        )
        .await
        .unwrap();
    // An admitted package is never collected, referenced or not.
    let pass = store
        .mark_and_collect_garbage_in_db(&pool, 1, 0)
        .await
        .unwrap();
    assert!(!pass.draining.contains(&pkg.package_digest.to_string()));
    assert!(
        !pass.draining.is_empty(),
        "the dropped workflow definitions drain"
    );
    assert!(pass.deleted.is_empty());
    assert!(!pass.compacted.is_empty());
    // Revoked and referenced by no retained revision, it is.
    store
        .revoke_package_in_db(&pool, &pkg.package_digest)
        .await
        .unwrap();
    let pass = store
        .mark_and_collect_garbage_in_db(&pool, 1, 0)
        .await
        .unwrap();
    assert!(pass.draining.contains(&pkg.package_digest.to_string()));

    // Attempting to plan with the revoked, DRAINING package is rejected
    let mut with_revoked = empty_manifest();
    with_revoked.providers.push(provider_ref(&pkg));
    let err = store
        .plan_candidate_in_db(&pool, 4, with_revoked)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "PROVIDER_REVOKED");

    // Second GC sweep transitions DRAINING -> DELETED while receipts remain intact
    let pass = store
        .mark_and_collect_garbage_in_db(&pool, 1, 0)
        .await
        .unwrap();
    assert!(pass.deleted.contains(&pkg.package_digest.to_string()));
}

/// A fresh store reads everything committed back from the database:
/// the head, revisions with their times, packages, admissions, connections,
/// and the receipts an idempotent replay answers. A stored manifest that
/// does not verify fails the read closed.
#[tokio::test]
async fn a_fresh_store_reads_the_database_back_and_fails_closed_on_corruption() {
    let pool = pool().await;
    let store = ImmutableDeploymentStore::new("node-sql-1");
    let policy = Admission::community();
    let pkg = store
        .ingest_verified_archive_in_db(&pool, &sample_archive_entries())
        .await
        .unwrap();
    store
        .admit_package_in_db(&pool, &pkg.package_digest, &policy, None)
        .await
        .unwrap();
    store
        .store_connection_revision_in_db(
            &pool,
            "orders-db",
            1,
            &pkg.package_digest,
            json!({"url": "https://api.example.com"}),
            [(
                "connection_string".to_string(),
                "vault://secrets/orders#v1".to_string(),
            )]
            .into(),
        )
        .await
        .unwrap();

    let mut manifest = workflow_manifest(
        "orders-wf",
        1,
        &definition(&store, &pool, b"wf-sql-v1").await,
    );
    manifest.providers.push(provider_ref(&pkg));
    let plan = store
        .plan_candidate_in_db(&pool, 0, manifest)
        .await
        .unwrap();
    let committed = store
        .apply_candidate_in_db(
            &pool,
            &Apply {
                principal: "alice",
                idempotency_key: "sql-idem-1",
                plan: &plan,
                reason: "persist test",
            },
            &policy,
        )
        .await
        .unwrap();

    let (head_rev, epoch): (i64, i64) =
        sqlx::query_as("SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1")
            .fetch_one(sqlite(&pool))
            .await
            .unwrap();
    assert_eq!((head_rev, epoch), (1, 1));

    let restored = ImmutableDeploymentStore::default();
    restored.load_from_db_pool(&pool).await.expect("load");
    assert_eq!(restored.current_head(), (1, 1));
    let revisions = restored.list_revisions();
    assert_eq!(revisions.len(), 1);
    assert_eq!(
        revisions[0].created_at,
        store.get_revision(1).unwrap().created_at
    );
    assert_eq!(restored.list_packages().len(), 1);
    assert_eq!(
        restored
            .get_package(&pkg.package_digest)
            .unwrap()
            .distribution_json,
        pkg.distribution_json,
        "the full distribution.json is read back from its artifact"
    );
    assert!(
        matches!(restored.package_schemas(&pkg.package_digest), Some(Ok(_))),
        "the package's schemas compile when it enters the catalog"
    );
    assert_eq!(restored.list_admissions().len(), 1);
    assert_eq!(restored.list_connections().len(), 1);

    // TD-15: the receipt reads back as it was committed, not rebuilt from
    // the revision.
    let replay = restored
        .apply_candidate_in_db(
            &pool,
            &Apply {
                principal: "alice",
                idempotency_key: "sql-idem-1",
                plan: &plan,
                reason: "persist test",
            },
            &policy,
        )
        .await
        .expect("idempotent replay after restart answers the stored receipt");
    assert_eq!(replay, committed);
    assert_eq!(replay.receiving_node_id, "node-sql-1");

    // A second load reads only what is new: revision 2 joins revision 1.
    let plan_v2 = store
        .plan_candidate_in_db(&pool, 1, empty_manifest())
        .await
        .unwrap();
    store
        .apply_candidate_in_db(
            &pool,
            &Apply {
                principal: "alice",
                idempotency_key: "sql-idem-2",
                plan: &plan_v2,
                reason: "v2",
            },
            &policy,
        )
        .await
        .unwrap();
    restored.load_from_db_pool(&pool).await.unwrap();
    assert_eq!(restored.current_head(), (2, 2));
    assert_eq!(restored.list_revisions().len(), 2);

    // TD-23: compaction deletes the compacted revision's member rows, and
    // a peer's catalog learns the flag.
    let pass = store
        .mark_and_collect_garbage_in_db(&pool, 1, 0)
        .await
        .unwrap();
    assert_eq!(pass.compacted, vec![1]);
    let (member_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM deployment_members WHERE deployment_revision = 1")
            .fetch_one(sqlite(&pool))
            .await
            .unwrap();
    assert_eq!(member_count, 0);
    restored.load_from_db_pool(&pool).await.unwrap();
    assert!(restored.get_revision(1).unwrap().members_compacted);

    // TD-16: a tampered manifest_json fails closed.
    sqlx::query("UPDATE deployment_revisions SET manifest_json = '{bad json' WHERE revision = 2")
        .execute(sqlite(&pool))
        .await
        .unwrap();
    let err = ImmutableDeploymentStore::default()
        .load_from_db_pool(&pool)
        .await
        .expect_err("corrupt manifest_json must fail closed");
    assert_eq!(err.code(), "CORRUPT_PERSISTED_STATE");
    assert_eq!(err.class(), ErrorClass::Internal);

    // Bytes that do not hash to their digest are refused, not served.
    let digest = definition(&store, &pool, b"tampered").await;
    sqlx::query("UPDATE artifact_objects SET payload_bytes = x'00' WHERE digest = ?1")
        .bind(&digest)
        .execute(sqlite(&pool))
        .await
        .unwrap();
    let err = store.fetch_artifact(&pool, &digest).await.unwrap_err();
    assert_eq!(err.kind, DeploymentErrorKind::CorruptPersistedState);
}

#[tokio::test]
async fn connection_secret_rejection_canonical_digests_and_staged_gc_protection() {
    let pool = pool().await;
    let store = ImmutableDeploymentStore::new("node-sec-1");
    let policy = Admission::community();
    let pkg = store
        .ingest_verified_archive_in_db(&pool, &sample_archive_entries())
        .await
        .unwrap();
    store
        .admit_package_in_db(&pool, &pkg.package_digest, &policy, None)
        .await
        .unwrap();

    // TD-20: Reject nested secret keys and credential-bearing URLs in public_config
    for bad_cfg in [
        json!({ "nested": { "api_token": "tok-123" } }),
        json!({ "headers": { "Authorization": "Bearer secret-jwt" } }),
        json!({ "url": "postgres://admin:hunter2@db.example.com:5432/app" }),
        json!({ "url": "https://api.example.com/v1?api_key=secret123" }),
    ] {
        let err = store
            .store_connection_revision_in_db(
                &pool,
                "conn-bad",
                1,
                &pkg.package_digest,
                bad_cfg,
                Default::default(),
            )
            .await
            .expect_err("secret in public_config must be rejected");
        assert_eq!(err.code(), "CONNECTION_PUBLIC_SECRET_FORBIDDEN");
    }

    // TD-20: Key-reordered JSON produces identical canonical configuration_digest
    let conn_a = store
        .store_connection_revision_in_db(
            &pool,
            "conn-canon",
            1,
            &pkg.package_digest,
            json!({ "b": 2, "a": 1 }),
            Default::default(),
        )
        .await
        .unwrap();
    let conn_b = store
        .store_connection_revision_in_db(
            &pool,
            "conn-canon",
            1,
            &pkg.package_digest,
            json!({ "a": 1, "b": 2 }),
            Default::default(),
        )
        .await
        .expect("canonical public_config with reordered keys must be idempotent");
    assert_eq!(conn_a.configuration_digest, conn_b.configuration_digest);
    let err = store
        .store_connection_revision_in_db(
            &pool,
            "conn-canon",
            1,
            &pkg.package_digest,
            json!({ "a": 2 }),
            Default::default(),
        )
        .await
        .expect_err("a stored revision is immutable");
    assert_eq!(err.code(), "IMMUTABLE_CONNECTION_CONFLICT");

    // TD-22: Newly staged package and connection (never yet deployed) are
    // protected from GC — with no deployment, no pass runs at all.
    let pass = store
        .mark_and_collect_garbage_in_db(&pool, 1, 0)
        .await
        .unwrap();
    assert!(pass.is_empty());

    // TD-19: Connection member with wrong definition_digest is rejected
    let bad_conn_manifest = CanonicalManifest {
        schema_version: 2,
        providers: vec![provider_ref(&pkg)],
        provider_lock_digest: None,
        members: vec![ManifestMember {
            entity: EntityRef {
                kind: EntityKind::Connection,
                id: "conn-canon".to_string(),
                revision: crate::extensions::Revision::FIRST,
            },
            definition_digest: definition(&store, &pool, b"wrong-digest").await,
            owning_package: None,
            requires: Vec::new(),
            rollouts: Vec::new(),
        }],
    };
    let err = store
        .plan_candidate_in_db(&pool, 0, bad_conn_manifest)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "CONNECTION_DIGEST_MISMATCH");
}

#[tokio::test]
async fn cross_replica_db_transactional_cas_and_revocation_enforcement() {
    let db_pool = pool().await;
    let store_a = ImmutableDeploymentStore::new("replica-a");
    let store_b = ImmutableDeploymentStore::new("replica-b");
    let policy = Admission::community();

    let pkg = store_a
        .ingest_verified_archive_in_db(&db_pool, &sample_archive_entries())
        .await
        .unwrap();
    store_a
        .admit_package_in_db(&db_pool, &pkg.package_digest, &policy, None)
        .await
        .unwrap();

    let mut manifest_a =
        workflow_manifest("wf-a", 1, &definition(&store_a, &db_pool, b"wf-a-v1").await);
    manifest_a.providers.push(provider_ref(&pkg));
    let mut manifest_b =
        workflow_manifest("wf-b", 1, &definition(&store_a, &db_pool, b"wf-b-v1").await);
    manifest_b.providers.push(provider_ref(&pkg));

    // Both replicas plan against base_revision = 0 before either applies
    let plan_a = store_a
        .plan_candidate_in_db(&db_pool, 0, manifest_a)
        .await
        .unwrap();
    let plan_b = store_b
        .plan_candidate_in_db(&db_pool, 0, manifest_b)
        .await
        .unwrap();

    // Replica A commits revision 1
    let receipt_a = store_a
        .apply_candidate_in_db(
            &db_pool,
            &Apply {
                principal: "alice",
                idempotency_key: "idem-a",
                plan: &plan_a,
                reason: "deploy a",
            },
            &policy,
        )
        .await
        .expect("replica A wins CAS on base_revision 0");
    assert_eq!(receipt_a.committed_revision, 1);

    // Replica B's stale plan against base_revision 0 must fail inside the DB transaction
    let err_b = store_b
        .apply_candidate_in_db(
            &db_pool,
            &Apply {
                principal: "bob",
                idempotency_key: "idem-b",
                plan: &plan_b,
                reason: "deploy b",
            },
            &policy,
        )
        .await
        .expect_err("competing deployment on stale base_revision must fail");
    assert_eq!(err_b.code(), "DEPLOYMENT_HEAD_CONFLICT");
    assert_eq!(err_b.class(), ErrorClass::Conflict);

    // Now Replica B plans against revision 1, but Replica A revokes the provider package first
    let plan_b2 = store_b
        .plan_candidate_in_db(&db_pool, 1, plan_b.candidate_manifest.clone())
        .await
        .unwrap();
    store_a
        .revoke_package_in_db(&db_pool, &pkg.package_digest)
        .await
        .unwrap();

    let err_revoked = store_b
        .apply_candidate_in_db(
            &db_pool,
            &Apply {
                principal: "bob",
                idempotency_key: "idem-b2",
                plan: &plan_b2,
                reason: "deploy b2",
            },
            &policy,
        )
        .await
        .expect_err("DB-level package revocation must block replica B's apply");
    assert_eq!(err_revoked.code(), "ADMISSION_INVALIDATED");
}

#[tokio::test]
async fn a_failed_write_publishes_nothing() {
    let db_pool = pool().await;
    let store = ImmutableDeploymentStore::new("node-tx");
    let pkg = store
        .ingest_verified_archive_in_db(&db_pool, &sample_archive_entries())
        .await
        .unwrap();
    store
        .admit_package_in_db(&db_pool, &pkg.package_digest, &Admission::community(), None)
        .await
        .unwrap();

    // Request 1 commits connection revision 1
    store
        .store_connection_revision_in_db(
            &db_pool,
            "conn-1",
            1,
            &pkg.package_digest,
            json!({"url": "https://one.example.com"}),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(store.list_connections().len(), 1);

    // Request 2's write fails in the database.
    sqlx::query(
        "CREATE TRIGGER fail_conn_insert BEFORE INSERT ON connection_revisions \
             BEGIN SELECT RAISE(FAIL, 'simulated write failure'); END",
    )
    .execute(sqlite(&db_pool))
    .await
    .unwrap();
    let err = store
        .store_connection_revision_in_db(
            &db_pool,
            "conn-2",
            1,
            &pkg.package_digest,
            json!({"url": "https://two.example.com"}),
            Default::default(),
        )
        .await
        .expect_err("DB failure must return error");
    assert_eq!(err.code(), "DB_WRITE_ERROR");
    assert_eq!(err.class(), ErrorClass::Unavailable);

    // Request 1's committed state remains intact and Request 2 was never
    // published, to the catalog or the database.
    let conns = store.list_connections();
    assert_eq!(conns.len(), 1);
    assert_eq!(conns[0].connection_id, "conn-1");
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM connection_revisions")
        .fetch_one(sqlite(&db_pool))
        .await
        .unwrap();
    assert_eq!(count, 1);
}

/// A connection's provider must be declared in the manifest, unless it is a
/// built-in driver, which ships with the binary and has no package.
#[tokio::test]
async fn a_builtin_provider_needs_no_declaration_and_an_uploaded_one_does() {
    let pool = pool().await;
    let store = ImmutableDeploymentStore::new("node-builtin");
    let pkg = store
        .ingest_verified_archive_in_db(&pool, &sample_archive_entries())
        .await
        .unwrap();
    store
        .admit_package_in_db(&pool, &pkg.package_digest, &Admission::community(), None)
        .await
        .unwrap();
    let builtin =
        super::super::foundation::builtin_provider_digest(super::super::foundation::DRIVER_HTTP)
            .expect("a built-in driver");
    let connection = async |id: &str, provider: &str| {
        let row = store
            .store_connection_revision_in_db(
                &pool,
                id,
                1,
                provider,
                json!({"url": "https://api.example.com"}),
                Default::default(),
            )
            .await
            .unwrap();
        CanonicalManifest {
            schema_version: 2,
            providers: Vec::new(),
            provider_lock_digest: None,
            members: vec![ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Connection,
                    id: id.to_string(),
                    revision: crate::extensions::Revision::FIRST,
                },
                definition_digest: row.configuration_digest.to_string(),
                owning_package: None,
                requires: Vec::new(),
                rollouts: Vec::new(),
            }],
        }
    };

    let builtin_manifest = connection("builtin-api", &builtin).await;
    store
        .plan_candidate_in_db(&pool, 0, builtin_manifest)
        .await
        .expect("a built-in provider plans undeclared");
    let uploaded_manifest = connection("uploaded-api", &pkg.package_digest).await;
    let err = store
        .plan_candidate_in_db(&pool, 0, uploaded_manifest)
        .await
        .expect_err("an undeclared package is refused");
    assert_eq!(err.code(), "CONNECTION_PROVIDER_NOT_DECLARED");
}

/// A key naming an endpoint is public, even when it mentions a token; a key
/// naming a token is a secret wherever it sits.
#[test]
fn an_endpoint_key_is_public_and_a_token_key_is_not() {
    for key in [
        "token_url",
        "TOKEN_URL",
        "authorization_endpoint",
        "token_endpoint",
        "redirect_uri",
        "jwks_url",
    ] {
        assert!(!is_public_config_secret_key(key), "{key} names an endpoint");
    }
    for key in [
        "token",
        "access_token",
        "refresh_token",
        "api_token",
        "session_token",
        "client_secret",
        "password",
    ] {
        assert!(is_public_config_secret_key(key), "{key} names a secret");
    }

    let locators = std::collections::BTreeMap::from([(
        "auth.client_secret".to_string(),
        "env://OAUTH_CLIENT_SECRET".to_string(),
    )]);
    check_connection_config(
        &serde_json::json!({"auth": {"type": "oauth2",
            "token_url": "https://idp.example.com/oauth2/token"}}),
        &locators,
    )
    .expect("an endpoint is public config");
    // The value is still screened: credentials in it are refused.
    let err = check_connection_config(
        &serde_json::json!({"auth": {
            "token_url": "https://idp.example.com/oauth2/token?access_token=x"}}),
        &locators,
    )
    .expect_err("a secret-named query parameter is refused");
    assert_eq!(err.code(), "CONNECTION_PUBLIC_SECRET_FORBIDDEN");
    let err = check_connection_config(
        &serde_json::json!({"auth": {"refresh_token": "literal"}}),
        &std::collections::BTreeMap::new(),
    )
    .expect_err("a token key is refused");
    assert_eq!(err.code(), "CONNECTION_PUBLIC_SECRET_FORBIDDEN");
}

/// What one collection pass may never take, each case on its own: the
/// retained revisions' references, every connection's configuration, a
/// package that is not revoked and its files, and what the grace window
/// names. And the two phases in order, the revival of a re-referenced
/// draining artifact, and a re-upload reviving a deleted one.
#[tokio::test]
async fn collection_never_takes_what_is_referenced_retained_admitted_or_recent() {
    const GRACE: u64 = 3_600;
    let pool = pool().await;
    let store = ImmutableDeploymentStore::new("node-gc");
    let policy = Admission::community();
    let pass = async || {
        store
            .mark_and_collect_garbage_in_db(&pool, 2, GRACE)
            .await
            .unwrap()
    };

    // Nothing deployed yet: no pass runs, whatever is stored.
    let orphan = definition(&store, &pool, b"orphan").await;
    backdate_all(&pool).await;
    assert!(pass().await.is_empty());

    // Revisions 1..=3, each naming a different definition; retain 2 keeps
    // the head (3) and 2.
    let mut head = 0;
    let mut digests = Vec::new();
    for (i, bytes) in [&b"wf-1"[..], b"wf-2", b"wf-3"].into_iter().enumerate() {
        let d = definition(&store, &pool, bytes).await;
        let plan = store
            .plan_candidate_in_db(&pool, head, workflow_manifest("wf", 1, &d))
            .await
            .unwrap();
        head = store
            .apply_candidate_in_db(
                &pool,
                &Apply {
                    principal: "ops",
                    idempotency_key: &format!("gc-{i}"),
                    plan: &plan,
                    reason: "r",
                },
                &policy,
            )
            .await
            .unwrap()
            .committed_revision;
        digests.push(d);
    }
    // A connection revision and a package awaiting admission, neither in any
    // manifest; then, after everything is backdated, a recent upload no
    // revision names yet.
    let conn = store
        .store_connection_revision_in_db(
            &pool,
            "gc-conn",
            1,
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            json!({"url": "https://api.example.com"}),
            Default::default(),
        )
        .await
        .unwrap();
    let pkg = store
        .ingest_verified_archive_in_db(&pool, &sample_archive_entries())
        .await
        .unwrap();
    let pkg_files = extract_inventory_file_digests(&pkg.inventory_json);
    backdate_all(&pool).await;
    let pending = definition(&store, &pool, b"uploaded-for-the-next-apply").await;

    let first = pass().await;
    assert_eq!(
        first.draining.iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([&orphan, &digests[0]]),
        "only the orphan and the definition of the compacted revision 1 drain"
    );
    assert_eq!(first.compacted, vec![1]);
    for kept in [
        &digests[1],
        &digests[2],
        &conn.configuration_digest.to_string(),
        &pending,
    ] {
        assert_eq!(
            state_of(&store, &pool, kept).await,
            ArtifactState::Active,
            "{kept}"
        );
    }
    assert_eq!(
        state_of(&store, &pool, &pkg.package_digest).await,
        ArtifactState::Active
    );
    for file in &pkg_files {
        assert_eq!(
            state_of(&store, &pool, file).await,
            ArtifactState::Active,
            "package file {file}"
        );
    }
    // A compacted revision is no longer a rollback target.
    let err = store
        .rollback_to_revision_in_db(
            &pool,
            &Rollback {
                principal: "ops",
                idempotency_key: "gc-rb",
                expected_head_revision: head,
                target_revision: 1,
                reason: "r",
            },
            &policy,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "ROLLBACK_TARGET_COMPACTED");

    // Revision 1's definition is named again by a new head before the next
    // pass: it is put back, not deleted. The orphan goes.
    let err = store
        .plan_candidate_in_db(&pool, head, workflow_manifest("wf", 1, &digests[0]))
        .await
        .unwrap_err();
    assert_eq!(
        err.code(),
        "ARTIFACT_NOT_ACTIVE",
        "a draining artifact cannot be deployed until it is uploaded again"
    );
    definition(&store, &pool, b"wf-1").await;
    assert_eq!(
        state_of(&store, &pool, &digests[0]).await,
        ArtifactState::Active,
        "re-upload revives"
    );
    let plan = store
        .plan_candidate_in_db(&pool, head, workflow_manifest("wf", 1, &digests[0]))
        .await
        .unwrap();
    store
        .apply_candidate_in_db(
            &pool,
            &Apply {
                principal: "ops",
                idempotency_key: "gc-4",
                plan: &plan,
                reason: "r",
            },
            &policy,
        )
        .await
        .unwrap();
    backdate_all(&pool).await;
    let second = pass().await;
    assert_eq!(second.deleted, vec![orphan.clone()]);
    assert!(second.deleted_bytes > 0);
    let row = store.fetch_artifact(&pool, &orphan).await.unwrap().unwrap();
    assert_eq!(row.state, ArtifactState::Deleted);
    assert!(row.bytes.is_none());
    assert_eq!(
        state_of(&store, &pool, &digests[0]).await,
        ArtifactState::Active
    );
    for kept in [&conn.configuration_digest, &pkg.package_digest] {
        assert_eq!(
            state_of(&store, &pool, kept).await,
            ArtifactState::Active,
            "{kept}"
        );
    }

    // A deleted artifact uploaded again is served again.
    assert_eq!(definition(&store, &pool, b"orphan").await, orphan);
    let row = store.fetch_artifact(&pool, &orphan).await.unwrap().unwrap();
    assert_eq!(row.state, ArtifactState::Active);
    assert_eq!(row.bytes.as_deref(), Some(&b"orphan"[..]));
}

/// A draining artifact that a pass finds referenced again — a peer applied a
/// revision naming it between two passes — is revived, never deleted.
#[tokio::test]
async fn a_draining_artifact_a_retained_revision_names_is_revived() {
    let pool = pool().await;
    let store = ImmutableDeploymentStore::new("node-gc-revive");
    let d = definition(&store, &pool, b"wf").await;
    let plan = store
        .plan_candidate_in_db(&pool, 0, workflow_manifest("wf", 1, &d))
        .await
        .unwrap();
    store
        .apply_candidate_in_db(
            &pool,
            &Apply {
                principal: "ops",
                idempotency_key: "rv-1",
                plan: &plan,
                reason: "r",
            },
            &Admission::community(),
        )
        .await
        .unwrap();
    sqlx::query(
        "UPDATE artifact_objects SET state = 'draining', drain_epoch = 0 WHERE digest = ?1",
    )
    .bind(&d)
    .execute(sqlite(&pool))
    .await
    .unwrap();
    let out = store
        .mark_and_collect_garbage_in_db(&pool, 1, 0)
        .await
        .unwrap();
    assert!(out.deleted.is_empty(), "deleted nothing");
    assert_eq!(out.revived, vec![d.clone()]);
    assert_eq!(state_of(&store, &pool, &d).await, ArtifactState::Active);
}

/// A revision another node committed is read from the database when the
/// catalog has not caught up with it, and the catalog then catches up
/// without re-reading what it holds.
#[tokio::test]
async fn a_peer_revision_is_read_through_and_then_loaded_incrementally() {
    let pool = pool().await;
    let a = ImmutableDeploymentStore::new("node-a");
    let b = ImmutableDeploymentStore::new("node-b");
    let policy = Admission::community();
    let mut head = 0;
    for i in 1..=3u64 {
        let d = definition(&a, &pool, format!("wf-{i}").as_bytes()).await;
        let plan = a
            .plan_candidate_in_db(&pool, head, workflow_manifest("wf", i, &d))
            .await
            .unwrap();
        head = a
            .apply_candidate_in_db(
                &pool,
                &Apply {
                    principal: "ops",
                    idempotency_key: &format!("inc-{i}"),
                    plan: &plan,
                    reason: "r",
                },
                &policy,
            )
            .await
            .unwrap()
            .committed_revision;
        if i == 1 {
            b.load_from_db_pool(&pool).await.unwrap();
        }
    }
    assert_eq!(b.current_head(), (1, 1));
    assert!(b.get_revision(3).is_none(), "the catalog is behind");
    let read_through = b.revision(&pool, 3).await.unwrap().expect("read through");
    assert_eq!(read_through.revision, 3);

    // Revision 1 as the catalog holds it is kept, not re-read: a row
    // tampered with after it was loaded is not seen again.
    sqlx::query("UPDATE deployment_revisions SET reason = 'changed' WHERE revision = 1")
        .execute(sqlite(&pool))
        .await
        .unwrap();
    b.load_from_db_pool(&pool).await.unwrap();
    assert_eq!(b.current_head(), (3, 3));
    assert_eq!(b.list_revisions().len(), 3);
    assert_eq!(b.get_revision(1).unwrap().reason, "r");
    assert_eq!(
        b.list_revisions()
            .iter()
            .map(|r| r.revision)
            .collect::<HashSet<_>>(),
        HashSet::from([1, 2, 3])
    );
}

/// A provider's answer is held to its operation's `output_schema`: the
/// fixture's `contacts.get` answers a contact whose `display_name` is a
/// non-empty string. A refused answer is a contract violation carrying the
/// effect status it is given (never retried), naming the field; an
/// operation or package with no declared schema checks nothing.
#[tokio::test]
async fn an_answer_is_held_to_the_operation_output_schema() {
    let pool = pool().await;
    let store = ImmutableDeploymentStore::new("node-test-1");
    let pkg = store
        .ingest_verified_archive_in_db(&pool, &sample_archive_entries())
        .await
        .expect("ingested");
    let digest = pkg.package_digest.to_string();
    let check = |output: Value| {
        store.check_operation_output(
            &digest,
            "contacts.get",
            "crm",
            &output,
            EffectStatus::Completed,
        )
    };

    check(json!({"contact_id": "c-1", "display_name": "Ada"})).expect("a conforming answer");

    let refused = check(json!({"contact_id": "c-1", "display_name": ""})).unwrap_err();
    assert_eq!(refused.code, crate::extensions::codes::CONTRACT_VIOLATION);
    assert_eq!(refused.effect_status, EffectStatus::Completed);
    assert!(
        refused.message.contains("`display_name` in its answer"),
        "{}",
        refused.message
    );
    assert_eq!(
        refused
            .details
            .as_deref()
            .and_then(|d| d["pointer"].as_str()),
        Some("/display_name")
    );

    let unknown = check(json!({"contact_id": "c-1", "display_name": "Ada", "extra": 1}));
    assert!(unknown.is_err(), "additionalProperties is false");

    check(json!("anything")).expect_err("not an object");
    store
        .check_operation_output(
            &digest,
            "no.such.op",
            "crm",
            &json!(1),
            EffectStatus::Completed,
        )
        .expect("an undeclared operation declares no schema");
    store
        .check_operation_output(
            "sha256:none",
            "contacts.get",
            "crm",
            &json!(1),
            EffectStatus::Completed,
        )
        .expect("no stored package, nothing to check");
}

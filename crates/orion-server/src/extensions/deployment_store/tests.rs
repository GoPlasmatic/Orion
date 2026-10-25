use super::*;
use crate::extensions::CommunityAdmissionPolicy;
use std::path::PathBuf;

fn sample_archive_entries() -> Vec<ArchiveEntryInput> {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2/package-identity");
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

#[test]
fn typed_refs_disjoint_rollouts_and_canonical_manifest_verification() {
    // Reject integer/digest confusion on ProviderRef
    let bad_provider = ProviderRef {
        publisher: "org.orion".to_string(),
        provider_id: "org.orion.http".to_string(),
        version: "42".to_string(),
        digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .to_string(),
    };
    assert_eq!(
        bad_provider.validate().unwrap_err().code,
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
        validate_rollout_partition(&gap_rollout).unwrap_err().code,
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

#[test]
fn end_to_end_archive_admit_plan_apply_rollback_and_gc() {
    let store = ImmutableDeploymentStore::new("node-test-1");
    let policy = CommunityAdmissionPolicy::new();

    // 1. Reject symlink in archive (V2-04.1)
    let mut bad_entries = sample_archive_entries();
    bad_entries[0].is_symlink = true;
    assert_eq!(
        store
            .ingest_verified_archive(&bad_entries)
            .unwrap_err()
            .code,
        "ARCHIVE_SYMLINK_DENIED"
    );

    // 2. Ingest verified archive and admit package (V2-04.1, V2-04.2)
    let entries = sample_archive_entries();
    let pkg = store.ingest_verified_archive(&entries).expect("ingested");
    let adm = store
        .admit_package(&pkg.package_digest, &policy, None)
        .expect("admitted");
    assert!(!adm.revoked);

    // Upload/install alone does not advance deployment_head
    assert_eq!(store.current_head(), (0, 0));

    // 3. Plan & commit revision 1 (V2-04.3, V2-04.4)
    let manifest_v1 = CanonicalManifest {
        schema_version: 2,
        providers: vec![ProviderRef {
            publisher: pkg.publisher.clone(),
            provider_id: pkg.provider_id.clone(),
            version: pkg.version.clone(),
            digest: pkg.package_digest.clone(),
        }],
        provider_lock_digest: None,
        members: vec![ManifestMember {
            entity: EntityRef {
                kind: EntityKind::Workflow,
                id: "orders-wf".to_string(),
                revision: 1,
            },
            definition_digest: store
                .store_definition_artifact(b"wf-v1".to_vec())
                .unwrap()
                .digest,
            owning_package: None,
            requires: Vec::new(),
            rollouts: Vec::new(),
        }],
    };
    let plan_v1 = store.plan_candidate(0, manifest_v1).expect("planned v1");
    let receipt_v1 = store
        .apply_candidate("alice", "idem-1", &plan_v1, "initial deploy", &policy)
        .expect("applied v1");
    assert_eq!(receipt_v1.committed_revision, 1);
    assert_eq!(store.current_head(), (1, 1));

    // Lost-response retry with same principal + idempotency_key + request returns original receipt
    let retry_v1 = store
        .apply_candidate("alice", "idem-1", &plan_v1, "initial deploy", &policy)
        .expect("idempotent retry");
    assert_eq!(retry_v1, receipt_v1);
    assert_eq!(store.current_head(), (1, 1));

    // Reusing idempotency_key with different reason/request fails
    let conflict = store
        .apply_candidate("alice", "idem-1", &plan_v1, "changed reason", &policy)
        .unwrap_err();
    assert_eq!(conflict.code, "IDEMPOTENCY_KEY_CONFLICT");

    // 4. Plan & commit revision 2
    let manifest_v2 = CanonicalManifest {
        schema_version: 2,
        providers: Vec::new(),
        provider_lock_digest: None,
        members: vec![ManifestMember {
            entity: EntityRef {
                kind: EntityKind::Workflow,
                id: "orders-wf".to_string(),
                revision: 2,
            },
            definition_digest: store
                .store_definition_artifact(b"wf-v2".to_vec())
                .unwrap()
                .digest,
            owning_package: None,
            requires: Vec::new(),
            rollouts: Vec::new(),
        }],
    };
    let plan_v2 = store.plan_candidate(1, manifest_v2).expect("planned v2");
    let receipt_v2 = store
        .apply_candidate("alice", "idem-2", &plan_v2, "upgrade wf", &policy)
        .expect("applied v2");
    assert_eq!(receipt_v2.committed_revision, 2);

    // 5. Rollback to revision 1 creates revision 3 (V2-04.5)
    let (rollback_receipt, warnings) = store
        .rollback_to_revision(
            &Rollback {
                principal: "alice",
                idempotency_key: "idem-3",
                expected_head_revision: 2,
                target_revision: 1,
                reason: "rollback to v1",
            },
            &policy,
        )
        .expect("rolled back to v1");
    assert_eq!(rollback_receipt.committed_revision, 3);
    assert_eq!(rollback_receipt.manifest_digest, receipt_v1.manifest_digest);
    assert!(!warnings.is_empty());

    // 6. Two-phase GC marks unreferenced artifacts DRAINING then DELETED (V2-04.6)
    let mut empty_manifest = CanonicalManifest {
        schema_version: 2,
        providers: Vec::new(),
        provider_lock_digest: None,
        members: Vec::new(),
    };
    let plan_v4 = store.plan_candidate(3, empty_manifest.clone()).unwrap();
    store
        .apply_candidate("alice", "idem-4", &plan_v4, "clear estate", &policy)
        .unwrap();
    let (draining, deleted, compacted) = store.mark_and_collect_garbage(1);
    assert!(!draining.is_empty());
    assert!(deleted.is_empty());
    assert!(!compacted.is_empty());

    // Attempting to plan with a DRAINING artifact is rejected
    empty_manifest.providers.push(ProviderRef {
        publisher: pkg.publisher,
        provider_id: pkg.provider_id,
        version: pkg.version,
        digest: pkg.package_digest.clone(),
    });
    assert_eq!(
        store.plan_candidate(4, empty_manifest).unwrap_err().code,
        "ARTIFACT_NOT_ACTIVE"
    );

    // Second GC sweep transitions DRAINING -> DELETED while receipts remain intact
    let (_, deleted2, _) = store.mark_and_collect_garbage(1);
    assert!(deleted2.contains(&pkg.package_digest));
}

#[test]
fn legacy_backfill_converts_community_estate_and_blocks_enterprise_connectors() {
    let http_cfg = json!({ "type": "http", "base_url": "https://api.example.com" });
    let wf_json = json!({ "id": "wf1", "tasks": [] });
    let manifest = ImmutableDeploymentStore::build_legacy_bootstrap_manifest(
        EditionTier::Community,
        &[("ext-api", &http_cfg)],
        &[("wf1", 1, &wf_json)],
        &[("orders", 1, "wf1", 1)],
    )
    .expect("community estate converts");
    assert_eq!(manifest.providers.len(), 1);
    assert_eq!(manifest.providers[0].provider_id, "org.orion.http");

    // Enterprise connector (kafka) blocks Community backfill with actionable code
    let kafka_cfg = json!({ "type": "kafka", "brokers": ["localhost:9092"] });
    let err = ImmutableDeploymentStore::build_legacy_bootstrap_manifest(
        EditionTier::Community,
        &[("events-bus", &kafka_cfg)],
        &[],
        &[],
    )
    .unwrap_err();
    assert_eq!(err.code, "BACKFILL_ENTERPRISE_PROVIDER_REQUIRED");
}

#[tokio::test]
async fn sqlite_migration_and_persistence_roundtrip() {
    use std::str::FromStr;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
        .await
        .unwrap();
    crate::storage::migrator_for(crate::storage::DbBackend::Sqlite)
        .run(&pool)
        .await
        .expect("sqlite migrations including 023_v2_immutable_deployments must apply");

    let store = ImmutableDeploymentStore::new("node-sql-1");
    let policy = CommunityAdmissionPolicy::new();
    let entries = sample_archive_entries();
    let pkg = store.ingest_verified_archive(&entries).unwrap();
    store
        .admit_package(&pkg.package_digest, &policy, None)
        .unwrap();
    store
        .store_connection_revision(
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
        .unwrap();

    let manifest = CanonicalManifest {
        schema_version: 2,
        providers: vec![ProviderRef {
            publisher: pkg.publisher.clone(),
            provider_id: pkg.provider_id.clone(),
            version: pkg.version.clone(),
            digest: pkg.package_digest.clone(),
        }],
        provider_lock_digest: None,
        members: vec![ManifestMember {
            entity: EntityRef {
                kind: EntityKind::Workflow,
                id: "orders-wf".to_string(),
                revision: 1,
            },
            definition_digest: store
                .store_definition_artifact(b"wf-sql-v1".to_vec())
                .unwrap()
                .digest,
            owning_package: None,
            requires: Vec::new(),
            rollouts: Vec::new(),
        }],
    };
    let plan = store.plan_candidate(0, manifest).unwrap();
    let committed = store
        .apply_candidate("alice", "sql-idem-1", &plan, "persist test", &policy)
        .unwrap();

    store
        .persist_to_sqlite(&pool)
        .await
        .expect("persist_to_sqlite succeeds");

    let (head_rev, epoch): (i64, i64) =
        sqlx::query_as("SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((head_rev, epoch), (1, 1));

    let (op_rev,): (i64,) = sqlx::query_as(
            "SELECT committed_revision FROM deployment_operations WHERE principal = 'alice' AND idempotency_key = 'sql-idem-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(op_rev, 1);

    let restored = ImmutableDeploymentStore::default();
    restored
        .load_from_sqlite(&pool)
        .await
        .expect("load_from_sqlite succeeds");
    assert_eq!(restored.current_head(), (1, 1));
    assert_eq!(restored.list_revisions().len(), 1);
    assert_eq!(restored.list_packages().len(), 1);
    assert_eq!(restored.list_admissions().len(), 1);
    assert_eq!(restored.list_connections().len(), 1);

    // TD-15: Verify artifact_objects and deployment_operations (receipts) hydrated across restart
    let replay_receipt = restored
        .apply_candidate("alice", "sql-idem-1", &plan, "persist test", &policy)
        .expect("idempotent replay after restart must return hydrated receipt");
    assert_eq!(replay_receipt.committed_revision, 1);
    // The receipt reads back as it was committed, not rebuilt from the revision.
    assert_eq!(replay_receipt.operation_id, committed.operation_id);
    assert_eq!(replay_receipt.epoch, committed.epoch);
    assert_eq!(replay_receipt.receiving_node_id, "node-sql-1");

    // Re-planning with the hydrated package must succeed (proving artifact_objects hydrated as ACTIVE)
    let manifest_v2 = CanonicalManifest {
        schema_version: 2,
        providers: vec![ProviderRef {
            publisher: pkg.publisher.clone(),
            provider_id: pkg.provider_id.clone(),
            version: pkg.version.clone(),
            digest: pkg.package_digest.clone(),
        }],
        provider_lock_digest: None,
        members: Vec::new(),
    };
    let plan_v2 = restored
        .plan_candidate(1, manifest_v2)
        .expect("hydrated package artifact must be ACTIVE");
    restored
        .apply_candidate("alice", "sql-idem-2", &plan_v2, "v2 deploy", &policy)
        .unwrap();
    restored.persist_to_sqlite(&pool).await.unwrap();

    // TD-23: Compaction deletes compacted deployment_members rows in SQLite
    let (_, _, compacted) = restored.mark_and_collect_garbage(1);
    assert_eq!(compacted, vec![1]);
    restored.persist_to_sqlite(&pool).await.unwrap();
    let (member_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM deployment_members WHERE deployment_revision = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        member_count, 0,
        "compacted deployment_members rows must be deleted from SQL"
    );

    // TD-16: Tampered manifest_json in SQLite fails closed with CORRUPT_PERSISTED_STATE
    sqlx::query("UPDATE deployment_revisions SET manifest_json = '{bad json' WHERE revision = 2")
        .execute(&pool)
        .await
        .unwrap();
    let corrupt_store = ImmutableDeploymentStore::default();
    let err = corrupt_store
        .load_from_sqlite(&pool)
        .await
        .expect_err("corrupt manifest_json must fail closed");
    assert_eq!(err.code, "CORRUPT_PERSISTED_STATE");
}

#[test]
fn connection_secret_rejection_canonical_digests_and_staged_gc_protection() {
    let store = ImmutableDeploymentStore::new("node-sec-1");
    let policy = CommunityAdmissionPolicy::new();
    let entries = sample_archive_entries();
    let pkg = store.ingest_verified_archive(&entries).unwrap();
    store
        .admit_package(&pkg.package_digest, &policy, None)
        .unwrap();

    // TD-20: Reject nested secret keys and credential-bearing URLs in public_config
    for bad_cfg in [
        json!({ "nested": { "api_token": "tok-123" } }),
        json!({ "headers": { "Authorization": "Bearer secret-jwt" } }),
        json!({ "url": "postgres://admin:hunter2@db.example.com:5432/app" }),
        json!({ "url": "https://api.example.com/v1?api_key=secret123" }),
    ] {
        let err = store
            .store_connection_revision(
                "conn-bad",
                1,
                &pkg.package_digest,
                bad_cfg,
                Default::default(),
            )
            .expect_err("secret in public_config must be rejected");
        assert_eq!(err.code, "CONNECTION_PUBLIC_SECRET_FORBIDDEN");
    }

    // TD-20: Key-reordered JSON produces identical canonical configuration_digest
    let conn_a = store
        .store_connection_revision(
            "conn-canon",
            1,
            &pkg.package_digest,
            json!({ "b": 2, "a": 1 }),
            Default::default(),
        )
        .unwrap();
    let conn_b = store
        .store_connection_revision(
            "conn-canon",
            1,
            &pkg.package_digest,
            json!({ "a": 1, "b": 2 }),
            Default::default(),
        )
        .expect("canonical public_config with reordered keys must be idempotent");
    assert_eq!(conn_a.configuration_digest, conn_b.configuration_digest);

    // TD-22: Newly staged package and connection (never yet deployed) are protected from GC
    let (draining, deleted, _) = store.mark_and_collect_garbage(1);
    assert!(draining.is_empty());
    assert!(deleted.is_empty());

    // TD-19: Connection member with wrong definition_digest or undeclared provider is rejected
    let bad_conn_manifest = CanonicalManifest {
        schema_version: 2,
        providers: vec![ProviderRef {
            publisher: pkg.publisher.clone(),
            provider_id: pkg.provider_id.clone(),
            version: pkg.version.clone(),
            digest: pkg.package_digest.clone(),
        }],
        provider_lock_digest: None,
        members: vec![ManifestMember {
            entity: EntityRef {
                kind: EntityKind::Connection,
                id: "conn-canon".to_string(),
                revision: 1,
            },
            definition_digest: store
                .store_definition_artifact(b"wrong-digest".to_vec())
                .unwrap()
                .digest,
            owning_package: None,
            requires: Vec::new(),
            rollouts: Vec::new(),
        }],
    };
    assert_eq!(
        store.plan_candidate(0, bad_conn_manifest).unwrap_err().code,
        "CONNECTION_DIGEST_MISMATCH"
    );
}

#[tokio::test]
async fn cross_replica_db_transactional_cas_and_revocation_enforcement() {
    use std::str::FromStr;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
        .await
        .unwrap();
    crate::storage::migrator_for(crate::storage::DbBackend::Sqlite)
        .run(&pool)
        .await
        .unwrap();
    let db_pool = crate::storage::DbPool::Sqlite(pool);

    let store_a = ImmutableDeploymentStore::new("replica-a");
    let store_b = ImmutableDeploymentStore::new("replica-b");
    let policy = CommunityAdmissionPolicy::new();

    let entries = sample_archive_entries();
    let pkg = store_a
        .ingest_verified_archive_in_db(&db_pool, &entries)
        .await
        .unwrap();
    store_a
        .admit_package_in_db(&db_pool, &pkg.package_digest, &policy, None)
        .await
        .unwrap();

    let manifest_a = CanonicalManifest {
        schema_version: 2,
        providers: vec![ProviderRef {
            publisher: pkg.publisher.clone(),
            provider_id: pkg.provider_id.clone(),
            version: pkg.version.clone(),
            digest: pkg.package_digest.clone(),
        }],
        provider_lock_digest: None,
        members: vec![ManifestMember {
            entity: EntityRef {
                kind: EntityKind::Workflow,
                id: "wf-a".to_string(),
                revision: 1,
            },
            definition_digest: store_a
                .store_definition_artifact_in_db(&db_pool, b"wf-a-v1".to_vec())
                .await
                .unwrap()
                .digest,
            owning_package: None,
            requires: Vec::new(),
            rollouts: Vec::new(),
        }],
    };
    let manifest_b = CanonicalManifest {
        schema_version: 2,
        providers: vec![ProviderRef {
            publisher: pkg.publisher.clone(),
            provider_id: pkg.provider_id.clone(),
            version: pkg.version.clone(),
            digest: pkg.package_digest.clone(),
        }],
        provider_lock_digest: None,
        members: vec![ManifestMember {
            entity: EntityRef {
                kind: EntityKind::Workflow,
                id: "wf-b".to_string(),
                revision: 1,
            },
            definition_digest: store_a
                .store_definition_artifact_in_db(&db_pool, b"wf-b-v1".to_vec())
                .await
                .unwrap()
                .digest,
            owning_package: None,
            requires: Vec::new(),
            rollouts: Vec::new(),
        }],
    };

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
        .apply_candidate_in_db(&db_pool, "alice", "idem-a", &plan_a, "deploy a", &policy)
        .await
        .expect("replica A wins CAS on base_revision 0");
    assert_eq!(receipt_a.committed_revision, 1);

    // Replica B's stale plan against base_revision 0 must fail inside the DB transaction
    let err_b = store_b
        .apply_candidate_in_db(&db_pool, "bob", "idem-b", &plan_b, "deploy b", &policy)
        .await
        .expect_err("competing deployment on stale base_revision must fail");
    assert_eq!(err_b.code, "DEPLOYMENT_HEAD_CONFLICT");

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
        .apply_candidate_in_db(&db_pool, "bob", "idem-b2", &plan_b2, "deploy b2", &policy)
        .await
        .expect_err("DB-level package revocation must block replica B's apply");
    assert_eq!(err_revoked.code, "ADMISSION_INVALIDATED");
}

#[tokio::test]
async fn failed_persistence_never_undoes_committed_in_memory_state() {
    use std::str::FromStr;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
        .await
        .unwrap();
    crate::storage::migrator_for(crate::storage::DbBackend::Sqlite)
        .run(&pool)
        .await
        .unwrap();
    let db_pool = crate::storage::DbPool::Sqlite(pool.clone());

    let store = ImmutableDeploymentStore::new("node-tx");
    let entries = sample_archive_entries();
    let pkg = store
        .ingest_verified_archive_in_db(&db_pool, &entries)
        .await
        .unwrap();
    let policy = CommunityAdmissionPolicy::new();
    store
        .admit_package_in_db(&db_pool, &pkg.package_digest, &policy, None)
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

    // Simulate a DB write failure for Request 2 after staged mutation succeeds
    sqlx::query(
        "CREATE TRIGGER fail_conn_insert BEFORE INSERT ON connection_revisions \
             BEGIN SELECT RAISE(FAIL, 'simulated write failure'); END",
    )
    .execute(&pool)
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
    assert_eq!(err.code, "DB_WRITE_ERROR");

    // Request 1's committed state remains intact and Request 2 was never published to memory
    let conns = store.list_connections();
    assert_eq!(conns.len(), 1);
    assert_eq!(conns[0].connection_id, "conn-1");
}

/// A connection's provider must be declared in the manifest, unless it is a
/// built-in driver, which ships with the binary and has no package.
#[test]
fn a_builtin_provider_needs_no_declaration_and_an_uploaded_one_does() {
    let store = ImmutableDeploymentStore::new("node-builtin");
    let policy = CommunityAdmissionPolicy::new();
    let pkg = store
        .ingest_verified_archive(&sample_archive_entries())
        .unwrap();
    store
        .admit_package(&pkg.package_digest, &policy, None)
        .unwrap();
    let builtin =
        super::super::foundation::builtin_provider_digest(super::super::foundation::DRIVER_HTTP)
            .expect("a built-in driver");
    let connection = |id: &str, provider: &str| {
        let row = store
            .store_connection_revision(
                id,
                1,
                provider,
                json!({"url": "https://api.example.com"}),
                Default::default(),
            )
            .unwrap();
        CanonicalManifest {
            schema_version: 2,
            providers: Vec::new(),
            provider_lock_digest: None,
            members: vec![ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Connection,
                    id: id.to_string(),
                    revision: 1,
                },
                definition_digest: row.configuration_digest,
                owning_package: None,
                requires: Vec::new(),
                rollouts: Vec::new(),
            }],
        }
    };

    store
        .plan_candidate(0, connection("builtin-api", &builtin))
        .expect("a built-in provider plans undeclared");
    let err = store
        .plan_candidate(0, connection("uploaded-api", &pkg.package_digest))
        .expect_err("an undeclared package is refused");
    assert_eq!(err.code, "CONNECTION_PROVIDER_NOT_DECLARED");
}

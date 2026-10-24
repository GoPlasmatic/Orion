//! Two replicas over one database converge on the deployment store's head:
//! a manifest applied, rolled back and revoked on replica A is what replica B
//! serves after reconciling.
//!
//! An integration test rather than a deployment-store unit test because it
//! assembles whole nodes (`bootstrap::assemble`), which the store sits below.

use orion::extensions::deployment_store::Rollback;
use orion::extensions::package_identity::{canonical_bytes, sha256_digest};
use orion::extensions::{
    ArchiveEntryInput, CanonicalManifest, EntityKind, EntityRef, ManifestMember, ProviderRef,
};
use serde_json::{Value, json};
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

#[tokio::test]
async fn two_replicas_converge_on_apply_rollback_and_revocation() {
    use std::str::FromStr;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
        .await
        .unwrap();
    orion::storage::migrator_for(orion::storage::DbBackend::Sqlite)
        .run(&pool)
        .await
        .unwrap();
    let db_pool = orion::storage::DbPool::Sqlite(pool);

    let metrics_handle = orion::metrics::init_metrics_with_instance(None);

    let mut cfg_a = orion::config::AppConfig::default();
    cfg_a.cluster.instance_id = "replica-a".to_string();
    cfg_a.cluster.epoch_poll_interval_ms = 20;
    let assembled_a = orion::bootstrap::assemble(orion::bootstrap::AssembleParams {
        config: std::sync::Arc::new(cfg_a),
        pool: db_pool.clone(),
        metrics_handle: metrics_handle.clone(),
    })
    .await
    .expect("assemble replica A");
    let state_a = assembled_a.state;

    let mut cfg_b = orion::config::AppConfig::default();
    cfg_b.cluster.instance_id = "replica-b".to_string();
    cfg_b.cluster.epoch_poll_interval_ms = 20;
    let assembled_b = orion::bootstrap::assemble(orion::bootstrap::AssembleParams {
        config: std::sync::Arc::new(cfg_b),
        pool: db_pool.clone(),
        metrics_handle,
    })
    .await
    .expect("assemble replica B");
    let state_b = assembled_b.state;

    // Create workflow v1 and v2 and channel v1 in the shared DB (even as draft/inactive,
    // manifest_definitions resolves the exact revision named in the manifest!)
    let wf1 = state_a
        .repos
        .workflows
        .create(
            &orion::storage::repositories::workflows::CreateWorkflowRequest {
                workflow_id: Some("orders-wf".to_string()),
                name: "Orders V1".to_string(),
                description: None,
                priority: 0,
                condition: json!(true),
                tasks: json!([{
                    "id": "t1",
                    "name": "Set V1",
                    "function": {
                        "name": "map",
                        "input": {
                            "mappings": [{"path": "data.version", "logic": "v1"}]
                        }
                    }
                }]),
                tags: Vec::new(),
                loop_config: None,
                continue_on_error: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(wf1.version, 1);

    let ch1 = state_a
        .repos
        .channels
        .create(
            &orion::storage::repositories::channels::CreateChannelRequest {
                channel_id: Some("orders-ch".to_string()),
                name: "orders-ch".to_string(),
                description: None,
                channel_type: orion::storage::models::ChannelType::Sync,
                protocol: orion::storage::models::ChannelProtocol::Rest,
                methods: Some(vec!["POST".to_string()]),
                route_pattern: Some("/orders".to_string()),
                topic: None,
                consumer_group: None,
                transport_config: json!({}),
                workflow_id: Some("orders-wf".to_string()),
                config: json!({}),
                priority: 0,
                tags: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(ch1.version, 1);

    // Ingest & admit provider package and create connection on Replica A
    let entries = sample_archive_entries();
    let pkg = state_a
        .composition
        .deployments
        .ingest_verified_archive_in_db(&state_a.db_pool, &entries)
        .await
        .unwrap();
    state_a
        .composition
        .deployments
        .admit_package_in_db(
            &state_a.db_pool,
            &pkg.package_digest,
            state_a.composition.admission.as_ref(),
            None,
        )
        .await
        .unwrap();
    let conn = state_a
        .composition
        .deployments
        .store_connection_revision_in_db(
            &state_a.db_pool,
            "crm-http",
            1,
            &pkg.package_digest,
            json!({"url": "https://crm.example.com"}),
            None,
        )
        .await
        .unwrap();

    // Deploy revision 1 containing orders-ch@1, orders-wf@1, and crm-http@1 on Replica A
    let manifest_rev1 = CanonicalManifest {
        schema_version: 2,
        providers: vec![ProviderRef {
            publisher: pkg.publisher.clone(),
            provider_id: pkg.provider_id.clone(),
            version: pkg.version.clone(),
            digest: pkg.package_digest.clone(),
        }],
        provider_lock_digest: None,
        members: vec![
            ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Channel,
                    id: "orders-ch".to_string(),
                    revision: 1,
                },
                definition_digest: sha256_digest(b"orders-ch-v1"),
                owning_package: None,
                requires: vec![EntityRef {
                    kind: EntityKind::Workflow,
                    id: "orders-wf".to_string(),
                    revision: 1,
                }],
                rollouts: Vec::new(),
            },
            ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Workflow,
                    id: "orders-wf".to_string(),
                    revision: 1,
                },
                definition_digest: sha256_digest(b"orders-wf-v1"),
                owning_package: None,
                requires: Vec::new(),
                rollouts: Vec::new(),
            },
            ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Connection,
                    id: "crm-http".to_string(),
                    revision: 1,
                },
                definition_digest: conn.configuration_digest.clone(),
                owning_package: None,
                requires: Vec::new(),
                rollouts: Vec::new(),
            },
        ],
    };
    let plan_rev1 = state_a
        .composition
        .deployments
        .plan_candidate_in_db(&state_a.db_pool, 0, manifest_rev1)
        .await
        .unwrap();
    let receipt_rev1 = state_a
        .composition
        .deployments
        .apply_candidate_in_db(
            &state_a.db_pool,
            "admin",
            "idem-rev1",
            &plan_rev1,
            "deploy rev 1",
            state_a.composition.admission.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(receipt_rev1.committed_revision, 1);

    orion::runtime::reconcile_deployment_revision(&state_a, 1)
        .await
        .expect("replica A reconciles revision 1");
    assert_eq!(
        state_a
            .composition
            .reconciler
            .local_status()
            .applied_revision,
        1
    );
    assert!(
        state_a
            .runtime
            .load()
            .channels
            .get_by_name("orders-ch")
            .is_some(),
        "Claim 1: manifest channel orders-ch@1 must be serving on replica A"
    );

    // Claim 4: Replica B reconciles automatically from the shared DB
    assert!(orion::cluster::reconcile_v2_state_once(&state_b).await);
    assert_eq!(
        state_b
            .composition
            .reconciler
            .local_status()
            .applied_revision,
        1
    );
    assert!(
        state_b
            .runtime
            .load()
            .channels
            .get_by_name("orders-ch")
            .is_some(),
        "Claim 4: replica B must automatically learn and serve orders-ch@1"
    );
    assert!(
        state_b
            .composition
            .deployments
            .get_connection("crm-http", 1)
            .is_some(),
        "Claim 4: replica B must automatically learn connection crm-http@1"
    );

    // Deploy revision 2 removing orders-ch (empty members) and verify both replicas stop serving it
    let manifest_rev2 = CanonicalManifest {
        schema_version: 2,
        providers: Vec::new(),
        provider_lock_digest: None,
        members: Vec::new(),
    };
    let plan_rev2 = state_a
        .composition
        .deployments
        .plan_candidate_in_db(&state_a.db_pool, 1, manifest_rev2)
        .await
        .unwrap();
    let receipt_rev2 = state_a
        .composition
        .deployments
        .apply_candidate_in_db(
            &state_a.db_pool,
            "admin",
            "idem-rev2",
            &plan_rev2,
            "remove channel",
            state_a.composition.admission.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(receipt_rev2.committed_revision, 2);
    orion::runtime::reconcile_deployment_revision(&state_a, 2)
        .await
        .expect("replica A reconciles revision 2");
    assert!(
        state_a
            .runtime
            .load()
            .channels
            .get_by_name("orders-ch")
            .is_none(),
        "Claim 1: removing channel in revision 2 must stop serving it on replica A"
    );

    assert!(orion::cluster::reconcile_v2_state_once(&state_b).await);
    assert_eq!(
        state_b
            .composition
            .reconciler
            .local_status()
            .applied_revision,
        2
    );
    assert!(
        state_b
            .runtime
            .load()
            .channels
            .get_by_name("orders-ch")
            .is_none(),
        "Claim 1 & 4: replica B must stop serving removed channel after reconciling revision 2"
    );

    // Rollback to revision 1 -> creates revision 3 and restores orders-ch@1
    let (rollback_receipt, _) = state_a
        .composition
        .deployments
        .rollback_to_revision_in_db(
            &state_a.db_pool,
            &Rollback {
                principal: "admin",
                idempotency_key: "idem-rb3",
                expected_head_revision: 2,
                target_revision: 1,
                reason: "rollback to rev 1",
            },
            state_a.composition.admission.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(rollback_receipt.committed_revision, 3);
    orion::runtime::reconcile_deployment_revision(&state_a, 3)
        .await
        .expect("replica A reconciles rollback revision 3");
    assert!(
        state_a
            .runtime
            .load()
            .channels
            .get_by_name("orders-ch")
            .is_some(),
        "Claim 1: rollback to revision 1 must restore orders-ch@1 on replica A"
    );

    // Revoke package on Replica A and verify Replica B refreshes revocations on reconcile
    state_a
        .composition
        .deployments
        .revoke_package_in_db(&state_a.db_pool, &pkg.package_digest)
        .await
        .unwrap();
    let _ = orion::cluster::reconcile_v2_state_once(&state_b).await;
    assert!(
        state_b
            .composition
            .admission
            .is_digest_revoked(&pkg.package_digest),
        "Claim 4: replica B must refresh package revocation into its AdmissionPolicy"
    );

    state_a
        .tasks
        .shutdown(std::time::Duration::from_secs(1))
        .await;
    state_b
        .tasks
        .shutdown(std::time::Duration::from_secs(1))
        .await;
}

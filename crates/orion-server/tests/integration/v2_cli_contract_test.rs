//! The v2 admin contract end to end: every typed call `orion-cli` makes,
//! through `orion_client::V2Admin`, against the real routes of an assembled
//! node.

use serde_json::{Value, json};

#[tokio::test]
async fn convergence_requires_a_committed_revision_even_on_a_stale_replica() {
    use crate::common;
    use axum::http::StatusCode;

    let db = common::ScratchDir::new("convergence_revision");
    let mut config = orion::config::AppConfig::default();
    config.storage.url = format!("{}?mode=rwc", db.url());
    config.cluster.epoch_poll_interval_ms = 3_600_000;
    let writer = common::test_state_with_config(config.clone()).await;
    let reader = common::test_state_with_config(config).await;
    let app = orion::server::build_router(reader.clone());

    // A peer's commit exists in the database while the reader's catalog is empty.
    common::deploy::commit_unchecked(&writer, 0, vec![]).await;
    assert_eq!(reader.deployments.current_head().0, 0);
    for target in ["this-node", "observed"] {
        let (status, report) = common::deploy::send(
            &app,
            "GET",
            &format!("/api/v2/admin/deployments/1/convergence?target={target}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["converged"], false, "{report}");
        assert_eq!(report["target_revision"], 1);
        for missing in [0, 2, u64::MAX] {
            let (status, error) = common::deploy::send(
                &app,
                "GET",
                &format!("/api/v2/admin/deployments/{missing}/convergence?target={target}"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{error}");
            assert_eq!(error["error"]["code"], "REVISION_NOT_FOUND", "{error}");
        }
    }
    common::deploy::commit_unchecked(&writer, 1, vec![]).await;
    let (status, report) =
        common::deploy::send(&app, "GET", "/api/v2/admin/deployments/1/convergence", None).await;
    assert_eq!(status, StatusCode::OK, "historical revision: {report}");
    let orion::storage::DbPool::Sqlite(pool) = &reader.db_pool else {
        panic!("SQLite harness");
    };
    sqlx::query("UPDATE deployment_revisions SET members_compacted = TRUE WHERE revision = 1")
        .execute(pool)
        .await
        .unwrap();
    let (status, report) =
        common::deploy::send(&app, "GET", "/api/v2/admin/deployments/1/convergence", None).await;
    assert_eq!(status, StatusCode::OK, "compacted revision: {report}");
    sqlx::query("ALTER TABLE deployment_revisions RENAME TO unavailable_revisions")
        .execute(pool)
        .await
        .unwrap();
    let (status, error) =
        common::deploy::send(&app, "GET", "/api/v2/admin/deployments/3/convergence", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{error}");
    assert_eq!(error["error"]["code"], "DB_READ_ERROR", "{error}");
    sqlx::query("ALTER TABLE unavailable_revisions RENAME TO deployment_revisions")
        .execute(pool)
        .await
        .unwrap();
    writer
        .tasks
        .shutdown(std::time::Duration::from_secs(2))
        .await;
    reader
        .tasks
        .shutdown(std::time::Duration::from_secs(2))
        .await;
}

/// The v2 admin router behind `orion_client::V2Admin`: every typed
/// client call, sent through the real routes.
struct RouterClient(axum::Router);

impl orion_client::V2Admin for RouterClient {
    type Error = orion_client::ClientError;

    async fn send(
        &self,
        method: orion_client::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, Self::Error> {
        use axum::body::{Body, to_bytes};
        use tower::ServiceExt as _;
        let mut req = axum::http::Request::builder()
            .method(method.as_str())
            .uri(path);
        let body = match body {
            Some(body) => {
                req = req.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&body)?)
            }
            None => Body::empty(),
        };
        let resp = self
            .0
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        if !status.is_success() {
            return Err(orion_client::ClientError::from_response(
                orion_client::StatusCode::from_u16(status.as_u16()).unwrap(),
                &bytes,
            ));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// Every typed client call the CLI makes, against the real router:
/// upload, admit, connect, probe, plan, apply, read back, roll back,
/// revoke.
#[tokio::test]
async fn cli_and_server_contract_end_to_end_against_running_server() {
    use orion_api::v2::*;
    use orion_client::V2Admin as _;
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
    let mut config = orion::config::AppConfig::default();
    config.cluster.instance_id = "node-cli-test".to_string();
    let assembled = orion::bootstrap::assemble(
        orion::bootstrap::AssembleParams {
            config: std::sync::Arc::new(config),
            pool: db_pool,
            metrics_handle,
        },
        orion::extensions::CompositionRoot::community(),
    )
    .await
    .expect("assemble test server");
    let client = RouterClient(
        axum::Router::new()
            .nest(
                "/api/v2/admin",
                orion::server::routes::v2_admin::v2_admin_routes(
                    10 * 1024 * 1024,
                    10 * 1024 * 1024,
                ),
            )
            .with_state(assembled.state),
    );

    // 1. Capabilities
    let cap = client.runtime_capabilities().await.unwrap();
    assert_eq!(cap.api_version, orion_api::v2::API_VERSION);
    assert_eq!(client.server_api_version().await.unwrap(), Some(1));
    assert_eq!(cap.edition, EditionTier::Community);
    assert_eq!(cap.wasm_worlds, vec!["orion:plugin@1.0.0"]);
    assert_eq!(cap.channel_drivers, vec!["cron", "http", "rest"]);
    assert_eq!(cap.node_status.node_id, "node-cli-test");
    assert!(
        cap.provider_backends
            .iter()
            .any(|b| b.driver_id == "org.orion.sqlite" && b.package_digest.is_some()),
        "{cap:?}"
    );

    // 2. Scaffold, lint, pack
    let scaffold = client
        .authoring_scaffold(&ScaffoldRequest {
            publisher: "org.orion".to_string(),
            provider_id: "org.orion.memory-cache".to_string(),
            version: "2.0.0".to_string(),
            kind: ImplementationKind::Rust,
        })
        .await
        .unwrap();
    assert!(!scaffold.files.is_empty());
    assert_eq!(scaffold.implementation, ImplementationKind::Rust);
    let bundle = UploadPackageRequest {
        files: scaffold.files.clone(),
        ..Default::default()
    };
    assert!(client.authoring_lint(&bundle).await.unwrap().valid);
    let packed = client.authoring_pack(&bundle).await.unwrap();
    assert!(packed.package_digest.starts_with("sha256:"));
    assert!(packed.files.contains_key("inventory.json"));

    // 3. Upload, read back, admit
    let uploaded = client
        .upload_connector_package(&UploadPackageRequest {
            files: packed.files.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(uploaded.package.digest, packed.package_digest);
    let digest = uploaded.package.digest.clone();
    let read = client.get_connector_package(&digest).await.unwrap();
    assert_eq!(read.package.digest, digest);
    assert!(read.admission.is_none(), "not admitted yet");
    let admitted = client
        .admit_connector_package(&digest, &AdmitPackageRequest::default())
        .await
        .unwrap();
    assert_eq!(admitted.admission.package_digest, digest);
    let listed = client.list_connector_packages().await.unwrap();
    assert_eq!(listed.packages.len(), 1);
    assert!(listed.packages[0].admitted);
    assert!(
        client
            .get_connector_package(&digest)
            .await
            .unwrap()
            .package
            .admitted
    );

    // 4. A connection revision, read back and probed
    let created = client
        .create_connection_revision(
            "cache-main",
            &CreateConnectionRevisionRequest {
                revision: 1,
                provider_digest: digest.clone(),
                public_config: json!({"endpoint": "https://cache.example.com"}),
                secret_locators: Default::default(),
                bindings: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(created.connection.revision, 1);
    let revision = client
        .get_connection_revision("cache-main", 1)
        .await
        .unwrap();
    assert_eq!(
        revision.configuration_digest,
        created.connection.configuration_digest
    );
    let revisions = client
        .list_connection_revisions("cache-main")
        .await
        .unwrap();
    assert_eq!(revisions.count, 1);
    let probe = client.probe_connection("cache-main", 1).await.unwrap();
    assert!(probe.healthy, "{probe:?}");
    assert!(probe.details.error.is_none(), "{probe:?}");
    let missing = client.probe_connection("cache-main", 2).await.unwrap_err();
    assert_eq!(missing.status().map(|s| s.as_u16()), Some(404));

    // 5. Plan, then apply with the reviewed digest
    let manifest = DeploymentManifest {
        providers: vec![ManifestProvider {
            publisher: "org.orion".to_string(),
            provider_id: "org.orion.memory-cache".to_string(),
            version: "2.0.0".to_string(),
            digest: digest.clone(),
        }],
        ..DeploymentManifest::of_members(vec![ManifestMember {
            entity: EntityRef {
                kind: EntityKind::Connection,
                id: "cache-main".to_string(),
                revision: 1,
            },
            definition_digest: created.connection.configuration_digest.clone(),
            owning_package: None,
            requires: Vec::new(),
            rollouts: Vec::new(),
        }])
    };
    let plan = client
        .plan_deployment(&CreateDeploymentPlanRequest {
            base_revision: 0,
            manifest: manifest.clone(),
        })
        .await
        .unwrap();
    assert!(plan.reviewed_manifest_digest.starts_with("sha256:"));
    assert_eq!(
        plan.added_entities,
        vec![EntityRef {
            kind: EntityKind::Connection,
            id: "cache-main".into(),
            revision: 1,
        }]
    );
    assert!(plan.updated_entities.is_empty() && plan.removed_entities.is_empty());

    let apply = |key: &str, reviewed: &str| {
        CreateDeploymentRequest::Apply(ApplyDeploymentRequest {
            idempotency_key: key.to_string(),
            base_revision: 0,
            manifest: manifest.clone(),
            reason: "cli apply".to_string(),
            reviewed_manifest_digest: reviewed.to_string(),
        })
    };
    let mismatch = client
        .create_deployment(&apply(
            "idem-bad",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        ))
        .await
        .unwrap_err();
    match mismatch {
        orion_client::ClientError::Api { status, error } => {
            assert_eq!(status.as_u16(), 409);
            assert_eq!(error.code, "REVIEWED_DIGEST_MISMATCH");
        }
        other => panic!("{other:?}"),
    }
    let applied = client
        .create_deployment(&apply("idem-apply-1", &plan.reviewed_manifest_digest))
        .await
        .unwrap();
    assert_eq!(applied.committed_revision, 1);
    assert_eq!(applied.receipt.committed_revision, 1);

    // 6. Convergence, and the revision read back as a page and in full
    let conv = client
        .deployment_convergence(1, Some("this-node"))
        .await
        .unwrap();
    assert!(conv.converged);
    assert_eq!(conv.target_revision, 1);
    let page = client.list_deployments(None, None).await.unwrap();
    assert_eq!(page.head_revision, 1);
    assert_eq!(page.data.len(), 1);
    assert_eq!(page.data[0].member_count, 1);
    assert!(page.next_cursor.is_none());
    let got = client.get_deployment(1).await.unwrap();
    assert_eq!(got.summary.revision, 1);
    assert_eq!(got.summary.manifest_digest, plan.reviewed_manifest_digest);
    assert_eq!(got.manifest, manifest);

    // 7. Rollback: a new revision deploying revision 1 again
    let rolled = client
        .create_deployment(&CreateDeploymentRequest::Rollback(
            RollbackDeploymentRequest {
                idempotency_key: "idem-rb-1".to_string(),
                base_revision: 1,
                rollback_to: 1,
                reason: "rollback test".to_string(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(rolled.committed_revision, 2);
    assert!(
        !rolled.warnings.is_empty(),
        "a rollback says what it does not undo"
    );
    let first = client.list_deployments(Some(1), None).await.unwrap();
    assert_eq!(first.data[0].revision, 2);
    let cursor = first.next_cursor.expect("an older page follows");
    let second = client
        .list_deployments(Some(1), Some(&cursor))
        .await
        .unwrap();
    assert_eq!(second.data[0].revision, 1);
    assert!(second.next_cursor.is_none());

    // 8. Revoke
    let revoked = client
        .revoke_connector_package(&digest, &RevokePackageRequest::default())
        .await
        .unwrap();
    assert_eq!(revoked.package_digest, digest);
    let read = client.get_connector_package(&digest).await.unwrap();
    assert!(read.package.revoked);
}

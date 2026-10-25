//! `connector_call` under a deployed generation: a workflow may call a
//! connection its manifest member requires, and nothing else.

use orion::extensions::package_identity::sha256_digest;
use orion::extensions::{CanonicalManifest, EntityKind, EntityRef, ManifestMember};
use serde_json::{Value, json};

fn cache_workflow(id: &str) -> orion::storage::repositories::workflows::CreateWorkflowRequest {
    orion::storage::repositories::workflows::CreateWorkflowRequest {
        workflow_id: Some(id.to_string()),
        name: id.to_string(),
        description: None,
        priority: 0,
        condition: json!(true),
        tasks: json!([
            {"id": "set", "function": {"name": "connector_call", "input": {
                "connection": "local-cache",
                "target_id": "org.orion.memory-cache",
                "operation": "cache.set",
                "input": {"key": "k", "value": "v"},
                "output": "data.set"
            }}},
            {"id": "get", "function": {"name": "connector_call", "input": {
                "connection": "local-cache",
                "target_id": "org.orion.memory-cache",
                "operation": "cache.get",
                "input": {"key": "k"},
                "output": "data.got"
            }}}
        ]),
        tags: Vec::new(),
        loop_config: None,
        continue_on_error: false,
    }
}

fn channel(
    id: &str,
    workflow: &str,
) -> orion::storage::repositories::channels::CreateChannelRequest {
    orion::storage::repositories::channels::CreateChannelRequest {
        channel_id: Some(id.to_string()),
        name: id.to_string(),
        description: None,
        channel_type: orion::storage::models::ChannelType::Sync,
        protocol: orion::storage::models::ChannelProtocol::Rest,
        methods: Some(vec!["POST".to_string()]),
        route_pattern: Some(format!("/{id}")),
        topic: None,
        consumer_group: None,
        transport_config: json!({}),
        workflow_id: Some(workflow.to_string()),
        config: json!({}),
        priority: 0,
        tags: Vec::new(),
    }
}

fn member(kind: EntityKind, id: &str, digest: String, requires: Vec<EntityRef>) -> ManifestMember {
    ManifestMember {
        entity: EntityRef {
            kind,
            id: id.to_string(),
            revision: 1,
        },
        definition_digest: digest,
        owning_package: None,
        requires,
        rollouts: Vec::new(),
    }
}

fn entity(kind: EntityKind, id: &str) -> EntityRef {
    EntityRef {
        kind,
        id: id.to_string(),
        revision: 1,
    }
}

#[tokio::test]
async fn a_workflow_calls_only_the_connections_its_member_requires() {
    let state = orion::bootstrap::assemble(orion::bootstrap::AssembleParams {
        config: std::sync::Arc::new(orion::config::AppConfig::default()),
        pool: {
            use std::str::FromStr;
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(4)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:").unwrap(),
                )
                .await
                .unwrap();
            orion::storage::migrator_for(orion::storage::DbBackend::Sqlite)
                .run(&pool)
                .await
                .unwrap();
            orion::storage::DbPool::Sqlite(pool)
        },
        metrics_handle: orion::metrics::init_metrics_with_instance(None),
    })
    .await
    .expect("boots")
    .state;

    for wf in ["allowed-wf", "rogue-wf"] {
        state
            .repos
            .workflows
            .create(&cache_workflow(wf))
            .await
            .unwrap();
    }
    for (ch, wf) in [("allowed-ch", "allowed-wf"), ("rogue-ch", "rogue-wf")] {
        state.repos.channels.create(&channel(ch, wf)).await.unwrap();
    }

    // The connection names the built-in memory-cache driver by its built-in
    // digest, so the manifest declares no provider package for it.
    let deployments = &state.composition.deployments;
    let builtin = orion::extensions::foundation::builtin_provider_digest(
        orion::extensions::foundation::DRIVER_MEMORY_CACHE,
    )
    .expect("a built-in driver");
    let conn = deployments
        .store_connection_revision_in_db(
            &state.db_pool,
            "local-cache",
            1,
            &builtin,
            json!({}),
            None,
        )
        .await
        .unwrap();

    let manifest = CanonicalManifest {
        schema_version: 2,
        providers: Vec::new(),
        provider_lock_digest: None,
        members: vec![
            member(
                EntityKind::Channel,
                "allowed-ch",
                sha256_digest(b"allowed-ch"),
                vec![entity(EntityKind::Workflow, "allowed-wf")],
            ),
            member(
                EntityKind::Channel,
                "rogue-ch",
                sha256_digest(b"rogue-ch"),
                vec![entity(EntityKind::Workflow, "rogue-wf")],
            ),
            member(
                EntityKind::Workflow,
                "allowed-wf",
                sha256_digest(b"allowed-wf"),
                vec![entity(EntityKind::Connection, "local-cache")],
            ),
            member(
                EntityKind::Workflow,
                "rogue-wf",
                sha256_digest(b"rogue-wf"),
                Vec::new(),
            ),
            member(
                EntityKind::Connection,
                "local-cache",
                conn.configuration_digest.clone(),
                Vec::new(),
            ),
        ],
    };
    let plan = deployments
        .plan_candidate_in_db(&state.db_pool, 0, manifest)
        .await
        .unwrap();
    let receipt = deployments
        .apply_candidate_in_db(
            &state.db_pool,
            "admin",
            "idem-authority",
            &plan,
            "deploy",
            state.composition.admission.as_ref(),
        )
        .await
        .unwrap();
    orion::runtime::reconcile_deployment_revision(&state, receipt.committed_revision)
        .await
        .expect("the revision builds");

    let app = orion::server::build_router(state.clone());
    let post = |path: &'static str| {
        let app = app.clone();
        async move {
            let resp = tower::ServiceExt::oneshot(
                app,
                axum::http::Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(json!({"data": {}}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
            let status = resp.status();
            let body: Value = serde_json::from_slice(
                &axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap_or(Value::Null);
            (status, body)
        }
    };

    let (status, body) = post("/api/v2/data/allowed-ch").await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["data"]["got"]["value"], "v", "{body}");

    let (status, body) = post("/api/v2/data/rogue-ch").await;
    assert_ne!(status, axum::http::StatusCode::OK, "{body}");
    assert!(
        body.to_string()
            .contains("does not require that connection"),
        "{body}"
    );
}

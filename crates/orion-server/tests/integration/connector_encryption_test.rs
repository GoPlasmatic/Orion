//! H3: optional encryption at rest for `connectors.config_json`.
//!
//! With `storage.connector_encryption_key` set, what reaches the database
//! column is an AES-256-GCM envelope — a dump of the table shows no
//! credential — while the API surface is unchanged: reads decrypt
//! transparently (then mask, as always).

use crate::common;
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

fn key_hex() -> String {
    "a1".repeat(32)
}

async fn state_with_key() -> orion::server::state::AppState {
    let mut config = orion::config::AppConfig::default();
    config.storage.connector_encryption_key = key_hex();
    let (state, _handles) = common::test_state_with_handles(config).await;
    state
}

/// Read the raw stored column, bypassing the repository (and its decryption).
async fn raw_config_json(state: &orion::server::state::AppState, name: &str) -> String {
    use sea_query::{Expr, ExprTrait, Query};
    let (sql, values) = orion::storage::build_sqlx(
        state.db_pool.backend(),
        Query::select()
            .column(orion::storage::schema::Connectors::ConfigJson)
            .from(orion::storage::schema::Connectors::Table)
            .and_where(Expr::col(orion::storage::schema::Connectors::Name).eq(name)),
    );
    state
        .db_pool
        .fetch_scalar::<String>(&sql, values)
        .await
        .expect("raw row")
}

#[tokio::test]
async fn the_stored_column_is_ciphertext_and_the_api_is_unchanged() {
    let state = state_with_key().await;
    let app = orion::server::build_router(state.clone());

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "name": "enc-http",
                "connector_type": "http",
                "config": {"type": "http", "url": "https://api.example.com",
                            "auth": {"type": "bearer", "token": "sk-live-secret"}}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The column carries the envelope, not the credential — this is the
    // whole point: a DB dump shows nothing.
    let raw = raw_config_json(&state, "enc-http").await;
    assert!(
        raw.starts_with("enc:v1:"),
        "column must be enveloped: {raw}"
    );
    assert!(
        !raw.contains("sk-live-secret") && !raw.contains("api.example.com"),
        "no plaintext may reach the column: {raw}"
    );

    // The API read is byte-for-byte what an unencrypted install serves:
    // decrypted, then masked.
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "GET",
            "/api/v1/admin/connectors",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let row = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "enc-http")
        .expect("listed");
    let config: serde_json::Value =
        serde_json::from_str(row["config_json"].as_str().unwrap()).unwrap();
    assert_eq!(config["url"], "https://api.example.com");
    assert_eq!(config["auth"]["token"], "******");
}

/// A plaintext row written before the key was configured keeps loading —
/// turning encryption on is a config change, not a migration.
#[tokio::test]
async fn pre_existing_plaintext_rows_keep_loading() {
    let state = state_with_key().await;
    let app = orion::server::build_router(state.clone());

    // Simulate a pre-key row by writing plaintext straight to the column.
    use sea_query::Query;
    let (sql, values) = orion::storage::build_sqlx(
        state.db_pool.backend(),
        Query::insert()
            .into_table(orion::storage::schema::Connectors::Table)
            .columns([
                orion::storage::schema::Connectors::Id,
                orion::storage::schema::Connectors::Name,
                orion::storage::schema::Connectors::ConnectorType,
                orion::storage::schema::Connectors::ConfigJson,
            ])
            .values_panic([
                "legacy-1".into(),
                "legacy-http".into(),
                "http".into(),
                r#"{"type":"http","url":"https://legacy.example.com"}"#.into(),
            ]),
    );
    state
        .db_pool
        .execute_query(&sql, values)
        .await
        .expect("seed");

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "GET",
            "/api/v1/admin/connectors/legacy-1",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let config: serde_json::Value =
        serde_json::from_str(body["data"]["config_json"].as_str().unwrap()).unwrap();
    assert_eq!(config["url"], "https://legacy.example.com");

    // Its next write re-encrypts it.
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PUT",
            "/api/v1/admin/connectors/legacy-1",
            Some(json!({"enabled": false})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let raw = raw_config_json(&state, "legacy-http").await;
    assert!(
        raw.starts_with("enc:v1:"),
        "the first write after the key must re-encrypt: {raw}"
    );
}

//! `/export` and `/validate` across all three primitives.
//!
//! Only workflows had either. You could push an estate but not pull one: no
//! snapshot of an environment, no diff of staging against production, no
//! recovery into version control. The unit of truth was the database, and every
//! team's review process is git.
//!
//! The round trip below is the property that matters — export, wipe, import,
//! and get the same estate back — plus the two things that make an exported
//! bundle safe to commit and an imported one safe to trust.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common;
use crate::common::{body_json, json_request};

async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request("POST", uri, Some(body)))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

async fn get(app: &axum::Router, uri: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(json_request("GET", uri, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
    body_json(resp).await
}

/// Workflows, channels and an `env://`-authored connector survive an export
/// into a fresh instance.
///
/// This is the whole point of the task: an estate you can only push is an
/// estate you cannot review, diff, or recover.
///
/// The connector uses `env://` deliberately. That is the authored form a
/// bundle *can* round-trip, and the reason is the next test: a literal secret
/// exports as `******`, which is safe to commit and — by design — not
/// importable.
#[tokio::test]
async fn an_estate_survives_an_export_import_round_trip() {
    let source = common::test_app().await;

    common::create_and_activate_channel(
        &source,
        "orders",
        common::echo_workflow("orders-workflow"),
    )
    .await;
    let (status, body) = post(
        &source,
        "/api/v1/admin/connectors",
        json!({
            "id": "crm", "name": "crm", "connector_type": "http",
            "config": {
                "url": "https://api.example.com",
                "auth": {"type": "bearer", "token": "env://ORION_TEST_CRM_TOKEN"}
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let workflows = get(&source, "/api/v1/admin/workflows/export").await;
    let channels = get(&source, "/api/v1/admin/channels/export").await;
    let connectors = get(&source, "/api/v1/admin/connectors/export").await;

    assert!(!workflows["data"].as_array().unwrap().is_empty());
    assert!(!channels["data"].as_array().unwrap().is_empty());
    assert!(!connectors["data"].as_array().unwrap().is_empty());

    // A fresh instance, standing in for "staging promoted into production".
    let target = common::test_app().await;
    for (uri, exported) in [
        ("/api/v1/admin/workflows/import", &workflows),
        ("/api/v1/admin/channels/import", &channels),
        ("/api/v1/admin/connectors/import", &connectors),
    ] {
        let (status, body) = post(&target, uri, exported["data"].clone()).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(body["data"]["failed"], 0, "{uri} reported failures: {body}");
        assert!(
            body["data"]["imported"].as_u64().unwrap_or(0) > 0,
            "{uri} imported nothing: {body}"
        );
    }

    // And the target now holds what the source did, references included.
    let reimported = get(&target, "/api/v1/admin/channels/export").await;
    let names: Vec<&str> = reimported["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(
        names.contains(&"orders"),
        "channel did not survive: {names:?}"
    );

    let conns = get(&target, "/api/v1/admin/connectors/export").await;
    assert_eq!(
        conns["data"][0]["config"]["auth"]["token"], "env://ORION_TEST_CRM_TOKEN",
        "a secret *reference* is not a secret and must survive the trip: {conns}"
    );
}

/// A literal secret exports as `******`, and re-importing that bundle fails.
///
/// Both halves are the intended contract. Masking is what makes an export safe
/// to commit; refusing the masked value on the way back in is what keeps
/// `******` from being stored as a real credential and failing later at the
/// first request instead of here, where the operator is looking.
///
/// The way to author a connector that round-trips is `env://`, which the test
/// above covers.
#[tokio::test]
async fn a_literal_secret_is_masked_on_export_and_refused_on_import() {
    let source = common::test_app().await;
    let (status, body) = post(
        &source,
        "/api/v1/admin/connectors",
        json!({
            "id": "literal", "name": "literal", "connector_type": "db",
            "config": {"connection_string": "postgres://u:pw@db/orders"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let exported = get(&source, "/api/v1/admin/connectors/export").await;
    let dumped = serde_json::to_string(&exported).unwrap();
    assert!(
        !dumped.contains("pw@db"),
        "an exported bundle must be safe to commit: {dumped}"
    );

    let target = common::test_app().await;
    let (status, body) = post(
        &target,
        "/api/v1/admin/connectors/import",
        exported["data"].clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"]["imported"], 0,
        "a masked credential must not be stored as if it were real: {body}"
    );
    assert_eq!(body["data"]["failed"], 1, "{body}");
}

/// An exported connector carries no secret values.
///
/// Safe by construction rather than by this handler remembering: `mask_connector`
/// is the only constructor of `ConnectorResponse` and the stored row does not
/// serialize at all (D27). Asserted anyway, because "an export you can commit"
/// is the property the feature is sold on.
#[tokio::test]
async fn an_exported_connector_has_its_secrets_masked() {
    let app = common::test_app().await;
    let (status, _) = post(
        &app,
        "/api/v1/admin/connectors",
        json!({
            "id": "secretive",
            "name": "secretive",
            "connector_type": "http",
            "config": {
                "url": "https://api.example.com",
                "auth": {"type": "bearer", "token": "super-secret-value"}
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let exported = get(&app, "/api/v1/admin/connectors/export").await;
    let dumped = serde_json::to_string(&exported).unwrap();
    assert!(
        !dumped.contains("super-secret-value"),
        "an exported bundle must be safe to commit: {dumped}"
    );
}

/// `/validate` agrees with `POST` on a payload create rejects.
///
/// R20's lesson, applied to the two endpoints that just gained one: a validator
/// that re-derives the create rules drifts, and drifts in the dangerous
/// direction — reporting `valid: true` for something create refuses is worse
/// than having no validator, because it is a linter that lies.
#[tokio::test]
async fn channel_validate_agrees_with_create() {
    let app = common::test_app().await;
    // Empty name: rejected by the create-path validator.
    let bad = json!({
        "channel_id": "bad", "name": "", "channel_type": "sync",
        "protocol": "http", "workflow_id": "wf"
    });

    let validated = post(&app, "/api/v1/admin/channels/validate", bad.clone()).await;
    assert_eq!(validated.0, StatusCode::OK);
    assert_eq!(
        validated.1["data"]["valid"], false,
        "validate accepted what create rejects: {}",
        validated.1
    );

    let created = post(&app, "/api/v1/admin/channels", bad).await;
    assert_eq!(
        created.0,
        StatusCode::BAD_REQUEST,
        "create accepted what validate rejected: {}",
        created.1
    );
}

/// A channel config the registry would quarantine is reported at validate time.
///
/// `ChannelConfig` is `deny_unknown_fields`, so a typo'd guard key is a load
/// failure rather than a silently absent guard. Surfacing it here turns a
/// quarantine an operator discovers from traffic into an answer they get before
/// storing anything.
#[tokio::test]
async fn channel_validate_reports_an_unparseable_config() {
    let app = common::test_app().await;
    let (status, body) = post(
        &app,
        "/api/v1/admin/channels/validate",
        // Valid in every respect except the config, so the config error is what
        // the assertion below is actually reading — an otherwise-invalid payload
        // would be refused for those reasons first and never reach the config.
        json!({
            "channel_id": "typo", "name": "typo", "channel_type": "sync",
            "protocol": "http", "workflow_id": "wf",
            "route_pattern": "/typo", "methods": ["POST"],
            "config": {"deduplicaton": {"header": "Idempotency-Key"}}
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["valid"], false, "{body}");
    let errors = serde_json::to_string(&body["data"]["errors"]).unwrap();
    assert!(
        errors.contains("config"),
        "the error must point at config: {errors}"
    );
}

#[tokio::test]
async fn connector_validate_agrees_with_create() {
    let app = common::test_app().await;
    let bad = json!({
        "id": "bad-conn", "name": "bad-conn",
        "connector_type": "http", "config": {}
    });

    let validated = post(&app, "/api/v1/admin/connectors/validate", bad.clone()).await;
    assert_eq!(validated.0, StatusCode::OK);
    let created = post(&app, "/api/v1/admin/connectors", bad).await;

    assert_eq!(
        validated.1["data"]["valid"].as_bool(),
        Some(created.0 == StatusCode::CREATED),
        "validate said {} but create answered {}",
        validated.1["data"]["valid"],
        created.0
    );
}

/// An unresolvable `env://` reference is a warning, not an error.
///
/// A bundle is routinely validated on a machine holding none of the production
/// secrets — a CI runner checking a pull request. Failing there would make the
/// endpoint useless for the promotion flow it exists to serve.
#[tokio::test]
async fn an_unresolvable_secret_reference_is_a_warning_not_an_error() {
    let app = common::test_app().await;
    let (status, body) = post(
        &app,
        "/api/v1/admin/connectors/validate",
        json!({
            "id": "env-conn", "name": "env-conn", "connector_type": "http",
            "config": {
                "url": "https://api.example.com",
                "auth": {"type": "bearer", "token": "env://ORION_TEST_DEFINITELY_UNSET"}
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"]["valid"], true,
        "an unset secret on this host must not make the definition invalid: {body}"
    );
    let warnings = serde_json::to_string(&body["data"]["warnings"]).unwrap();
    assert!(
        warnings.contains("secret reference"),
        "but it must be reported: {warnings}"
    );
}

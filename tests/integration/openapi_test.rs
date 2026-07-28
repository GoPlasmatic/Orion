use crate::common;

use tower::ServiceExt;

#[tokio::test]
async fn test_openapi_spec_endpoint() {
    let app = common::test_app().await;
    let req = common::json_request("GET", "/api/v1/openapi.json", None);
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), 200);

    let body = common::body_json(response).await;
    assert_eq!(body["openapi"], "3.1.0");
    assert!(body["info"]["title"].as_str().unwrap().contains("Orion"));
    assert!(!body["paths"].as_object().unwrap().is_empty());
    assert!(
        !body["components"]["schemas"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn test_openapi_spec_all_endpoints() {
    let app = common::test_app().await;
    let req = common::json_request("GET", "/api/v1/openapi.json", None);
    let response = app.oneshot(req).await.unwrap();
    let body = common::body_json(response).await;
    let paths = body["paths"].as_object().unwrap();

    // B4 closed the gap on admin routes — backups, audit, and the
    // functions endpoint are decorated and registered. C8 added the data
    // plane (served by one catch-all handler, documented as a templated
    // path), the DLQ group, and the two Kubernetes probes.
    let expected = [
        "/api/v1/admin/traces",
        "/api/v1/admin/traces/{id}",
        "/health",
        "/metrics",
        // B4 additions:
        "/api/v1/admin/functions",
        "/api/v1/admin/audit-logs",
        "/api/v1/admin/backups",
        // C8 additions:
        "/api/v1/data/{channel}",
        "/api/v1/data/{channel}/async",
        "/healthz",
        "/readyz",
        "/api/v1/admin/trace-dlq",
        "/api/v1/admin/trace-dlq/{id}",
        "/api/v1/admin/trace-dlq/{id}/requeue",
        "/api/v1/admin/trace-dlq/purge",
    ];

    for path in &expected {
        assert!(paths.contains_key(*path), "Missing path: {}", path);
    }
}

#[tokio::test]
async fn openapi_documents_new_b4_tags() {
    let app = common::test_app().await;
    let req = common::json_request("GET", "/api/v1/openapi.json", None);
    let response = app.oneshot(req).await.unwrap();
    let body = common::body_json(response).await;
    // The `tags` block surfaces in the spec so Swagger UI groups
    // endpoints; B4 added Functions, Audit, Backups.
    let tag_names: Vec<&str> = body["tags"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(tag_names.contains(&"Functions"));
    assert!(tag_names.contains(&"Audit"));
    assert!(tag_names.contains(&"Backups"));
}

/// C8 regression guard: the served spec must never go back to advertising an
/// unauthenticated API. Before C8 there was no `securitySchemes` block and no
/// `security` on any operation, so every generated client and codegen tool was
/// told the admin API needed no credential — while the shipped Helm chart and
/// HA compose enable `admin_auth` by default.
#[tokio::test]
async fn openapi_declares_admin_security() {
    let app = common::test_app().await;
    let req = common::json_request("GET", "/api/v1/openapi.json", None);
    let response = app.oneshot(req).await.unwrap();
    let body = common::body_json(response).await;

    let schemes = body["components"]["securitySchemes"]
        .as_object()
        .expect("securitySchemes must be present");
    // Default `Authorization: Bearer <key>` plus the custom-header form
    // selected by `admin_auth.header`.
    assert_eq!(schemes["admin_bearer"]["type"], "http");
    assert_eq!(schemes["admin_bearer"]["scheme"], "bearer");
    assert_eq!(schemes["admin_api_key"]["type"], "apiKey");
    assert_eq!(schemes["admin_api_key"]["in"], "header");

    let paths = body["paths"].as_object().unwrap();
    // Everything the admin-auth middleware guards must say so.
    for path in [
        "/api/v1/admin/channels",
        "/api/v1/admin/workflows",
        "/api/v1/admin/connectors",
        "/api/v1/admin/trace-dlq",
        "/api/v1/admin/audit-logs",
        "/api/v1/admin/traces",
        "/metrics",
    ] {
        let security = &paths[path]["get"]["security"];
        assert!(
            security.is_array() && !security.as_array().unwrap().is_empty(),
            "{path} must declare a security requirement"
        );
        assert!(
            paths[path]["get"]["responses"]["401"].is_object(),
            "{path} must document its 401"
        );
    }

    // ...and nothing else may: the data plane and the probes are open by
    // design, and claiming otherwise would be just as wrong.
    for (path, method) in [
        ("/api/v1/data/{channel}", "post"),
        ("/api/v1/data/{channel}/async", "post"),
        ("/health", "get"),
        ("/healthz", "get"),
        ("/readyz", "get"),
    ] {
        assert!(
            paths[path][method]["security"].is_null(),
            "{method} {path} is unauthenticated and must not claim otherwise"
        );
    }
}

/// C8: the data plane is the product's primary user-facing API. It was absent
/// from the spec entirely because `dynamic_handler` serves every channel from
/// one catch-all route. It is documented as a templated path — assert the
/// response shapes operators actually need, including the async `202` and the
/// `Warning: 299` header emitted when trace persistence is off.
#[tokio::test]
async fn openapi_documents_the_data_plane() {
    let app = common::test_app().await;
    let req = common::json_request("GET", "/api/v1/openapi.json", None);
    let response = app.oneshot(req).await.unwrap();
    let body = common::body_json(response).await;

    let sync = &body["paths"]["/api/v1/data/{channel}"]["post"];
    assert_eq!(
        sync["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ProcessRequest"
    );
    assert_eq!(
        sync["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ProcessResponse"
    );
    // 400 validation, 409 dedup, 429 rate limit, 503 backpressure/CIRCUIT_OPEN,
    // 504 timeout — the statuses a client has to branch on.
    for status in [
        "400", "403", "404", "409", "415", "429", "502", "503", "504",
    ] {
        assert!(
            sync["responses"][status]["content"]["application/json"]["schema"]["$ref"]
                == "#/components/schemas/ErrorResponse",
            "sync data plane must document {status} with the shared error envelope"
        );
    }

    let async_submit = &body["paths"]["/api/v1/data/{channel}/async"]["post"];
    let accepted = &async_submit["responses"]["202"];
    assert_eq!(
        accepted["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/AsyncSubmitResponse"
    );
    // R11: the 202 has one shape. It used to answer `{"trace_id": null}` plus a
    // `Warning: 299` header when `trace.mode = off` — a receipt for a result
    // that could never be fetched, with the nullability baked into the spec.
    assert!(
        accepted["headers"]["warning"].is_null(),
        "the 202 must not document a Warning header — trace_id is unconditional"
    );
    let ack = &body["components"]["schemas"]["AsyncSubmitResponse"];
    assert!(
        !ack["properties"]["trace_id"].is_null(),
        "AsyncSubmitResponse must describe its trace_id field"
    );
    let required: Vec<&str> = ack["required"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(
        required.contains(&"trace_id") && required.contains(&"trace_token"),
        "both must be required, got {required:?}"
    );
}

/// C9 (partial): `ErrorResponse` used to be a registered schema that no
/// `responses(...)` entry referenced, making error shapes undiscoverable.
#[tokio::test]
async fn openapi_references_the_shared_error_envelope() {
    let app = common::test_app().await;
    let req = common::json_request("GET", "/api/v1/openapi.json", None);
    let response = app.oneshot(req).await.unwrap();
    let body = common::body_json(response).await;

    let detail = &body["components"]["schemas"]["ErrorDetail"]["properties"];
    for field in ["code", "message", "request_id", "details"] {
        assert!(
            detail[field].is_object(),
            "ErrorDetail must describe `{field}`"
        );
    }

    // Every operation carries the shared 500.
    for (path, item) in body["paths"].as_object().unwrap() {
        for (method, operation) in item.as_object().unwrap() {
            assert_eq!(
                operation["responses"]["500"]["content"]["application/json"]["schema"]["$ref"],
                "#/components/schemas/ErrorResponse",
                "{method} {path} is missing the shared 500 response"
            );
        }
    }
}

/// The checked-in `docs/openapi.json` must match what the binary emits via
/// `dump-openapi`. Keeps the static spec from drifting as the API (or the
/// crate version) changes. If this fails, regenerate it:
/// `cargo run -- dump-openapi > docs/openapi.json`.
#[test]
fn committed_openapi_json_is_up_to_date() {
    let committed =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/openapi.json")).expect(
            "docs/openapi.json should exist — run `cargo run -- dump-openapi > docs/openapi.json`",
        );

    let generated = orion::server::routes::openapi::pretty_json();

    // `println!` adds a trailing newline to the committed file; trim both ends.
    assert_eq!(
        committed.trim_end(),
        generated.trim_end(),
        "docs/openapi.json is stale — regenerate with: cargo run -- dump-openapi > docs/openapi.json"
    );
}

#[tokio::test]
async fn test_swagger_ui_accessible() {
    let app = common::test_app().await;
    let req = common::json_request("GET", "/docs/", None);
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), 200);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&bytes);
    // Any HTML page contains "html"; require the Swagger UI itself.
    assert!(
        html.to_lowercase().contains("swagger"),
        "expected the Swagger UI page, got: {}",
        html.chars().take(200).collect::<String>()
    );
}

// ============================================================
// S17: the docs surface is gated by server.docs.enabled
// ============================================================

use orion::config::AppConfig;

fn production_config() -> AppConfig {
    AppConfig {
        environment: "production".to_string(),
        ..AppConfig::default()
    }
}

/// In production the spec and Swagger UI are not served by default — the
/// routes are not registered at all, so both paths 404 (not 401): their
/// existence is not advertised to anonymous callers.
#[tokio::test]
async fn docs_are_404_in_production_by_default() {
    let app = common::test_app_with_config(production_config()).await;

    for path in ["/docs", "/docs/", "/api/v1/openapi.json"] {
        let resp = app
            .clone()
            .oneshot(common::json_request("GET", path, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{path} must 404 when docs are disabled");
        // The 404 goes through the normal fallback, error envelope included.
        let body = common::body_json(resp).await;
        assert_eq!(body["error"]["code"], "NOT_FOUND", "{path}");
    }
}

/// An explicit `true` wins over the environment: production deployments can
/// opt back in.
#[tokio::test]
async fn explicit_docs_enabled_wins_in_production() {
    let mut config = production_config();
    config.server.docs.enabled = Some(true);
    let app = common::test_app_with_config(config).await;

    let resp = app
        .clone()
        .oneshot(common::json_request("GET", "/api/v1/openapi.json", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = common::body_json(resp).await;
    assert_eq!(body["openapi"], "3.1.0");
}

/// …and an explicit `false` wins in development.
#[tokio::test]
async fn explicit_docs_disabled_wins_in_development() {
    let mut config = AppConfig::default();
    config.server.docs.enabled = Some(false);
    let app = common::test_app_with_config(config).await;

    for path in ["/docs/", "/api/v1/openapi.json"] {
        let resp = app
            .clone()
            .oneshot(common::json_request("GET", path, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{path} must 404 when docs are disabled");
    }
}

/// Disabling the docs must not take the admin or data planes with it.
#[tokio::test]
async fn disabling_docs_leaves_the_rest_of_the_api_served() {
    let app = common::test_app_with_config(production_config()).await;

    let resp = app
        .clone()
        .oneshot(common::json_request("GET", "/health", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = app
        .clone()
        .oneshot(common::json_request("GET", "/api/v1/admin/workflows", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

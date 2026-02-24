mod common;

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

    let expected = [
        "/api/v1/admin/rules",
        "/api/v1/admin/rules/{id}",
        "/api/v1/admin/rules/{id}/status",
        "/api/v1/admin/rules/{id}/test",
        "/api/v1/admin/rules/import",
        "/api/v1/admin/rules/export",
        "/api/v1/admin/rules/validate",
        "/api/v1/admin/connectors",
        "/api/v1/admin/connectors/{id}",
        "/api/v1/admin/engine/status",
        "/api/v1/admin/engine/reload",
        "/api/v1/data/{channel}",
        "/api/v1/data/{channel}/async",
        "/api/v1/data/jobs/{id}",
        "/api/v1/data/batch",
        "/health",
        "/metrics",
    ];

    for path in &expected {
        assert!(paths.contains_key(*path), "Missing path: {}", path);
    }
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
    assert!(html.contains("swagger") || html.contains("Swagger") || html.contains("html"));
}

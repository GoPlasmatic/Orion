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

    // Data channel routes use a dynamic handler (not in OpenAPI spec).
    // B4 closed the gap on admin routes — backups, audit, and the new
    // functions endpoint are now decorated and registered.
    let expected = [
        "/api/v1/data/traces",
        "/api/v1/data/traces/{id}",
        "/health",
        "/metrics",
        // B4 additions:
        "/api/v1/admin/functions",
        "/api/v1/admin/audit-logs",
        "/api/v1/admin/backups",
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

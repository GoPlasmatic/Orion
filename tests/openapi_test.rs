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

    // Only paths explicitly listed in the ApiDoc paths() macro are present.
    // Data channel routes use a dynamic handler (not in OpenAPI spec).
    // Admin routes have utoipa annotations but are NOT yet registered in ApiDoc.
    let expected = [
        "/api/v1/data/traces",
        "/api/v1/data/traces/{id}",
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

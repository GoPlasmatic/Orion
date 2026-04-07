pub mod admin_auth;
pub mod observability;
#[cfg(feature = "otel")]
pub mod otel;
pub mod rate_limit;
pub mod routes;
pub mod state;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, header};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::config::CorsConfig;
use crate::server::state::AppState;

#[cfg(feature = "tls")]
pub mod tls;
#[cfg(feature = "otel")]
pub mod trace_context;

/// Build the Axum router with all middleware layers.
pub fn build_router(state: AppState) -> Router {
    let x_request_id = axum::http::HeaderName::from_static("x-request-id");
    let max_body_size = state.config.ingest.max_payload_size;
    let cors = build_cors(&state.config.cors);

    #[cfg(feature = "otel")]
    let otel_enabled = state.config.tracing.enabled;

    let rate_limit_enabled = state.rate_limit_state.is_some();

    let router = routes::api_routes()
        .layer(DefaultBodyLimit::max(max_body_size))
        .layer(CompressionLayer::new())
        // Security response headers
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
        ))
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
        .layer(cors);

    // HSTS header (only when TLS is enabled)
    let router = if state.config.server.tls.enabled {
        router.layer(SetResponseHeaderLayer::overriding(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=63072000; includeSubDomains"),
        ))
    } else {
        router
    };

    // Rate limiting layer (conditional)
    let router = if rate_limit_enabled {
        router.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::rate_limit_middleware,
        ))
    } else {
        router
    };

    // Admin auth layer (conditional, after rate limiting)
    let router = if state.config.admin_auth.enabled {
        router.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_auth::admin_auth_middleware,
        ))
    } else {
        router
    };

    // HTTP metrics layer (unconditional, outermost to capture 429s)
    let router = router.layer(axum::middleware::from_fn(
        observability::http_metrics_middleware,
    ));

    // When OTel is compiled in and enabled, add trace context extraction middleware
    #[cfg(feature = "otel")]
    let router = if otel_enabled {
        router.layer(axum::middleware::from_fn(
            trace_context::extract_trace_context,
        ))
    } else {
        router
    };

    // Panic recovery layer (outermost — catches panics from all inner layers)
    let router = router.layer(CatchPanicLayer::custom(
        |_: Box<dyn std::any::Any + Send>| {
            crate::metrics::record_error("panic");
            tracing::error!("Handler panicked — recovered by CatchPanicLayer");
            let body = serde_json::json!({
                "error": {
                    "code": "INTERNAL_ERROR",
                    "message": "Internal server error"
                }
            });
            // Avoid unwrap inside panic handler — a second panic would abort the process.
            let json = serde_json::to_string(&body).unwrap_or_else(|_| {
                r#"{"error":{"code":"INTERNAL_ERROR","message":"Internal server error"}}"#
                    .to_string()
            });
            axum::http::Response::builder()
                .status(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json))
                .unwrap_or_else(|_| {
                    // Last-resort fallback: minimal valid response
                    let mut resp =
                        axum::http::Response::new(axum::body::Body::from("Internal server error"));
                    *resp.status_mut() = axum::http::StatusCode::INTERNAL_SERVER_ERROR;
                    resp
                })
        },
    ));

    router.with_state(state)
}

/// Build a CORS layer from configuration.
fn build_cors(config: &CorsConfig) -> CorsLayer {
    if config.allowed_origins.len() == 1 && config.allowed_origins[0] == "*" {
        CorsLayer::permissive()
    } else {
        let origins: Vec<axum::http::HeaderValue> = config
            .allowed_origins
            .iter()
            .filter_map(|o| {
                o.parse().ok().or_else(|| {
                    tracing::warn!(origin = %o, "Invalid CORS origin ignored");
                    None
                })
            })
            .collect();
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
    }
}

/// Wait for SIGTERM or SIGINT for graceful shutdown.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "Failed to install Ctrl+C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to install SIGTERM handler");
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("Shutdown signal received, starting graceful shutdown");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_cors_permissive() {
        let config = CorsConfig {
            allowed_origins: vec!["*".to_string()],
        };
        // Should not panic
        let _layer = build_cors(&config);
    }

    #[test]
    fn test_build_cors_specific_origins() {
        let config = CorsConfig {
            allowed_origins: vec![
                "https://example.com".to_string(),
                "https://app.example.com".to_string(),
            ],
        };
        let _layer = build_cors(&config);
    }

    #[test]
    fn test_build_cors_single_specific_origin() {
        let config = CorsConfig {
            allowed_origins: vec!["https://myapp.com".to_string()],
        };
        let _layer = build_cors(&config);
    }

    #[test]
    fn test_build_cors_invalid_origin_filtered() {
        let config = CorsConfig {
            allowed_origins: vec![
                "https://valid.com".to_string(),
                "not a valid origin \x00".to_string(),
            ],
        };
        // Should not panic - invalid origins are filtered out
        let _layer = build_cors(&config);
    }
}

pub mod admin_auth;
pub mod drain;
pub mod serve;
pub mod extract;
pub mod observability;
pub mod otel;
pub mod rate_limit;
pub mod request_context;
pub mod routes;
pub mod state;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

use crate::config::CorsConfig;
use crate::server::state::AppState;

pub mod tls;
pub mod trace_context;

/// Single middleware that sets all security response headers in one pass,
/// replacing 5 separate `SetResponseHeaderLayer` wrappers.
async fn security_headers_middleware(req: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    response
}

/// Build the Axum router with all middleware layers.
pub fn build_router(state: AppState) -> Router {
    let x_request_id = axum::http::HeaderName::from_static("x-request-id");
    let max_body_size = state.config.ingest.max_payload_size;
    let cors = build_cors(&state.config.cors);

    let otel_enabled = state.config.tracing.enabled;

    let rate_limit_enabled = state.rate_limit_state.is_some();

    let router = routes::api_routes()
        .layer(DefaultBodyLimit::max(max_body_size))
        // Single middleware replaces 5 separate SetResponseHeaderLayer wrappers
        .layer(axum::middleware::from_fn(security_headers_middleware))
        // Scope per-request task-local REQUEST_ID so OrionError responses
        // can embed it in the JSON body (clients then don't need to read
        // both header and body to correlate). Must run inside SetRequestIdLayer
        // so the header is populated before we read it.
        .layer(axum::middleware::from_fn(request_context::request_id_scope))
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid));

    // Response compression (gzip/br/zstd via tower-http). Disabled by default —
    // for small JSON responses the DEFLATE cost outweighs any bandwidth saving.
    // Operators serving large responses should opt in.
    let router = if state.config.server.compression.enabled {
        router.layer(CompressionLayer::new())
    } else {
        router
    };

    // Only add TraceLayer when tracing is enabled to avoid span processing overhead
    let router = if otel_enabled {
        // In cluster mode, stamp the request span with this node's identity
        // so multi-replica traces/logs are attributable (multi-instance C2).
        let instance_id = state
            .cluster
            .enabled
            .then(|| state.cluster.instance_id.clone());
        router.layer(TraceLayer::new_for_http().make_span_with(
            move |req: &axum::extract::Request<_>| {
                let span = tracing::info_span!(
                    "request",
                    method = %req.method(),
                    uri = %req.uri(),
                    version = ?req.version(),
                    instance_id = tracing::field::Empty,
                );
                if let Some(ref id) = instance_id {
                    span.record("instance_id", tracing::field::display(id));
                }
                span
            },
        ))
    } else {
        router
    };

    let router = router.layer(cors);

    // HSTS header (only when TLS is enabled)
    let router = if state.config.server.tls.enabled {
        router.layer(axum::middleware::from_fn(
            |req: axum::extract::Request, next: Next| async move {
                let mut response = next.run(req).await;
                response.headers_mut().insert(
                    header::STRICT_TRANSPORT_SECURITY,
                    HeaderValue::from_static("max-age=63072000; includeSubDomains"),
                );
                response
            },
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

    // HTTP metrics layer (gated by metrics.enabled — when disabled the no-op
    // recorder still costs label hashing + indexmap lookups per request via the
    // metrics crate's macros, so we skip the layer entirely).
    let router = if state.config.metrics.enabled {
        router.layer(axum::middleware::from_fn(
            observability::http_metrics_middleware,
        ))
    } else {
        router
    };

    // When OTel is enabled, add trace context extraction middleware
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
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::PUT,
                axum::http::Method::PATCH,
                axum::http::Method::DELETE,
                axum::http::Method::HEAD,
                axum::http::Method::OPTIONS,
            ])
            .allow_headers([
                axum::http::header::CONTENT_TYPE,
                axum::http::header::AUTHORIZATION,
                axum::http::header::ACCEPT,
                axum::http::HeaderName::from_static("x-api-key"),
                axum::http::HeaderName::from_static("idempotency-key"),
                axum::http::HeaderName::from_static("x-request-id"),
            ])
            .expose_headers([
                axum::http::HeaderName::from_static("x-request-id"),
                axum::http::header::RETRY_AFTER,
            ])
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

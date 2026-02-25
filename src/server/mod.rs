pub mod observability;
#[cfg(feature = "otel")]
pub mod otel;
pub mod rate_limit;
pub mod routes;
pub mod state;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use tower_http::cors::CorsLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

use crate::config::CorsConfig;
use crate::server::state::AppState;

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
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
        .layer(cors);

    // Rate limiting layer (conditional)
    let router = if rate_limit_enabled {
        router.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::rate_limit_middleware,
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

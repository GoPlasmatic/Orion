pub mod routes;
pub mod state;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use tower_http::cors::CorsLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

use crate::config::CorsConfig;
use crate::server::state::AppState;

/// Build the Axum router with all middleware layers.
pub fn build_router(state: AppState) -> Router {
    let x_request_id = axum::http::HeaderName::from_static("x-request-id");
    let max_body_size = state.config.ingest.max_payload_size;
    let cors = build_cors(&state.config.cors);

    routes::api_routes()
        .layer(DefaultBodyLimit::max(max_body_size))
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
}

/// Build a CORS layer from configuration.
fn build_cors(config: &CorsConfig) -> CorsLayer {
    if config.allowed_origins.len() == 1 && config.allowed_origins[0] == "*" {
        CorsLayer::permissive()
    } else {
        let origins: Vec<axum::http::HeaderValue> = config
            .allowed_origins
            .iter()
            .filter_map(|o| o.parse().ok())
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

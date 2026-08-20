pub mod admin_auth;
pub mod drain;
pub mod extract;
pub mod observability;
pub mod otel;
pub mod rate_limit;
pub mod request_context;
pub mod routes;
pub mod serve;
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

    // LAYER ORDER — read this before touching anything below.
    //
    // `Router::layer` wraps: the layer added LAST is the OUTERMOST, so it sees
    // the request FIRST and the response LAST. The list below is therefore in
    // reverse of request-processing order, innermost first.
    //
    // Getting this backwards is not cosmetic (proposal S16). The previous order
    // put admin auth outside rate limiting, so a wrong key returned 401 without
    // ever reaching the limiter — credential guessing was entirely unmetered;
    // put CORS inside admin auth, so browser preflight to any admin route was
    // 401'd before CorsLayer could answer it (browsers never send credentials on
    // preflight, making the admin API unusable from a browser); and put the
    // request-id scope and security headers inside both, so 401 and 429
    // responses carried neither `x-request-id` nor CSP/nosniff/X-Frame-Options.
    //
    // Request order is: set/propagate request id -> request-id scope ->
    // security headers -> catch panic -> CORS -> HSTS -> OTel -> trace ->
    // metrics -> rate limit -> admin auth -> compression -> body limit -> route.
    let router = routes::api_routes(routes::RouteOptions {
        max_admin_body_size: state.config.server.max_admin_body_size,
        docs_enabled: state.config.docs_enabled(),
        metrics_enabled: state.config.metrics.on_main_listener(),
    })
    // Innermost: bound the body before a handler ever reads it. This is the
    // data-plane bound; `admin_routes` re-applies its own, closer to the
    // handler, so raising one does not raise the other (R16).
    .layer(DefaultBodyLimit::max(max_body_size));

    // Response compression (gzip/br/zstd via tower-http). Disabled by default —
    // for small JSON responses the DEFLATE cost outweighs any bandwidth saving.
    // Operators serving large responses should opt in.
    let router = if state.config.server.compression.enabled {
        router.layer(CompressionLayer::new())
    } else {
        router
    };

    // Admin auth — INNER to rate limiting, so a rejected key has already been
    // counted by the limiter. The reverse (the previous order) left credential
    // guessing completely unthrottled, because this layer returns 401 without
    // calling `next.run`.
    let router = if state.config.admin_auth.enabled {
        router.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_auth::admin_auth_middleware,
        ))
    } else {
        router
    };

    // Rate limiting — OUTER to admin auth (see above), inner to observability
    // so throttled requests are still traced and counted.
    let router = if rate_limit_enabled {
        router.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::rate_limit_middleware,
        ))
    } else {
        router
    };

    // HTTP metrics layer (gated by metrics.enabled — when disabled the no-op
    // recorder still costs label hashing + indexmap lookups per request via the
    // metrics crate's macros, so we skip the layer entirely). Inner to
    // SetRequestIdLayer so it can read the id it logs.
    let router = if state.config.metrics.enabled {
        router.layer(axum::middleware::from_fn(
            observability::http_metrics_middleware,
        ))
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

    // When OTel is enabled, add trace context extraction middleware
    let router = if otel_enabled {
        router.layer(axum::middleware::from_fn(
            trace_context::extract_trace_context,
        ))
    } else {
        router
    };

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

    // CORS — OUTER to admin auth so a browser preflight (`OPTIONS`, sent
    // without credentials by definition) is answered by CorsLayer instead of
    // being rejected with 401.
    let router = router.layer(cors);

    // Panic recovery — outer to every layer that can panic, but INNER to the
    // response-shaping layers below, so a recovered 500 still leaves through
    // them and carries security headers and `x-request-id`.
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

    // Outermost band — these must wrap *every* response, including the 401 from
    // admin auth, the 429 from the rate limiter, and the 500 from panic
    // recovery. Previously they sat innermost, so exactly those three responses
    // escaped without security headers and without a request id.
    let router = router
        // Single middleware replaces 5 separate SetResponseHeaderLayer wrappers.
        .layer(axum::middleware::from_fn(security_headers_middleware))
        // Scope the per-request task-local REQUEST_CONTEXT so OrionError
        // responses can embed the request id in the JSON body (clients then
        // don't need to read both header and body to correlate) and the audit
        // log can record the caller's address and user-agent (O7). Must run
        // inside SetRequestIdLayer so the header is populated before we read
        // it.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            request_context::request_context_scope,
        ))
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid));

    router.with_state(state)
}

/// Build the router for the dedicated metrics listener (`metrics.bind_addr`,
/// O12).
///
/// Deliberately minimal: one route, no admin auth, no CORS, no rate limiting,
/// no compression, no request-id plumbing. The whole point of the second
/// listener is that a scraper reaching it needs no credential — so the
/// operator, not this process, is responsible for keeping the address off any
/// network that should not have it. Everything else 404s through the same
/// JSON-envelope fallback as the main listener, and panics are still caught so
/// a scrape can never take the process down.
///
/// Only ever built when `metrics.enabled` is true (see
/// [`crate::config::MetricsConfig::dedicated_bind_addr`]) — a second listener
/// serving a permanently empty body would be the same defect one interface
/// over.
pub fn metrics_router(state: AppState) -> Router {
    Router::new()
        .route("/metrics", axum::routing::get(routes::metrics_endpoint))
        .fallback(|| async {
            crate::errors::OrionError::NotFound(
                "This listener serves GET /metrics only".to_string(),
            )
        })
        .layer(axum::middleware::from_fn(security_headers_middleware))
        .layer(CatchPanicLayer::new())
        .with_state(state)
}

/// Build a CORS layer from configuration.
///
/// The header lists are passed explicitly on **both** branches — never
/// `AllowHeaders::any()`. That is a deliberate behaviour change on the default
/// config: `CorsLayer::permissive()` emits a literal
/// `Access-Control-Allow-Headers: *`, and per the Fetch Standard `Authorization`
/// is a *CORS non-wildcard request-header name* that `*` never covers. So on a
/// default install a browser calling the admin API with a bearer token failed
/// preflight, while the named-origin branch worked because it listed
/// `AUTHORIZATION` by name. Sending the explicit list is strictly widening: it
/// authorizes everything `*` did, plus the header `*` silently withheld.
fn build_cors(config: &CorsConfig) -> CorsLayer {
    let wildcard_origin = config.allowed_origins.len() == 1 && config.allowed_origins[0] == "*";

    // `allow_credentials` + a wildcard origin is refused in `CorsConfig::validate`,
    // which runs before this — tower-http would otherwise assert inside
    // `Layer::layer` and panic the process at boot.
    let layer = if wildcard_origin {
        CorsLayer::new().allow_origin(tower_http::cors::Any)
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
        // `Vary: Origin` keeps deriving itself for the list case; calling
        // `.vary()` would pin it and disable that derivation.
        CorsLayer::new().allow_origin(origins)
    };

    let layer = layer
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::PATCH,
            axum::http::Method::DELETE,
            axum::http::Method::HEAD,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers(config.effective_allowed_headers())
        .expose_headers(config.effective_exposed_headers())
        .allow_credentials(config.allow_credentials);

    // `None` skips the call entirely, preserving today's behaviour of omitting
    // the header. `Some(0)` is deliberately *not* the same — it emits
    // `Access-Control-Max-Age: 0`.
    match config.max_age_secs {
        Some(secs) => layer.max_age(std::time::Duration::from_secs(secs)),
        None => layer,
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
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn cors(origins: &[&str]) -> CorsConfig {
        CorsConfig {
            allowed_origins: origins.iter().map(|o| o.to_string()).collect(),
            ..Default::default()
        }
    }

    /// Drive a real preflight through the layer and hand back the response
    /// headers. These tests used to assert only "does not panic", which is why
    /// the `Authorization` defect below went unnoticed for the life of the
    /// layer.
    async fn preflight(
        config: &CorsConfig,
        origin: &str,
        request_headers: &str,
    ) -> axum::http::HeaderMap {
        let app = Router::new()
            .route("/x", axum::routing::get(|| async { "ok" }))
            .layer(build_cors(config));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/x")
                    .header("Origin", origin)
                    .header("Access-Control-Request-Method", "GET")
                    .header("Access-Control-Request-Headers", request_headers)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("preflight");
        assert_ne!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        resp.headers().clone()
    }

    /// Drive a real (non-preflight) request. `Access-Control-Expose-Headers`
    /// is only meaningful on the actual response, never on the preflight, so
    /// the two have to be asserted separately.
    async fn actual_request(config: &CorsConfig, origin: &str) -> axum::http::HeaderMap {
        let app = Router::new()
            .route("/x", axum::routing::get(|| async { "ok" }))
            .layer(build_cors(config));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/x")
                    .header("Origin", origin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
        resp.headers().clone()
    }

    fn header_list(headers: &axum::http::HeaderMap, name: &str) -> Vec<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The defect. `CorsLayer::permissive()` emits
    /// `Access-Control-Allow-Headers: *`, and per the Fetch Standard
    /// `Authorization` is a CORS non-wildcard request-header name that `*`
    /// never covers — so on the **default** config a browser calling the admin
    /// API with a bearer token failed preflight. The fix sends the explicit
    /// list on both branches; this test is the assertion that would have
    /// caught it.
    #[tokio::test]
    async fn the_wildcard_origin_branch_still_names_authorization() {
        let headers = preflight(&cors(&["*"]), "https://app.example.com", "authorization").await;
        let allowed = header_list(&headers, "access-control-allow-headers");
        assert!(
            allowed.iter().any(|h| h == "authorization"),
            "'*' does not authorize Authorization; it must be listed by name, got {allowed:?}"
        );
        assert_eq!(
            headers
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("*"),
            "a wildcard origin config still answers any origin"
        );
    }

    #[tokio::test]
    async fn the_named_origin_branch_names_authorization_too() {
        let config = cors(&["https://app.example.com"]);
        let headers = preflight(&config, "https://app.example.com", "authorization").await;
        assert!(
            header_list(&headers, "access-control-allow-headers")
                .iter()
                .any(|h| h == "authorization")
        );
    }

    /// A custom request header is admitted once declared — the case that was
    /// inexpressible under any production-legal config.
    #[tokio::test]
    async fn a_configured_request_header_passes_preflight() {
        let config = CorsConfig {
            allowed_origins: vec!["https://app.example.com".to_string()],
            additional_allowed_headers: vec!["deviceId".to_string()],
            ..Default::default()
        };
        let headers = preflight(
            &config,
            "https://app.example.com",
            "authorization, deviceid",
        )
        .await;
        let allowed = header_list(&headers, "access-control-allow-headers");
        assert!(allowed.iter().any(|h| h == "deviceid"), "{allowed:?}");
        // Additive, not replacing: the base list survives.
        for base in [
            "authorization",
            "content-type",
            "x-api-key",
            "idempotency-key",
        ] {
            assert!(
                allowed.iter().any(|h| h == base),
                "the built-in {base} must survive an additional_allowed_headers entry: {allowed:?}"
            );
        }
    }

    #[tokio::test]
    async fn credentials_and_exposed_headers_are_emitted_when_configured() {
        let config = CorsConfig {
            allowed_origins: vec!["https://app.example.com".to_string()],
            additional_exposed_headers: vec!["set-cookie".to_string()],
            allow_credentials: true,
            max_age_secs: Some(600),
            ..Default::default()
        };
        let headers = preflight(&config, "https://app.example.com", "content-type").await;
        assert_eq!(
            headers
                .get("access-control-allow-credentials")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
        assert_eq!(
            headers
                .get("access-control-max-age")
                .and_then(|v| v.to_str().ok()),
            Some("600")
        );

        // Expose-headers rides the actual response, not the preflight.
        let headers = actual_request(&config, "https://app.example.com").await;
        let exposed = header_list(&headers, "access-control-expose-headers");
        assert!(exposed.iter().any(|h| h == "set-cookie"), "{exposed:?}");
        assert!(
            exposed.iter().any(|h| h == "x-request-id"),
            "the built-in exposed headers must survive: {exposed:?}"
        );
        assert_eq!(
            headers
                .get("access-control-allow-credentials")
                .and_then(|v| v.to_str().ok()),
            Some("true"),
            "the page can only read the response if credentials are allowed on it too"
        );
    }

    /// `None` omits the header entirely — today's behaviour, and distinct from
    /// `Some(0)`, which emits `Access-Control-Max-Age: 0`.
    #[tokio::test]
    async fn an_unset_max_age_omits_the_header() {
        let headers = preflight(
            &cors(&["https://app.example.com"]),
            "https://app.example.com",
            "content-type",
        )
        .await;
        assert!(headers.get("access-control-max-age").is_none());
        assert_eq!(
            headers
                .get("access-control-allow-credentials")
                .and_then(|v| v.to_str().ok()),
            None,
            "credentials off must be byte-identical to not calling the setter"
        );
    }

    #[test]
    fn build_cors_tolerates_an_unparseable_origin() {
        // Invalid origins are dropped with a warning rather than failing the
        // build — unchanged behaviour, pinned so the new branches do not alter
        // it.
        let config = cors(&["https://valid.com", "not a valid origin \x00"]);
        let _layer = build_cors(&config);
    }
}

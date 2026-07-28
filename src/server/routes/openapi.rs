use serde::Serialize;
use serde_json::Value;
use utoipa::openapi::path::Operation;
use utoipa::openapi::security::{
    ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme,
};
use utoipa::openapi::{ContentBuilder, Ref, RefOr, Response, ResponseBuilder};
use utoipa::{Modify, OpenApi};

use crate::server::admin_auth::is_guarded_path;

/// Error response body matching Orion's
/// `{"error": {"code": "...", "message": "...", "request_id": "..."}}` format.
///
/// Every 4xx/5xx from `OrionError` — and the 500 emitted by the panic-recovery
/// layer — uses this envelope.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ErrorDetail {
    /// Stable machine-readable code, e.g. `NOT_FOUND`, `VALIDATION_ERROR`,
    /// `RATE_LIMITED`, `CIRCUIT_OPEN`, `TIMEOUT`, `INTERNAL_ERROR`.
    #[schema(example = "VALIDATION_ERROR")]
    code: String,
    /// Human-readable message. Internal 5xx messages are replaced with a
    /// generic string; the detail is logged and kept in the trace.
    message: String,
    /// Per-field breakdown, present only on `VALIDATION_ERROR`.
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Vec<ErrorFieldDetail>>,
    /// Echo of the request's `x-request-id`. Omitted when the request carried
    /// no id and none was generated.
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
}

/// One entry of `error.details` on a validation failure.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ErrorFieldDetail {
    /// JSON pointer-ish path to the offending field.
    #[schema(example = "data.amount")]
    path: String,
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    got: Option<Value>,
}

/// Component name of the default `Authorization: Bearer <token>` scheme.
const SCHEME_BEARER: &str = "admin_bearer";
/// Component name of the custom-header scheme used when `admin_auth.header`
/// is set to something other than `Authorization`.
const SCHEME_API_KEY: &str = "admin_api_key";

/// Registers the admin auth schemes and stamps `security` onto exactly the
/// operations the admin-auth middleware guards (C8).
///
/// Applying this programmatically rather than per-`#[utoipa::path]` is
/// deliberate. The middleware authorizes by **path prefix**
/// ([`is_guarded_path`]), not per handler, so a per-handler annotation would
/// be a second, hand-maintained copy of that rule — and a new admin route
/// added without it would silently publish as an open endpoint, which is the
/// exact regression C8 exists to close.
///
/// While walking every operation it also attaches the shared error responses
/// (C9): a `401` on guarded operations and a `500` everywhere, both referencing
/// [`ErrorResponse`], which was previously a registered-but-unreferenced schema.
/// Existing declarations are never overwritten.
pub(crate) struct SecurityAddon;

/// The admin credential can satisfy either scheme depending on
/// `admin_auth.header`, so the requirement list is an OR.
fn admin_security() -> Vec<SecurityRequirement> {
    let no_scopes: [&str; 0] = [];
    vec![
        SecurityRequirement::new(SCHEME_BEARER, no_scopes),
        SecurityRequirement::new(SCHEME_API_KEY, no_scopes),
    ]
}

/// A JSON response whose body is `#/components/schemas/ErrorResponse`.
fn error_response(description: &str) -> RefOr<Response> {
    RefOr::T(
        ResponseBuilder::new()
            .description(description)
            .content(
                "application/json",
                ContentBuilder::new()
                    .schema(Some(Ref::from_schema_name("ErrorResponse")))
                    .build(),
            )
            .build(),
    )
}

/// Insert `response` under `status` unless the operation already documents it.
fn ensure_response(operation: &mut Operation, status: &str, response: RefOr<Response>) {
    operation
        .responses
        .responses
        .entry(status.to_string())
        .or_insert(response);
}

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                SCHEME_BEARER,
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .description(Some(
                            "Admin API key presented as `Authorization: Bearer <key>` — the \
                             default. Enforced on `/api/v1/admin/*`, `/metrics`, and \
                             `/api/v1/admin/traces*` whenever `admin_auth.enabled` is true, which \
                             the shipped Helm chart and HA compose files set. Keys come from \
                             `admin_auth.api_keys`, either in plaintext or as `sha256:<64-hex>` \
                             digests. The data plane (`POST /api/v1/data/{channel}`) is not \
                             covered by this scheme.",
                        ))
                        .build(),
                ),
            );
            components.add_security_scheme(
                SCHEME_API_KEY,
                SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                    "X-API-Key",
                    "Alternative to `admin_bearer`, active when `admin_auth.header` names a \
                     header other than `Authorization`. The raw key is sent as that header's \
                     value with no `Bearer ` prefix. `X-API-Key` is the conventional choice and \
                     is what this document shows, but the header name is deployment-specific — \
                     substitute whatever `admin_auth.header` is set to. Exactly one of the two \
                     schemes is live at a time; they are listed as alternatives because this \
                     document is generated without knowledge of a deployment's config.",
                ))),
            );
        }

        for (path, item) in openapi.paths.paths.iter_mut() {
            let guarded = is_guarded_path(path);
            let operations = [
                item.get.as_mut(),
                item.put.as_mut(),
                item.post.as_mut(),
                item.delete.as_mut(),
                item.options.as_mut(),
                item.head.as_mut(),
                item.patch.as_mut(),
                item.trace.as_mut(),
            ];
            for operation in operations.into_iter().flatten() {
                if guarded {
                    operation.security = Some(admin_security());
                    ensure_response(
                        operation,
                        "401",
                        error_response(
                            "Missing or invalid admin API key. Only returned when \
                             `admin_auth.enabled` is true.",
                        ),
                    );
                }
                ensure_response(
                    operation,
                    "500",
                    error_response("Unexpected internal error (`INTERNAL_ERROR`)"),
                );
            }
        }
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Orion — Declarative Services Runtime API",
        version = env!("CARGO_PKG_VERSION"),
        description = "\
Declarative services runtime platform.

**Authentication.** The admin API (`/api/v1/admin/*`), the Prometheus endpoint \
(`/metrics`), and the trace read endpoints (`/api/v1/admin/traces*`) require an \
admin API key when `admin_auth.enabled` is true — the default in the shipped \
Helm chart and HA compose files. Operations that need it carry a `security` \
block; see the `admin_bearer` / `admin_api_key` schemes for how the key is \
presented and for the `admin_auth.header` config key that selects between them.

The data plane (`/api/v1/data/{channel}`) and the health probes are \
deliberately unauthenticated: channel-level access control is expressed \
through each channel's `validation_logic` and CORS configuration.

**Errors.** Every non-2xx response uses the `ErrorResponse` envelope: \
`{\"error\": {\"code\", \"message\", \"request_id\"}}`, plus `details` on \
validation failures.",
        license(name = "Apache-2.0"),
    ),
    modifiers(&SecurityAddon),
    tags(
        (name = "Channels", description = "Channel management"),
        (name = "Workflows", description = "Workflow management"),
        (name = "Connectors", description = "Connector management"),
        (name = "Engine", description = "Engine control"),
        (name = "Functions", description = "Engine function schemas"),
        (name = "Audit", description = "Admin audit-log history"),
        (name = "Traces", description = "Execution trace listing and polling"),
        (name = "Trace DLQ", description = "Dead-letter queue inspection, replay, and purge"),
        (name = "Backups", description = "Database backup management (SQLite only)"),
        (name = "Data", description = "Data processing"),
        (name = "Operational", description = "Health and metrics"),
    ),
    paths(
        // Channels
        super::admin::channels::list_channels,
        super::admin::channels::create_channel,
        super::admin::channels::get_channel,
        super::admin::channels::update_channel,
        super::admin::channels::delete_channel,
        super::admin::channels::change_channel_status,
        super::admin::channels::list_channel_versions,
        super::admin::channels::create_new_channel_version,
        super::admin::channels::import_channels,
        // Workflows
        super::admin::workflows::list_workflows,
        super::admin::workflows::create_workflow,
        super::admin::workflows::get_workflow,
        super::admin::workflows::update_workflow,
        super::admin::workflows::delete_workflow,
        super::admin::workflows::change_workflow_status,
        super::admin::workflows::update_rollout,
        super::admin::workflows::list_workflow_versions,
        super::admin::workflows::create_new_workflow_version,
        super::admin::workflows::test_workflow,
        super::admin::workflows::import_workflows,
        super::admin::workflows::export_workflows,
        super::admin::workflows::validate_workflow,
        // Connectors
        super::admin::connectors::list_connectors,
        super::admin::connectors::create_connector,
        super::admin::connectors::get_connector,
        super::admin::connectors::update_connector,
        super::admin::connectors::delete_connector,
        super::admin::connectors::list_circuit_breakers,
        super::admin::connectors::reset_circuit_breaker,
        super::admin::connectors::import_connectors,
        // Engine
        super::admin::engine::engine_status,
        super::admin::engine::engine_reload,
        // Functions (A1: input-schema registry surfaced for tooling)
        super::admin::functions::list_functions,
        // Audit logs
        super::admin::audit::list_audit_logs,
        // Trace DLQ
        super::admin::trace_dlq::list_trace_dlq,
        super::admin::trace_dlq::get_trace_dlq_entry,
        super::admin::trace_dlq::requeue_trace_dlq_entry,
        super::admin::trace_dlq::purge_trace_dlq,
        // Backups
        super::admin::backups::create_backup,
        super::admin::backups::list_backups,
        // Data plane (C8) — one catch-all handler, two documented operations
        super::data::dynamic_handler,
        super::data::submit_channel_request_async_docs,
        super::data::traces::list_traces,
        super::data::traces::get_trace,
        // Operational
        super::health_check,
        super::liveness_check,
        super::readiness_check,
        super::metrics_endpoint,
    ),
    components(
        schemas(
            crate::storage::models::Workflow,
            crate::storage::models::Channel,
            crate::storage::models::Connector,
            crate::storage::models::Trace,
            crate::storage::repositories::workflows::CreateWorkflowRequest,
            crate::storage::repositories::workflows::UpdateWorkflowRequest,
            crate::storage::repositories::workflows::StatusChangeRequest,
            crate::storage::repositories::workflows::RolloutUpdateRequest,
            crate::storage::repositories::channels::CreateChannelRequest,
            crate::storage::repositories::channels::UpdateChannelRequest,
            crate::storage::repositories::channels::ChannelStatusChangeRequest,
            crate::storage::repositories::connectors::CreateConnectorRequest,
            crate::storage::repositories::connectors::UpdateConnectorRequest,
            super::admin::trace_dlq::PurgeTraceDlqRequest,
            super::admin::workflows::ValidationEnvelope,
            super::data::ProcessRequest,
            super::data::ProcessResponse,
            super::data::ProcessTaskError,
            super::data::AsyncSubmitResponse,
            ErrorResponse,
            ErrorDetail,
            ErrorFieldDetail,
        )
    )
)]
pub(crate) struct ApiDoc;

/// The public HTTP API's OpenAPI 3.1 spec, pretty-printed as JSON.
///
/// Shared by the `orion-server dump-openapi` subcommand and the drift-check
/// integration test so both serialize the spec identically. The committed
/// copy lives at `docs/openapi.json`; regenerate it with
/// `cargo run -- dump-openapi > docs/openapi.json` whenever the API changes.
pub fn pretty_json() -> String {
    serde_json::to_string_pretty(&ApiDoc::openapi())
        .expect("OpenAPI spec is always serializable to JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> utoipa::openapi::OpenApi {
        ApiDoc::openapi()
    }

    #[test]
    fn security_schemes_are_registered() {
        let components = spec().components.expect("components");
        assert!(components.security_schemes.contains_key(SCHEME_BEARER));
        assert!(components.security_schemes.contains_key(SCHEME_API_KEY));
    }

    /// Every operation the admin-auth middleware guards must advertise the
    /// requirement, and nothing else may.
    ///
    /// One path enforces auth in its handler rather than the middleware:
    /// `GET /api/v1/admin/traces/{id}` (R12) accepts *either* an admin
    /// credential or the per-submission `trace_token`, so it documents a 401
    /// without being in `is_guarded_path`. It is pinned here explicitly so a
    /// new unguarded-but-401 route still fails this test until reviewed.
    #[test]
    fn security_matches_the_middleware_guard() {
        const HANDLER_ENFORCED_401: &[&str] = &["/api/v1/admin/traces/{id}"];
        let spec = spec();
        for (path, item) in &spec.paths.paths {
            let expected = is_guarded_path(path);
            let handler_enforced = HANDLER_ENFORCED_401.contains(&path.as_str());
            for (method, operation) in [
                ("get", &item.get),
                ("put", &item.put),
                ("post", &item.post),
                ("delete", &item.delete),
                ("patch", &item.patch),
            ] {
                let Some(operation) = operation else { continue };
                assert_eq!(
                    operation.security.is_some(),
                    expected,
                    "{method} {path}: security block does not match is_guarded_path()"
                );
                assert_eq!(
                    operation.responses.responses.contains_key("401"),
                    expected || handler_enforced,
                    "{method} {path}: 401 response does not match is_guarded_path()"
                );
            }
        }
    }

    #[test]
    fn every_operation_documents_a_500() {
        let spec = spec();
        for (path, item) in &spec.paths.paths {
            for operation in [&item.get, &item.put, &item.post, &item.delete, &item.patch]
                .into_iter()
                .flatten()
            {
                assert!(
                    operation.responses.responses.contains_key("500"),
                    "{path}: missing shared 500 response"
                );
            }
        }
    }

    /// The data plane is the product's primary API and must not be
    /// unauthenticated *and* undocumented at the same time.
    #[test]
    fn data_plane_and_probes_are_documented() {
        let spec = spec();
        for path in [
            "/api/v1/data/{channel}",
            "/api/v1/data/{channel}/async",
            "/healthz",
            "/readyz",
        ] {
            assert!(spec.paths.paths.contains_key(path), "missing path: {path}");
        }
        let data = spec
            .paths
            .paths
            .get("/api/v1/data/{channel}")
            .and_then(|item| item.post.as_ref())
            .expect("POST /api/v1/data/{channel}");
        // Unauthenticated by design — assert it explicitly so a future blanket
        // `security` default cannot quietly claim otherwise.
        assert!(data.security.is_none());
        for status in ["200", "400", "409", "429", "503", "504"] {
            assert!(
                data.responses.responses.contains_key(status),
                "data plane missing {status}"
            );
        }
        let async_submit = spec
            .paths
            .paths
            .get("/api/v1/data/{channel}/async")
            .and_then(|item| item.post.as_ref())
            .expect("POST /api/v1/data/{channel}/async");
        let accepted = async_submit
            .responses
            .responses
            .get("202")
            .expect("202 accepted");
        let warning_documented = match accepted {
            RefOr::T(response) => response.headers.contains_key("warning"),
            RefOr::Ref(_) => false,
        };
        assert!(
            warning_documented,
            "202 must document the `Warning: 299` header emitted when tracing is off"
        );
    }
}

use serde_json::Value;
use utoipa::openapi::path::Operation;
use utoipa::openapi::security::{
    ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme,
};
use utoipa::openapi::{Content, ContentBuilder, Ref, RefOr, Response, ResponseBuilder};
use utoipa::{Modify, OpenApi};

use crate::server::admin_auth::is_guarded_path;
use crate::storage::models::TraceListItemResponse;

/// The spec is generated without a config, so it has to pick one deployment
/// shape to describe for the paths whose registration is config-dependent.
/// `/metrics` is documented as a path of the main listener, so the guard
/// predicate is evaluated as if it is registered there — see
/// [`is_guarded_path`].
const METRICS_ON_DOCUMENTED_LISTENER: bool = true;

/// Error response body matching Orion's
/// `{"error": {"code": "...", "message": "...", "request_id": "..."}}` format.
///
/// Every 4xx/5xx from `OrionError` — and the 500 emitted by the panic-recovery
/// layer — uses this envelope.
///
/// These are the shared contract types, not mirrors of them. Three hand-written
/// structs used to stand in here and had already drifted: they published
/// `details` as a nullable array, while `ErrorBody` omits the key when empty and
/// never sends null. `schema(as = …)` on the orion-api types keeps the component
/// names the document has always used, since renaming one breaks clients
/// generated against it.
pub(crate) use orion_api::{
    ErrorBody as ErrorDetail, ErrorEnvelope as ErrorResponse, FieldError as ErrorFieldDetail,
};

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

/// The `application/json` body every error response carries. Shared so the
/// injected 401/500 and the annotation-declared 4xx/5xx cannot describe the
/// same envelope differently.
fn error_content() -> Content {
    ContentBuilder::new()
        .schema(Some(Ref::from_schema_name("ErrorResponse")))
        .build()
}

/// A JSON response whose body is `#/components/schemas/ErrorResponse`.
fn error_response(description: &str) -> RefOr<Response> {
    RefOr::T(
        ResponseBuilder::new()
            .description(description)
            .content("application/json", error_content())
            .build(),
    )
}

/// Give every declared-but-empty 4xx/5xx the `ErrorResponse` body (R22).
///
/// A `(status = 404, description = "…")` in a `#[utoipa::path]` emits a
/// response with no `content` — utoipa only fills one in when the annotation
/// also names a `body`. Thirty of them were published that way, saying an
/// error exists but not what it looks like. Every non-2xx body Orion can
/// produce is rendered by `OrionError`'s `IntoResponse`, so the shape is known
/// centrally; filling it here means a new endpoint cannot forget it. A
/// response that *does* declare content is left alone.
fn fill_error_content(operation: &mut Operation) {
    for (status, response) in operation.responses.responses.iter_mut() {
        if !status.starts_with('4') && !status.starts_with('5') {
            continue;
        }
        if let RefOr::T(response) = response
            && response.content.is_empty()
        {
            response
                .content
                .insert("application/json".to_string(), error_content());
        }
    }
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
            // `true`: this document describes the main listener, and it
            // documents `/metrics` — so it must describe the configuration in
            // which that path exists there (`metrics.enabled = true`, no
            // `metrics.bind_addr`). The other two configurations do not serve
            // `/metrics` on this listener at all, and the operation's own
            // description says so.
            let guarded = is_guarded_path(path, METRICS_ON_DOCUMENTED_LISTENER);
            // Paired with the method name because the read-only-key refusal
            // below is method-dependent — `admin_auth` allows GET/HEAD on a
            // read-only key and refuses everything else.
            let operations = [
                ("GET", item.get.as_mut()),
                ("PUT", item.put.as_mut()),
                ("POST", item.post.as_mut()),
                ("DELETE", item.delete.as_mut()),
                ("OPTIONS", item.options.as_mut()),
                ("HEAD", item.head.as_mut()),
                ("PATCH", item.patch.as_mut()),
                ("TRACE", item.trace.as_mut()),
            ];
            for (method, operation) in operations
                .into_iter()
                .filter_map(|(m, op)| op.map(|op| (m, op)))
            {
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
                    // S13: a read-only key authenticates but does not authorize
                    // a mutation. Injected rather than annotated for the same
                    // reason the 401 is — the rule lives in the middleware and
                    // is method-based, so a per-handler annotation would be a
                    // second copy of it that a new route could forget.
                    if !matches!(method, "GET" | "HEAD") {
                        ensure_response(
                            operation,
                            "403",
                            error_response(
                                "The presented admin key is read-only, and this method \
                                 mutates. Only returned when `admin_auth.enabled` is true.",
                            ),
                        );
                    }
                }
                ensure_response(
                    operation,
                    "500",
                    error_response("Unexpected internal error (`INTERNAL_ERROR`)"),
                );
                fill_error_content(operation);
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
through each channel's `validation_logic` and `origin_allow_list`.

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
        (name = "Packages", description = "Package receipts — what package versions are \
            staged or applied here (K14)"),
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
        super::admin::channels::export_channels,
        super::admin::channels::validate_channel,
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
        super::admin::workflows::workflow_dependencies,
        // Connectors
        super::admin::connectors::list_connectors,
        super::admin::connectors::create_connector,
        super::admin::connectors::get_connector,
        super::admin::connectors::update_connector,
        super::admin::connectors::delete_connector,
        super::admin::connectors::list_circuit_breakers,
        super::admin::connectors::reset_circuit_breaker,
        super::admin::connectors::import_connectors,
        super::admin::connectors::export_connectors,
        super::admin::connectors::validate_connector,
        super::admin::connectors::test_connector,
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
        // Packages (K14)
        super::admin::packages::list_packages,
        super::admin::packages::get_package,
        super::admin::packages::put_package,
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
            // R22/D28: only shapes an endpoint actually returns, and never a
            // row struct. Row structs were registered here and `$ref`d by
            // nothing — they described `condition_json` / `tasks_json` as
            // strings, which no response carries. `ConnectorResponse` is the
            // masked wire shape for a single connector; the `Connector` row it
            // is built from carries the unmasked config and cannot be
            // serialized at all.
            crate::storage::models::ConnectorResponse,
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
            super::admin::ValidationEnvelope,
            crate::storage::models::PackageReceiptResponse,
            crate::storage::models::PackageState,
            super::admin::workflows::WorkflowDependencies,
            super::admin::workflows::ConnectorDependency,
            crate::storage::repositories::packages::PutPackageReceiptRequest,
            super::admin::packages::PackageDetail,
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
    /// Three paths answer 401 from somewhere other than the admin middleware,
    /// and each is listed explicitly so a *new* unguarded-but-401 route still
    /// fails this test until someone reviews it:
    ///
    /// * `GET /api/v1/admin/traces/{id}` (R12) authenticates in its handler,
    ///   accepting either an admin credential or the per-submission
    ///   `trace_token`.
    /// * The two data-plane paths authenticate per *channel*, in the ingress
    ///   guards, when the channel carries an `auth` block. That is not admin
    ///   auth and must not advertise a `security` block — the requirement is a
    ///   property of the individual channel, which this document cannot name —
    ///   but the 401 is real and belongs in the responses.
    #[test]
    fn security_matches_the_middleware_guard() {
        const HANDLER_ENFORCED_401: &[&str] = &[
            "/api/v1/admin/traces/{id}",
            "/api/v1/data/{channel}",
            "/api/v1/data/{channel}/async",
        ];
        let spec = spec();
        for (path, item) in &spec.paths.paths {
            let expected = is_guarded_path(path, METRICS_ON_DOCUMENTED_LISTENER);
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

    /// R22: a response that names a status but no body is a spec that says
    /// "an error happens here" without saying what it looks like. 44 of the 48
    /// 2xx responses and 30 of the 4xx/5xx were published that way. `204` is
    /// exempt — a body would be a protocol violation.
    #[test]
    fn every_response_that_can_carry_a_body_describes_one() {
        let spec = spec();
        let mut missing = Vec::new();
        for (path, item) in &spec.paths.paths {
            for (verb, operation) in [
                ("GET", &item.get),
                ("PUT", &item.put),
                ("POST", &item.post),
                ("DELETE", &item.delete),
                ("PATCH", &item.patch),
            ] {
                let Some(operation) = operation else { continue };
                for (status, response) in &operation.responses.responses {
                    if status == "204" {
                        continue;
                    }
                    let RefOr::T(response) = response else {
                        continue;
                    };
                    if response.content.is_empty() {
                        missing.push(format!("{verb} {path} -> {status}"));
                    }
                }
            }
        }
        missing.sort();
        assert!(
            missing.is_empty(),
            "{} response(s) declare a status with no body schema:\n  {}",
            missing.len(),
            missing.join("\n  ")
        );
    }

    /// No row struct's name may reach the published document (R22, D17, D28).
    ///
    /// A row struct describes storage, not the wire: publishing one advertises
    /// `condition_json` / `tasks_json` as opaque strings on a shape no client
    /// receives, and — the reason this is a security rule and not a tidiness
    /// one — it advertises columns like `Trace::access_token_hash` that exist
    /// to be compared, never shown. There are no longer any exceptions:
    /// `Connector`, `TraceDlqEntry` and `AuditLogEntry` each publish a
    /// `*Response` DTO now, and none of the rows can derive `Serialize` at all.
    ///
    /// The generic envelopes inline their type parameter into the component
    /// name (`DataEnvelope_ConnectorResponse`), so a row leaking in through a
    /// `body =` annotation is caught by the substring check too.
    #[test]
    fn no_storage_row_struct_is_published_unless_it_is_the_wire_shape() {
        const ROW_STRUCTS: &[&str] = &[
            "Workflow",
            "Channel",
            "Connector",
            "Trace",
            "TraceListRow",
            "TraceDlqEntry",
            "TraceDlqSummary",
            "AuditLogEntry",
            "PackageReceipt",
        ];
        let spec = spec();
        let schemas = spec
            .components
            .as_ref()
            .expect("components")
            .schemas
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for row in ROW_STRUCTS {
            for name in &schemas {
                // `ConnectorResponse` legitimately starts with `Connector`;
                // only a whole `_`-separated segment is a row leaking through.
                let leaks = name.split('_').any(|segment| segment == *row);
                assert!(
                    !leaks,
                    "`{name}` publishes the `{row}` storage row — no endpoint \
                     returns it. Publish the DTO from storage::models::dto instead."
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
        // R11: the 202 no longer has a conditional shape to document. It used
        // to answer `{"trace_id": null}` plus a `Warning: 299` header when
        // `trace.mode = off` — a receipt for a result that could never be
        // fetched. The row is now written before the 202 is sent, so the id is
        // always pollable and the header is gone.
        let no_warning_header = match accepted {
            RefOr::T(response) => !response.headers.contains_key("warning"),
            RefOr::Ref(_) => false,
        };
        assert!(
            no_warning_header,
            "the 202 must not document a `Warning` header — `trace_id` is unconditional"
        );
        let schemas = spec
            .components
            .as_ref()
            .map(|c| &c.schemas)
            .expect("components.schemas");
        let async_response = serde_json::to_value(
            schemas
                .get("AsyncSubmitResponse")
                .expect("AsyncSubmitResponse is registered"),
        )
        .expect("schema serializes");
        let required = async_response["required"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();
        assert!(
            required.contains(&"trace_id") && required.contains(&"trace_token"),
            "both must be required — a nullable trace_id was R11's permanent half: \
             {async_response}"
        );
    }
}

// ============================================================
// Response envelopes (R17 shape, R22 schemas)
// ============================================================

/// The `{"data": …}` envelope every admin 2xx carries (R17).
///
/// Generic so each resource gets a described response without a hand-written
/// wrapper struct per endpoint. utoipa inlines the type parameter, so
/// `DataEnvelope<WorkflowResponse>` publishes the full shape. Before R22, 44
/// of the 48 2xx responses had no `content` block at all.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct DataEnvelope<T> {
    #[allow(dead_code)] // Schema-only: the runtime builds this with `json!`.
    data: T,
}

/// The paginated envelope: `data` plus the three counters, and nothing else.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct PaginatedEnvelope<T> {
    #[allow(dead_code)]
    data: Vec<T>,
    /// Total rows matching the filter, ignoring `limit`/`offset`.
    #[allow(dead_code)]
    total: i64,
    #[allow(dead_code)]
    limit: i64,
    #[allow(dead_code)]
    offset: i64,
}

// ---------------------------------------------------------------------------
// Schema-only bodies for the operational and batch endpoints (R22).
//
// These endpoints build their responses with `json!`, so there is no runtime
// struct to point `body =` at. Declaring the shape here is what makes the
// published document describe them at all; the
// `every_response_that_can_carry_a_body_describes_one` test keeps it that way.
// ---------------------------------------------------------------------------

/// One row of `GET /api/v1/admin/connectors`: the stored connector with
/// secrets masked, plus the registry's view of whether it actually loaded.
///
/// Declared separately from [`crate::storage::models::ConnectorResponse`]
/// because `PaginatedEnvelope<ConnectorResponse>` would under-describe the row
/// by exactly the three fields an operator reads the list for.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct ConnectorListItem {
    id: String,
    name: String,
    connector_type: String,
    /// Connector config with every secret replaced by `******`, verbatim.
    config_json: String,
    /// The same masked config, parsed — the shape `POST`/`PUT` accept.
    config: serde_json::Value,
    enabled: bool,
    /// Selection labels (K6); filter the list with `?tag=`.
    tags: Vec<String>,
    /// `sha256:…` over the canonical importable content (K10).
    content_hash: String,
    created_at: String,
    updated_at: String,
    /// `loaded`, `failed`, or `disabled`.
    #[schema(example = "loaded")]
    load_status: String,
    /// Why the connector failed to load. Present only when
    /// `load_status = "failed"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    load_error: Option<String>,
    /// Which load stage failed. Present only when `load_status = "failed"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    load_error_stage: Option<String>,
}

/// One item of `GET /api/v1/admin/connectors/export`.
///
/// Declared separately from [`crate::storage::models::ConnectorResponse`]
/// because an export has to emit the shape `/import` *accepts*, not the shape
/// the read endpoints return: `config` is the parsed object here, where the
/// read shape carries `config_json` as a string. Publishing the read type for
/// this route promised three fields the body does not carry (`config_json`,
/// `created_at`, `updated_at`) and hid the one it does.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct ConnectorExportItem {
    id: String,
    name: String,
    connector_type: String,
    /// The parsed connector config, with every secret replaced by `******`.
    /// `env://`-style references survive the masking as references.
    config: serde_json::Value,
    enabled: bool,
    /// Selection labels (K6); filter the export with `?tag=`.
    tags: serde_json::Value,
    /// `sha256:…` over the canonical importable content (K10), computed on the
    /// stored (unmasked) config. Ignored by `/import`, like the other extras.
    content_hash: String,
}

// Result of a bulk import (R18, K2). This used to be a schema-only mirror of
// what `import_response` built with `json!`; the shared `orion-api` type is
// now both the runtime shape and the published schema, so the mirror is gone
// and the two cannot drift. Re-exported so `#[utoipa::path]` bodies keep
// referencing it by its old path.
pub(crate) use orion_api::ImportResult;

/// `GET /api/v1/admin/engine/status`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct EngineStatus {
    version: String,
    uptime_seconds: i64,
    workflows_count: u64,
    active_workflows: u64,
    /// Distinct channel names across the loaded workflows.
    channels: Vec<String>,
}

/// `POST /api/v1/admin/engine/reload`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct EngineReloaded {
    reloaded: bool,
    workflows_count: u64,
}

/// `GET /api/v1/admin/connectors/circuit-breakers`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct CircuitBreakerStates {
    /// Whether `engine.circuit_breaker.enabled` is set. When false, `breakers`
    /// is empty because nothing is tracked.
    enabled: bool,
    /// Always `"node"` — breaker state is per-replica, never cluster-wide (F21).
    scope: String,
    /// Which node's map this is.
    instance_id: String,
    /// `channel:connector` → `closed` | `open` | `half_open`. Node-local.
    breakers: std::collections::HashMap<String, String>,
}

/// `POST /api/v1/admin/connectors/circuit-breakers/{key}`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct CircuitBreakerReset {
    reset: bool,
    key: String,
    /// Whether the breaker existed on the node that served the request.
    /// Breakers are node-local; in cluster mode the reset is broadcast over
    /// the epoch bus regardless, so `false` here is not a failure (F21).
    found_on_this_node: bool,
}

/// `POST /api/v1/admin/trace-dlq/purge`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct DlqPurgeResult {
    /// Rows deleted.
    purged: u64,
    /// Echo of the requested age bound.
    older_than_hours: i64,
}

/// `POST /api/v1/admin/workflows/{id}/test` — a dry run against sample input.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct WorkflowTestResult {
    /// Whether the workflow's condition matched the supplied input.
    matched: bool,
    /// Per-step execution trace: `task_id` and `result`
    /// (`executed` | `skipped` | `failed`) for each step.
    trace: Value,
    /// The message data after the pipeline ran.
    output: Value,
    errors: Vec<Value>,
}

/// The backup `POST /api/v1/admin/backups` just wrote.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct BackupFile {
    filename: String,
    /// Absolute path on the node that served the request.
    path: String,
    size_bytes: u64,
    created_at: String,
}

/// One row of `GET /api/v1/admin/backups`. Deliberately not [`BackupFile`]:
/// the listing reads `mtime` off the directory entry and has no `path`, so
/// the two responses differ by two fields.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct BackupListItem {
    filename: String,
    size_bytes: u64,
    modified_at: String,
}

/// `GET /api/v1/admin/traces` — a page of trace rows.
///
/// Not [`PaginatedEnvelope`] (D8): `total` is present only when the request
/// asked for it with `include_total=true`, because the count scans the whole
/// filtered set; and `next_cursor` carries the keyset position of the last row
/// so the next page can be fetched without an `offset` skip.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct TracePageEnvelope {
    data: Vec<TraceListItemResponse>,
    /// Rows matching the filter, ignoring `limit`/`offset`. Present only with
    /// `include_total=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<i64>,
    limit: i64,
    offset: i64,
    /// Pass back as `?cursor=` to fetch the next page. Present when the page
    /// is ordered by `created_at` (the default) and may have a successor.
    /// Opaque — do not parse it.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// `GET /api/v1/admin/traces/{id}` — the list row plus the payloads.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct TraceDetail {
    id: String,
    status: String,
    mode: String,
    channel: String,
    channel_id: Option<String>,
    created_at: String,
    /// The full engine message. Present only when `status = "completed"`.
    /// `context.metadata` is stripped from the projection (S14).
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<Value>,
    /// Present only when `status = "failed"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<i64>,
    /// Per-task execution trace, when the channel enabled `task_details`.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_trace_json: Option<Value>,
}

/// One entry of the function catalogue from `GET /api/v1/admin/functions`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct FunctionSchemaItem {
    name: String,
    description: String,
    /// `connector`, `control`, `data`, or `utility`.
    category: String,
    /// `orion` for a handler Orion implements and input-schema validates,
    /// `engine` for a dataflow-rs built-in the engine executes itself.
    source: String,
    /// Other accepted spellings — `validation` carries `validate`. Omitted
    /// when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<String>,
    /// **Absent** for an engine built-in, which declares no input schema and
    /// is therefore not input-validated at create time.
    #[serde(skip_serializing_if = "Option::is_none")]
    input_fields: Option<Vec<Value>>,
    /// What a second run of this function does, as
    /// `{"kind": "..."}` — `pure`, `read`, `idempotent_write`, `unsafe_write`,
    /// or `depends_on` with the `input` that decides it (`http_call` on its
    /// `method`, `data_write` on its `op`).
    ///
    /// A different question from whether an *error* was transient: this is
    /// what a retry costs when it is. Orion retries tasks in several places —
    /// the trace DLQ, a Kafka redelivery, `http_call`'s transport retry — and
    /// this is how an author or a tool knows which of those are safe over a
    /// given function.
    retry_safety: Value,
}

/// A liveness / readiness / health probe body.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct HealthStatus {
    /// `ok` | `degraded`.
    status: String,
    version: String,
    uptime_seconds: i64,
    /// Per-subsystem state: `database`, `engine`, `connectors`, `channels`,
    /// plus `kafka` when `kafka.enabled` (O10) and `cluster_redis` in
    /// cluster mode.
    components: Value,
    /// Build provenance and detail, served only to an admin caller (O9).
    #[serde(skip_serializing_if = "Option::is_none")]
    git_hash: Option<String>,
}

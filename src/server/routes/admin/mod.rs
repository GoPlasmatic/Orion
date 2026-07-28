pub(crate) mod audit;
pub(crate) mod backups;
pub(crate) mod channels;
pub(crate) mod connectors;
pub(crate) mod engine;
pub(crate) mod functions;
pub(crate) mod trace_dlq;
pub(crate) mod workflows;

use axum::Router;
use axum::routing::{get, patch, post};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use axum::Extension;

use crate::engine::reload_engine;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;
use crate::storage::repositories::audit_logs::AuditLogRepository;

/// Largest batch any `/import` endpoint accepts.
///
/// Each item is a separate in-request DB round-trip holding a connection, plus
/// an audit-log write, and the only previous bound was the global 1 MB body
/// limit — which is tens of thousands of minimal JSON objects. That is a
/// self-inflicted DoS on the admin plane (proposal R14). Larger migrations
/// should be chunked.
pub(crate) const MAX_IMPORT_ITEMS: usize = 1000;

/// Reject an oversized import batch before any work is done.
pub(crate) fn check_import_batch_size(len: usize) -> Result<(), crate::errors::OrionError> {
    if len > MAX_IMPORT_ITEMS {
        return Err(crate::errors::OrionError::validation(format!(
            "import accepts at most {MAX_IMPORT_ITEMS} items per request, got {len} — \
             split the batch"
        )));
    }
    Ok(())
}

/// The four per-entity operations [`import_items`] drives.
///
/// A struct rather than four positional parameters: they are all closures with
/// interchangeable-looking types, so a transposition at a call site would
/// compile (the F44 hazard, one layer up).
pub(crate) struct ImportOps<V, K, E, C> {
    /// The same validation the singular `POST` endpoint runs.
    pub validate: V,
    /// The stored key a duplicate would collide on — `workflow_id`,
    /// `channel_id`, connector `name`. `None` when the item names none, in
    /// which case the store generates one and nothing can conflict.
    pub conflict_key: K,
    /// Whether that key is already taken.
    pub exists: E,
    /// Persist the item.
    pub create: C,
}

/// The one per-item driver behind all three `/import` endpoints.
///
/// R19: there used to be two. Workflows took `OrionJson<Vec<CreateWorkflowRequest>>`
/// and drove via `bulk_create`, so **one malformed item aborted the whole batch
/// with a 400**; channels and connectors took `OrionJson<Vec<Value>>` and
/// produced **one failed entry**. All three declared
/// `request_body = Vec<CreateXRequest>` in their `#[utoipa::path]`, so the spec
/// described neither behaviour correctly. Per-item is the right semantic for a
/// bulk endpoint that already reports `{imported, failed, errors[]}` — a batch
/// that reports counts should produce them.
///
/// R15: `?dry_run=true` now reads. It used to skip the database entirely, as
/// its own doc comment said — but the stated use case is CI pre-flight, and the
/// most common real failure is a **name conflict**, which is exactly what a
/// no-DB dry-run cannot see. A green dry-run therefore said nothing. Conflicts
/// against stored rows and duplicates *within the batch* are both reported now;
/// the second was free and previously missed entirely.
pub(crate) async fn import_items<T, V, K, E, EFut, C, CFut>(
    items: Vec<serde_json::Value>,
    dry_run: bool,
    ops: ImportOps<V, K, E, C>,
) -> (u64, u64, Vec<serde_json::Value>)
where
    T: serde::de::DeserializeOwned,
    V: Fn(&T) -> Result<(), crate::errors::OrionError>,
    K: Fn(&T) -> Option<String>,
    E: Fn(String) -> EFut,
    EFut: std::future::Future<Output = Result<bool, crate::errors::OrionError>>,
    C: Fn(T) -> CFut,
    CFut: std::future::Future<Output = Result<(), crate::errors::OrionError>>,
{
    let mut ok = 0u64;
    let mut failed = 0u64;
    let mut errors = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for (i, item) in items.into_iter().enumerate() {
        let mut fail = |e: String| {
            failed += 1;
            errors.push(json!({"index": i, "error": e}));
        };
        // Deserialize per item, so a single shape or enum typo is one failed
        // entry rather than a 400 for the whole batch.
        let parsed: T = match serde_json::from_value(item) {
            Ok(v) => v,
            Err(e) => {
                fail(e.to_string());
                continue;
            }
        };
        if let Err(e) = (ops.validate)(&parsed) {
            fail(e.client_message());
            continue;
        }

        // Conflict detection. On a real import the store enforces this anyway,
        // so it runs on dry-run only — where it is the whole point.
        if dry_run {
            if let Some(key) = (ops.conflict_key)(&parsed) {
                if seen.contains(&key) {
                    fail(format!(
                        "'{key}' appears more than once in this batch — the second \
                         item would conflict with the first"
                    ));
                    continue;
                }
                match (ops.exists)(key.clone()).await {
                    Ok(true) => {
                        fail(format!("'{key}' already exists"));
                        continue;
                    }
                    Ok(false) => seen.push(key),
                    // A probe that could not run must not be reported as a
                    // clean item: say so and let the operator retry.
                    Err(e) => {
                        fail(format!(
                            "could not check for a conflict: {}",
                            e.client_message()
                        ));
                        continue;
                    }
                }
            }
            ok += 1;
            continue;
        }

        if let Err(e) = (ops.create)(parsed).await {
            fail(e.client_message());
        } else {
            ok += 1;
        }
    }
    (ok, failed, errors)
}

/// The `?dry_run=true` response envelope shared by all three import endpoints.
///
/// Same four fields as [`import_response`], distinguished only by
/// `dry_run: true` (proposal R18). Pre-1.0 this returned six fields for two
/// facts — `would_create`/`would_fail` alongside a hardcoded `imported: 0`
/// and a `failed` that always equalled `would_fail`.
pub(crate) fn dry_run_response(
    would_import: u64,
    would_fail: u64,
    errors: Vec<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    import_envelope(true, would_import, would_fail, errors)
}

/// The real-import response envelope shared by all three import endpoints.
pub(crate) fn import_response(
    imported: u64,
    failed: u64,
    errors: Vec<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    import_envelope(false, imported, failed, errors)
}

fn import_envelope(
    dry_run: bool,
    imported: u64,
    failed: u64,
    errors: Vec<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "data": {
            "dry_run": dry_run,
            "imported": imported,
            "failed": failed,
            "errors": errors,
        }
    }))
}

/// Query parameters accepted by all three `/import` endpoints (B6).
///
/// R27: lived in `workflows.rs` while its four sibling helpers
/// (`check_import_batch_size`, `import_items`, `dry_run_response`,
/// `import_response`) lived here, so channels and connectors imported it from a
/// module they otherwise have nothing to do with.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ImportQuery {
    /// When true, validate each item and report what would happen without
    /// writing. Probes for conflicts against stored rows and for duplicates
    /// within the batch (R15).
    #[serde(default)]
    pub dry_run: bool,
}

/// Shared version filter used by both channel and workflow version endpoints.
#[derive(Debug, Deserialize)]
pub(crate) struct VersionFilter {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

impl VersionFilter {
    /// The effective page bounds.
    ///
    /// R24: `unwrap_or(50)` / `unwrap_or(0)` were written out in both
    /// `list_channel_versions` and `list_workflow_versions`. Two copies of a
    /// default is how defaults drift — and the repository layer encodes the
    /// same `50` a third time, in `clamp_pagination`. Route-layer callers read
    /// it from here; the repository still clamps, because it is the one that
    /// must not be handed an unbounded `LIMIT` by any caller.
    pub fn limit_offset(&self) -> (i64, i64) {
        crate::storage::repositories::helpers::clamp_pagination(self.limit, self.offset)
    }
}

/// Emit a structured audit log event for admin mutations.
/// Persists to the database via fire-and-forget to avoid blocking the response.
fn audit_log(
    repo: &Arc<dyn AuditLogRepository>,
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) {
    let who = principal
        .as_ref()
        .map(|e| e.0.key_prefix.as_str())
        .unwrap_or("anonymous");
    tracing::info!(
        target: "audit",
        principal = %who,
        action = %action,
        resource_type = %resource_type,
        resource_id = %resource_id,
        "admin_audit_event"
    );
    crate::metrics::record_admin_audit(action, resource_type);

    // Read the request-scoped id here: `tokio::spawn` below starts a fresh
    // task that does not inherit task-locals.
    let details = crate::server::request_context::REQUEST_ID
        .try_with(|id| id.clone())
        .ok()
        .filter(|id| !id.is_empty())
        .map(|id| json!({ "request_id": id }).to_string());

    // Fire-and-forget DB persistence — audit logging must never block admin responses
    let repo = repo.clone();
    let who = who.to_string();
    let action = action.to_string();
    let resource_type = resource_type.to_string();
    let resource_id = resource_id.to_string();
    tokio::spawn(async move {
        if let Err(e) = repo
            .insert(
                &who,
                &action,
                &resource_type,
                &resource_id,
                details.as_deref(),
            )
            .await
        {
            tracing::warn!(error = %e, "Failed to persist audit log entry");
        }
    });
}

/// Record an audit-log event for a mutation that intentionally does NOT
/// trigger an engine reload because the target is a draft (drafts are not
/// in the engine). Use at draft create/update/import call sites so the
/// no-reload choice is explicit at the call site rather than implied by
/// the absence of [`audit_and_reload`].
fn audit_log_draft_only(
    repo: &Arc<dyn AuditLogRepository>,
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) {
    audit_log(repo, principal, action, resource_type, resource_id);
}

/// Record an audit-log event and trigger an engine reload. The standard
/// post-mutation sequence for admin operations that change the active set
/// (activate / archive / delete / update-rollout). Drafts do NOT reload —
/// use [`audit_log_draft_only`] in those code paths.
async fn audit_and_reload(
    state: &AppState,
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) -> Result<(), crate::errors::OrionError> {
    audit_log(
        &state.audit_log_repo,
        principal,
        action,
        resource_type,
        resource_id,
    );
    reload_engine(state).await?;
    state.cluster.bump_config_epoch().await
}

/// The admin API, with its own body limit.
///
/// R16: `DefaultBodyLimit::max(ingest.max_payload_size)` — a name that says
/// *data plane* — was a single global layer, so bulk import, connector config
/// PUTs and `POST /workflows/{id}/test` shared a ceiling with anonymous channel
/// traffic. Raising it for a big import raised it for the unauthenticated plane
/// too. Applied here it sits closer to the handler than the global one, so it
/// wins for these routes and nowhere else.
pub fn admin_routes(max_body_size: usize) -> Router<AppState> {
    let channel_routes = Router::new()
        .route(
            "/",
            get(channels::list_channels).post(channels::create_channel),
        )
        .route("/import", post(channels::import_channels))
        .route(
            "/{id}",
            get(channels::get_channel)
                .put(channels::update_channel)
                .delete(channels::delete_channel),
        )
        .route("/{id}/status", patch(channels::change_channel_status))
        .route(
            "/{id}/versions",
            get(channels::list_channel_versions).post(channels::create_new_channel_version),
        );

    let workflow_routes = Router::new()
        .route(
            "/",
            get(workflows::list_workflows).post(workflows::create_workflow),
        )
        .route("/import", post(workflows::import_workflows))
        .route("/export", get(workflows::export_workflows))
        .route("/validate", post(workflows::validate_workflow))
        .route(
            "/{id}",
            get(workflows::get_workflow)
                .put(workflows::update_workflow)
                .delete(workflows::delete_workflow),
        )
        .route("/{id}/status", patch(workflows::change_workflow_status))
        .route(
            "/{id}/versions",
            get(workflows::list_workflow_versions).post(workflows::create_new_workflow_version),
        )
        .route("/{id}/rollout", patch(workflows::update_rollout))
        .route("/{id}/test", post(workflows::test_workflow));

    let connector_routes = Router::new()
        .route(
            "/",
            get(connectors::list_connectors).post(connectors::create_connector),
        )
        .route("/import", post(connectors::import_connectors))
        .route(
            "/{id}",
            get(connectors::get_connector)
                .put(connectors::update_connector)
                .delete(connectors::delete_connector),
        )
        .route("/circuit-breakers", get(connectors::list_circuit_breakers))
        .route(
            "/circuit-breakers/{key}",
            post(connectors::reset_circuit_breaker),
        );

    let engine_routes = Router::new()
        .route("/status", get(engine::engine_status))
        .route("/reload", post(engine::engine_reload));

    let audit_routes = Router::new().route("/", get(audit::list_audit_logs));

    let function_routes = Router::new().route("/", get(functions::list_functions));

    // R8: the trace reads live on the admin plane because that is what they
    // are. `GET /traces` is admin-only, and `GET /traces/{id}` authenticates
    // itself (admin credential, or the per-submission capability token from
    // the async 202) — see `admin_auth::is_guarded_path`, which exempts it
    // from the blanket admin guard for exactly that reason.
    let trace_routes = Router::new()
        .route("/", get(crate::server::routes::data::traces::list_traces))
        .route("/{id}", get(crate::server::routes::data::traces::get_trace));

    let trace_dlq_routes = Router::new()
        .route("/", get(trace_dlq::list_trace_dlq))
        .route("/purge", post(trace_dlq::purge_trace_dlq))
        .route("/{id}", get(trace_dlq::get_trace_dlq_entry))
        .route("/{id}/requeue", post(trace_dlq::requeue_trace_dlq_entry));

    // R27: `/backups` used to be appended after the chain, forcing a `mut`
    // binding for no reason. One chain, one binding.
    let backup_routes =
        Router::new().route("/", post(backups::create_backup).get(backups::list_backups));

    Router::new()
        .nest("/channels", channel_routes)
        .nest("/workflows", workflow_routes)
        .nest("/connectors", connector_routes)
        .nest("/engine", engine_routes)
        .nest("/functions", function_routes)
        .nest("/audit-logs", audit_routes)
        .nest("/traces", trace_routes)
        .nest("/trace-dlq", trace_dlq_routes)
        .nest("/backups", backup_routes)
        .layer(axum::extract::DefaultBodyLimit::max(max_body_size))
}

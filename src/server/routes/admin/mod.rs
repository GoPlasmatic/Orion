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
use serde::Serialize;
use serde_json::json;

use axum::Extension;

use crate::engine::reload_engine;
use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;

/// A status-change request narrowed to the two transitions the API offers,
/// so the handler's `match` is exhaustive over what can actually happen.
///
/// Lives here rather than in `storage::models` (D28): nothing below the route
/// layer has an opinion about which transitions an operator may request —
/// `EntityStatus` is the domain vocabulary, this is the handler's reading of a
/// request body.
#[derive(Debug)]
pub(crate) enum StatusAction {
    Activate,
    Archive,
}

impl StatusAction {
    pub(crate) fn parse(
        status: crate::storage::models::EntityStatus,
    ) -> Result<Self, crate::errors::OrionError> {
        use crate::storage::models::EntityStatus;
        match status {
            EntityStatus::Active => Ok(Self::Activate),
            EntityStatus::Archived => Ok(Self::Archive),
            EntityStatus::Draft => Err(crate::errors::OrionError::validation(
                "Invalid status transition to 'draft'. Use 'active' or 'archived'".to_string(),
            )),
        }
    }
}

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

// ============================================================
// The `/validate` response shape, shared by all three entities
// ============================================================
//
// One definition rather than three: `valid` has to mean the same thing on every
// endpoint, and the fastest way to make it stop meaning that is to let each
// entity own its own copy.

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ValidationIssue {
    pub(crate) field: String,
    pub(crate) message: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ValidationResponse {
    pub(crate) valid: bool,
    pub(crate) errors: Vec<ValidationIssue>,
    pub(crate) warnings: Vec<ValidationIssue>,
}

/// The `{"data": …}` envelope (R17) around a [`ValidationResponse`]. Typed
/// rather than a `json!` literal so the declared `body =` below cannot drift
/// from what the handler actually sends.
impl ValidationEnvelope {
    /// The one place `valid` is derived.
    ///
    /// The type was hoisted here so `valid` means the same thing on every
    /// endpoint; leaving each handler to compute `errors.is_empty()` for itself
    /// left the one field whose meaning must not drift being written in three
    /// places.
    pub(crate) fn new(errors: Vec<ValidationIssue>, warnings: Vec<ValidationIssue>) -> Self {
        Self {
            data: ValidationResponse {
                valid: errors.is_empty(),
                errors,
                warnings,
            },
        }
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ValidationEnvelope {
    pub(crate) data: ValidationResponse,
}

/// Render an `OrionError` from the create path as `/validate` issues, keeping
/// the per-field detail where there is any.
pub(crate) fn issues_from_error(err: OrionError) -> Vec<ValidationIssue> {
    match err {
        OrionError::Validation { details, .. } if !details.is_empty() => details
            .into_iter()
            .map(|d| ValidationIssue {
                field: d.path,
                message: d.message,
            })
            .collect(),
        other => vec![ValidationIssue {
            field: "(root)".to_string(),
            message: other.client_message(),
        }],
    }
}

/// Rows per repository call on an export path (D7). An export still returns
/// everything that matches, but never asks the database for more than this in
/// one query.
pub(crate) const EXPORT_PAGE_SIZE: i64 = 500;

/// Page through a repository until a short page says the table is exhausted.
///
/// The one paging loop behind all three `/export` endpoints. `fetch` is handed
/// `(limit, offset)` because the three repositories take three different filter
/// types and only workflows have a plain `list` — a shared *trait* would have
/// been a larger change than a shared *loop*, and the loop is the part with the
/// invariant worth stating once.
///
/// Not a snapshot: each page is an independent query with no transaction
/// spanning them, so rows mutated concurrently between pages can be skipped or
/// duplicated within a single export response.
///
/// Invariant: `page_size` must lie in `1..=1000` — the repositories clamp the
/// limit they are handed (`clamp_pagination`), so a larger request comes back
/// as at most 1000 rows, the short-page check misreads that as "exhausted", and
/// the export silently truncates. Enforced by the `assert!` below (R29: a
/// `debug_assert!` compiled out of release builds, where the stated
/// consequence — a silently truncated export — is exactly what must not
/// happen; the check is once per export and the panic surfaces as a 500
/// through `CatchPanicLayer` instead of corrupt output).
pub(crate) async fn collect_pages<T, F, Fut>(
    page_size: i64,
    fetch: F,
) -> Result<Vec<T>, crate::errors::OrionError>
where
    F: Fn(i64, i64) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<T>, crate::errors::OrionError>>,
{
    assert!(
        (1..=1000).contains(&page_size),
        "page_size {page_size} is outside the repository clamp (1..=1000); \
         a clamped page would silently truncate the export"
    );
    let mut out = Vec::new();
    let mut offset = 0i64;
    loop {
        let page = fetch(page_size, offset).await?;
        let page_len = page.len() as i64;
        out.extend(page);
        if page_len < page_size {
            return Ok(out);
        }
        offset += page_size;
    }
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

/// The audit actor when no admin credential was presented — which is every
/// request when `admin_auth.enabled = false`.
const ANONYMOUS_PRINCIPAL: &str = "anonymous";

/// The request context recorded alongside every audit row (O7).
///
/// An audit trail whose only fields are *who* and *what* cannot answer the
/// question an investigation actually asks — *from where, and as part of which
/// request*. `client_ip` comes from the same trusted-proxy policy the rate
/// limiter uses, so a caller cannot dictate it with a forged
/// `X-Forwarded-For`; `request_id` ties the row to the access log and to the
/// `error.request_id` the client was handed.
///
/// `None` when the task-local is out of scope (a unit test calling a handler
/// directly), and individual fields are omitted when empty rather than
/// recorded as `""`.
fn request_details() -> Option<String> {
    let ctx = crate::server::request_context::current()?;
    let mut details = serde_json::Map::new();
    if !ctx.request_id.is_empty() {
        details.insert("request_id".into(), json!(ctx.request_id));
    }
    if !ctx.client_ip.is_empty() {
        details.insert("client_ip".into(), json!(ctx.client_ip));
    }
    if let Some(ua) = ctx.user_agent {
        details.insert("user_agent".into(), json!(ua));
    }
    (!details.is_empty()).then(|| serde_json::Value::Object(details).to_string())
}

/// Emit a structured audit log event for admin mutations.
///
/// O7: the row goes onto the bounded, shutdown-drained
/// [`crate::queue::audit_queue`] rather than into a detached `tokio::spawn`,
/// so a mutation accepted moments before SIGTERM is still recorded and a slow
/// database cannot spawn one writer task per admin request.
fn audit_log(
    queue: &crate::queue::audit_queue::AuditQueue,
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) {
    let who = principal
        .as_ref()
        .map(|e| e.0.key_id.as_str())
        .unwrap_or(ANONYMOUS_PRINCIPAL);
    let details = request_details();
    tracing::info!(
        target: "audit",
        principal = %who,
        action = %action,
        resource_type = %resource_type,
        resource_id = %resource_id,
        details = details.as_deref().unwrap_or("{}"),
        "admin_audit_event"
    );
    crate::metrics::record_admin_audit(action, resource_type);

    queue.submit(crate::queue::audit_queue::AuditEvent {
        principal: who.to_string(),
        action: action.to_string(),
        resource_type: resource_type.to_string(),
        resource_id: resource_id.to_string(),
        details,
    });
}

/// Record an audit-log event for a mutation that intentionally does NOT
/// trigger an engine reload because the target is a draft (drafts are not
/// in the engine). Use at draft create/update/import call sites so the
/// no-reload choice is explicit at the call site rather than implied by
/// the absence of [`audit_and_reload`].
fn audit_log_draft_only(
    queue: &crate::queue::audit_queue::AuditQueue,
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) {
    audit_log(queue, principal, action, resource_type, resource_id);
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
        &state.audit_queue,
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
        .route("/export", get(channels::export_channels))
        .route("/validate", post(channels::validate_channel))
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
        .route("/export", get(connectors::export_connectors))
        .route("/validate", post(connectors::validate_connector))
        .route(
            "/{id}",
            get(connectors::get_connector)
                .put(connectors::update_connector)
                .delete(connectors::delete_connector),
        )
        .route("/{id}/test", post(connectors::test_connector))
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

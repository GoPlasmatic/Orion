pub(crate) mod audit;
pub(crate) mod backups;
pub(crate) mod channels;
pub(crate) mod connectors;
pub(crate) mod engine;
pub(crate) mod functions;
pub(crate) mod packages;
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

/// How an `/import` treats an item whose conflict key is already stored (K2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OnConflict {
    /// Refuse the item — a 409-shaped entry in `errors[]`. The pre-K2
    /// behaviour, and still the default.
    #[default]
    Fail,
    /// Leave the stored entity alone and report the item as `skipped`.
    Skip,
    /// Upsert: an existing draft is updated in place; an active entity whose
    /// content differs gets a new draft version carrying the item's content;
    /// content-identical items are reported `unchanged` and write nothing.
    /// This is what makes a re-run of the same artifact a no-op.
    NewVersion,
}

/// What one import item did (or, on dry-run, would do).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImportAction {
    Created,
    /// A versioned entity's existing draft was replaced with this content.
    UpdatedDraft,
    /// A connector (unversioned) was updated in place.
    Updated,
    /// A new draft version was created carrying this content.
    NewVersion,
    /// Stored content is identical (DB-owned fields excluded); nothing written.
    Unchanged,
    /// `on_conflict=skip` and the key exists; nothing written.
    Skipped,
}

impl ImportAction {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::UpdatedDraft => "updated_draft",
            Self::Updated => "updated",
            Self::NewVersion => "new_version",
            Self::Unchanged => "unchanged",
            Self::Skipped => "skipped",
        }
    }

    /// Whether this action wrote (or would write) anything.
    fn is_write(&self) -> bool {
        matches!(
            self,
            Self::Created | Self::UpdatedDraft | Self::Updated | Self::NewVersion
        )
    }
}

/// The per-entity operations [`import_items`] drives.
///
/// A struct rather than positional parameters: they are all closures with
/// interchangeable-looking types, so a transposition at a call site would
/// compile (the F44 hazard, one layer up).
pub(crate) struct ImportOps<V, K, E, C, U> {
    /// The same validation the singular `POST` endpoint runs.
    pub validate: V,
    /// The stored key a duplicate would collide on — `workflow_id`,
    /// `channel_id`, connector `name`. `None` when the item names none, in
    /// which case the store generates one and nothing can conflict.
    pub conflict_key: K,
    /// Whether that key is already taken.
    pub exists: E,
    /// Persist the item as a fresh entity.
    pub create: C,
    /// K2: resolve one item against the store under `on_conflict=new_version`
    /// — create / update draft / new version / unchanged — writing only when
    /// its second argument (`dry_run`) is false. Per-kind because the four
    /// outcomes are made of per-kind repository verbs.
    pub upsert: U,
}

/// Everything one `/import` call produced, dry-run or real.
#[derive(Default)]
pub(crate) struct ImportOutcome {
    /// Items that wrote (or would write): created / updated / new version.
    pub imported: u64,
    pub failed: u64,
    /// Content-identical items (K2) — nothing written, and deliberately not
    /// counted as `imported`: a re-run of the same artifact reports 0 imports.
    pub unchanged: u64,
    /// Items skipped under `on_conflict=skip`.
    pub skipped: u64,
    /// One `{index, error}` entry per failed item.
    pub errors: Vec<serde_json::Value>,
    /// One `{index, id, action}` entry per non-failed item (K2) — the
    /// per-item report a packaging CLI turns into its plan/apply output.
    pub results: Vec<serde_json::Value>,
}

impl ImportOutcome {
    /// The `(id, action)` of every item that wrote, for the per-entity audit
    /// rows (K5). Items with no conflict key have no client-visible id to
    /// audit and are covered by the summary row alone.
    pub(crate) fn written(&self) -> impl Iterator<Item = (&str, &str)> {
        self.results.iter().filter_map(|r| {
            let action = r.get("action")?.as_str()?;
            if !matches!(
                action,
                "created" | "updated" | "updated_draft" | "new_version"
            ) {
                return None;
            }
            Some((r.get("id")?.as_str()?, action))
        })
    }

    fn record(&mut self, index: usize, key: Option<&str>, action: ImportAction) {
        if action.is_write() {
            self.imported += 1;
        } else if action == ImportAction::Unchanged {
            self.unchanged += 1;
        } else {
            self.skipped += 1;
        }
        self.results.push(json!({
            "index": index,
            "id": key,
            "action": action.as_str(),
        }));
    }

    fn fail(&mut self, index: usize, error: String) {
        self.failed += 1;
        self.errors.push(json!({"index": index, "error": error}));
    }
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
/// R15: `?dry_run=true` reads. It used to skip the database entirely, as
/// its own doc comment said — but the stated use case is CI pre-flight, and the
/// most common real failure is a **name conflict**, which is exactly what a
/// no-DB dry-run cannot see. A green dry-run therefore said nothing. Conflicts
/// against stored rows and duplicates *within the batch* are both reported now;
/// the second was free and previously missed entirely.
///
/// K2: `on_conflict` selects what an already-stored key means — `fail` (the
/// default), `skip`, or `new_version` (upsert via `ops.upsert`). In the two
/// non-default modes an in-batch duplicate key is refused in *both* dry-run
/// and real runs: the second item would silently rewrite what the first just
/// staged, which is never what a batch author meant. Dry-run reports the
/// action each item *would* take, which is half of a promotion plan.
pub(crate) async fn import_items<T, V, K, E, EFut, C, CFut, U, UFut>(
    items: Vec<serde_json::Value>,
    dry_run: bool,
    on_conflict: OnConflict,
    ops: ImportOps<V, K, E, C, U>,
) -> ImportOutcome
where
    T: serde::de::DeserializeOwned,
    V: Fn(&T) -> Result<(), crate::errors::OrionError>,
    K: Fn(&T) -> Option<String>,
    E: Fn(String) -> EFut,
    EFut: std::future::Future<Output = Result<bool, crate::errors::OrionError>>,
    C: Fn(T) -> CFut,
    CFut: std::future::Future<Output = Result<(), crate::errors::OrionError>>,
    U: Fn(T, bool) -> UFut,
    UFut: std::future::Future<Output = Result<ImportAction, crate::errors::OrionError>>,
{
    let mut out = ImportOutcome::default();
    let mut seen: Vec<String> = Vec::new();

    for (i, item) in items.into_iter().enumerate() {
        // Deserialize per item, so a single shape or enum typo is one failed
        // entry rather than a 400 for the whole batch.
        let parsed: T = match serde_json::from_value(item) {
            Ok(v) => v,
            Err(e) => {
                out.fail(i, e.to_string());
                continue;
            }
        };
        if let Err(e) = (ops.validate)(&parsed) {
            out.fail(i, e.client_message());
            continue;
        }

        let key = (ops.conflict_key)(&parsed);

        // In-batch duplicates are refused in every mode that would otherwise
        // resolve them silently: under `fail` the store answers anyway (a
        // real run 409s the second item), and under `skip`/`new_version` the
        // second item would overwrite what the first staged moments ago.
        if let Some(ref key) = key {
            if seen.contains(key) {
                out.fail(
                    i,
                    format!(
                        "'{key}' appears more than once in this batch — the second \
                         item would conflict with the first"
                    ),
                );
                continue;
            }
            if dry_run || on_conflict != OnConflict::Fail {
                seen.push(key.clone());
            }
        }

        match on_conflict {
            OnConflict::Fail => {
                // Conflict detection. On a real import the store enforces
                // this anyway, so the probe runs on dry-run only — where it
                // is the whole point.
                if dry_run {
                    if let Some(ref key) = key {
                        match (ops.exists)(key.clone()).await {
                            Ok(true) => {
                                out.fail(i, format!("'{key}' already exists"));
                                continue;
                            }
                            Ok(false) => {}
                            // A probe that could not run must not be reported
                            // as a clean item: say so and let the operator
                            // retry.
                            Err(e) => {
                                out.fail(
                                    i,
                                    format!(
                                        "could not check for a conflict: {}",
                                        e.client_message()
                                    ),
                                );
                                continue;
                            }
                        }
                    }
                    out.record(i, key.as_deref(), ImportAction::Created);
                    continue;
                }
                match (ops.create)(parsed).await {
                    Ok(()) => out.record(i, key.as_deref(), ImportAction::Created),
                    Err(e) => out.fail(i, e.client_message()),
                }
            }
            OnConflict::Skip => {
                if let Some(ref key) = key {
                    match (ops.exists)(key.clone()).await {
                        Ok(true) => {
                            out.record(i, Some(key), ImportAction::Skipped);
                            continue;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            out.fail(
                                i,
                                format!("could not check for a conflict: {}", e.client_message()),
                            );
                            continue;
                        }
                    }
                }
                if dry_run {
                    out.record(i, key.as_deref(), ImportAction::Created);
                    continue;
                }
                match (ops.create)(parsed).await {
                    Ok(()) => out.record(i, key.as_deref(), ImportAction::Created),
                    Err(e) => out.fail(i, e.client_message()),
                }
            }
            OnConflict::NewVersion => match (ops.upsert)(parsed, dry_run).await {
                Ok(action) => out.record(i, key.as_deref(), action),
                Err(e) => out.fail(i, e.client_message()),
            },
        }
    }
    out
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

/// The response envelope shared by all three import endpoints, dry-run and
/// real (R18): the same fields either way, distinguished only by `dry_run`.
/// Pre-1.0 the dry-run shape returned six fields for two facts —
/// `would_create`/`would_fail` alongside a hardcoded `imported: 0` and a
/// `failed` that always equalled `would_fail`.
pub(crate) fn import_response(
    dry_run: bool,
    outcome: ImportOutcome,
) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "data": {
            "dry_run": dry_run,
            "imported": outcome.imported,
            "failed": outcome.failed,
            "unchanged": outcome.unchanged,
            "skipped": outcome.skipped,
            "errors": outcome.errors,
            "results": outcome.results,
        }
    }))
}

/// Query parameters accepted by all three `/import` endpoints (B6).
///
/// R27: lived in `workflows.rs` while its sibling helpers
/// (`check_import_batch_size`, `import_items`, `import_response`) lived here,
/// so channels and connectors imported it from a module they otherwise have
/// nothing to do with.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ImportQuery {
    /// When true, validate each item and report what would happen without
    /// writing. Probes for conflicts against stored rows and for duplicates
    /// within the batch (R15), and under `on_conflict=new_version` reports
    /// the per-item action the real import would take (K2).
    #[serde(default)]
    pub dry_run: bool,
    /// What an already-stored conflict key means: `fail` (default — the item
    /// is refused), `skip`, or `new_version` (upsert: update the draft in
    /// place, or cut a new draft version over an active entity; identical
    /// content is a no-op). K2.
    #[serde(default)]
    pub on_conflict: OnConflict,
}

/// When an active-set mutation rebuilds the engine (K4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReloadMode {
    /// Rebuild the engine and bump the cluster config epoch as part of this
    /// request — the default, and the pre-K4 behaviour.
    #[default]
    Now,
    /// Commit the row but leave the running engine (and, in cluster mode,
    /// every peer) serving the previous configuration until someone calls
    /// `POST /api/v1/admin/engine/reload`. For a bundle apply this turns
    /// O(entities) engine rebuilds and epoch bumps into exactly one — the
    /// caller activates everything with `reload=defer` and finishes with one
    /// explicit reload, which also bumps the epoch for the peers.
    Defer,
}

/// Query parameters accepted by the two `PATCH /{id}/status` endpoints
/// (K3, K4).
///
/// `?dry_run=true` runs every activation (or archive) gate — the same checks
/// the real transition runs, in the same order — reports the findings as a
/// [`ValidationEnvelope`], and writes nothing. This is the server-state half
/// of a promotion plan: route collisions and the active set cannot be
/// checked client-side, and without a pre-flight a "plan" cannot promise the
/// matching "apply" will activate.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct StatusChangeQuery {
    /// When true, run the transition's gates and report findings without
    /// writing. The response body is the `/validate` envelope, not the
    /// entity.
    #[serde(default)]
    pub dry_run: bool,
    /// `now` (default) reloads the engine as part of this request; `defer`
    /// commits the row and leaves the reload to a later
    /// `POST /engine/reload` (K4).
    #[serde(default)]
    pub reload: ReloadMode,
}

/// Query parameter accepted by `PATCH /workflows/{id}/rollout` (K4) — the
/// other active-set mutation a bundle apply performs per entity.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ReloadQuery {
    /// `now` (default) reloads the engine as part of this request; `defer`
    /// commits the row and leaves the reload to a later
    /// `POST /engine/reload` (K4).
    #[serde(default)]
    pub reload: ReloadMode,
}

/// The audit actor when no admin credential was presented — which is every
/// request when `admin_auth.enabled = false`. `pub(crate)` because the
/// package-receipt PUT records the same principal *in the row*, not only on
/// the audit trail, and the two spellings must not drift.
pub(crate) const ANONYMOUS_PRINCIPAL: &str = "anonymous";

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
    // K5: what this mutation was part of, per the caller's own labelling —
    // the packaging CLI sends `package=<name>@<version>` on every call of an
    // apply, so the trail groups a multi-request operation without guesswork.
    if let Some(cc) = ctx.change_context {
        details.insert("change_context".into(), json!(cc));
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
///
/// K4: `reload` is [`ReloadMode::Defer`] only where the caller opted in via
/// query parameter (status changes, rollout); the row is committed and the
/// audit event recorded, but the engine keeps serving the previous active set
/// — on this node *and* every peer, since the epoch bump is deferred with the
/// rebuild — until `POST /engine/reload` runs. Deletes always reload: nothing
/// batches a delete.
async fn audit_and_reload(
    state: &AppState,
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    reload: ReloadMode,
) -> Result<(), crate::errors::OrionError> {
    audit_log(
        &state.audit_queue,
        principal,
        action,
        resource_type,
        resource_id,
    );
    if reload == ReloadMode::Defer {
        return Ok(());
    }
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
        .route("/{id}/dependencies", get(workflows::workflow_dependencies))
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

    // K14: package receipts — read receipts, and the PUT the packaging CLI
    // claims/flips around an apply.
    let package_routes = Router::new()
        .route("/", get(packages::list_packages))
        .route(
            "/{name}",
            get(packages::get_package).put(packages::put_package),
        );

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
        .nest("/packages", package_routes)
        .layer(axum::extract::DefaultBodyLimit::max(max_body_size))
}

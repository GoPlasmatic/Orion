pub(crate) mod audit;
pub(crate) mod backups;
pub(crate) mod channels;
pub(crate) mod connectors;
pub(crate) mod engine;
pub(crate) mod functions;
pub(crate) mod packages;
pub(crate) mod plugins;
pub(crate) mod services;
pub(crate) mod trace_dlq;
pub(crate) mod workflows;

use axum::Router;
use axum::routing::{get, patch, post};
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use axum::Extension;

use crate::errors::OrionError;
use crate::runtime::reload_engine;
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

// The import vocabulary — `OnConflict` (the `?on_conflict=` values) and
// `ImportAction` (the `results[].action` values) — is wire contract and lives
// in the shared `orion-api` crate; re-exported here under the pre-1.0 paths.
pub(crate) use orion_api::{ImportAction, ImportItemError, ImportItemResult, OnConflict};

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
    pub errors: Vec<ImportItemError>,
    /// One `{index, id, action}` entry per non-failed item (K2) — the
    /// per-item report a packaging CLI turns into its plan/apply output.
    pub results: Vec<ImportItemResult>,
    /// Ids of the items that wrote, collected as `record` classifies them —
    /// so the K5 audit filter cannot drift from [`ImportAction::is_write`]
    /// the way a re-parse of `results` strings could.
    written: Vec<String>,
}

impl ImportOutcome {
    /// The id of every written item, for the per-entity audit rows (K5).
    /// Items with no conflict key have no client-visible id to audit and are
    /// covered by the summary row alone.
    pub(crate) fn written(&self) -> impl Iterator<Item = &str> {
        self.written.iter().map(String::as_str)
    }

    fn record(&mut self, index: usize, key: Option<&str>, action: ImportAction) {
        if action.is_write() {
            self.imported += 1;
            if let Some(key) = key {
                self.written.push(key.to_string());
            }
        } else if action == ImportAction::Unchanged {
            self.unchanged += 1;
        } else {
            self.skipped += 1;
        }
        self.results.push(ImportItemResult {
            index: index as u64,
            id: key.map(str::to_string),
            action: action.as_str().to_string(),
        });
    }

    fn fail(&mut self, index: usize, error: String) {
        self.failed += 1;
        self.errors.push(ImportItemError {
            index: index as u64,
            error,
        });
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
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

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
                seen.insert(key.clone());
            }
        }

        if on_conflict == OnConflict::NewVersion {
            match (ops.upsert)(parsed, dry_run).await {
                Ok(action) => out.record(i, key.as_deref(), action),
                Err(e) => out.fail(i, e.client_message()),
            }
            continue;
        }

        // Fail and Skip differ only in what a stored key means; the probe
        // runs whenever that answer is needed — always under `skip`, and on
        // dry-run under `fail` (a real `fail` run lets the store's own
        // constraint answer, which is R15's whole point in reverse).
        if let Some(ref key) = key
            && (dry_run || on_conflict == OnConflict::Skip)
        {
            match (ops.exists)(key.clone()).await {
                Ok(true) => {
                    if on_conflict == OnConflict::Skip {
                        out.record(i, Some(key), ImportAction::Skipped);
                    } else {
                        out.fail(i, format!("'{key}' already exists"));
                    }
                    continue;
                }
                Ok(false) => {}
                // A probe that could not run must not be reported as a clean
                // item: say so and let the operator retry.
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
        } else {
            match (ops.create)(parsed).await {
                Ok(()) => out.record(i, key.as_deref(), ImportAction::Created),
                Err(e) => out.fail(i, e.client_message()),
            }
        }
    }
    out
}

/// K2: what `on_conflict=new_version` does with an item whose id is already
/// stored, given the latest version's status and whether the stored content
/// equals the item's. One definition for both versioned kinds, because it
/// carries the one non-obvious invariant: an **archived** entity with
/// identical content still gets a new draft version — the point of
/// re-importing an archived entity is to activate it again, and activation
/// needs a draft.
pub(crate) fn versioned_upsert_action(status: &str, identical: bool) -> ImportAction {
    use crate::storage::models::EntityStatus;
    if status == EntityStatus::Draft.as_str() {
        if identical {
            ImportAction::Unchanged
        } else {
            ImportAction::UpdatedDraft
        }
    } else if identical && status == EntityStatus::Active.as_str() {
        ImportAction::Unchanged
    } else {
        ImportAction::NewVersion
    }
}

/// What [`versioned_upsert`] needs of a versioned entity.
///
/// The import upsert was written out once per kind — `upsert_channel` and
/// `upsert_workflow` were the same forty lines modulo the repository type, the
/// id field's name, and which pair of content-hash functions to call. The
/// sequence they share is not incidental: *no id means create; an id nobody
/// has seen means create; otherwise compare content, ask
/// [`versioned_upsert_action`] what that means, and apply it — or, on a dry
/// run, apply nothing and report what would have happened.* Two copies of that
/// is two places for the dry-run branch to stop matching the real one.
///
/// A small adapter per entity rather than a change to the repository traits:
/// the traits are the storage contract and four other callers depend on their
/// shapes, while this is one route-layer sequence that happens to need four of
/// their methods.
pub(crate) trait VersionedUpsert {
    /// The stored row type, as `get_by_id` returns it.
    type Row;
    /// The create/replace request type.
    type Request;

    /// The entity id the request names, or `None` when it names none — the
    /// store generates one and there is nothing to conflict with.
    fn request_id(req: &Self::Request) -> Option<String>;
    /// The latest stored version's status, as `EntityStatus::as_str` spells it.
    fn row_status(row: &Self::Row) -> &str;
    /// Whether the stored row and the request carry the same content, by the
    /// same content hash the import receipt reports.
    fn content_matches(row: &Self::Row, req: &Self::Request) -> Result<bool, OrionError>;

    async fn create(&self, req: &Self::Request) -> Result<(), OrionError>;
    async fn get_by_id(&self, id: &str) -> Result<Self::Row, OrionError>;
    async fn create_new_version(&self, id: &str) -> Result<(), OrionError>;
    async fn replace_draft(&self, id: &str, req: &Self::Request) -> Result<(), OrionError>;
}

/// Import one versioned entity under `on_conflict=new_version`, or report what
/// that would do when `dry_run`.
///
/// The dry-run branch is the reason this is one function: it must decide
/// *exactly* what the real path decides and then do none of it, and the only
/// way to be sure of that is for both to be the same code.
pub(crate) async fn versioned_upsert<T: VersionedUpsert>(
    entity: &T,
    req: T::Request,
    dry_run: bool,
) -> Result<ImportAction, OrionError> {
    let Some(id) = T::request_id(&req) else {
        // No id → the store generates one; nothing to conflict with.
        if !dry_run {
            entity.create(&req).await?;
        }
        return Ok(ImportAction::Created);
    };

    let latest = match entity.get_by_id(&id).await {
        Ok(latest) => latest,
        Err(OrionError::NotFound(_)) => {
            if !dry_run {
                entity.create(&req).await?;
            }
            return Ok(ImportAction::Created);
        }
        Err(e) => return Err(e),
    };

    let action =
        versioned_upsert_action(T::row_status(&latest), T::content_matches(&latest, &req)?);
    if !dry_run {
        match action {
            ImportAction::UpdatedDraft => {
                entity.replace_draft(&id, &req).await?;
            }
            ImportAction::NewVersion => {
                entity.create_new_version(&id).await?;
                entity.replace_draft(&id, &req).await?;
            }
            _ => {}
        }
    }
    Ok(action)
}

/// What a versioned entity's status transition needs, per kind.
///
/// The gates themselves live in [`services`] and are unchanged; what this adds
/// is that the *list* of them is written once. It used to be written twice per
/// entity — once in `change_*_status` and once in `dry_run_status_change` —
/// with the second copy's doc comment asserting that it called "the same
/// functions the un-dry-run path calls". That was true, by inspection, in two
/// places, which is the arrangement whose failure is silent: a gate added to
/// the real path and not to the dry run makes `valid: true` mean less than
/// "the real request would succeed", and nothing says so.
///
/// A route-layer adapter per entity rather than a change to the repository
/// traits, for the same reason [`VersionedUpsert`] is one: this is a sequence
/// two handlers share, not a storage contract four other callers depend on.
pub(crate) trait VersionedLifecycle {
    /// The stored row, as `get_by_id` returns it.
    type Row;

    /// The entity noun used in messages: `"channel"`, `"workflow"`.
    const NOUN: &'static str;

    /// The latest stored version's status, as `EntityStatus::as_str` spells it.
    fn row_status(row: &Self::Row) -> &str;

    async fn get_by_id(&self, id: &str) -> Result<Self::Row, OrionError>;

    /// Whether any version of this entity is currently active.
    async fn has_active(&self, id: &str) -> Result<bool, OrionError>;

    /// Every gate activation runs, **all** of them evaluated.
    ///
    /// Returning a list rather than short-circuiting is what lets one
    /// implementation serve both callers: a plan wants every problem at once,
    /// and the real path wants the first — which it can take from the same
    /// list. The alternative, a `Result` the dry run calls repeatedly, is two
    /// orders again.
    async fn activation_gates(&self, row: &Self::Row) -> Vec<OrionError>;
}

/// The real path: run the activation gates and fail on the first refusal.
pub(crate) async fn check_activation<T: VersionedLifecycle>(
    entity: &T,
    row: &T::Row,
) -> Result<(), OrionError> {
    match entity.activation_gates(row).await.into_iter().next() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// K3: every gate the real transition runs, as findings instead of failures.
///
/// The rule that keeps this honest is the `/validate` rule (R20) one endpoint
/// over: the checks are the *same functions* the real path calls, so
/// `valid: true` cannot come to mean something weaker than "the un-dry-run
/// request would succeed". Findings the real path reports as a 4xx arrive here
/// as `errors` entries in a 200 — a plan wants the report, not the failure —
/// including "not found", so a CLI can pre-flight a whole package without
/// tripping over the first missing entity.
/// Returns the findings rather than a [`ValidationEnvelope`] because `valid` is
/// derived once, at construction, from the final lists — so a caller with
/// findings of its own (a workflow's rollout arithmetic) must contribute them
/// before the envelope exists, not push them into one afterwards.
pub(crate) async fn status_change_findings<T: VersionedLifecycle>(
    entity: &T,
    id: &str,
    action: &StatusAction,
) -> Result<Vec<ValidationIssue>, OrionError> {
    let noun = T::NOUN;
    let mut errors = Vec::new();

    match action {
        StatusAction::Activate => match entity.get_by_id(id).await {
            Ok(latest) => {
                if T::row_status(&latest) != crate::storage::models::EntityStatus::Draft.as_str() {
                    errors.push(ValidationIssue {
                        field: "status".to_string(),
                        message: format!(
                            "No draft version found for {noun} '{id}' — create a new \
                             version first"
                        ),
                    });
                } else {
                    for e in entity.activation_gates(&latest).await {
                        errors.extend(issues_from_error(e));
                    }
                }
            }
            Err(OrionError::NotFound(_)) => errors.push(ValidationIssue {
                field: "(root)".to_string(),
                message: format!(
                    "{}{} '{id}' not found",
                    noun[..1].to_uppercase(),
                    &noun[1..]
                ),
            }),
            Err(e) => return Err(e),
        },
        StatusAction::Archive => {
            if !entity.has_active(id).await? {
                errors.push(ValidationIssue {
                    field: "status".to_string(),
                    message: format!("No active version found for {noun} '{id}'"),
                });
            }
        }
    }

    Ok(errors)
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
    // Built as the shared `ImportResult` — the type the CLI deserializes and
    // the OpenAPI document publishes — so the three cannot drift.
    let report = orion_api::ImportResult {
        dry_run,
        imported: outcome.imported,
        failed: outcome.failed,
        unchanged: outcome.unchanged,
        skipped: outcome.skipped,
        errors: outcome.errors,
        results: outcome.results,
    };
    axum::Json(json!({ "data": report }))
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
    let ctx = crate::request_context::current()?;
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
    queue.submit(audit_event(principal, action, resource_type, resource_id));
}

/// Build one audit event, and emit the parts of the trail that are not the
/// database row: the `audit`-target log line and the Prometheus counter.
///
/// Split out of [`audit_log`] because the row now has two sinks (§2.6). A
/// mutation that touches the active set carries its event into the
/// transaction that makes the change ([`audited_write`]); everything else —
/// an event with no entity write to join — goes on the queue. Both need the
/// log line and the counter, and neither should be the place they are
/// written.
fn audit_event(
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) -> crate::queue::audit_queue::AuditEvent {
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

    crate::queue::audit_queue::AuditEvent {
        principal: who.to_string(),
        action: action.to_string(),
        resource_type: resource_type.to_string(),
        resource_id: resource_id.to_string(),
        details,
    }
}

/// Begin an active-set mutation whose audit row commits with it (§2.6).
///
/// The route runs its write against `guard.tx()` and finishes with
/// `guard.commit()`; the audit row is written by that commit and nowhere else,
/// so there is no ordering for a caller to get wrong and no window in which
/// the change is live and unrecorded. Dropping the guard on any `?` rolls the
/// write back.
///
/// The reload stays *outside* — see [`reload_after_commit`].
async fn audited_write<'a>(
    state: &'a AppState,
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) -> Result<crate::storage::repositories::AuditedWrite<'a>, crate::errors::OrionError> {
    state
        .repos
        .audited(audit_event(principal, action, resource_type, resource_id))
        .await
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

/// Queue an audit-log event and trigger an engine reload.
///
/// **For events with no entity write to join.** Since §2.6 every mutation that
/// puts a change live — activate, archive, delete, update-rollout, and all
/// three connector writes — carries its audit row into the transaction that
/// makes the change ([`audited_write`]) and then calls
/// [`reload_after_commit`], because an audit row written after its mutation
/// has committed can be lost for a change that is already live. What is left
/// here is `POST /engine/reload`, which republishes the engine without writing
/// a row of its own: there is no transaction to join, and nothing for a lost
/// audit row to misrepresent. Drafts do NOT reload — use
/// [`audit_log_draft_only`] in those code paths.
///
/// K4: `reload` is [`ReloadMode::Defer`] only where the caller opted in via
/// query parameter (status changes, rollout); the row is committed and the
/// audit event recorded, but the engine keeps serving the previous active set
/// — on this node *and* every peer, since the epoch bump is deferred with the
/// rebuild — until `POST /engine/reload` runs. Deletes always reload: nothing
/// batches a delete.
///
/// **A failed reload IS returned to the caller here** — the opposite of
/// [`reload_after_commit`], and the reason the two are separate functions.
///
/// That one serves the mutation routes, where the row has already committed by
/// the time the reload runs: answering `5xx` would tell the client its change
/// failed when it did not, and the natural response — retry — writes a second
/// version or collides with the first. So there the failure is reported where
/// it is actionable, on `/health`. This function's only caller is
/// `POST /engine/reload`, which has no committed write for an error to
/// misdescribe: a caller who *asked* for a reload is told when it did not
/// happen. Swallowing it there tells a deploy pipeline gating on this route
/// that a rollout landed when the node is still serving the previous
/// generation. The `/health` degradation is raised as well — the two are not
/// alternatives.
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
    // `?`, where `reload_after_commit` deliberately discards: see the note
    // above. A manual reload that failed must not answer `200`.
    reload_engine(state).await?;
    state
        .cluster
        .bump_config_epoch(crate::cluster::EpochScope::Definitions)
        .await;
    Ok(())
}

/// The half of [`audit_and_reload`] that runs *after* the row is committed:
/// republish the engine, then tell the cluster.
///
/// Separate from the audit half because since §2.6 an active-set mutation
/// writes its audit row inside its own transaction, and this is what is left
/// to do once that transaction has committed. It must stay outside the
/// transaction in both directions: a reload reads the rows this change wrote,
/// so it cannot run before the commit, and it is not a database write, so
/// rolling back could not undo it.
///
/// Every caveat in [`audit_and_reload`]'s doc about a failed reload applies
/// here unchanged — it is the code that does the ignoring.
async fn reload_after_commit(
    state: &AppState,
    reload: ReloadMode,
) -> Result<(), crate::errors::OrionError> {
    // `Definitions` scope: a channel or workflow row moved, which changes the
    // engine and the channel estate and nothing else. A peer answering this
    // reloads those and leaves its connector pools alone — before the scope
    // existed, every activation dropped every pooled connection on every node.
    reload_after_commit_scoped(state, reload, crate::cluster::EpochScope::Definitions).await
}

/// [`reload_after_commit`] with the scope the mutation names — `Plugins`
/// for the plugin routes, whose peers must compare the active plugin set
/// rather than only republish definitions.
async fn reload_after_commit_scoped(
    state: &AppState,
    reload: ReloadMode,
    scope: crate::cluster::EpochScope,
) -> Result<(), crate::errors::OrionError> {
    if reload == ReloadMode::Defer {
        return Ok(());
    }
    // Not `?`: see the note above. The degradation is on `/health`.
    let _ = reload_engine(state).await;
    state.cluster.bump_config_epoch(scope).await;
    Ok(())
}

/// The admin API, with its own body limit.
///
/// R16: `DefaultBodyLimit::max(ingest.max_payload_size)` — a name that says
/// *data plane* — was a single global layer, so bulk import, connector config
/// PUTs and `POST /workflows/{id}/test` shared a ceiling with anonymous channel
/// traffic. Raising it for a big import raised it for the unauthenticated plane
/// too. Applied here it sits closer to the handler than the global one, so it
/// wins for these routes and nowhere else.
pub fn admin_routes(max_body_size: usize, plugin_body_size: usize) -> Router<AppState> {
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

    // A component is up to `plugins.max_component_bytes`, and base64 is
    // four thirds of that; the admin plane's own limit is sized for JSON
    // definitions. Applied on this router alone so nothing else grows.
    let plugin_routes = Router::new()
        .route("/", get(plugins::list_plugins).post(plugins::create_plugin))
        .route("/import", post(plugins::import_plugins))
        .route("/export", get(plugins::export_plugins))
        .route("/validate", post(plugins::validate_plugin))
        .route(
            "/{id}",
            get(plugins::get_plugin)
                .put(plugins::update_plugin)
                .delete(plugins::delete_plugin),
        )
        .route("/{id}/status", patch(plugins::change_plugin_status))
        .route("/{id}/dependencies", get(plugins::plugin_dependencies))
        .route(
            "/{id}/versions",
            get(plugins::list_plugin_versions).post(plugins::create_new_plugin_version),
        )
        .layer(axum::extract::DefaultBodyLimit::max(plugin_body_size));

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
        .nest("/plugins", plugin_routes)
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

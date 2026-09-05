//! The plugin repository: the versioned entity and its content-addressed
//! artifacts.
//!
//! A plugin follows the workflow lifecycle exactly, through the same shared
//! machinery in `super::versioned`: integer versions, one draft per id,
//! active rows immutable, `draft → active → archived`. Exactly one version
//! is active at a time — activating a draft archives the previously active
//! version in the same transaction, so a function name resolves to one
//! digest per generation.
//!
//! The bytes are separate. `plugin_artifacts` is keyed by the digest the
//! server computed on upload, inserted once and shared by every version that
//! names it, and swept when nothing names it any more. A row here never
//! carries bytes: `list_active` is what every reload reads, and the loader
//! fetches an artifact only for a digest it has not compiled yet.

use async_trait::async_trait;
use sea_query::{Asterisk, Condition, Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};

use crate::errors::OrionError;
use crate::storage::models::{EntityStatus, Plugin, PluginArtifact};
use crate::storage::schema::{PluginArtifacts, Plugins};
use crate::storage::{DbPool, DbTransaction, build_sqlx};

pub use super::helpers::PaginatedResult;
use super::helpers::{
    Page, Projection, VersionFilter, WriteStatement, clamp_pagination, map_duplicate, paginate,
    parse_sort_order,
};
use super::versioned::{self, VersionedSpec};

fn spec() -> VersionedSpec {
    use sea_query::IntoIden;
    VersionedSpec {
        table: Plugins::Table.into_iden(),
        id_col: Plugins::PluginId.into_iden(),
        version_col: Plugins::Version.into_iden(),
        status_col: Plugins::Status.into_iden(),
        priority_col: None,
        updated_at_col: Plugins::UpdatedAt.into_iden(),
        label: "Plugin",
        noun: "plugin",
    }
}

impl versioned::HasVersion for Plugin {
    fn version(&self) -> i64 {
        self.version
    }
}

// -- DTOs --

/// What `POST /plugins` and the import accept.
///
/// `manifest` is either the TOML text (a string) or the manifest as a JSON
/// object — the CLI sends the file it read, an export round-trips the
/// object. `component` is the component bytes as base64; it may be omitted
/// when `digest` names an artifact this instance already holds, which is
/// what an export without `?include_artifacts=true` produces.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreatePluginRequest {
    /// Must equal the manifest's `name` when given; the manifest is the
    /// source of truth for the id.
    pub plugin_id: Option<String>,
    pub manifest: serde_json::Value,
    /// The component, base64-encoded.
    pub component: Option<String>,
    /// `sha256:…` of a component already stored, when `component` is absent.
    pub digest: Option<String>,
    /// A detached Ed25519 signature over the digest string, base64. Required
    /// when the node's `[plugins.trust]` names public keys; ignored, but
    /// stored, when it does not.
    pub signature: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `PUT /plugins/{id}`: every field optional, absent means keep.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdatePluginRequest {
    pub manifest: Option<serde_json::Value>,
    pub component: Option<String>,
    pub digest: Option<String>,
    pub signature: Option<String>,
    pub tags: Option<Vec<String>>,
}

/// A create request resolved by the route — manifest validated, bytes
/// decoded, compiled and self-tested, digest computed — into the columns a
/// row stores. The repository never sees the wire shape.
#[derive(Debug, Clone)]
pub struct PluginDraft {
    pub plugin_id: String,
    pub manifest_json: String,
    pub digest: String,
    pub tags_json: String,
    /// The detached signature over `digest`, already verified by the route
    /// when `[plugins.trust]` names keys; stored so every load re-verifies.
    pub signature: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PluginFilter {
    pub status: Option<String>,
    pub tag: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Column to sort by: plugin_id (default), status, created_at, updated_at.
    pub sort_by: Option<String>,
    /// Sort direction: asc (default) or desc.
    pub sort_order: Option<String>,
    /// Export only: carry each component as base64 under `component`.
    pub include_artifacts: Option<bool>,
}

// -- Repository trait --

#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Create a plugin as draft v1. `artifact` is stored under the draft's
    /// digest when given and not already present; the row and the artifact
    /// commit together.
    async fn create(
        &self,
        draft: &PluginDraft,
        artifact: Option<&[u8]>,
    ) -> Result<Plugin, OrionError>;
    /// The latest version of a plugin.
    async fn get_by_id(&self, plugin_id: &str) -> Result<Plugin, OrionError>;
    async fn list_paginated(
        &self,
        filter: &PluginFilter,
    ) -> Result<PaginatedResult<Plugin>, OrionError>;
    /// Every current plugin matching `filter`, as one consistent snapshot —
    /// the export contract.
    async fn snapshot(&self, filter: &PluginFilter) -> Result<Vec<Plugin>, OrionError>;
    /// Replace the draft's content. Errors if no draft exists.
    async fn replace_draft(
        &self,
        plugin_id: &str,
        draft: &PluginDraft,
        artifact: Option<&[u8]>,
    ) -> Result<Plugin, OrionError>;
    /// Delete all versions of a plugin, then any artifact nothing names.
    async fn delete(&self, plugin_id: &str) -> Result<(), OrionError>;
    async fn delete_tx(&self, tx: &mut DbTransaction, plugin_id: &str) -> Result<(), OrionError>;
    /// Every active version, by id — what a reload loads.
    async fn list_active(&self) -> Result<Vec<Plugin>, OrionError>;
    /// Activate the draft, archiving the previously active version in the
    /// same transaction.
    async fn activate(&self, plugin_id: &str) -> Result<Plugin, OrionError>;
    async fn activate_tx(
        &self,
        tx: &mut DbTransaction,
        plugin_id: &str,
    ) -> Result<Plugin, OrionError>;
    async fn archive(&self, plugin_id: &str) -> Result<Plugin, OrionError>;
    async fn archive_tx(
        &self,
        tx: &mut DbTransaction,
        plugin_id: &str,
    ) -> Result<Plugin, OrionError>;
    /// A new draft copied from the latest version.
    async fn create_new_version(&self, plugin_id: &str) -> Result<Plugin, OrionError>;
    async fn list_versions(
        &self,
        plugin_id: &str,
        filter: &VersionFilter,
    ) -> Result<PaginatedResult<Plugin>, OrionError>;
    /// The component bytes under `digest`, if stored.
    async fn get_artifact(&self, digest: &str) -> Result<Option<Vec<u8>>, OrionError>;
    async fn artifact_exists(&self, digest: &str) -> Result<bool, OrionError>;
}

// -- SQL implementation --

pub struct SqlPluginRepository {
    pool: DbPool,
}

impl SqlPluginRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

fn build_insert(draft: &PluginDraft, version: i64) -> sea_query::InsertStatement {
    let mut q = Query::insert();
    q.into_table(Plugins::Table)
        .columns([
            Plugins::PluginId,
            Plugins::Version,
            Plugins::Status,
            Plugins::Digest,
            Plugins::ManifestJson,
            Plugins::TagsJson,
            Plugins::Signature,
        ])
        .values_panic([
            Expr::val(draft.plugin_id.as_str()),
            Expr::val(version),
            Expr::val(EntityStatus::Draft.as_str()),
            Expr::val(draft.digest.as_str()),
            Expr::val(draft.manifest_json.as_str()),
            Expr::val(draft.tags_json.as_str()),
            Expr::val(draft.signature.clone()),
        ]);
    q
}

/// The UPDATE that rewrites a draft's content. `status = 'draft'` is part of
/// the statement, so it cannot rewrite a version promoted between the check
/// and the write.
fn build_update(draft: &PluginDraft) -> sea_query::UpdateStatement {
    Query::update()
        .table(Plugins::Table)
        .value(Plugins::Digest, draft.digest.as_str())
        .value(Plugins::ManifestJson, draft.manifest_json.as_str())
        .value(Plugins::TagsJson, draft.tags_json.as_str())
        .value(Plugins::Signature, draft.signature.clone())
        .and_where(Expr::col(Plugins::PluginId).eq(draft.plugin_id.as_str()))
        .and_where(Expr::col(Plugins::Status).eq(EntityStatus::Draft.as_str()))
        .to_owned()
}

fn build_condition(filter: &PluginFilter) -> Condition {
    let mut cond = Condition::all();
    if let Some(ref status) = filter.status {
        cond = cond.add(Expr::col(Plugins::Status).eq(status.as_str()));
    }
    if let Some(ref tag) = filter.tag {
        cond = cond
            .add(Expr::col(Plugins::TagsJson).like(super::helpers::tag_like_pattern(tag.as_str())));
    }
    cond
}

/// Store `bytes` under `digest` unless a row already holds it. Inside the
/// caller's transaction, so the artifact and the version naming it commit
/// together — an artifact nothing names is what the sweep removes, and a
/// version naming an artifact that never arrived is the failure this
/// prevents.
async fn insert_artifact_if_absent_tx(
    tx: &mut DbTransaction,
    digest: &str,
    bytes: &[u8],
) -> Result<(), OrionError> {
    let (sql, values) = build_sqlx(
        tx.backend(),
        Query::select()
            .column(PluginArtifacts::Digest)
            .from(PluginArtifacts::Table)
            .and_where(Expr::col(PluginArtifacts::Digest).eq(digest)),
    );
    if tx
        .fetch_optional_as::<DigestRow>(&sql, values)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let mut insert = Query::insert();
    insert
        .into_table(PluginArtifacts::Table)
        .columns([
            PluginArtifacts::Digest,
            PluginArtifacts::Bytes,
            PluginArtifacts::Size,
        ])
        .values_panic([
            Expr::val(digest),
            Expr::val(bytes.to_vec()),
            Expr::val(bytes.len() as i64),
        ]);
    let (sql, values) = build_sqlx(tx.backend(), &mut insert);
    match tx.execute_query(&sql, values).await {
        Ok(_) => Ok(()),
        // Two uploads of one component racing: whichever lost holds the same
        // bytes under the same digest, which is the outcome either wanted.
        Err(e) => match map_duplicate(e, String::new) {
            OrionError::Conflict(_) => Ok(()),
            other => Err(other),
        },
    }
}

/// Remove every artifact no version names. Run after any write that can
/// orphan one — a delete, or a draft replaced with a different component.
async fn sweep_orphan_artifacts_tx(tx: &mut DbTransaction) -> Result<u64, OrionError> {
    let (sql, values) = build_sqlx(
        tx.backend(),
        Query::delete()
            .from_table(PluginArtifacts::Table)
            .and_where(
                Expr::col(PluginArtifacts::Digest).not_in_subquery(
                    Query::select()
                        .column(Plugins::Digest)
                        .from(Plugins::Table)
                        .to_owned(),
                ),
            ),
    );
    Ok(tx.execute_query(&sql, values).await?)
}

#[derive(sqlx::FromRow)]
struct DigestRow {
    #[allow(dead_code)]
    digest: String,
}

/// Write a draft row (INSERT or UPDATE) with its artifact in one transaction
/// and read the row back inside it.
async fn write_draft(
    pool: &DbPool,
    draft: &PluginDraft,
    version: i64,
    mut write: WriteStatement<'_>,
    artifact: Option<&[u8]>,
    sweep: bool,
    map_write_err: impl FnOnce(sqlx::Error) -> OrionError,
) -> Result<Plugin, OrionError> {
    let mut tx = pool.begin_write_tx().await?;
    if let Some(bytes) = artifact {
        insert_artifact_if_absent_tx(&mut tx, &draft.digest, bytes).await?;
    }
    let mut read_back = Query::select()
        .column(Asterisk)
        .from(Plugins::Table)
        .and_where(Expr::col(Plugins::PluginId).eq(draft.plugin_id.as_str()))
        .and_where(Expr::col(Plugins::Version).eq(version))
        .to_owned();
    let write = match &mut write {
        WriteStatement::Insert(q) => WriteStatement::Insert(q),
        WriteStatement::Update(q) => WriteStatement::Update(q),
    };
    let row: Plugin = super::helpers::write_returning_row_tx(
        &mut tx,
        write,
        &mut read_back,
        map_write_err,
        || {
            OrionError::NotFound(format!(
                "Plugin '{}' version {version} not found",
                draft.plugin_id
            ))
        },
    )
    .await?;
    if sweep {
        sweep_orphan_artifacts_tx(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(row)
}

#[async_trait]
impl PluginRepository for SqlPluginRepository {
    async fn create(
        &self,
        draft: &PluginDraft,
        artifact: Option<&[u8]>,
    ) -> Result<Plugin, OrionError> {
        crate::metrics::timed_db_op("plugins.create", async {
            let mut insert = build_insert(draft, 1);
            let id = draft.plugin_id.clone();
            write_draft(
                &self.pool,
                draft,
                1,
                WriteStatement::Insert(&mut insert),
                artifact,
                false,
                |e| map_duplicate(e, || format!("Plugin with id '{id}' already exists")),
            )
            .await
        })
        .await
    }

    async fn get_by_id(&self, plugin_id: &str) -> Result<Plugin, OrionError> {
        crate::metrics::timed_db_op("plugins.get_by_id", async {
            versioned::get_latest(&self.pool, &spec(), plugin_id).await
        })
        .await
    }

    async fn list_paginated(
        &self,
        filter: &PluginFilter,
    ) -> Result<PaginatedResult<Plugin>, OrionError> {
        crate::metrics::timed_db_op("plugins.list_paginated", async {
            use sea_query::IntoIden;
            let cond = build_condition(filter);
            let (limit, offset) = clamp_pagination(filter.limit, filter.offset);
            let sort_iden = match filter.sort_by.as_deref() {
                Some("status") => Plugins::Status,
                Some("created_at") => Plugins::CreatedAt,
                Some("updated_at") => Plugins::UpdatedAt,
                _ => Plugins::PluginId,
            };
            // Ascending by default: ids are names, and a name list is read
            // alphabetically. The shared parser defaults to `desc`, which is
            // right for a priority and wrong for a name.
            let order = match filter.sort_order.as_deref() {
                None => Order::Asc,
                other => parse_sort_order(other),
            };
            paginate(
                &self.pool,
                Page {
                    from: Plugins::Table.into_iden(),
                    projection: Projection::All,
                    cond: cond.add(versioned::is_current_version(&spec())),
                    sort: sort_iden.into_iden(),
                    order,
                    limit,
                    offset,
                },
            )
            .await
        })
        .await
    }

    async fn snapshot(&self, filter: &PluginFilter) -> Result<Vec<Plugin>, OrionError> {
        crate::metrics::timed_db_op("plugins.snapshot", async {
            super::helpers::snapshot_pages(
                &self.pool,
                super::helpers::EXPORT_PAGE_SIZE,
                |limit, offset| {
                    Query::select()
                        .column(Asterisk)
                        .from(Plugins::Table)
                        .cond_where(build_condition(filter))
                        .and_where(versioned::is_current_version(&spec()))
                        .order_by(Plugins::PluginId, Order::Asc)
                        .limit(limit as u64)
                        .offset(offset as u64)
                        .to_owned()
                },
            )
            .await
        })
        .await
    }

    async fn replace_draft(
        &self,
        plugin_id: &str,
        draft: &PluginDraft,
        artifact: Option<&[u8]>,
    ) -> Result<Plugin, OrionError> {
        crate::metrics::timed_db_op("plugins.replace_draft", async {
            let existing: Plugin = versioned::require_draft(&self.pool, &spec(), plugin_id).await?;
            let mut update = build_update(draft);
            // A replaced component may orphan the old one, so sweep.
            write_draft(
                &self.pool,
                draft,
                existing.version,
                WriteStatement::Update(&mut update),
                artifact,
                true,
                OrionError::Storage,
            )
            .await
        })
        .await
    }

    async fn delete(&self, plugin_id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("plugins.delete", async {
            let mut tx = self.pool.begin_write_tx().await?;
            self.delete_tx(&mut tx, plugin_id).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn delete_tx(&self, tx: &mut DbTransaction, plugin_id: &str) -> Result<(), OrionError> {
        versioned::delete_all_versions_tx(tx, &spec(), plugin_id).await?;
        sweep_orphan_artifacts_tx(tx).await?;
        Ok(())
    }

    async fn list_active(&self) -> Result<Vec<Plugin>, OrionError> {
        crate::metrics::timed_db_op("plugins.list_active", async {
            versioned::list_active(&self.pool, &spec()).await
        })
        .await
    }

    async fn activate(&self, plugin_id: &str) -> Result<Plugin, OrionError> {
        crate::metrics::timed_db_op("plugins.activate", async {
            let mut tx = self.pool.begin_write_tx().await?;
            let activated = self.activate_tx(&mut tx, plugin_id).await?;
            tx.commit().await?;
            Ok(activated)
        })
        .await
    }

    async fn activate_tx(
        &self,
        tx: &mut DbTransaction,
        plugin_id: &str,
    ) -> Result<Plugin, OrionError> {
        let draft: Plugin = versioned::require_draft_tx(tx, &spec(), plugin_id).await?;
        // Exactly one active version per id: whatever was active is archived
        // in the same transaction that promotes the draft.
        let (sql, values) = build_sqlx(
            tx.backend(),
            &mut versioned::archive_actives_query(&spec(), plugin_id, None),
        );
        tx.execute_query(&sql, values).await?;
        let (sql, values) = build_sqlx(
            tx.backend(),
            Query::update()
                .table(Plugins::Table)
                .value(Plugins::Status, EntityStatus::Active.as_str())
                .and_where(Expr::col(Plugins::PluginId).eq(plugin_id))
                .and_where(Expr::col(Plugins::Version).eq(draft.version)),
        );
        tx.execute_query(&sql, values).await?;
        versioned::get_version_tx(tx, &spec(), plugin_id, draft.version).await
    }

    async fn archive(&self, plugin_id: &str) -> Result<Plugin, OrionError> {
        crate::metrics::timed_db_op("plugins.archive", async {
            versioned::archive_latest_active(&self.pool, &spec(), plugin_id).await
        })
        .await
    }

    async fn archive_tx(
        &self,
        tx: &mut DbTransaction,
        plugin_id: &str,
    ) -> Result<Plugin, OrionError> {
        versioned::archive_latest_active_tx(tx, &spec(), plugin_id).await
    }

    async fn create_new_version(&self, plugin_id: &str) -> Result<Plugin, OrionError> {
        crate::metrics::timed_db_op("plugins.create_new_version", async {
            versioned::ensure_no_draft::<Plugin>(&self.pool, &spec(), plugin_id).await?;
            let latest = self.get_by_id(plugin_id).await?;
            let new_version = latest.version + 1;
            let draft = PluginDraft {
                plugin_id: plugin_id.to_string(),
                manifest_json: latest.manifest_json.clone(),
                digest: latest.digest.clone(),
                tags_json: latest.tags_json.clone(),
                // The same digest, so the same signature still holds.
                signature: latest.signature.clone(),
            };
            let mut insert = build_insert(&draft, new_version);
            versioned::write_returning_version(
                &self.pool,
                &spec(),
                WriteStatement::Insert(&mut insert),
                plugin_id,
                new_version,
                OrionError::Storage,
            )
            .await
        })
        .await
    }

    async fn list_versions(
        &self,
        plugin_id: &str,
        filter: &VersionFilter,
    ) -> Result<PaginatedResult<Plugin>, OrionError> {
        crate::metrics::timed_db_op("plugins.list_versions", async {
            versioned::list_versions(&self.pool, &spec(), plugin_id, filter).await
        })
        .await
    }

    async fn get_artifact(&self, digest: &str) -> Result<Option<Vec<u8>>, OrionError> {
        crate::metrics::timed_db_op("plugins.get_artifact", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::select()
                    .column(Asterisk)
                    .from(PluginArtifacts::Table)
                    .and_where(Expr::col(PluginArtifacts::Digest).eq(digest)),
            );
            Ok(self
                .pool
                .fetch_optional_as::<PluginArtifact>(&sql, values)
                .await?
                .map(|a| a.bytes))
        })
        .await
    }

    async fn artifact_exists(&self, digest: &str) -> Result<bool, OrionError> {
        let (sql, values) = build_sqlx(
            self.pool.backend(),
            Query::select()
                .column(PluginArtifacts::Digest)
                .from(PluginArtifacts::Table)
                .and_where(Expr::col(PluginArtifacts::Digest).eq(digest)),
        );
        Ok(self
            .pool
            .fetch_optional_as::<DigestRow>(&sql, values)
            .await?
            .is_some())
    }
}

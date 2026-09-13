//! The model repository: ONNX model versions and the artifact references
//! they carry.
//!
//! A model follows the workflow lifecycle exactly, through the same shared
//! machinery in `super::versioned` that plugins use: integer versions, one
//! draft per id, active rows immutable, `draft → active → archived`, and
//! exactly one active version per id — activating a draft archives the
//! previously active version in the same transaction, so a model id resolves
//! to one digest per generation.
//!
//! What differs from plugins is that there are no bytes here. A version
//! stores a *reference* — `artifact_json`: the connector, key and claimed
//! digest of an object in a bucket — and two derived columns the lifecycle
//! does not own. `admission_json` is the verdict of the node that fetched and
//! probed the artifact; `stats_json` is what that probe read out of it. Both
//! start out empty (`{"state":"pending"}` and NULL), are reset whenever a
//! draft's content is replaced, and are copied forward by
//! [`ModelRepository::create_new_version`] because the reference they
//! describe is unchanged. They are written only through
//! [`ModelRepository::set_admission`] and [`ModelRepository::set_stats`],
//! and they sit outside the active-immutability trigger — see the `models`
//! migration for why.

use async_trait::async_trait;
use sea_query::{Asterisk, Condition, Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::OrionError;
use crate::storage::models::{EntityStatus, Model};
use crate::storage::schema::Models;
use crate::storage::{DbPool, DbTransaction, build_sqlx};

pub use super::helpers::PaginatedResult;
use super::helpers::{
    Page, Projection, VersionFilter, WriteStatement, clamp_pagination, map_duplicate, paginate,
    parse_sort_order,
};
use super::versioned::{self, VersionedSpec};

pub use orion_api::dto::ModelArtifactRef;

/// The `admission_json` every new draft starts with: no node has probed the
/// artifact yet.
///
/// Spelled as the serialisation of [`orion_api::dto::ModelAdmission`] with
/// only `state` set, which is already the canonical form
/// [`ModelRepository::set_admission`] stores every later verdict in — so the
/// `admission` filter can select on `"state":"…"` as a substring.
pub const ADMISSION_PENDING_JSON: &str = r#"{"state":"pending"}"#;

fn spec() -> VersionedSpec {
    use sea_query::IntoIden;
    VersionedSpec {
        table: Models::Table.into_iden(),
        id_col: Models::ModelId.into_iden(),
        version_col: Models::Version.into_iden(),
        status_col: Models::Status.into_iden(),
        priority_col: None,
        updated_at_col: Models::UpdatedAt.into_iden(),
        label: "Model",
        noun: "model",
    }
}

impl versioned::HasVersion for Model {
    fn version(&self) -> i64 {
        self.version
    }
}

// -- DTOs --

/// What `POST /models` and the import accept.
///
/// `manifest` is the manifest as a JSON object; `artifact` says where the
/// bytes are and what digest the author claims for them. Nothing is
/// uploaded: the server never holds model bytes, it fetches them from the
/// connector at admission.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateModelRequest {
    /// Must equal the manifest's `name` when given; the manifest is the
    /// source of truth for the id.
    pub model_id: Option<String>,
    pub manifest: Value,
    /// The object-storage reference. `size` is optional and advisory.
    pub artifact: ModelArtifactRef,
    /// A detached Ed25519 signature over the digest string, base64, when
    /// the deployment requires one; stored either way.
    pub signature: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `PUT /models/{id}`: every field optional, absent means keep.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateModelRequest {
    pub manifest: Option<Value>,
    pub artifact: Option<ModelArtifactRef>,
    pub signature: Option<String>,
    pub tags: Option<Vec<String>>,
}

/// A create request resolved by the route — manifest validated, reference
/// checked against a connector that exists, digest lifted out of it — into
/// the columns a row stores. The repository never sees the wire shape, and
/// never sees the two derived columns either: a draft is always pending.
#[derive(Debug, Clone)]
pub struct ModelDraft {
    pub model_id: String,
    pub manifest_json: String,
    /// The serialised [`ModelArtifactRef`].
    pub artifact_json: String,
    /// The reference's digest, repeated as the indexed column.
    pub digest: String,
    pub tags_json: String,
    /// The detached signature over `digest`, already verified by the route
    /// when the deployment names trust keys; stored so every load re-verifies.
    pub signature: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ModelFilter {
    pub status: Option<String>,
    pub tag: Option<String>,
    /// Admission state to select: `pending`, `passed` or `failed`.
    pub admission: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Column to sort by: model_id (default), status, created_at, updated_at.
    pub sort_by: Option<String>,
    /// Sort direction: asc (default) or desc.
    pub sort_order: Option<String>,
}

// -- Repository trait --

#[async_trait]
pub trait ModelRepository: Send + Sync {
    /// Create a model as draft v1, pending admission and without stats.
    async fn create(&self, draft: &ModelDraft) -> Result<Model, OrionError>;
    /// The latest version of a model.
    async fn get_by_id(&self, model_id: &str) -> Result<Model, OrionError>;
    /// One specific version.
    async fn get_version(&self, model_id: &str, version: i64) -> Result<Model, OrionError>;
    async fn list_paginated(
        &self,
        filter: &ModelFilter,
    ) -> Result<PaginatedResult<Model>, OrionError>;
    /// Every current model matching `filter`, as one consistent snapshot —
    /// the export contract.
    async fn snapshot(&self, filter: &ModelFilter) -> Result<Vec<Model>, OrionError>;
    /// Replace the draft's content, resetting admission to pending and
    /// clearing the stats: the verdict was about the old reference. Errors
    /// if no draft exists.
    async fn replace_draft(&self, model_id: &str, draft: &ModelDraft) -> Result<Model, OrionError>;
    /// Delete all versions of a model.
    async fn delete(&self, model_id: &str) -> Result<(), OrionError>;
    async fn delete_tx(&self, tx: &mut DbTransaction, model_id: &str) -> Result<(), OrionError>;
    /// Every active version, by id — what a reload loads.
    async fn list_active(&self) -> Result<Vec<Model>, OrionError>;
    /// Activate the draft, archiving the previously active version in the
    /// same transaction.
    async fn activate(&self, model_id: &str) -> Result<Model, OrionError>;
    async fn activate_tx(
        &self,
        tx: &mut DbTransaction,
        model_id: &str,
    ) -> Result<Model, OrionError>;
    async fn archive(&self, model_id: &str) -> Result<Model, OrionError>;
    async fn archive_tx(&self, tx: &mut DbTransaction, model_id: &str)
    -> Result<Model, OrionError>;
    /// A new draft copied from the latest version — admission and stats
    /// included, because the reference they describe is unchanged.
    async fn create_new_version(&self, model_id: &str) -> Result<Model, OrionError>;
    async fn list_versions(
        &self,
        model_id: &str,
        filter: &VersionFilter,
    ) -> Result<PaginatedResult<Model>, OrionError>;
    /// Record a node's admission verdict on one version. `admission_json`
    /// must be a JSON object with a string `state`; it is stored in compact
    /// form. `NotFound` when the version does not exist.
    async fn set_admission(
        &self,
        model_id: &str,
        version: i64,
        admission_json: &str,
    ) -> Result<(), OrionError>;
    /// Record (or, with `None`, clear) what admission read out of one
    /// version. `NotFound` when the version does not exist.
    async fn set_stats(
        &self,
        model_id: &str,
        version: i64,
        stats_json: Option<&str>,
    ) -> Result<(), OrionError>;
}

// -- SQL implementation --

pub struct SqlModelRepository {
    pool: DbPool,
}

impl SqlModelRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

/// The INSERT for a draft row. The two derived columns are parameters rather
/// than constants because `create_new_version` carries the previous
/// version's forward, where `create` starts from nothing.
fn build_insert(
    draft: &ModelDraft,
    version: i64,
    admission_json: &str,
    stats_json: Option<&str>,
) -> sea_query::InsertStatement {
    let mut q = Query::insert();
    q.into_table(Models::Table)
        .columns([
            Models::ModelId,
            Models::Version,
            Models::Status,
            Models::Digest,
            Models::ManifestJson,
            Models::ArtifactJson,
            Models::AdmissionJson,
            Models::StatsJson,
            Models::TagsJson,
            Models::Signature,
        ])
        .values_panic([
            Expr::val(draft.model_id.as_str()),
            Expr::val(version),
            Expr::val(EntityStatus::Draft.as_str()),
            Expr::val(draft.digest.as_str()),
            Expr::val(draft.manifest_json.as_str()),
            Expr::val(draft.artifact_json.as_str()),
            Expr::val(admission_json),
            Expr::val(stats_json.map(str::to_string)),
            Expr::val(draft.tags_json.as_str()),
            Expr::val(draft.signature.clone()),
        ]);
    q
}

/// The UPDATE that rewrites a draft's content and resets what was derived
/// from the old content. `status = 'draft'` is part of the statement, so it
/// cannot rewrite a version promoted between the check and the write.
fn build_update(model_id: &str, draft: &ModelDraft) -> sea_query::UpdateStatement {
    Query::update()
        .table(Models::Table)
        .value(Models::Digest, draft.digest.as_str())
        .value(Models::ManifestJson, draft.manifest_json.as_str())
        .value(Models::ArtifactJson, draft.artifact_json.as_str())
        .value(Models::AdmissionJson, ADMISSION_PENDING_JSON)
        .value(Models::StatsJson, None::<String>)
        .value(Models::TagsJson, draft.tags_json.as_str())
        .value(Models::Signature, draft.signature.clone())
        .and_where(Expr::col(Models::ModelId).eq(model_id))
        .and_where(Expr::col(Models::Status).eq(EntityStatus::Draft.as_str()))
        .to_owned()
}

/// The `LIKE` pattern for "this admission document's `state` is `state`".
///
/// Sound only because every stored `admission_json` is in canonical form —
/// sorted keys, no whitespace: [`ADMISSION_PENDING_JSON`] is written that way
/// and `set_admission` normalises what it is given — so `"state":"passed"`
/// occurs as a key/value pair and nowhere else: inside a string value the
/// quotes are escaped as `\"` and cannot match. Wildcards in `state` are
/// escaped the way `tag_like_pattern` escapes a tag, for the same SQLite
/// reason.
fn admission_like_pattern(state: &str) -> sea_query::LikeExpr {
    let escaped = state
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    sea_query::LikeExpr::new(format!("%\"state\":\"{escaped}\"%")).escape('\\')
}

fn build_condition(filter: &ModelFilter) -> Condition {
    let mut cond = Condition::all();
    if let Some(ref status) = filter.status {
        cond = cond.add(Expr::col(Models::Status).eq(status.as_str()));
    }
    if let Some(ref tag) = filter.tag {
        cond = cond
            .add(Expr::col(Models::TagsJson).like(super::helpers::tag_like_pattern(tag.as_str())));
    }
    if let Some(ref state) = filter.admission {
        cond = cond.add(Expr::col(Models::AdmissionJson).like(admission_like_pattern(state)));
    }
    cond
}

/// The stored form of a verdict: parsed, checked to be an object with a
/// string `state`, and re-serialised in canonical form.
///
/// The repository is the last line between a caller and a row that every
/// later read would fail on — `ModelResponse` decodes the column strictly.
/// [`crate::storage::content::canonical_json`] rather than
/// `serde_json::to_string`: sorted keys and no whitespace whatever the caller
/// wrote and whatever `preserve_order` does to a `Value`, so the column has
/// one spelling — the one the `admission` filter matches on.
fn normalise_admission(admission_json: &str) -> Result<String, OrionError> {
    let doc: Value = serde_json::from_str(admission_json)?;
    if !doc.get("state").is_some_and(Value::is_string) {
        return Err(OrionError::Internal {
            context: "an admission verdict must be a JSON object with a string `state`".to_string(),
            source: None,
        });
    }
    Ok(crate::storage::content::canonical_json(&doc))
}

/// The stored form of a stats document: parsed and re-serialised in canonical
/// form, for the same reason as [`normalise_admission`] — a column that does
/// not parse takes every read of the row down with it.
fn normalise_stats(stats_json: &str) -> Result<String, OrionError> {
    let doc: Value = serde_json::from_str(stats_json)?;
    if !doc.is_object() {
        return Err(OrionError::Internal {
            context: "model stats must be a JSON object".to_string(),
            source: None,
        });
    }
    Ok(crate::storage::content::canonical_json(&doc))
}

/// `NotFound` for a version-addressed UPDATE that matched no row.
///
/// Sound on every backend: sqlx opens MySQL with `CLIENT_FOUND_ROWS`, so the
/// count is rows *matched*, not rows changed — re-recording an identical
/// verdict is not a miss.
fn version_or_missing(rows: u64, model_id: &str, version: i64) -> Result<(), OrionError> {
    if rows == 0 {
        return Err(OrionError::NotFound(format!(
            "Model '{model_id}' version {version} not found"
        )));
    }
    Ok(())
}

#[async_trait]
impl ModelRepository for SqlModelRepository {
    async fn create(&self, draft: &ModelDraft) -> Result<Model, OrionError> {
        crate::metrics::timed_db_op("models.create", async {
            let mut insert = build_insert(draft, 1, ADMISSION_PENDING_JSON, None);
            let id = draft.model_id.clone();
            versioned::write_returning_version(
                &self.pool,
                &spec(),
                WriteStatement::Insert(&mut insert),
                &draft.model_id,
                1,
                |e| map_duplicate(e, || format!("Model with id '{id}' already exists")),
            )
            .await
        })
        .await
    }

    async fn get_by_id(&self, model_id: &str) -> Result<Model, OrionError> {
        crate::metrics::timed_db_op("models.get_by_id", async {
            versioned::get_latest(&self.pool, &spec(), model_id).await
        })
        .await
    }

    async fn get_version(&self, model_id: &str, version: i64) -> Result<Model, OrionError> {
        crate::metrics::timed_db_op("models.get_version", async {
            versioned::get_version(&self.pool, &spec(), model_id, version).await
        })
        .await
    }

    async fn list_paginated(
        &self,
        filter: &ModelFilter,
    ) -> Result<PaginatedResult<Model>, OrionError> {
        crate::metrics::timed_db_op("models.list_paginated", async {
            use sea_query::IntoIden;
            let cond = build_condition(filter);
            let (limit, offset) = clamp_pagination(filter.limit, filter.offset);
            let sort_iden = match filter.sort_by.as_deref() {
                Some("status") => Models::Status,
                Some("created_at") => Models::CreatedAt,
                Some("updated_at") => Models::UpdatedAt,
                _ => Models::ModelId,
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
                    from: Models::Table.into_iden(),
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

    async fn snapshot(&self, filter: &ModelFilter) -> Result<Vec<Model>, OrionError> {
        crate::metrics::timed_db_op("models.snapshot", async {
            super::helpers::snapshot_pages(
                &self.pool,
                super::helpers::EXPORT_PAGE_SIZE,
                |limit, offset| {
                    Query::select()
                        .column(Asterisk)
                        .from(Models::Table)
                        .cond_where(build_condition(filter))
                        .and_where(versioned::is_current_version(&spec()))
                        .order_by(Models::ModelId, Order::Asc)
                        .limit(limit as u64)
                        .offset(offset as u64)
                        .to_owned()
                },
            )
            .await
        })
        .await
    }

    async fn replace_draft(&self, model_id: &str, draft: &ModelDraft) -> Result<Model, OrionError> {
        crate::metrics::timed_db_op("models.replace_draft", async {
            let existing: Model = versioned::require_draft(&self.pool, &spec(), model_id).await?;
            let mut update = build_update(model_id, draft);
            versioned::write_returning_version(
                &self.pool,
                &spec(),
                WriteStatement::Update(&mut update),
                model_id,
                existing.version,
                OrionError::Storage,
            )
            .await
        })
        .await
    }

    async fn delete(&self, model_id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("models.delete", async {
            versioned::delete_all_versions(&self.pool, &spec(), model_id).await
        })
        .await
    }

    async fn delete_tx(&self, tx: &mut DbTransaction, model_id: &str) -> Result<(), OrionError> {
        versioned::delete_all_versions_tx(tx, &spec(), model_id).await
    }

    async fn list_active(&self) -> Result<Vec<Model>, OrionError> {
        crate::metrics::timed_db_op("models.list_active", async {
            versioned::list_active(&self.pool, &spec()).await
        })
        .await
    }

    async fn activate(&self, model_id: &str) -> Result<Model, OrionError> {
        crate::metrics::timed_db_op("models.activate", async {
            let mut tx = self.pool.begin_write_tx().await?;
            let activated = self.activate_tx(&mut tx, model_id).await?;
            tx.commit().await?;
            Ok(activated)
        })
        .await
    }

    async fn activate_tx(
        &self,
        tx: &mut DbTransaction,
        model_id: &str,
    ) -> Result<Model, OrionError> {
        let draft: Model = versioned::require_draft_tx(tx, &spec(), model_id).await?;
        // Exactly one active version per id: whatever was active is archived
        // in the same transaction that promotes the draft.
        let (sql, values) = build_sqlx(
            tx.backend(),
            &mut versioned::archive_actives_query(&spec(), model_id, None),
        );
        tx.execute_query(&sql, values).await?;
        let (sql, values) = build_sqlx(
            tx.backend(),
            Query::update()
                .table(Models::Table)
                .value(Models::Status, EntityStatus::Active.as_str())
                .and_where(Expr::col(Models::ModelId).eq(model_id))
                .and_where(Expr::col(Models::Version).eq(draft.version)),
        );
        tx.execute_query(&sql, values).await?;
        versioned::get_version_tx(tx, &spec(), model_id, draft.version).await
    }

    async fn archive(&self, model_id: &str) -> Result<Model, OrionError> {
        crate::metrics::timed_db_op("models.archive", async {
            versioned::archive_latest_active(&self.pool, &spec(), model_id).await
        })
        .await
    }

    async fn archive_tx(
        &self,
        tx: &mut DbTransaction,
        model_id: &str,
    ) -> Result<Model, OrionError> {
        versioned::archive_latest_active_tx(tx, &spec(), model_id).await
    }

    async fn create_new_version(&self, model_id: &str) -> Result<Model, OrionError> {
        crate::metrics::timed_db_op("models.create_new_version", async {
            versioned::ensure_no_draft::<Model>(&self.pool, &spec(), model_id).await?;
            let latest = self.get_by_id(model_id).await?;
            let new_version = latest.version + 1;
            let draft = ModelDraft {
                model_id: model_id.to_string(),
                manifest_json: latest.manifest_json.clone(),
                artifact_json: latest.artifact_json.clone(),
                digest: latest.digest.clone(),
                tags_json: latest.tags_json.clone(),
                // The same digest, so the same signature still holds.
                signature: latest.signature.clone(),
            };
            // The same reference, so what a node found about it still holds
            // too: the verdict and the stats come forward with the draft.
            let mut insert = build_insert(
                &draft,
                new_version,
                &latest.admission_json,
                latest.stats_json.as_deref(),
            );
            versioned::write_returning_version(
                &self.pool,
                &spec(),
                WriteStatement::Insert(&mut insert),
                model_id,
                new_version,
                OrionError::Storage,
            )
            .await
        })
        .await
    }

    async fn list_versions(
        &self,
        model_id: &str,
        filter: &VersionFilter,
    ) -> Result<PaginatedResult<Model>, OrionError> {
        crate::metrics::timed_db_op("models.list_versions", async {
            versioned::list_versions(&self.pool, &spec(), model_id, filter).await
        })
        .await
    }

    async fn set_admission(
        &self,
        model_id: &str,
        version: i64,
        admission_json: &str,
    ) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("models.set_admission", async {
            let stored = normalise_admission(admission_json)?;
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(Models::Table)
                    .value(Models::AdmissionJson, stored)
                    .and_where(Expr::col(Models::ModelId).eq(model_id))
                    .and_where(Expr::col(Models::Version).eq(version)),
            );
            version_or_missing(
                self.pool.execute_query(&sql, values).await?,
                model_id,
                version,
            )
        })
        .await
    }

    async fn set_stats(
        &self,
        model_id: &str,
        version: i64,
        stats_json: Option<&str>,
    ) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("models.set_stats", async {
            let stored = stats_json.map(normalise_stats).transpose()?;
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(Models::Table)
                    .value(Models::StatsJson, stored)
                    .and_where(Expr::col(Models::ModelId).eq(model_id))
                    .and_where(Expr::col(Models::Version).eq(version)),
            );
            version_or_missing(
                self.pool.execute_query(&sql, values).await?,
                model_id,
                version,
            )
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{"abi":"1","name":"m","version":"1.0.0","format":"onnx","inputs":[{"name":"x"}],"outputs":[{"name":"y"}]}"#;
    const ARTIFACT: &str =
        r#"{"connector":"models","key":"m/1.onnx","digest":"sha256:aaaa","size":10}"#;
    const PASSED: &str = r#"{"state":"passed","node":"node-a"}"#;
    // In canonical form (sorted keys) so a stored value compares equal to it.
    const STATS: &str = r#"{"artifact_bytes":10,"device":"cpu","ir_version":9,"nodes":2,"opset":17,"parameters":5,"probe_ms":1.5,"runtime":"ort"}"#;

    fn draft(id: &str) -> ModelDraft {
        ModelDraft {
            model_id: id.to_string(),
            manifest_json: MANIFEST.to_string(),
            artifact_json: ARTIFACT.to_string(),
            digest: "sha256:aaaa".to_string(),
            tags_json: r#"["scoring"]"#.to_string(),
            signature: None,
        }
    }

    async fn repo() -> SqlModelRepository {
        SqlModelRepository::new(crate::storage::test_sqlite_pool().await)
    }

    /// The pending literal is the DTO's own spelling, so a client parsing a
    /// fresh version sees `state: "pending"` and nothing else, and it is
    /// already in the canonical form the repository stores every later
    /// verdict in — the spelling the filter relies on.
    #[test]
    fn the_pending_literal_is_the_dto_serialised() {
        let parsed: orion_api::dto::ModelAdmission =
            serde_json::from_str(ADMISSION_PENDING_JSON).expect("parses");
        assert_eq!(parsed.state, "pending");
        assert_eq!(
            serde_json::to_string(&parsed).expect("serialises"),
            ADMISSION_PENDING_JSON
        );
        assert_eq!(
            normalise_admission(ADMISSION_PENDING_JSON).expect("normalises"),
            ADMISSION_PENDING_JSON
        );
    }

    /// A verdict is stored in canonical form whatever spacing and key order
    /// the caller used, and a document that is not a verdict — no `state`,
    /// or not an object — is refused before it can reach a row.
    #[test]
    fn a_verdict_is_normalised_and_a_non_verdict_refused() {
        assert_eq!(
            normalise_admission(r#"{ "state" : "passed" , "node" : "n1" }"#).expect("verdict"),
            r#"{"node":"n1","state":"passed"}"#
        );
        for bad in ["{}", r#"{"state": 1}"#, "[]", "\"passed\"", "not json"] {
            assert!(normalise_admission(bad).is_err(), "{bad} must be refused");
        }
        assert!(normalise_stats("[]").is_err());
        assert!(normalise_stats("nope").is_err());
        assert_eq!(normalise_stats("{ }").expect("stats"), "{}");
    }

    /// The filter's pattern escapes LIKE wildcards and carries the ESCAPE
    /// clause SQLite needs, like the tag pattern does.
    #[test]
    fn the_admission_pattern_escapes_wildcards() {
        let (sql, _) = Query::select()
            .column(Asterisk)
            .from(Models::Table)
            .cond_where(build_condition(&ModelFilter {
                admission: Some("pass_ed%".to_string()),
                ..Default::default()
            }))
            .build(sea_query::SqliteQueryBuilder);
        assert!(sql.contains("LIKE"), "{sql}");
        assert!(sql.contains("ESCAPE"), "{sql}");
    }

    #[tokio::test]
    async fn create_stores_a_pending_draft_with_no_stats() {
        let repo = repo().await;
        let created = repo.create(&draft("m-create")).await.expect("create");
        assert_eq!(created.version, 1);
        assert_eq!(created.status, EntityStatus::Draft.as_str());
        assert_eq!(created.digest, "sha256:aaaa");
        assert_eq!(created.artifact_json, ARTIFACT);
        assert_eq!(created.manifest_json, MANIFEST);
        assert_eq!(created.admission_json, ADMISSION_PENDING_JSON);
        assert!(created.stats_json.is_none());
        assert!(created.signature.is_none());

        let read = repo.get_by_id("m-create").await.expect("read");
        assert_eq!(read.version, 1);
        let by_version = repo.get_version("m-create", 1).await.expect("version");
        assert_eq!(by_version.model_id, "m-create");
        assert!(matches!(
            repo.get_version("m-create", 2).await,
            Err(OrionError::NotFound(_))
        ));
    }

    /// One draft per id: a second `create` is a 409, not a second row, and
    /// `create_new_version` while a draft exists is refused the same way.
    #[tokio::test]
    async fn a_second_draft_for_the_same_id_is_a_conflict() {
        let repo = repo().await;
        repo.create(&draft("m-dup")).await.expect("create");
        assert!(
            matches!(
                repo.create(&draft("m-dup")).await,
                Err(OrionError::Conflict(_))
            ),
            "the second create must be a Conflict"
        );
        assert!(matches!(
            repo.create_new_version("m-dup").await,
            Err(OrionError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn activating_archives_the_previously_active_version() {
        let repo = repo().await;
        repo.create(&draft("m-act")).await.expect("create");
        let v1 = repo.activate("m-act").await.expect("activate v1");
        assert_eq!((v1.version, v1.status.as_str()), (1, "active"));

        let v2 = repo.create_new_version("m-act").await.expect("v2 draft");
        assert_eq!((v2.version, v2.status.as_str()), (2, "draft"));
        let v2 = repo.activate("m-act").await.expect("activate v2");
        assert_eq!(v2.status, EntityStatus::Active.as_str());

        assert_eq!(
            repo.get_version("m-act", 1).await.expect("v1").status,
            EntityStatus::Archived.as_str()
        );
        let active = repo.list_active().await.expect("active");
        assert_eq!(
            active
                .iter()
                .map(|m| (m.model_id.as_str(), m.version))
                .collect::<Vec<_>>(),
            [("m-act", 2)]
        );

        let archived = repo.archive("m-act").await.expect("archive");
        assert_eq!(archived.version, 2);
        assert!(repo.list_active().await.expect("active").is_empty());
    }

    /// The trigger: an active row's content cannot be rewritten in place —
    /// while the two derived columns, deliberately outside it, still can.
    #[tokio::test]
    async fn an_active_rows_content_is_immutable_but_its_verdict_is_not() {
        let repo = repo().await;
        repo.create(&draft("m-immutable")).await.expect("create");
        repo.activate("m-immutable").await.expect("activate");

        let (sql, values) = build_sqlx(
            repo.pool.backend(),
            Query::update()
                .table(Models::Table)
                .value(Models::ManifestJson, r#"{"abi":"2"}"#)
                .and_where(Expr::col(Models::ModelId).eq("m-immutable"))
                .and_where(Expr::col(Models::Version).eq(1)),
        );
        let err = repo
            .pool
            .execute_query(&sql, values)
            .await
            .expect_err("an active row's manifest must not be editable");
        assert!(
            err.to_string()
                .contains("Cannot modify content of active models"),
            "{err}"
        );

        repo.set_admission("m-immutable", 1, PASSED)
            .await
            .expect("a verdict may land on an active row");
        repo.set_stats("m-immutable", 1, Some(STATS))
            .await
            .expect("stats may land on an active row");
        let row = repo.get_version("m-immutable", 1).await.expect("row");
        assert_eq!(row.status, EntityStatus::Active.as_str());
        assert_eq!(row.admission_json, r#"{"node":"node-a","state":"passed"}"#);
        assert_eq!(row.stats_json.as_deref(), Some(STATS));
    }

    #[tokio::test]
    async fn set_admission_and_set_stats_address_one_version() {
        let repo = repo().await;
        repo.create(&draft("m-verdict")).await.expect("create");
        repo.activate("m-verdict").await.expect("activate");
        repo.create_new_version("m-verdict").await.expect("v2");

        repo.set_admission("m-verdict", 2, PASSED)
            .await
            .expect("set admission");
        repo.set_stats("m-verdict", 2, Some(STATS))
            .await
            .expect("set stats");

        let v1 = repo.get_version("m-verdict", 1).await.expect("v1");
        assert_eq!(v1.admission_json, ADMISSION_PENDING_JSON, "v1 untouched");
        assert!(v1.stats_json.is_none());
        let v2 = repo.get_version("m-verdict", 2).await.expect("v2");
        assert_eq!(v2.admission_json, r#"{"node":"node-a","state":"passed"}"#);
        assert_eq!(v2.stats_json.as_deref(), Some(STATS));

        repo.set_stats("m-verdict", 2, None)
            .await
            .expect("clear stats");
        assert!(
            repo.get_version("m-verdict", 2)
                .await
                .expect("v2")
                .stats_json
                .is_none()
        );

        assert!(matches!(
            repo.set_admission("m-verdict", 9, PASSED).await,
            Err(OrionError::NotFound(_))
        ));
        assert!(matches!(
            repo.set_stats("m-verdict", 9, Some(STATS)).await,
            Err(OrionError::NotFound(_))
        ));
        assert!(
            repo.set_admission("m-verdict", 2, "{}").await.is_err(),
            "a document with no state is not a verdict"
        );
    }

    /// A new version names the same reference, so the verdict and the stats
    /// a node already recorded about it come forward with the draft.
    #[tokio::test]
    async fn a_new_version_copies_the_verdict_and_the_stats() {
        let repo = repo().await;
        repo.create(&draft("m-copy")).await.expect("create");
        repo.set_admission("m-copy", 1, PASSED)
            .await
            .expect("admit");
        repo.set_stats("m-copy", 1, Some(STATS))
            .await
            .expect("stats");
        repo.activate("m-copy").await.expect("activate");

        let v2 = repo.create_new_version("m-copy").await.expect("v2");
        assert_eq!(v2.version, 2);
        assert_eq!(v2.status, EntityStatus::Draft.as_str());
        assert_eq!(v2.admission_json, r#"{"node":"node-a","state":"passed"}"#);
        assert_eq!(v2.stats_json.as_deref(), Some(STATS));
        assert_eq!(v2.artifact_json, ARTIFACT);
        assert_eq!(v2.digest, "sha256:aaaa");
    }

    /// Replacing a draft's content resets what was derived from the old
    /// content: the verdict was about a reference that no longer applies.
    #[tokio::test]
    async fn replacing_a_draft_resets_admission_and_stats() {
        let repo = repo().await;
        repo.create(&draft("m-replace")).await.expect("create");
        repo.set_admission("m-replace", 1, PASSED)
            .await
            .expect("admit");
        repo.set_stats("m-replace", 1, Some(STATS))
            .await
            .expect("stats");

        let mut replacement = draft("m-replace");
        replacement.digest = "sha256:bbbb".to_string();
        replacement.artifact_json =
            r#"{"connector":"models","key":"m/2.onnx","digest":"sha256:bbbb"}"#.to_string();
        replacement.signature = Some("sig".to_string());
        let replaced = repo
            .replace_draft("m-replace", &replacement)
            .await
            .expect("replace");
        assert_eq!(replaced.version, 1);
        assert_eq!(replaced.digest, "sha256:bbbb");
        assert_eq!(replaced.signature.as_deref(), Some("sig"));
        assert_eq!(replaced.admission_json, ADMISSION_PENDING_JSON);
        assert!(replaced.stats_json.is_none());

        // No draft to replace once it is active.
        repo.activate("m-replace").await.expect("activate");
        assert!(matches!(
            repo.replace_draft("m-replace", &replacement).await,
            Err(OrionError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn the_list_filters_by_admission_state_and_tag() {
        let repo = repo().await;
        repo.create(&draft("m-a")).await.expect("create a");
        let mut b = draft("m-b");
        b.tags_json = r#"["other"]"#.to_string();
        repo.create(&b).await.expect("create b");
        repo.set_admission("m-a", 1, PASSED).await.expect("admit a");

        let ids = |page: PaginatedResult<Model>| -> Vec<String> {
            page.data.into_iter().map(|m| m.model_id).collect()
        };
        let passed = repo
            .list_paginated(&ModelFilter {
                admission: Some("passed".to_string()),
                ..Default::default()
            })
            .await
            .expect("passed");
        assert_eq!(passed.total, 1);
        assert_eq!(ids(passed), ["m-a"]);

        let pending = repo
            .list_paginated(&ModelFilter {
                admission: Some("pending".to_string()),
                ..Default::default()
            })
            .await
            .expect("pending");
        assert_eq!(ids(pending), ["m-b"]);

        let tagged = repo
            .list_paginated(&ModelFilter {
                tag: Some("scoring".to_string()),
                ..Default::default()
            })
            .await
            .expect("tag");
        assert_eq!(ids(tagged), ["m-a"]);

        let all = repo
            .list_paginated(&ModelFilter::default())
            .await
            .expect("all");
        assert_eq!(all.total, 2);
        assert_eq!(ids(all), ["m-a", "m-b"], "ascending by id by default");

        let snapshot = repo
            .snapshot(&ModelFilter::default())
            .await
            .expect("snapshot");
        assert_eq!(snapshot.len(), 2);
    }

    /// The list and the snapshot read the *current* version of each id, and
    /// the version history reads every one.
    #[tokio::test]
    async fn the_list_reads_the_current_version_and_the_history_reads_all() {
        let repo = repo().await;
        repo.create(&draft("m-hist")).await.expect("create");
        repo.activate("m-hist").await.expect("activate");
        repo.create_new_version("m-hist").await.expect("v2");

        let page = repo
            .list_paginated(&ModelFilter::default())
            .await
            .expect("list");
        assert_eq!(page.total, 1);
        assert_eq!(page.data[0].version, 2);

        let history = repo
            .list_versions("m-hist", &VersionFilter::default())
            .await
            .expect("history");
        assert_eq!(
            history.data.iter().map(|m| m.version).collect::<Vec<_>>(),
            [2, 1],
            "newest first"
        );

        repo.delete("m-hist").await.expect("delete");
        assert!(matches!(
            repo.get_by_id("m-hist").await,
            Err(OrionError::NotFound(_))
        ));
        assert!(matches!(
            repo.delete("m-hist").await,
            Err(OrionError::NotFound(_))
        ));
    }
}

use crate::storage::DbPool;
use async_trait::async_trait;
use sea_query::{Condition, Expr, Query};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::OrionError;
use crate::storage::models::{Channel, EntityStatus};
use crate::storage::{
    build_sqlx,
    schema::{Channels, CurrentChannels},
};

use super::helpers::PaginatedResult;
use super::helpers::{
    Page, Projection, WriteStatement, clamp_pagination, map_duplicate, optional_string_value,
    paginate, parse_sort_order,
};
use super::versioned::{self, VersionedSpec};

/// The versioned-lifecycle spec shared machinery operates on.
fn spec() -> VersionedSpec {
    use sea_query::IntoIden;
    VersionedSpec {
        table: Channels::Table.into_iden(),
        id_col: Channels::ChannelId.into_iden(),
        version_col: Channels::Version.into_iden(),
        status_col: Channels::Status.into_iden(),
        priority_col: Channels::Priority.into_iden(),
        updated_at_col: Channels::UpdatedAt.into_iden(),
        label: "Channel",
        noun: "channel",
    }
}

impl versioned::HasVersion for Channel {
    fn version(&self) -> i64 {
        self.version
    }
}

// -- DTOs --

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateChannelRequest {
    pub channel_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub channel_type: crate::storage::models::ChannelType,
    pub protocol: crate::storage::models::ChannelProtocol,
    pub methods: Option<Vec<String>>,
    pub route_pattern: Option<String>,
    pub topic: Option<String>,
    pub consumer_group: Option<String>,
    #[serde(default = "default_empty_object")]
    pub transport_config: Value,
    pub workflow_id: Option<String>,
    #[serde(default = "default_empty_object")]
    pub config: Value,
    #[serde(default)]
    pub priority: i64,
    /// Selection labels (K6), same contract as workflow tags — filter with
    /// `?tag=` on list and export.
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct UpdateChannelRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub methods: Option<Vec<String>>,
    pub route_pattern: Option<String>,
    pub topic: Option<String>,
    pub consumer_group: Option<String>,
    pub transport_config: Option<Value>,
    pub workflow_id: Option<String>,
    pub config: Option<Value>,
    pub priority: Option<i64>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ChannelStatusChangeRequest {
    pub status: crate::storage::models::EntityStatus,
}

#[derive(Debug, Default, Deserialize, Serialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ChannelFilter {
    pub status: Option<String>,
    pub channel_type: Option<String>,
    pub protocol: Option<String>,
    /// Only channels carrying this tag (K6).
    pub tag: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Column to sort by: priority (default), name, status, channel_type, protocol, created_at, updated_at.
    pub sort_by: Option<String>,
    /// Sort direction: asc or desc (default).
    pub sort_order: Option<String>,
}

// -- Repository trait --

#[async_trait]
pub trait ChannelRepository: Send + Sync {
    /// Create a new channel as draft v1.
    async fn create(&self, req: &CreateChannelRequest) -> Result<Channel, OrionError>;
    /// Get the latest version of a channel.
    async fn get_by_id(&self, channel_id: &str) -> Result<Channel, OrionError>;
    /// List channels with pagination using the current_channels view.
    async fn list_paginated(
        &self,
        filter: &ChannelFilter,
    ) -> Result<PaginatedResult<Channel>, OrionError>;
    /// Update the draft version of a channel. Errors if no draft exists.
    async fn update_draft(
        &self,
        channel_id: &str,
        req: &UpdateChannelRequest,
    ) -> Result<Channel, OrionError>;
    /// Replace the draft's entire content with a create-shaped request (K2).
    ///
    /// The upsert import needs full-replacement semantics: `update_draft`
    /// merges `Option` fields (and cannot clear one, or change
    /// `channel_type`/`protocol` at all), so an imported draft would never
    /// converge on the artifact's content. Errors if no draft exists.
    async fn replace_draft(
        &self,
        channel_id: &str,
        req: &CreateChannelRequest,
    ) -> Result<Channel, OrionError>;
    /// Delete all versions of a channel.
    async fn delete(&self, channel_id: &str) -> Result<(), OrionError>;
    /// List all active channels for engine loading.
    async fn list_active(&self) -> Result<Vec<Channel>, OrionError>;
    /// Activate the draft version of a channel.
    async fn activate(&self, channel_id: &str) -> Result<Channel, OrionError>;
    /// Archive the latest active version of a channel.
    async fn archive(&self, channel_id: &str) -> Result<Channel, OrionError>;
    /// Create a new draft version by copying the latest version.
    async fn create_new_version(&self, channel_id: &str) -> Result<Channel, OrionError>;
    /// List all versions of a channel.
    async fn list_versions(
        &self,
        channel_id: &str,
        filter: &super::helpers::VersionFilter,
    ) -> Result<PaginatedResult<Channel>, OrionError>;
}

// -- SQL implementation --

pub struct SqlChannelRepository {
    pool: DbPool,
}

impl SqlChannelRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// K7: refuse a channel name another `channel_id` already holds.
    ///
    /// The data plane and `channel_call` address channels by **name**, and the
    /// registry keeps one entry per name — a second channel sharing one
    /// silently races for the slot and loses requests to the winner. Names
    /// are compared against the *current* version of every other channel (the
    /// row that serves now or would on next activation).
    ///
    /// Enforced here rather than as DDL because the invariant is per-id, not
    /// per-row — every version of one channel legitimately repeats the name —
    /// and MySQL cannot express the partial unique index that would encode
    /// that. Check-then-write, like the route-collision gate: a racing pair
    /// can slip through, and the activation gate re-checks against the active
    /// set where it matters.
    async fn ensure_name_unclaimed(&self, name: &str, own_id: &str) -> Result<(), OrionError> {
        let (sql, values) = build_sqlx(
            Query::select()
                .column(Channels::ChannelId)
                .from(CurrentChannels::Table)
                .and_where(Expr::col(Channels::Name).eq(name))
                .and_where(Expr::col(Channels::ChannelId).ne(own_id))
                .limit(1),
        );
        if let Some((holder,)) = self
            .pool
            .fetch_optional_as::<(String,)>(&sql, values)
            .await?
        {
            return Err(OrionError::Conflict(format!(
                "Channel name '{name}' is already used by channel id '{holder}' — \
                 channel names must be unique because the data plane and channel_call \
                 address channels by name"
            )));
        }
        Ok(())
    }
}

/// Fields needed to materialise one row of the `channels` table — used by
/// `build_channel_insert` to avoid a 15-argument positional signature across
/// `create` and `create_new_version`.
struct ChannelInsertRow<'a> {
    channel_id: &'a str,
    version: i64,
    name: &'a str,
    description: sea_query::Value,
    channel_type: &'a str,
    protocol: &'a str,
    methods_json: sea_query::Value,
    route_pattern: sea_query::Value,
    topic: sea_query::Value,
    consumer_group: sea_query::Value,
    transport_config_json: &'a str,
    workflow_id: sea_query::Value,
    config_json: &'a str,
    status: &'a str,
    priority: i64,
    tags_json: &'a str,
}

/// Build the INSERT statement for a channel row.
fn build_channel_insert(row: ChannelInsertRow<'_>) -> sea_query::InsertStatement {
    let mut q = Query::insert();
    q.into_table(Channels::Table)
        .columns([
            Channels::ChannelId,
            Channels::Version,
            Channels::Name,
            Channels::Description,
            Channels::ChannelType,
            Channels::Protocol,
            Channels::MethodsJson,
            Channels::RoutePattern,
            Channels::Topic,
            Channels::ConsumerGroup,
            Channels::TransportConfigJson,
            Channels::WorkflowId,
            Channels::ConfigJson,
            Channels::Status,
            Channels::Priority,
            Channels::TagsJson,
        ])
        .values_panic([
            Expr::val(row.channel_id).into(),
            Expr::val(row.version).into(),
            Expr::val(row.name).into(),
            Expr::val(row.description).into(),
            Expr::val(row.channel_type).into(),
            Expr::val(row.protocol).into(),
            Expr::val(row.methods_json).into(),
            Expr::val(row.route_pattern).into(),
            Expr::val(row.topic).into(),
            Expr::val(row.consumer_group).into(),
            Expr::val(row.transport_config_json).into(),
            Expr::val(row.workflow_id).into(),
            Expr::val(row.config_json).into(),
            Expr::val(row.status).into(),
            Expr::val(row.priority).into(),
            Expr::val(row.tags_json).into(),
        ]);
    q
}

fn build_condition(filter: &ChannelFilter) -> Condition {
    let mut cond = Condition::all();
    if let Some(ref status) = filter.status {
        cond = cond.add(Expr::col(Channels::Status).eq(status.as_str()));
    }
    if let Some(ref channel_type) = filter.channel_type {
        cond = cond.add(Expr::col(Channels::ChannelType).eq(channel_type.as_str()));
    }
    if let Some(ref protocol) = filter.protocol {
        cond = cond.add(Expr::col(Channels::Protocol).eq(protocol.as_str()));
    }
    if let Some(ref tag) = filter.tag {
        cond = cond.add(
            Expr::col(Channels::TagsJson).like(super::helpers::tag_like_pattern(tag.as_str())),
        );
    }
    cond
}

#[async_trait]
impl ChannelRepository for SqlChannelRepository {
    async fn create(&self, req: &CreateChannelRequest) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.create", async {
            let channel_id = req
                .channel_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            self.ensure_name_unclaimed(&req.name, &channel_id).await?;
            let methods_json = req
                .methods
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let transport_config_json = serde_json::to_string(&req.transport_config)?;
            let config_json = serde_json::to_string(&req.config)?;
            let tags_json = serde_json::to_string(&req.tags)?;

            let methods_val = optional_string_value(methods_json.as_deref());
            let description_val = optional_string_value(req.description.as_deref());
            let route_pattern_val = optional_string_value(req.route_pattern.as_deref());
            let topic_val = optional_string_value(req.topic.as_deref());
            let consumer_group_val = optional_string_value(req.consumer_group.as_deref());
            let workflow_id_val = optional_string_value(req.workflow_id.as_deref());

            let mut insert = build_channel_insert(ChannelInsertRow {
                channel_id: channel_id.as_str(),
                version: 1,
                name: req.name.as_str(),
                description: description_val,
                channel_type: req.channel_type.as_str(),
                protocol: req.protocol.as_str(),
                methods_json: methods_val,
                route_pattern: route_pattern_val,
                topic: topic_val,
                consumer_group: consumer_group_val,
                transport_config_json: transport_config_json.as_str(),
                workflow_id: workflow_id_val,
                config_json: config_json.as_str(),
                status: EntityStatus::Draft.as_str(),
                priority: req.priority,
                tags_json: tags_json.as_str(),
            });

            // D23: the INSERT and the row it wrote travel together.
            // D16: a duplicate id is the client's mistake, not ours — 409.
            versioned::write_returning_version(
                &self.pool,
                &spec(),
                WriteStatement::Insert(&mut insert),
                &channel_id,
                1,
                |e| {
                    map_duplicate(e, || {
                        format!("Channel with id '{channel_id}' already exists")
                    })
                },
            )
            .await
        })
        .await
    }

    async fn get_by_id(&self, channel_id: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.get_by_id", async {
            versioned::get_latest(&self.pool, &spec(), channel_id).await
        })
        .await
    }

    async fn list_paginated(
        &self,
        filter: &ChannelFilter,
    ) -> Result<PaginatedResult<Channel>, OrionError> {
        crate::metrics::timed_db_op("channels.list_paginated", async {
            let cond = build_condition(filter);
            let (limit, offset) = clamp_pagination(filter.limit, filter.offset);

            use sea_query::IntoIden;
            let sort_iden = match filter.sort_by.as_deref() {
                Some("name") => Channels::Name,
                Some("status") => Channels::Status,
                Some("channel_type") => Channels::ChannelType,
                Some("protocol") => Channels::Protocol,
                Some("created_at") => Channels::CreatedAt,
                Some("updated_at") => Channels::UpdatedAt,
                _ => Channels::Priority,
            };
            let order = parse_sort_order(filter.sort_order.as_deref());
            paginate(
                &self.pool,
                Page {
                    from: CurrentChannels::Table.into_iden(),
                    projection: Projection::All,
                    cond,
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

    async fn update_draft(
        &self,
        channel_id: &str,
        req: &UpdateChannelRequest,
    ) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.update_draft", async {
            let existing: Channel =
                versioned::require_draft(&self.pool, &spec(), channel_id).await?;

            let name = req.name.as_deref().unwrap_or(&existing.name);
            if name != existing.name {
                self.ensure_name_unclaimed(name, channel_id).await?;
            }
            let description = req
                .description
                .as_deref()
                .or(existing.description.as_deref());
            let priority = req.priority.unwrap_or(existing.priority);

            let methods_json = match &req.methods {
                Some(m) => Some(serde_json::to_string(m)?),
                None => existing.methods_json.clone(),
            };
            let route_pattern = req
                .route_pattern
                .as_deref()
                .or(existing.route_pattern.as_deref());
            let topic = req.topic.as_deref().or(existing.topic.as_deref());
            let consumer_group = req
                .consumer_group
                .as_deref()
                .or(existing.consumer_group.as_deref());
            let transport_config_json = match &req.transport_config {
                Some(tc) => serde_json::to_string(tc)?,
                None => existing.transport_config_json.clone(),
            };
            let workflow_id = req
                .workflow_id
                .as_deref()
                .or(existing.workflow_id.as_deref());
            let config_json = match &req.config {
                Some(c) => serde_json::to_string(c)?,
                None => existing.config_json.clone(),
            };
            let tags_json = match &req.tags {
                Some(t) => serde_json::to_string(t)?,
                None => existing.tags_json.clone(),
            };

            let description_val = optional_string_value(description);
            let methods_val = optional_string_value(methods_json.as_deref());
            let route_pattern_val = optional_string_value(route_pattern);
            let topic_val = optional_string_value(topic);
            let consumer_group_val = optional_string_value(consumer_group);
            let workflow_id_val = optional_string_value(workflow_id);

            let mut update = Query::update()
                .table(Channels::Table)
                .value(Channels::Name, name)
                .value(Channels::Description, description_val)
                .value(Channels::MethodsJson, methods_val)
                .value(Channels::RoutePattern, route_pattern_val)
                .value(Channels::Topic, topic_val)
                .value(Channels::ConsumerGroup, consumer_group_val)
                .value(
                    Channels::TransportConfigJson,
                    transport_config_json.as_str(),
                )
                .value(Channels::WorkflowId, workflow_id_val)
                .value(Channels::ConfigJson, config_json.as_str())
                .value(Channels::Priority, priority)
                .value(Channels::TagsJson, tags_json.as_str())
                .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                .and_where(Expr::col(Channels::Status).eq(EntityStatus::Draft.as_str()))
                .to_owned();

            // D23: the UPDATE and the row it wrote travel together.
            versioned::write_returning_version(
                &self.pool,
                &spec(),
                WriteStatement::Update(&mut update),
                channel_id,
                existing.version,
                OrionError::Storage,
            )
            .await
        })
        .await
    }

    async fn replace_draft(
        &self,
        channel_id: &str,
        req: &CreateChannelRequest,
    ) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.replace_draft", async {
            let existing: Channel =
                versioned::require_draft(&self.pool, &spec(), channel_id).await?;

            if req.name != existing.name {
                self.ensure_name_unclaimed(&req.name, channel_id).await?;
            }

            let methods_json = req
                .methods
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let transport_config_json = serde_json::to_string(&req.transport_config)?;
            let config_json = serde_json::to_string(&req.config)?;
            let tags_json = serde_json::to_string(&req.tags)?;

            let mut update = Query::update()
                .table(Channels::Table)
                .value(Channels::Name, req.name.as_str())
                .value(
                    Channels::Description,
                    optional_string_value(req.description.as_deref()),
                )
                .value(Channels::ChannelType, req.channel_type.as_str())
                .value(Channels::Protocol, req.protocol.as_str())
                .value(
                    Channels::MethodsJson,
                    optional_string_value(methods_json.as_deref()),
                )
                .value(
                    Channels::RoutePattern,
                    optional_string_value(req.route_pattern.as_deref()),
                )
                .value(Channels::Topic, optional_string_value(req.topic.as_deref()))
                .value(
                    Channels::ConsumerGroup,
                    optional_string_value(req.consumer_group.as_deref()),
                )
                .value(
                    Channels::TransportConfigJson,
                    transport_config_json.as_str(),
                )
                .value(
                    Channels::WorkflowId,
                    optional_string_value(req.workflow_id.as_deref()),
                )
                .value(Channels::ConfigJson, config_json.as_str())
                .value(Channels::Priority, req.priority)
                .value(Channels::TagsJson, tags_json.as_str())
                .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                .and_where(Expr::col(Channels::Status).eq(EntityStatus::Draft.as_str()))
                .to_owned();

            versioned::write_returning_version(
                &self.pool,
                &spec(),
                WriteStatement::Update(&mut update),
                channel_id,
                existing.version,
                OrionError::Storage,
            )
            .await
        })
        .await
    }

    async fn delete(&self, channel_id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("channels.delete", async {
            versioned::delete_all_versions(&self.pool, &spec(), channel_id).await
        })
        .await
    }

    async fn list_active(&self) -> Result<Vec<Channel>, OrionError> {
        crate::metrics::timed_db_op("channels.list_active", async {
            versioned::list_active(&self.pool, &spec()).await
        })
        .await
    }

    async fn activate(&self, channel_id: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.activate", async {
            let mut tx = self.pool.begin_tx().await?;

            let draft: Channel = versioned::require_draft_tx(&mut tx, &spec(), channel_id).await?;

            // Archive current active versions
            let (archive_sql, archive_values) = build_sqlx(&mut versioned::archive_actives_query(
                &spec(),
                channel_id,
                None,
            ));
            tx.execute_query(&archive_sql, archive_values).await?;

            // Activate the draft
            let (activate_sql, activate_values) = build_sqlx(
                Query::update()
                    .table(Channels::Table)
                    .value(Channels::Status, EntityStatus::Active.as_str())
                    .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                    .and_where(Expr::col(Channels::Version).eq(draft.version)),
            );

            tx.execute_query(&activate_sql, activate_values).await?;

            // D23: read the promoted row back inside the transaction that
            // promoted it — this used to run on the pool after `tx.commit()`.
            let activated =
                versioned::get_version_tx(&mut tx, &spec(), channel_id, draft.version).await?;

            tx.commit().await?;

            Ok(activated)
        })
        .await
    }

    async fn archive(&self, channel_id: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.archive", async {
            versioned::archive_latest_active(&self.pool, &spec(), channel_id).await
        })
        .await
    }

    async fn create_new_version(&self, channel_id: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.create_new_version", async {
            versioned::ensure_no_draft::<Channel>(&self.pool, &spec(), channel_id).await?;

            // Find the latest version to copy from
            let latest = self.get_by_id(channel_id).await?;

            let new_version = latest.version + 1;

            let description_val = optional_string_value(latest.description.as_deref());
            let methods_val = optional_string_value(latest.methods_json.as_deref());
            let route_pattern_val = optional_string_value(latest.route_pattern.as_deref());
            let topic_val = optional_string_value(latest.topic.as_deref());
            let consumer_group_val = optional_string_value(latest.consumer_group.as_deref());
            let workflow_id_val = optional_string_value(latest.workflow_id.as_deref());

            let mut insert = build_channel_insert(ChannelInsertRow {
                channel_id,
                version: new_version,
                name: latest.name.as_str(),
                description: description_val,
                channel_type: latest.channel_type.as_str(),
                protocol: latest.protocol.as_str(),
                methods_json: methods_val,
                route_pattern: route_pattern_val,
                topic: topic_val,
                consumer_group: consumer_group_val,
                transport_config_json: latest.transport_config_json.as_str(),
                workflow_id: workflow_id_val,
                config_json: latest.config_json.as_str(),
                status: EntityStatus::Draft.as_str(),
                priority: latest.priority,
                tags_json: latest.tags_json.as_str(),
            });

            // D23: the INSERT and the row it wrote travel together.
            versioned::write_returning_version(
                &self.pool,
                &spec(),
                WriteStatement::Insert(&mut insert),
                channel_id,
                new_version,
                OrionError::Storage,
            )
            .await
        })
        .await
    }

    async fn list_versions(
        &self,
        channel_id: &str,
        filter: &super::helpers::VersionFilter,
    ) -> Result<PaginatedResult<Channel>, OrionError> {
        crate::metrics::timed_db_op("channels.list_versions", async {
            versioned::list_versions(&self.pool, &spec(), channel_id, filter).await
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_query::Asterisk;

    /// Initialize the DB backend for unit tests that call `build_sqlx`.
    fn init_test_backend() {
        crate::storage::set_backend_for_test(crate::storage::DbBackend::Sqlite);
    }

    #[test]
    fn test_build_condition_empty() {
        init_test_backend();
        let filter = ChannelFilter::default();
        let cond = build_condition(&filter);
        // Build a query with the condition to verify it produces valid SQL
        let (sql, _) = build_sqlx(
            Query::select()
                .column(Asterisk)
                .from(CurrentChannels::Table)
                .cond_where(cond),
        );
        // Empty Condition::all() produces WHERE TRUE -- no actual column filters
        assert!(
            !sql.contains("\"status\""),
            "empty filter should not filter by status, got: {}",
            sql
        );
    }

    #[test]
    fn test_build_condition_status() {
        init_test_backend();
        let filter = ChannelFilter {
            status: Some(EntityStatus::Active.as_str().to_string()),
            ..Default::default()
        };
        let cond = build_condition(&filter);
        let (sql, _) = build_sqlx(
            Query::select()
                .column(Asterisk)
                .from(CurrentChannels::Table)
                .cond_where(cond),
        );
        assert!(
            sql.contains("status"),
            "SQL should contain status filter, got: {}",
            sql
        );
    }

    #[test]
    fn test_build_condition_channel_type() {
        init_test_backend();
        let filter = ChannelFilter {
            channel_type: Some("sync".to_string()),
            ..Default::default()
        };
        let cond = build_condition(&filter);
        let (sql, _) = build_sqlx(
            Query::select()
                .column(Asterisk)
                .from(CurrentChannels::Table)
                .cond_where(cond),
        );
        assert!(
            sql.contains("channel_type"),
            "SQL should contain channel_type filter, got: {}",
            sql
        );
    }

    #[test]
    fn test_build_condition_protocol() {
        init_test_backend();
        let filter = ChannelFilter {
            protocol: Some("rest".to_string()),
            ..Default::default()
        };
        let cond = build_condition(&filter);
        let (sql, _) = build_sqlx(
            Query::select()
                .column(Asterisk)
                .from(CurrentChannels::Table)
                .cond_where(cond),
        );
        assert!(
            sql.contains("protocol"),
            "SQL should contain protocol filter, got: {}",
            sql
        );
    }

    #[test]
    fn test_build_condition_all_filters() {
        init_test_backend();
        let filter = ChannelFilter {
            status: Some(EntityStatus::Draft.as_str().to_string()),
            channel_type: Some("async".to_string()),
            protocol: Some("kafka".to_string()),
            limit: Some(10),
            offset: Some(0),
            ..Default::default()
        };
        let cond = build_condition(&filter);
        let (sql, _) = build_sqlx(
            Query::select()
                .column(Asterisk)
                .from(CurrentChannels::Table)
                .cond_where(cond),
        );
        assert!(
            sql.contains("status"),
            "SQL should contain status filter, got: {}",
            sql
        );
        assert!(
            sql.contains("channel_type"),
            "SQL should contain channel_type filter, got: {}",
            sql
        );
        assert!(
            sql.contains("protocol"),
            "SQL should contain protocol filter, got: {}",
            sql
        );
    }

    #[test]
    fn test_default_empty_object() {
        let val = default_empty_object();
        assert!(val.is_object());
        assert_eq!(val, serde_json::json!({}));
    }

    #[test]
    fn test_create_channel_request_defaults() {
        use crate::storage::models::{ChannelProtocol, ChannelType};
        let json = r#"{"name":"orders","channel_type":"sync","protocol":"rest"}"#;
        let req: CreateChannelRequest = serde_json::from_str(json).expect("test");
        assert_eq!(req.name, "orders");
        assert_eq!(req.channel_type, ChannelType::Sync);
        assert_eq!(req.protocol, ChannelProtocol::Rest);
        assert!(req.channel_id.is_none());
        assert!(req.description.is_none());
        assert!(req.methods.is_none());
        assert!(req.route_pattern.is_none());
        assert!(req.topic.is_none());
        assert!(req.consumer_group.is_none());
        assert_eq!(req.transport_config, serde_json::json!({}));
        assert!(req.workflow_id.is_none());
        assert_eq!(req.config, serde_json::json!({}));
        assert_eq!(req.priority, 0);
    }

    #[test]
    fn test_create_channel_request_full() {
        let json = r#"{
            "channel_id": "ch-1",
            "name": "orders",
            "description": "Order channel",
            "channel_type": "sync",
            "protocol": "rest",
            "methods": ["POST", "PUT"],
            "route_pattern": "/orders/{id}",
            "transport_config": {"timeout": 5000},
            "workflow_id": "wf-1",
            "config": {"max_retries": 3},
            "priority": 10
        }"#;
        let req: CreateChannelRequest = serde_json::from_str(json).expect("test");
        assert_eq!(req.channel_id, Some("ch-1".to_string()));
        assert_eq!(
            req.methods,
            Some(vec!["POST".to_string(), "PUT".to_string()])
        );
        assert_eq!(req.route_pattern, Some("/orders/{id}".to_string()));
        assert_eq!(req.priority, 10);
    }

    #[test]
    fn test_update_channel_request_all_none() {
        let json = r#"{}"#;
        let req: UpdateChannelRequest = serde_json::from_str(json).expect("test");
        assert!(req.name.is_none());
        assert!(req.description.is_none());
        assert!(req.methods.is_none());
        assert!(req.route_pattern.is_none());
        assert!(req.topic.is_none());
        assert!(req.consumer_group.is_none());
        assert!(req.transport_config.is_none());
        assert!(req.workflow_id.is_none());
        assert!(req.config.is_none());
        assert!(req.priority.is_none());
    }

    #[test]
    fn test_channel_status_change_request() {
        let json = r#"{"status": "active"}"#;
        let req: ChannelStatusChangeRequest = serde_json::from_str(json).expect("test");
        assert_eq!(req.status, EntityStatus::Active);
    }

    #[test]
    fn test_channel_filter_default() {
        let filter = ChannelFilter::default();
        assert!(filter.status.is_none());
        assert!(filter.channel_type.is_none());
        assert!(filter.protocol.is_none());
        assert!(filter.limit.is_none());
        assert!(filter.offset.is_none());
    }
}

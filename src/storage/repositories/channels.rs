use crate::storage::DbPool;
use async_trait::async_trait;
use sea_query::{Asterisk, Condition, Expr, Func, Order, Query};
use sea_query_binder::SqlxBinder;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::OrionError;
use crate::storage::models::Channel;
use crate::storage::{
    query_builder,
    schema::{Channels, CurrentChannels},
};

use super::helpers::{clamp_pagination, optional_string_value};
use super::workflows::PaginatedResult;

// -- DTOs --

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateChannelRequest {
    pub channel_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub channel_type: String,
    pub protocol: String,
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
}

fn default_empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
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
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ChannelStatusChangeRequest {
    pub status: String,
}

#[derive(Debug, Default, Deserialize, Serialize, utoipa::IntoParams)]
pub struct ChannelFilter {
    pub status: Option<String>,
    pub channel_type: Option<String>,
    pub protocol: Option<String>,
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
    /// Get a specific version of a channel.
    async fn get_version(&self, channel_id: &str, version: i64) -> Result<Channel, OrionError>;
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
        limit: i64,
        offset: i64,
    ) -> Result<PaginatedResult<Channel>, OrionError>;
    /// Lookup an active channel by its name (for runtime routing).
    async fn get_active_by_name(&self, name: &str) -> Result<Channel, OrionError>;
}

// -- SQL implementation --

pub struct SqlChannelRepository {
    pool: DbPool,
}

impl SqlChannelRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
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
            let methods_json = req
                .methods
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let transport_config_json = serde_json::to_string(&req.transport_config)?;
            let config_json = serde_json::to_string(&req.config)?;

            let methods_val = optional_string_value(methods_json.as_deref());
            let description_val = optional_string_value(req.description.as_deref());
            let route_pattern_val = optional_string_value(req.route_pattern.as_deref());
            let topic_val = optional_string_value(req.topic.as_deref());
            let consumer_group_val = optional_string_value(req.consumer_group.as_deref());
            let workflow_id_val = optional_string_value(req.workflow_id.as_deref());

            let (sql, values) = Query::insert()
                .into_table(Channels::Table)
                .columns([
                    Channels::ChannelId,
                    Channels::Version,
                    Channels::Name,
                    Channels::Description,
                    Channels::ChannelType,
                    Channels::Protocol,
                    Channels::Methods,
                    Channels::RoutePattern,
                    Channels::Topic,
                    Channels::ConsumerGroup,
                    Channels::TransportConfigJson,
                    Channels::WorkflowId,
                    Channels::ConfigJson,
                    Channels::Status,
                    Channels::Priority,
                ])
                .values_panic([
                    Expr::val(channel_id.as_str()).into(),
                    Expr::val(1i64).into(),
                    Expr::val(req.name.as_str()).into(),
                    Expr::val(description_val).into(),
                    Expr::val(req.channel_type.as_str()).into(),
                    Expr::val(req.protocol.as_str()).into(),
                    Expr::val(methods_val).into(),
                    Expr::val(route_pattern_val).into(),
                    Expr::val(topic_val).into(),
                    Expr::val(consumer_group_val).into(),
                    Expr::val(transport_config_json.as_str()).into(),
                    Expr::val(workflow_id_val).into(),
                    Expr::val(config_json.as_str()).into(),
                    Expr::val("draft").into(),
                    Expr::val(req.priority).into(),
                ])
                .build_sqlx(query_builder());

            sqlx::query_with(&sql, values).execute(&self.pool).await?;

            self.get_version(&channel_id, 1).await
        })
        .await
    }

    async fn get_by_id(&self, channel_id: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.get_by_id", async {
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Channels::Table)
                .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                .order_by(Channels::Version, Order::Desc)
                .limit(1)
                .build_sqlx(query_builder());

            sqlx::query_as_with::<_, Channel, _>(&sql, values)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| OrionError::NotFound(format!("Channel '{}' not found", channel_id)))
        })
        .await
    }

    async fn get_version(&self, channel_id: &str, version: i64) -> Result<Channel, OrionError> {
        let (sql, values) = Query::select()
            .column(Asterisk)
            .from(Channels::Table)
            .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
            .and_where(Expr::col(Channels::Version).eq(version))
            .build_sqlx(query_builder());

        sqlx::query_as_with::<_, Channel, _>(&sql, values)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| {
                OrionError::NotFound(format!(
                    "Channel '{}' version {} not found",
                    channel_id, version
                ))
            })
    }

    async fn list_paginated(
        &self,
        filter: &ChannelFilter,
    ) -> Result<PaginatedResult<Channel>, OrionError> {
        crate::metrics::timed_db_op("channels.list_paginated", async {
            let cond = build_condition(filter);
            let (limit, offset) = clamp_pagination(filter.limit, filter.offset);

            let (count_sql, count_values) = Query::select()
                .expr(Func::count(Expr::col(Asterisk)))
                .from(CurrentChannels::Table)
                .cond_where(cond.clone())
                .build_sqlx(query_builder());

            let (total,): (i64,) = sqlx::query_as_with::<_, (i64,), _>(&count_sql, count_values)
                .fetch_one(&self.pool)
                .await?;

            // Sort column mapping
            let sort_iden = match filter.sort_by.as_deref() {
                Some("name") => Channels::Name,
                Some("status") => Channels::Status,
                Some("channel_type") => Channels::ChannelType,
                Some("protocol") => Channels::Protocol,
                Some("created_at") => Channels::CreatedAt,
                Some("updated_at") => Channels::UpdatedAt,
                _ => Channels::Priority,
            };
            let order = match filter.sort_order.as_deref() {
                Some("asc") => Order::Asc,
                _ => Order::Desc,
            };

            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(CurrentChannels::Table)
                .cond_where(cond)
                .order_by(sort_iden, order)
                .limit(limit as u64)
                .offset(offset as u64)
                .build_sqlx(query_builder());

            let data = sqlx::query_as_with::<_, Channel, _>(&sql, values)
                .fetch_all(&self.pool)
                .await?;

            Ok(PaginatedResult {
                data,
                total,
                limit,
                offset,
            })
        })
        .await
    }

    async fn update_draft(
        &self,
        channel_id: &str,
        req: &UpdateChannelRequest,
    ) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.update_draft", async {
            let (draft_sql, draft_values) = Query::select()
                .column(Asterisk)
                .from(Channels::Table)
                .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                .and_where(Expr::col(Channels::Status).eq("draft"))
                .build_sqlx(query_builder());

            let existing = sqlx::query_as_with::<_, Channel, _>(&draft_sql, draft_values)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| {
                    OrionError::BadRequest(format!(
                        "No draft version found for channel '{}'",
                        channel_id
                    ))
                })?;

            let name = req.name.as_deref().unwrap_or(&existing.name);
            let description = req
                .description
                .as_deref()
                .or(existing.description.as_deref());
            let priority = req.priority.unwrap_or(existing.priority);

            let methods_json = match &req.methods {
                Some(m) => Some(serde_json::to_string(m)?),
                None => existing.methods.clone(),
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

            let description_val = optional_string_value(description);
            let methods_val = optional_string_value(methods_json.as_deref());
            let route_pattern_val = optional_string_value(route_pattern);
            let topic_val = optional_string_value(topic);
            let consumer_group_val = optional_string_value(consumer_group);
            let workflow_id_val = optional_string_value(workflow_id);

            let (sql, values) = Query::update()
                .table(Channels::Table)
                .value(Channels::Name, name)
                .value(Channels::Description, description_val)
                .value(Channels::Methods, methods_val)
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
                .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                .and_where(Expr::col(Channels::Status).eq("draft"))
                .build_sqlx(query_builder());

            sqlx::query_with(&sql, values).execute(&self.pool).await?;

            self.get_version(channel_id, existing.version).await
        })
        .await
    }

    async fn delete(&self, channel_id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("channels.delete", async {
            let (sql, values) = Query::delete()
                .from_table(Channels::Table)
                .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                .build_sqlx(query_builder());

            let result = sqlx::query_with(&sql, values).execute(&self.pool).await?;

            if result.rows_affected() == 0 {
                return Err(OrionError::NotFound(format!(
                    "Channel '{}' not found",
                    channel_id
                )));
            }

            Ok(())
        })
        .await
    }

    async fn list_active(&self) -> Result<Vec<Channel>, OrionError> {
        crate::metrics::timed_db_op("channels.list_active", async {
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Channels::Table)
                .and_where(Expr::col(Channels::Status).eq("active"))
                .order_by(Channels::Priority, Order::Desc)
                .build_sqlx(query_builder());

            Ok(sqlx::query_as_with::<_, Channel, _>(&sql, values)
                .fetch_all(&self.pool)
                .await?)
        })
        .await
    }

    async fn activate(&self, channel_id: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.activate", async {
            let mut tx = self.pool.begin().await?;

            // Find the draft version
            let (draft_sql, draft_values) = Query::select()
                .column(Asterisk)
                .from(Channels::Table)
                .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                .and_where(Expr::col(Channels::Status).eq("draft"))
                .build_sqlx(query_builder());

            let draft = sqlx::query_as_with::<_, Channel, _>(&draft_sql, draft_values)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| {
                    OrionError::BadRequest(format!(
                        "No draft version found for channel '{}'",
                        channel_id
                    ))
                })?;

            // Archive current active versions
            let (archive_sql, archive_values) = Query::update()
                .table(Channels::Table)
                .value(Channels::Status, "archived")
                .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                .and_where(Expr::col(Channels::Status).eq("active"))
                .build_sqlx(query_builder());

            sqlx::query_with(&archive_sql, archive_values)
                .execute(&mut *tx)
                .await?;

            // Activate the draft
            let (activate_sql, activate_values) = Query::update()
                .table(Channels::Table)
                .value(Channels::Status, "active")
                .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                .and_where(Expr::col(Channels::Version).eq(draft.version))
                .build_sqlx(query_builder());

            sqlx::query_with(&activate_sql, activate_values)
                .execute(&mut *tx)
                .await?;

            tx.commit().await?;

            self.get_version(channel_id, draft.version).await
        })
        .await
    }

    async fn archive(&self, channel_id: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.archive", async {
            let (active_sql, active_values) = Query::select()
                .column(Asterisk)
                .from(Channels::Table)
                .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                .and_where(Expr::col(Channels::Status).eq("active"))
                .order_by(Channels::Version, Order::Desc)
                .limit(1)
                .build_sqlx(query_builder());

            let active = sqlx::query_as_with::<_, Channel, _>(&active_sql, active_values)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| {
                    OrionError::BadRequest(format!(
                        "No active version found for channel '{}'",
                        channel_id
                    ))
                })?;

            let (archive_sql, archive_values) = Query::update()
                .table(Channels::Table)
                .value(Channels::Status, "archived")
                .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                .and_where(Expr::col(Channels::Status).eq("active"))
                .build_sqlx(query_builder());

            sqlx::query_with(&archive_sql, archive_values)
                .execute(&self.pool)
                .await?;

            self.get_version(channel_id, active.version).await
        })
        .await
    }

    async fn create_new_version(&self, channel_id: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.create_new_version", async {
            // Check no draft already exists
            let (draft_sql, draft_values) = Query::select()
                .column(Asterisk)
                .from(Channels::Table)
                .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
                .and_where(Expr::col(Channels::Status).eq("draft"))
                .build_sqlx(query_builder());

            let existing_draft = sqlx::query_as_with::<_, Channel, _>(&draft_sql, draft_values)
                .fetch_optional(&self.pool)
                .await?;

            if existing_draft.is_some() {
                return Err(OrionError::Conflict(format!(
                    "Channel '{}' already has a draft version",
                    channel_id
                )));
            }

            // Find the latest version to copy from
            let latest = self.get_by_id(channel_id).await?;

            let new_version = latest.version + 1;

            let description_val = optional_string_value(latest.description.as_deref());
            let methods_val = optional_string_value(latest.methods.as_deref());
            let route_pattern_val = optional_string_value(latest.route_pattern.as_deref());
            let topic_val = optional_string_value(latest.topic.as_deref());
            let consumer_group_val = optional_string_value(latest.consumer_group.as_deref());
            let workflow_id_val = optional_string_value(latest.workflow_id.as_deref());

            let (sql, values) = Query::insert()
                .into_table(Channels::Table)
                .columns([
                    Channels::ChannelId,
                    Channels::Version,
                    Channels::Name,
                    Channels::Description,
                    Channels::ChannelType,
                    Channels::Protocol,
                    Channels::Methods,
                    Channels::RoutePattern,
                    Channels::Topic,
                    Channels::ConsumerGroup,
                    Channels::TransportConfigJson,
                    Channels::WorkflowId,
                    Channels::ConfigJson,
                    Channels::Status,
                    Channels::Priority,
                ])
                .values_panic([
                    Expr::val(channel_id).into(),
                    Expr::val(new_version).into(),
                    Expr::val(latest.name.as_str()).into(),
                    Expr::val(description_val).into(),
                    Expr::val(latest.channel_type.as_str()).into(),
                    Expr::val(latest.protocol.as_str()).into(),
                    Expr::val(methods_val).into(),
                    Expr::val(route_pattern_val).into(),
                    Expr::val(topic_val).into(),
                    Expr::val(consumer_group_val).into(),
                    Expr::val(latest.transport_config_json.as_str()).into(),
                    Expr::val(workflow_id_val).into(),
                    Expr::val(latest.config_json.as_str()).into(),
                    Expr::val("draft").into(),
                    Expr::val(latest.priority).into(),
                ])
                .build_sqlx(query_builder());

            sqlx::query_with(&sql, values).execute(&self.pool).await?;

            self.get_version(channel_id, new_version).await
        })
        .await
    }

    async fn list_versions(
        &self,
        channel_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<PaginatedResult<Channel>, OrionError> {
        let limit = limit.clamp(1, 1000);
        let offset = offset.max(0);

        let (count_sql, count_values) = Query::select()
            .expr(Func::count(Expr::col(Asterisk)))
            .from(Channels::Table)
            .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
            .build_sqlx(query_builder());

        let (total,): (i64,) = sqlx::query_as_with::<_, (i64,), _>(&count_sql, count_values)
            .fetch_one(&self.pool)
            .await?;

        let (sql, values) = Query::select()
            .column(Asterisk)
            .from(Channels::Table)
            .and_where(Expr::col(Channels::ChannelId).eq(channel_id))
            .order_by(Channels::Version, Order::Desc)
            .limit(limit as u64)
            .offset(offset as u64)
            .build_sqlx(query_builder());

        let data = sqlx::query_as_with::<_, Channel, _>(&sql, values)
            .fetch_all(&self.pool)
            .await?;

        Ok(PaginatedResult {
            data,
            total,
            limit,
            offset,
        })
    }

    async fn get_active_by_name(&self, name: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.get_active_by_name", async {
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Channels::Table)
                .and_where(Expr::col(Channels::Name).eq(name))
                .and_where(Expr::col(Channels::Status).eq("active"))
                .order_by(Channels::Version, Order::Desc)
                .limit(1)
                .build_sqlx(query_builder());

            sqlx::query_as_with::<_, Channel, _>(&sql, values)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| {
                    OrionError::NotFound(format!("No active channel found with name '{}'", name))
                })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_condition_empty() {
        let filter = ChannelFilter::default();
        let cond = build_condition(&filter);
        // Build a query with the condition to verify it produces valid SQL
        let (sql, _) = Query::select()
            .column(Asterisk)
            .from(CurrentChannels::Table)
            .cond_where(cond)
            .build_sqlx(query_builder());
        // Empty Condition::all() produces WHERE TRUE -- no actual column filters
        assert!(
            !sql.contains("\"status\""),
            "empty filter should not filter by status, got: {}",
            sql
        );
    }

    #[test]
    fn test_build_condition_status() {
        let filter = ChannelFilter {
            status: Some("active".to_string()),
            ..Default::default()
        };
        let cond = build_condition(&filter);
        let (sql, _) = Query::select()
            .column(Asterisk)
            .from(CurrentChannels::Table)
            .cond_where(cond)
            .build_sqlx(query_builder());
        assert!(
            sql.contains("status"),
            "SQL should contain status filter, got: {}",
            sql
        );
    }

    #[test]
    fn test_build_condition_channel_type() {
        let filter = ChannelFilter {
            channel_type: Some("sync".to_string()),
            ..Default::default()
        };
        let cond = build_condition(&filter);
        let (sql, _) = Query::select()
            .column(Asterisk)
            .from(CurrentChannels::Table)
            .cond_where(cond)
            .build_sqlx(query_builder());
        assert!(
            sql.contains("channel_type"),
            "SQL should contain channel_type filter, got: {}",
            sql
        );
    }

    #[test]
    fn test_build_condition_protocol() {
        let filter = ChannelFilter {
            protocol: Some("rest".to_string()),
            ..Default::default()
        };
        let cond = build_condition(&filter);
        let (sql, _) = Query::select()
            .column(Asterisk)
            .from(CurrentChannels::Table)
            .cond_where(cond)
            .build_sqlx(query_builder());
        assert!(
            sql.contains("protocol"),
            "SQL should contain protocol filter, got: {}",
            sql
        );
    }

    #[test]
    fn test_build_condition_all_filters() {
        let filter = ChannelFilter {
            status: Some("draft".to_string()),
            channel_type: Some("async".to_string()),
            protocol: Some("kafka".to_string()),
            limit: Some(10),
            offset: Some(0),
            ..Default::default()
        };
        let cond = build_condition(&filter);
        let (sql, _) = Query::select()
            .column(Asterisk)
            .from(CurrentChannels::Table)
            .cond_where(cond)
            .build_sqlx(query_builder());
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
        let json = r#"{"name":"orders","channel_type":"sync","protocol":"rest"}"#;
        let req: CreateChannelRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.name, "orders");
        assert_eq!(req.channel_type, "sync");
        assert_eq!(req.protocol, "rest");
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
        let req: CreateChannelRequest = serde_json::from_str(json).unwrap();
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
        let req: UpdateChannelRequest = serde_json::from_str(json).unwrap();
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
        let req: ChannelStatusChangeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.status, "active");
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

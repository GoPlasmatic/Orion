use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::errors::OrionError;
use crate::storage::models::Channel;

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

// -- SQLite implementation --

pub struct SqliteChannelRepository {
    pool: SqlitePool,
}

impl SqliteChannelRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn build_where_clause(filter: &ChannelFilter) -> (String, Vec<String>) {
    let mut clause = String::from("WHERE 1=1");
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref status) = filter.status {
        clause.push_str(" AND status = ?");
        binds.push(status.clone());
    }
    if let Some(ref channel_type) = filter.channel_type {
        clause.push_str(" AND channel_type = ?");
        binds.push(channel_type.clone());
    }
    if let Some(ref protocol) = filter.protocol {
        clause.push_str(" AND protocol = ?");
        binds.push(protocol.clone());
    }

    (clause, binds)
}

#[async_trait]
impl ChannelRepository for SqliteChannelRepository {
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

            sqlx::query(
                r#"INSERT INTO channels (channel_id, version, name, description, channel_type, protocol, methods, route_pattern, topic, consumer_group, transport_config_json, workflow_id, config_json, status, priority)
                   VALUES (?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'draft', ?)"#,
            )
            .bind(&channel_id)
            .bind(&req.name)
            .bind(&req.description)
            .bind(&req.channel_type)
            .bind(&req.protocol)
            .bind(&methods_json)
            .bind(&req.route_pattern)
            .bind(&req.topic)
            .bind(&req.consumer_group)
            .bind(&transport_config_json)
            .bind(&req.workflow_id)
            .bind(&config_json)
            .bind(req.priority)
            .execute(&self.pool)
            .await?;

            self.get_version(&channel_id, 1).await
        })
        .await
    }

    async fn get_by_id(&self, channel_id: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.get_by_id", async {
            sqlx::query_as::<_, Channel>(
                "SELECT * FROM channels WHERE channel_id = ? ORDER BY version DESC LIMIT 1",
            )
            .bind(channel_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| OrionError::NotFound(format!("Channel '{}' not found", channel_id)))
        })
        .await
    }

    async fn get_version(&self, channel_id: &str, version: i64) -> Result<Channel, OrionError> {
        sqlx::query_as::<_, Channel>("SELECT * FROM channels WHERE channel_id = ? AND version = ?")
            .bind(channel_id)
            .bind(version)
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
            let (where_clause, binds) = build_where_clause(filter);
            let limit = filter.limit.unwrap_or(50).clamp(1, 1000);
            let offset = filter.offset.unwrap_or(0).max(0);

            let count_query = format!("SELECT COUNT(*) FROM current_channels {where_clause}");
            let mut cq = sqlx::query_as::<_, (i64,)>(&count_query);
            for b in &binds {
                cq = cq.bind(b);
            }
            let (total,) = cq.fetch_one(&self.pool).await?;

            let data_query = format!(
                "SELECT * FROM current_channels {where_clause} ORDER BY priority DESC, name ASC LIMIT ? OFFSET ?"
            );
            let mut dq = sqlx::query_as::<_, Channel>(&data_query);
            for b in &binds {
                dq = dq.bind(b);
            }
            dq = dq.bind(limit).bind(offset);
            let data = dq.fetch_all(&self.pool).await?;

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
            let existing = sqlx::query_as::<_, Channel>(
                "SELECT * FROM channels WHERE channel_id = ? AND status = 'draft'",
            )
            .bind(channel_id)
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

            sqlx::query(
                r#"UPDATE channels
                   SET name = ?, description = ?, methods = ?, route_pattern = ?,
                       topic = ?, consumer_group = ?, transport_config_json = ?,
                       workflow_id = ?, config_json = ?, priority = ?
                   WHERE channel_id = ? AND status = 'draft'"#,
            )
            .bind(name)
            .bind(description)
            .bind(&methods_json)
            .bind(route_pattern)
            .bind(topic)
            .bind(consumer_group)
            .bind(&transport_config_json)
            .bind(workflow_id)
            .bind(&config_json)
            .bind(priority)
            .bind(channel_id)
            .execute(&self.pool)
            .await?;

            self.get_version(channel_id, existing.version).await
        })
        .await
    }

    async fn delete(&self, channel_id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("channels.delete", async {
            let result = sqlx::query("DELETE FROM channels WHERE channel_id = ?")
                .bind(channel_id)
                .execute(&self.pool)
                .await?;

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
            Ok(sqlx::query_as::<_, Channel>(
                "SELECT * FROM channels WHERE status = 'active' ORDER BY priority DESC",
            )
            .fetch_all(&self.pool)
            .await?)
        })
        .await
    }

    async fn activate(&self, channel_id: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.activate", async {
            let mut tx = self.pool.begin().await?;

            let draft = sqlx::query_as::<_, Channel>(
                "SELECT * FROM channels WHERE channel_id = ? AND status = 'draft'",
            )
            .bind(channel_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| {
                OrionError::BadRequest(format!(
                    "No draft version found for channel '{}'",
                    channel_id
                ))
            })?;

            sqlx::query(
                "UPDATE channels SET status = 'archived' WHERE channel_id = ? AND status = 'active'",
            )
            .bind(channel_id)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                "UPDATE channels SET status = 'active' WHERE channel_id = ? AND version = ?",
            )
            .bind(channel_id)
            .bind(draft.version)
            .execute(&mut *tx)
            .await?;

            tx.commit().await?;

            self.get_version(channel_id, draft.version).await
        })
        .await
    }

    async fn archive(&self, channel_id: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.archive", async {
            let active = sqlx::query_as::<_, Channel>(
                "SELECT * FROM channels WHERE channel_id = ? AND status = 'active' ORDER BY version DESC LIMIT 1",
            )
            .bind(channel_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| {
                OrionError::BadRequest(format!(
                    "No active version found for channel '{}'",
                    channel_id
                ))
            })?;

            sqlx::query(
                "UPDATE channels SET status = 'archived' WHERE channel_id = ? AND status = 'active'",
            )
            .bind(channel_id)
            .execute(&self.pool)
            .await?;

            self.get_version(channel_id, active.version).await
        })
        .await
    }

    async fn create_new_version(&self, channel_id: &str) -> Result<Channel, OrionError> {
        crate::metrics::timed_db_op("channels.create_new_version", async {
            // Check no draft already exists
            let existing_draft = sqlx::query_as::<_, Channel>(
                "SELECT * FROM channels WHERE channel_id = ? AND status = 'draft'",
            )
            .bind(channel_id)
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

            sqlx::query(
                r#"INSERT INTO channels (channel_id, version, name, description, channel_type, protocol, methods, route_pattern, topic, consumer_group, transport_config_json, workflow_id, config_json, status, priority)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'draft', ?)"#,
            )
            .bind(channel_id)
            .bind(new_version)
            .bind(&latest.name)
            .bind(&latest.description)
            .bind(&latest.channel_type)
            .bind(&latest.protocol)
            .bind(&latest.methods)
            .bind(&latest.route_pattern)
            .bind(&latest.topic)
            .bind(&latest.consumer_group)
            .bind(&latest.transport_config_json)
            .bind(&latest.workflow_id)
            .bind(&latest.config_json)
            .bind(latest.priority)
            .execute(&self.pool)
            .await?;

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

        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM channels WHERE channel_id = ?")
            .bind(channel_id)
            .fetch_one(&self.pool)
            .await?;

        let data = sqlx::query_as::<_, Channel>(
            "SELECT * FROM channels WHERE channel_id = ? ORDER BY version DESC LIMIT ? OFFSET ?",
        )
        .bind(channel_id)
        .bind(limit)
        .bind(offset)
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
            sqlx::query_as::<_, Channel>(
                "SELECT * FROM channels WHERE name = ? AND status = 'active' ORDER BY version DESC LIMIT 1",
            )
            .bind(name)
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
    fn test_build_where_clause_empty() {
        let filter = ChannelFilter::default();
        let (clause, binds) = build_where_clause(&filter);
        assert_eq!(clause, "WHERE 1=1");
        assert!(binds.is_empty());
    }

    #[test]
    fn test_build_where_clause_status() {
        let filter = ChannelFilter {
            status: Some("active".to_string()),
            ..Default::default()
        };
        let (clause, binds) = build_where_clause(&filter);
        assert!(clause.contains("AND status = ?"));
        assert_eq!(binds, vec!["active"]);
    }

    #[test]
    fn test_build_where_clause_channel_type() {
        let filter = ChannelFilter {
            channel_type: Some("sync".to_string()),
            ..Default::default()
        };
        let (clause, binds) = build_where_clause(&filter);
        assert!(clause.contains("AND channel_type = ?"));
        assert_eq!(binds, vec!["sync"]);
    }

    #[test]
    fn test_build_where_clause_protocol() {
        let filter = ChannelFilter {
            protocol: Some("rest".to_string()),
            ..Default::default()
        };
        let (clause, binds) = build_where_clause(&filter);
        assert!(clause.contains("AND protocol = ?"));
        assert_eq!(binds, vec!["rest"]);
    }

    #[test]
    fn test_build_where_clause_all_filters() {
        let filter = ChannelFilter {
            status: Some("draft".to_string()),
            channel_type: Some("async".to_string()),
            protocol: Some("kafka".to_string()),
            limit: Some(10),
            offset: Some(0),
        };
        let (clause, binds) = build_where_clause(&filter);
        assert!(clause.contains("AND status = ?"));
        assert!(clause.contains("AND channel_type = ?"));
        assert!(clause.contains("AND protocol = ?"));
        assert_eq!(binds.len(), 3);
        assert_eq!(binds[0], "draft");
        assert_eq!(binds[1], "async");
        assert_eq!(binds[2], "kafka");
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

//! Column identifiers for every Orion-owned table, as sea-query `Iden`s.
//!
//! # The `_json` suffix is load-bearing
//!
//! Every column here that holds a serialized JSON document is named `*_json`,
//! and every column named `*_json` holds one (D26). It is the only signal a
//! reader gets that the value has to go through `serde_json` before it means
//! anything — the storage type is `text` either way, so nothing else
//! distinguishes `tasks_json` from `name`.
//!
//! `workflows.tags` and `channels.methods` were the two exceptions until 1.0.0
//! and are now `tags_json` / `methods_json`. The wire format is unchanged: the
//! admin API still says `tags` and `methods`, because the DTOs in
//! [`super::models::dto`] name their own fields and are the only types that
//! reach the network.
//!
//! Two tests hold the line: `column_identifiers_are_pinned` in this module,
//! and `json_columns_carry_the_json_suffix` in [`super::models::dto`], which
//! enforces the rule for any column added later.

use sea_query::Iden;

// ============================================================
// Workflows table
// ============================================================

#[derive(Iden)]
pub enum Workflows {
    Table,
    WorkflowId,
    Version,
    Name,
    Description,
    Priority,
    Status,
    RolloutPercentage,
    ConditionJson,
    TasksJson,
    TagsJson,
    LoopJson,
    ContinueOnError,
    CreatedAt,
    UpdatedAt,
}

// ============================================================
// Channels table
// ============================================================

#[derive(Iden)]
pub enum Channels {
    Table,
    ChannelId,
    Version,
    Name,
    Description,
    ChannelType,
    Protocol,
    MethodsJson,
    RoutePattern,
    Topic,
    ConsumerGroup,
    TransportConfigJson,
    WorkflowId,
    ConfigJson,
    Status,
    Priority,
    TagsJson,
    CreatedAt,
    UpdatedAt,
}

// ============================================================
// Connectors table
// ============================================================

#[derive(Iden)]
pub enum Connectors {
    Table,
    Id,
    Name,
    ConnectorType,
    ConfigJson,
    Enabled,
    TagsJson,
    CreatedAt,
    UpdatedAt,
}

// ============================================================
// Connector OAuth2 runtime state (#268)
// ============================================================

#[derive(Iden)]
pub enum ConnectorOauthState {
    Table,
    ConnectorName,
    Fingerprint,
    StateJson,
    UpdatedAt,
}

// ============================================================
// Traces table
// ============================================================

#[derive(Iden)]
pub enum Traces {
    Table,
    Id,
    Channel,
    ChannelId,
    Mode,
    Status,
    InputJson,
    ResultJson,
    ErrorMessage,
    DurationMs,
    StartedAt,
    CompletedAt,
    CreatedAt,
    UpdatedAt,
    TaskTraceJson,
    AccessTokenHash,
}

// ============================================================
// Trace DLQ table
// ============================================================

#[derive(Iden)]
pub enum TraceDlq {
    Table,
    Id,
    TraceId,
    Channel,
    PayloadJson,
    MetadataJson,
    ErrorMessage,
    RetryCount,
    MaxRetries,
    NextRetryAt,
    CreatedAt,
    UpdatedAt,
    ClaimedBy,
    ClaimedUntil,
}

// ============================================================
// Cluster coordination tables
// ============================================================

#[derive(Iden, Clone, Copy)]
pub enum ConfigEpoch {
    Table,
    Id,
    Epoch,
    /// What the bumping node changed, so a peer can scope its resync.
    EpochScope,
    BreakerEpoch,
    BreakerKey,
    UpdatedAt,
}

#[derive(Iden)]
pub enum JobLeases {
    Table,
    JobName,
    Holder,
    ExpiresAt,
}

// ============================================================
// Packages table (K14 receipts)
// ============================================================

#[derive(Iden)]
pub enum Packages {
    Table,
    Name,
    Version,
    ContentHash,
    State,
    Principal,
    CreatedAt,
    UpdatedAt,
}

// ============================================================
// Audit Logs table
// ============================================================

#[derive(Iden)]
pub enum AuditLogs {
    Table,
    Id,
    Principal,
    Action,
    ResourceType,
    ResourceId,
    Details,
    CreatedAt,
}

// ============================================================
// Views
// ============================================================

#[derive(Iden)]
pub enum CurrentWorkflows {
    Table,
}

#[derive(Iden)]
pub enum CurrentChannels {
    Table,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The physical column names of the two versioned tables, pinned (D26).
    ///
    /// These identifiers are the only thing standing between a repository and
    /// the database, and they have to move in lockstep with three hand-written
    /// migration sets. Renaming a variant here without shipping
    /// `sqlite/009`, `postgres/013` and `mysql/011` — or the reverse — is a
    /// change no type checks, because a sea-query `Iden` compiles to a string.
    /// So the strings live here, once, and a rename has to be deliberate.
    ///
    /// Two entries changed for 1.0.0 — `tags_json` and `methods_json` (D26).
    /// The rule that put them there is enforced for future columns by
    /// `json_columns_carry_the_json_suffix` in `storage::models::dto`; this
    /// test is what makes a revert of *these two* fail by name.
    ///
    /// The suffix belongs to the column and never to the wire: the fields the
    /// admin API publishes are still `tags` and `methods`, which
    /// `wire_names_survive_the_column_rename` in the integration suite asserts
    /// against a live response.
    #[test]
    fn column_identifiers_are_pinned() {
        let workflows: Vec<String> = [
            Iden::to_string(&Workflows::WorkflowId),
            Iden::to_string(&Workflows::Version),
            Iden::to_string(&Workflows::Name),
            Iden::to_string(&Workflows::Description),
            Iden::to_string(&Workflows::Priority),
            Iden::to_string(&Workflows::Status),
            Iden::to_string(&Workflows::RolloutPercentage),
            Iden::to_string(&Workflows::ConditionJson),
            Iden::to_string(&Workflows::TasksJson),
            Iden::to_string(&Workflows::TagsJson),
            Iden::to_string(&Workflows::LoopJson),
            Iden::to_string(&Workflows::ContinueOnError),
            Iden::to_string(&Workflows::CreatedAt),
            Iden::to_string(&Workflows::UpdatedAt),
        ]
        .to_vec();
        assert_eq!(
            workflows,
            [
                "workflow_id",
                "version",
                "name",
                "description",
                "priority",
                "status",
                "rollout_percentage",
                "condition_json",
                "tasks_json",
                "tags_json",
                "loop_json",
                "continue_on_error",
                "created_at",
                "updated_at",
            ]
        );

        let channels: Vec<String> = [
            Iden::to_string(&Channels::ChannelId),
            Iden::to_string(&Channels::Version),
            Iden::to_string(&Channels::Name),
            Iden::to_string(&Channels::Description),
            Iden::to_string(&Channels::ChannelType),
            Iden::to_string(&Channels::Protocol),
            Iden::to_string(&Channels::MethodsJson),
            Iden::to_string(&Channels::RoutePattern),
            Iden::to_string(&Channels::Topic),
            Iden::to_string(&Channels::ConsumerGroup),
            Iden::to_string(&Channels::TransportConfigJson),
            Iden::to_string(&Channels::WorkflowId),
            Iden::to_string(&Channels::ConfigJson),
            Iden::to_string(&Channels::Status),
            Iden::to_string(&Channels::Priority),
            Iden::to_string(&Channels::TagsJson),
            Iden::to_string(&Channels::CreatedAt),
            Iden::to_string(&Channels::UpdatedAt),
        ]
        .to_vec();
        assert_eq!(
            channels,
            [
                "channel_id",
                "version",
                "name",
                "description",
                "channel_type",
                "protocol",
                "methods_json",
                "route_pattern",
                "topic",
                "consumer_group",
                "transport_config_json",
                "workflow_id",
                "config_json",
                "status",
                "priority",
                "tags_json",
                "created_at",
                "updated_at",
            ]
        );
    }
}

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

mod entities;
mod runtime;

pub use entities::{
    Channels, ConnectorOauthState, Connectors, CurrentChannels, CurrentWorkflows, Packages,
    PluginArtifacts, Plugins, Workflows,
};
pub use runtime::{
    AuditLogs, ClusterKv, ClusterRateWindows, ConfigEpoch, CronOccurrences, CronScheduleState,
    CronSingletonSlots, CronSingletons, JobLeases, SourceCheckpoints, SourceClaims, TraceDlq,
    Traces,
};

#[cfg(test)]
mod tests {
    use sea_query::Iden;

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
        let occurrences = [
            Iden::to_string(&CronOccurrences::Id),
            Iden::to_string(&CronOccurrences::ChannelId),
            Iden::to_string(&CronOccurrences::ChannelName),
            Iden::to_string(&CronOccurrences::ChannelVersion),
            Iden::to_string(&CronOccurrences::ExecutingVersion),
            Iden::to_string(&CronOccurrences::WorkflowId),
            Iden::to_string(&CronOccurrences::Trigger),
            Iden::to_string(&CronOccurrences::ScheduledFor),
            Iden::to_string(&CronOccurrences::Status),
            Iden::to_string(&CronOccurrences::Attempt),
            Iden::to_string(&CronOccurrences::ClaimedBy),
            Iden::to_string(&CronOccurrences::ClaimedUntil),
            Iden::to_string(&CronOccurrences::SingletonKey),
            Iden::to_string(&CronOccurrences::FencingToken),
            Iden::to_string(&CronOccurrences::TraceId),
            Iden::to_string(&CronOccurrences::ErrorMessage),
            Iden::to_string(&CronOccurrences::StartedAt),
            Iden::to_string(&CronOccurrences::CompletedAt),
            Iden::to_string(&CronOccurrences::CreatedAt),
            Iden::to_string(&CronOccurrences::UpdatedAt),
        ]
        .to_vec();
        assert_eq!(
            occurrences,
            [
                "id",
                "channel_id",
                "channel_name",
                "channel_version",
                "executing_version",
                "workflow_id",
                "trigger",
                "scheduled_for",
                "status",
                "attempt",
                "claimed_by",
                "claimed_until",
                "singleton_key",
                "fencing_token",
                "trace_id",
                "error_message",
                "started_at",
                "completed_at",
                "created_at",
                "updated_at",
            ]
        );

        assert_eq!(
            [
                Iden::to_string(&CronSingletons::SingletonKey),
                Iden::to_string(&CronSingletons::OccurrenceId),
                Iden::to_string(&CronSingletons::Holder),
                Iden::to_string(&CronSingletons::FencingToken),
                Iden::to_string(&CronSingletons::LeaseUntil),
            ]
            .to_vec(),
            [
                "singleton_key",
                "occurrence_id",
                "holder",
                "fencing_token",
                "lease_until",
            ]
        );

        assert_eq!(
            [
                Iden::to_string(&CronSingletonSlots::Table),
                Iden::to_string(&CronSingletonSlots::SingletonKey),
                Iden::to_string(&CronSingletonSlots::Slot),
                Iden::to_string(&CronSingletonSlots::OccurrenceId),
                Iden::to_string(&CronSingletonSlots::Holder),
                Iden::to_string(&CronSingletonSlots::FencingToken),
                Iden::to_string(&CronSingletonSlots::LeaseUntil),
                Iden::to_string(&CronSingletonSlots::UpdatedAt),
                Iden::to_string(&CronOccurrences::SingletonSlot),
            ]
            .to_vec(),
            [
                "cron_singleton_slots",
                "singleton_key",
                "slot",
                "occurrence_id",
                "holder",
                "fencing_token",
                "lease_until",
                "updated_at",
                "singleton_slot",
            ]
        );

        assert_eq!(
            [
                Iden::to_string(&CronScheduleState::ChannelId),
                Iden::to_string(&CronScheduleState::ChannelVersion),
                Iden::to_string(&CronScheduleState::ConfigHash),
                Iden::to_string(&CronScheduleState::NextFireAt),
                Iden::to_string(&CronScheduleState::PausedAt),
            ]
            .to_vec(),
            [
                "channel_id",
                "channel_version",
                "config_hash",
                "next_fire_at",
                "paused_at",
            ]
        );

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

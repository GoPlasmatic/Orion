//! Column identifiers for operational runtime, scheduling, and cluster tables (`MOD-05`).

use sea_query::Iden;

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
// Cron scheduling tables
// ============================================================

/// The per-channel cursor: where a schedule has got to.
#[derive(Iden, Clone, Copy)]
pub enum CronScheduleState {
    Table,
    ChannelId,
    ChannelVersion,
    ConfigHash,
    NextFireAt,
    PausedAt,
    UpdatedAt,
}

/// The durable run ledger and work queue.
#[derive(Iden, Clone, Copy)]
pub enum CronOccurrences {
    Table,
    Id,
    ChannelId,
    ChannelName,
    ChannelVersion,
    ExecutingVersion,
    WorkflowId,
    /// Quoted by sea-query on every backend, which matters here: `trigger` is a
    /// reserved word in MySQL.
    Trigger,
    ScheduledFor,
    Status,
    Attempt,
    ClaimedBy,
    ClaimedUntil,
    SingletonKey,
    FencingToken,
    /// The slot of `singleton_key` the attempt held; NULL under `allow`.
    SingletonSlot,
    TraceId,
    ErrorMessage,
    StartedAt,
    CompletedAt,
    CreatedAt,
    UpdatedAt,
}

/// One row per held singleton key.
#[derive(Iden, Clone, Copy)]
pub enum CronSingletons {
    Table,
    SingletonKey,
    OccurrenceId,
    Holder,
    FencingToken,
    LeaseUntil,
    UpdatedAt,
}

/// Slots `1..N` of a singleton key with `concurrency.slots > 1`. Slot 0 is
/// the `cron_singletons` row.
#[derive(Iden, Clone, Copy)]
pub enum CronSingletonSlots {
    Table,
    SingletonKey,
    Slot,
    OccurrenceId,
    Holder,
    FencingToken,
    LeaseUntil,
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
    /// The epoch [`ConfigEpoch::EpochScope`] was written for. A scope is only
    /// trustworthy when this matches [`ConfigEpoch::Epoch`] *and* the reader is
    /// applying that one epoch — see
    /// [`crate::cluster::EpochScope::for_advance`].
    EpochScopeAt,
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

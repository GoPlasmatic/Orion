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
    Tags,
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
    Methods,
    RoutePattern,
    Topic,
    ConsumerGroup,
    TransportConfigJson,
    WorkflowId,
    ConfigJson,
    Status,
    Priority,
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
    CreatedAt,
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

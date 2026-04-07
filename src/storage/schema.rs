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

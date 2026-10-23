//! Column identifiers for versioned and catalog entities (`MOD-05`).

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
    InventoryJson,
}

// ============================================================
// Views — retained for the contract phase only
// ============================================================

// §5: no repository reads these any more. `versioned::is_current_version` is
// the "latest version per id" predicate that replaced them, applied to the
// base table, so a new column reaches every reader without a view to recreate
// — the thing that made these expensive on Postgres and MySQL, where a
// `SELECT *` view resolves its column list at CREATE time.
//
// The views themselves stay in the schema for one release: a rolling deploy
// runs the previous binary against this database, and that binary still reads
// them. The migration that drops them is the contract half, and it also
// deletes these two idens.

#[derive(Iden)]
pub enum CurrentWorkflows {
    Table,
}

#[derive(Iden)]
pub enum CurrentChannels {
    Table,
}

// ============================================================
// Plugins: the versioned entity, and the content-addressed artifacts
// ============================================================

#[derive(Iden)]
pub enum Plugins {
    Table,
    PluginId,
    Version,
    Status,
    Digest,
    ManifestJson,
    TagsJson,
    /// Optional detached signature over `digest` (`[plugins.trust]`).
    Signature,
    CreatedAt,
    UpdatedAt,
}

/// Component bytes by digest. No `current_*` view and no iden for one: the
/// entity was added after `versioned::is_current_version` replaced the views.
#[derive(Iden)]
pub enum PluginArtifacts {
    Table,
    Digest,
    Bytes,
    Size,
    CreatedAt,
}

// ============================================================
// Models: the versioned entity; its bytes live in a bucket, not here
// ============================================================

#[derive(Iden)]
pub enum Models {
    Table,
    ModelId,
    Version,
    Status,
    Digest,
    ManifestJson,
    /// The artifact reference: `{"connector","key","digest","size"}`.
    ArtifactJson,
    /// The admission verdict — derived, outside the immutability trigger.
    AdmissionJson,
    /// What admission read out of the model — derived, nullable.
    StatsJson,
    TagsJson,
    /// Optional detached signature over `digest`.
    Signature,
    CreatedAt,
    UpdatedAt,
}

//! Column identifiers for every Orion-owned table, as sea-query `Iden`s.
//!
//! # The `_json` suffix is load-bearing
//!
//! Every column here that holds a serialized JSON document is named `*_json`,
//! and every column named `*_json` holds one (D26). It is the only signal a
//! reader gets that the value has to go through `serde_json` before it means
//! anything — the storage type is `text` either way, so nothing else
//! distinguishes `tasks_json` from `name`. Keep the rule for every column
//! added.

mod deployments;
mod runtime;

pub use deployments::{
    ArtifactObjects, ConnectionRevisions, DeploymentHead, DeploymentMembers, DeploymentOperations,
    DeploymentRevisions, ProviderAdmissions, ProviderPackages,
};
pub use runtime::{
    AuditLogs, ClusterKv, ClusterRateWindows, ConfigEpoch, ConnectorOauthState, CronOccurrences,
    CronScheduleState, CronSingletonSlots, CronSingletons, JobLeases, RuntimeNodes,
    SourceCheckpoints, SourceClaims, TraceDlq, Traces,
};

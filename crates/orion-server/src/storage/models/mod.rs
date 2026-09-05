//! Storage models, split by the job each type does (D28).
//!
//! Before this split one file held row structs, response DTOs, domain enums,
//! wire constants and a handler helper, and two more row structs lived in the
//! repositories that happened to read them. There was no answer to "where does
//! a new type go?", and the cost of that showed up as [`rows::Trace`] doing
//! three jobs at once — a table picture, a wire shape, and the carrier of a
//! credential verifier (D27).
//!
//! The rule, by module:
//!
//! | Module | Holds | Derives |
//! |---|---|---|
//! | [`rows`] | one struct per `SELECT` shape | `FromRow`, never `Serialize`/`ToSchema` |
//! | [`dto`] | one struct per response body | `Serialize + ToSchema` |
//! | [`enums`] | closed value sets and the string constants for them | both, freely |
//!
//! **Request** DTOs stay next to the repository method that consumes them:
//! they are that method's input contract, they carry its validation, and they
//! have no row to be confused with.
//!
//! A row reaches the wire only through a `From`/`TryFrom` into a [`dto`] type.
//! That is enforced, not merely documented — see
//! `rows::tests::row_structs_are_not_wire_types` and
//! `no_storage_row_struct_is_published_unless_it_is_the_wire_shape` in
//! `server::routes::openapi`.

pub mod dto;
pub mod enums;
pub mod rows;

pub use dto::{
    AuditLogEntryResponse, ChannelResponse, ConnectorResponse, CronOccurrenceResponse,
    CronOccurrenceSummaryResponse, CronScheduleStatusResponse, PackageReceiptResponse,
    PluginHealth, PluginResponse, TraceDlqEntryResponse, TraceDlqSummaryResponse,
    TraceListItemResponse, WorkflowResponse,
};
pub use enums::{
    CHANNEL_TYPE_ASYNC, CHANNEL_TYPE_SYNC, ChannelProtocol, ChannelType, EntityStatus,
    PackageState, TRACE_MODE_ASYNC, TRACE_MODE_CRON, TRACE_MODE_SYNC, TRACE_STATUS_COMPLETED,
    TRACE_STATUS_FAILED, TRACE_STATUS_PENDING, TRACE_STATUS_RUNNING, VALID_CHANNEL_TYPES,
};
pub use rows::{
    AuditLogEntry, Channel, Connector, ConnectorOauthStateRow, CronOccurrence, CronScheduleState,
    CronSingleton, PackageReceipt, Plugin, PluginArtifact, Trace, TraceDlqEntry, TraceDlqSummary,
    TraceListRow, Workflow,
};

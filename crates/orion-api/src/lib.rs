//! The Orion wire contract: every JSON shape the HTTP API serves that a
//! client is expected to read back.
//!
//! This crate is the single definition of what travels between `orion-server`
//! and its clients (`orion-cli`, the MCP tools, generated SDKs). The server
//! serializes these types; clients deserialize them. Before this crate
//! existed the CLI re-derived all of it as string literals — including three
//! error codes the server never emits — which is exactly the drift a shared
//! contract makes impossible.
//!
//! What lives here, by module:
//!
//! | Module | Holds |
//! |---|---|
//! | [`dto`] | one struct per response body the admin API serves |
//! | [`enums`] | closed value sets (status, protocol, …) and their string constants |
//! | [`envelope`] | the `{"data": …}` and paginated envelopes admin 2xx responses carry |
//! | [`error`] | the `{"error": {code, message, details[], request_id}}` envelope and the stable code registry |
//! | [`import`] | the bulk-import report and its vocabulary (`on_conflict`, per-item actions) |
//!
//! What deliberately does **not** live here: the server's `OrionError` enum
//! (its internal error domain — only the serialized envelope is contract),
//! database row types, request validation, and any I/O.
//!
//! Deserialization is tolerant by design: every field defaults, unknown
//! fields are ignored. A v1.0 CLI must keep reading pre-1.0 servers and a
//! v1.0 server's responses must keep parsing in older clients — version skew
//! across the fleet is normal during a rollout, not an error.
//!
//! The `utoipa` feature adds `ToSchema` derives so the server can publish
//! these exact types in its OpenAPI document; clients leave it off.

pub mod dto;
pub mod enums;
pub mod envelope;
pub mod error;
pub mod import;

pub use dto::{
    AuditLogEntryResponse, ChannelResponse, ConnectorResponse, PackageReceiptResponse,
    TraceDlqEntryResponse, TraceDlqSummaryResponse, TraceListItemResponse, WorkflowResponse,
};
pub use enums::{
    CHANNEL_TYPE_ASYNC, CHANNEL_TYPE_SYNC, ChannelProtocol, ChannelType, EntityStatus,
    PackageState, STATUS_ACTIVE, STATUS_ARCHIVED, STATUS_DRAFT, TRACE_MODE_ASYNC, TRACE_MODE_SYNC,
    TRACE_STATUS_COMPLETED, TRACE_STATUS_FAILED, TRACE_STATUS_PENDING, TRACE_STATUS_RUNNING,
    VALID_CHANNEL_TYPES,
};
pub use envelope::{Data, Paginated};
pub use error::{ErrorBody, ErrorEnvelope, FieldError, codes};
pub use import::{ImportAction, ImportItemError, ImportItemResult, ImportResult, OnConflict};

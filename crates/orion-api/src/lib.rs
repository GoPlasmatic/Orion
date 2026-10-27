//! The Orion wire contract: every JSON shape the HTTP API serves that a
//! client is expected to read back.
//!
//! This crate is the single definition of what travels between `orion-server`
//! and its clients (`orion-cli`, generated SDKs). The server
//! serializes these types; clients deserialize them, rather than re-deriving
//! the shapes and codes as string literals that drift from what the server
//! emits.
//!
//! What lives here, by module:
//!
//! | Module | Holds |
//! |---|---|
//! | [`digest`] | [`Digest`], the checked `sha256:<64 lowercase hex>` content digest |
//! | [`dto`] | one struct per response body the admin API serves |
//! | [`enums`] | closed value sets (status, protocol, …) and their string constants |
//! | [`envelope`] | the `{"data": …}` and paginated envelopes admin 2xx responses carry |
//! | [`error`] | the `{"error": {code, message, details[], request_id}}` envelope and the stable code registry |
//! | [`v2`] | the strict, versioned `/api/v2/admin` contract: deployments, connections, connector packages |
//!
//! What deliberately does **not** live here: the server's `OrionError` enum
//! (its internal error domain — only the serialized envelope is contract),
//! database row types, request validation, and any I/O.
//!
//! **Two disciplines, by surface.**
//!
//! The operational DTOs — [`dto`] and [`engine`]: traces, cron, health,
//! audit logs, the DLQ, engine status, functions — predate the v2 contract
//! and stay **tolerant**: every field carries `#[serde(default)]`, unknown
//! fields are ignored, and the fields that hold a growing vocabulary —
//! `status`, `state`, `channel_type`, `mode` — are `String` on the wire
//! rather than an enum. A CLI must keep reading an older server there, and
//! the other way round; compare those strings against the constants in
//! [`enums`], and an unrecognised value degrades to "not one I know"
//! instead of failing the whole response.
//!
//! The v2 admin contract in [`v2`] is **strict**: fields are required
//! unless their absence is documented to mean something, requests refuse
//! unknown fields, closed vocabularies are enums and server-generated parts
//! are typed. It does not absorb version skew with defaults; it negotiates
//! it. A server reports [`v2::API_VERSION`] in
//! `GET /api/v2/admin/runtime-capabilities`, and a client that speaks
//! another revision refuses to go on rather than misread an answer.
//!
//! The [`enums`] types themselves are **strict on purpose**, and are not part
//! of that tolerance. They exist for the request side and for server-internal
//! use, where an unrecognised value is a caller error worth a `400` — a
//! `PATCH /status` body of `{"status": "activ"}` must be refused, not quietly
//! accepted as an unknown variant. Their `Deserialize` impls return
//! `unknown_variant` deliberately.
//!
//! The `utoipa` feature adds `ToSchema` derives so the server can publish
//! these exact types in its OpenAPI document; clients leave it off.

pub mod digest;
pub mod dto;
pub mod engine;
pub mod enums;
pub mod envelope;
pub mod error;
pub mod v2;

pub use digest::{Digest, DigestError};
pub use dto::{
    AuditLogEntryResponse, BackupFileResponse, BackupListItemResponse, CircuitBreakerResetResponse,
    CircuitBreakerStatesResponse, CronOccurrenceResponse, CronOccurrenceSummaryResponse,
    CronScheduleStatusResponse, DlqPurgeResponse, HealthResponse, LivenessResponse,
    ReadinessComponents, ReadinessResponse, TraceDetailResponse, TraceDlqEntryResponse,
    TraceDlqSummaryResponse, TraceListItemResponse,
};
pub use engine::{
    CacheInvalidatedResponse, ChannelLoadIssueResponse, ConnectorLoadIssueResponse,
    EngineCapabilities, EngineLoadIssues, EngineStatusResponse, PluginLoadIssueResponse,
};
pub use enums::{
    CHANNEL_TYPE_ASYNC, CHANNEL_TYPE_SYNC, ChannelType, EntityStatus, STATUS_ACTIVE,
    STATUS_ARCHIVED, STATUS_DRAFT, TRACE_MODE_ASYNC, TRACE_MODE_CRON, TRACE_MODE_SYNC,
    TRACE_STATUS_COMPLETED, TRACE_STATUS_FAILED, TRACE_STATUS_PENDING, TRACE_STATUS_RUNNING,
    VALID_CHANNEL_TYPES,
};
pub use envelope::{Data, Paginated, TracePage};
pub use error::{ErrorBody, ErrorEnvelope, ErrorKind, FieldError, V2Code, codes, field_codes};

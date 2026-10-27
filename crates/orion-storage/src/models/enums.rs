//! Domain vocabulary: the small closed sets that both a row and a DTO spell,
//! plus the string constants the SQL and the wire agree on.
//!
//! The definitions live in the shared `orion-api` crate so the CLI reads the
//! same vocabulary the server writes; they are re-exported here under their
//! established paths.

pub use orion_api::enums::{
    CHANNEL_TYPE_ASYNC, CHANNEL_TYPE_SYNC, ChannelType, EntityStatus, STATUS_ACTIVE,
    STATUS_ARCHIVED, STATUS_DRAFT, TRACE_MODE_ASYNC, TRACE_MODE_CRON, TRACE_MODE_SYNC,
    TRACE_STATUS_COMPLETED, TRACE_STATUS_FAILED, TRACE_STATUS_PENDING, TRACE_STATUS_RUNNING,
    VALID_CHANNEL_TYPES,
};

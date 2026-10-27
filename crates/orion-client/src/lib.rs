//! The one HTTP client for Orion's admin and data APIs.
//!
//! Every caller that speaks the wire protocol — parsing the `{"error": …}`
//! envelope, unwrapping the `{"data": …}` one — goes through this transport,
//! so there is one implementation of it rather than one per caller:
//!
//! - [`OrionClient`] — auth (API key / bearer, custom header), optional
//!   `X-Orion-Change-Context`, configurable timeout, and the request verbs.
//! - [`ClientError`] — a typed error that keeps the server's error envelope
//!   ([`orion_api::ErrorBody`]) structured, so callers branch on
//!   `status`/`code` instead of matching prose.
//! - [`paths`] — every endpoint path, built in one place instead of format
//!   strings scattered through call sites.
//! - [`V2Admin`] — one typed method per v2 admin operation, over the
//!   `orion_api::v2` request and response DTOs.
//!
//! Two families of verbs, matching the two things a caller can want:
//!
//! - `get`/`post`/`put`/`patch`/… return the **full response body** — for
//!   passthrough tools (the CLI prints many responses verbatim).
//! - `get_data`/`post_data`/… **unwrap the `{"data": …}` envelope** every
//!   admin 2xx carries, tolerating the bare pre-1.0 shape — for callers that
//!   consume the payload.
//!
//! Presentation stays out: hints, colors, and message wording belong to the
//! binaries. This crate reports *what happened*; the CLI decides how to say
//! it.

mod client;
mod error;
pub mod paths;
mod v2;

pub use client::OrionClient;
pub use error::ClientError;
pub use paths::query_string;
pub use reqwest::{Method, StatusCode};
pub use v2::{ApiVersionMismatch, V2Admin, check_api_version};

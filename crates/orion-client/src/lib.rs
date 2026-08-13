//! The one HTTP client for Orion's admin and data APIs.
//!
//! Before this crate the workspace carried two hand-rolled clients for the
//! same wire protocol: `orion-cli`'s `OrionClient` and a second reqwest
//! wrapper embedded in the server's `package_cli`. Both parsed the same
//! `{"error": …}` envelope and unwrapped the same `{"data": …}` envelope,
//! independently. This crate is that transport, once:
//!
//! - [`OrionClient`] — auth (API key / bearer, custom header), optional
//!   `X-Orion-Change-Context`, configurable timeout, and the request verbs.
//! - [`ClientError`] — a typed error that keeps the server's error envelope
//!   ([`orion_api::ErrorBody`]) structured, so callers branch on
//!   `status`/`code` instead of matching prose.
//! - [`paths`] — every endpoint path, built in one place instead of format
//!   strings scattered through call sites.
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

pub use client::OrionClient;
pub use error::ClientError;
pub use paths::query_string;
pub use reqwest::StatusCode;

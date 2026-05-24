#![warn(clippy::unwrap_used, clippy::panic)]
// Test code legitimately uses unwrap/panic/etc. for assertion shorthand;
// silence the warns crate-wide under cfg(test) so individual test modules
// don't need their own `#[allow(...)]` markers.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::panic,
        clippy::field_reassign_with_default,
        clippy::needless_update,
        clippy::unnecessary_get_then_check
    )
)]

pub mod channel;
pub mod config;
pub mod connector;
pub mod engine;
pub mod errors;
pub mod kafka;
pub mod metrics;
pub mod queue;
pub mod server;
pub mod storage;
pub mod validation;

pub mod config;
pub mod dedup;
pub mod registry;
pub mod routing;

use std::num::NonZeroU32;

use governor::clock::DefaultClock;
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};

// Re-export all public types so that `crate::channel::*` paths continue working.
pub use config::{
    BackpressureConfig, ChannelCacheConfig, ChannelConfig, ChannelCorsConfig,
    ChannelRateLimitConfig, CompressionConfig, DeduplicationConfig,
};
pub use dedup::DeduplicationStore;
pub use registry::{ChannelRegistry, ChannelRuntimeConfig};
pub use routing::{RouteMatch, RouteTable};

/// Keyed rate limiter type — shared with rate_limit middleware.
pub type KeyedLimiter = RateLimiter<String, DashMapStateStore<String>, DefaultClock>;

/// Build a keyed rate limiter with the given RPS and burst values.
/// Shared between per-channel (DB-driven) and platform-level (config-driven) limiters.
pub fn build_keyed_limiter(rps: u32, burst: u32) -> KeyedLimiter {
    let quota = Quota::per_second(NonZeroU32::new(rps).unwrap_or(NonZeroU32::MIN))
        .allow_burst(NonZeroU32::new(burst).unwrap_or(NonZeroU32::MIN));
    RateLimiter::dashmap(quota)
}

pub mod auth;
pub mod config;
pub mod cookies;
pub mod cron;
pub mod error_body;
pub mod guards;
pub mod oauth2_login;
pub mod rate_limit_backend;
pub mod registry;
pub mod routing;

use std::num::NonZeroU32;

use governor::clock::DefaultClock;
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};

// Re-export all public types so that `crate::channel::*` paths continue working.
pub use config::{
    BackendErrorPolicy, BackpressureConfig, BodyMode, ChannelCacheConfig, ChannelConfig,
    ChannelRateLimitConfig, ChannelRequestConfig, DeduplicationConfig, IdTokenConfig,
    OAuth2LoginConfig, ReturnToConfig, StateCookieConfig,
};
pub use cron::{
    ConcurrencyConfig, ConcurrencyPolicy, CronDescriptor, CronIdentity, CronTransportConfig,
    MisfirePolicy, PassPlan, SkipSummary,
};
pub use oauth2_login::{CompiledOAuth2Login, Leg as OAuthLeg};
pub use rate_limit_backend::{LocalRateLimitBackend, RateLimitBackend, RedisRateLimitBackend};
pub use registry::{
    ChannelLoadIssue, ChannelLoader, ChannelRuntimeConfig, ChannelSnapshot, ClusterBackends,
    ReloadDeps,
};
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

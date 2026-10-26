//! The platform rate limiters (`[rate_limit]`): the global default and the
//! admin and data route-group budgets, keyed by client IP. The HTTP layer's
//! middleware applies them; the node holds them and prunes them.

use std::sync::Arc;

use super::{KeyedLimiter, build_keyed_limiter};
use crate::config::RateLimitConfig;

/// Holds platform-level rate limiters (global defaults + endpoint-level).
/// Per-channel rate limiters live in `ChannelSnapshot` and are applied by the
/// ingress guards, not here.
///
/// `trusted_proxies` deliberately does **not** live here even though the key
/// is spelled `rate_limit.trusted_proxies`: it decides whether a forwarded
/// header may name the client, which the per-channel limit asks with the
/// platform limiter off. It is on `AppState`
/// ([`crate::runtime::state::AppStateInner::trusted_proxies`]).
pub struct RateLimitState {
    pub default_limiter: Arc<KeyedLimiter>,
    pub admin_limiter: Option<Arc<KeyedLimiter>>,
    pub data_limiter: Option<Arc<KeyedLimiter>>,
}

impl RateLimitState {
    /// Build platform-level rate limiters from config.
    pub fn from_config(config: &RateLimitConfig) -> Self {
        let default_limiter = Arc::new(build_keyed_limiter(
            config.default_rps,
            config.default_burst,
        ));

        let admin_limiter = config
            .endpoints
            .admin_rps
            .map(|rps| Arc::new(build_keyed_limiter(rps, rps / 2 + 1)));

        let data_limiter = config
            .endpoints
            .data_rps
            .map(|rps| Arc::new(build_keyed_limiter(rps, rps / 2 + 1)));

        Self {
            default_limiter,
            admin_limiter,
            data_limiter,
        }
    }

    /// Prune every platform limiter's keyed store. The default limiter lives
    /// for the whole process and keys on client IP, so without a periodic
    /// sweep it grows one entry per address ever seen — unbounded behind a
    /// proxy that forwards a caller-influenced identity. Called from the
    /// supervised limiter-prune task alongside the per-channel limiters.
    pub fn prune(&self) {
        for limiter in [
            Some(&self.default_limiter),
            self.admin_limiter.as_ref(),
            self.data_limiter.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            limiter.retain_recent();
            limiter.shrink_to_fit();
        }
    }
}

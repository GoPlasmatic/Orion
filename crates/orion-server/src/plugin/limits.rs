//! What one invocation may consume, and the host state that enforces it.
//!
//! A [`Limits`] value is the host ceiling from `[plugins]` narrowed by the
//! plugin's override, computed once per handler at load. The store-side half
//! is [`HostState`]: Wasmtime asks its `ResourceLimiter` before every memory
//! or table growth, and the answer here is the one place a guest's memory is
//! bounded — the pooling allocator's per-instance reservation is the same
//! number, so a guest cannot grow past it even if this said yes.

use std::time::Duration;

use wasmtime::ResourceLimiter;

use crate::config::PluginsConfig;

/// The effective ceilings for one plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_memory_bytes: usize,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub timeout: Duration,
    pub max_concurrency: u32,
    pub fuel: u64,
}

impl Limits {
    /// The host ceiling narrowed by `plugin_id`'s override. An override can
    /// only lower a ceiling — `PluginsConfig::validate` refused anything
    /// else at startup — so `min` here is documentation, not a second gate.
    pub fn effective(config: &PluginsConfig, plugin_id: &str) -> Self {
        let o = config.override_for(plugin_id);
        let pick_usize =
            |ceiling: usize, over: Option<usize>| over.map_or(ceiling, |v| v.min(ceiling));
        Self {
            max_memory_bytes: pick_usize(
                config.max_memory_bytes,
                o.and_then(|o| o.max_memory_bytes),
            ),
            max_request_bytes: pick_usize(
                config.max_request_bytes,
                o.and_then(|o| o.max_request_bytes),
            ),
            max_response_bytes: pick_usize(
                config.max_response_bytes,
                o.and_then(|o| o.max_response_bytes),
            ),
            timeout: Duration::from_millis(
                o.and_then(|o| o.timeout_ms)
                    .map_or(config.max_timeout_ms, |v| v.min(config.max_timeout_ms)),
            ),
            max_concurrency: o
                .and_then(|o| o.max_concurrency)
                .map_or(config.max_concurrency_per_function, |v| {
                    v.min(config.max_concurrency_per_function)
                }),
            fuel: config.fuel_backstop,
        }
    }
}

/// Per-store host state: the memory limiter and what it observed.
///
/// A fresh one per invocation, so nothing a guest did — grow, trap, exhaust
/// — is visible to the next call.
pub struct HostState {
    pub limiter: MemoryLimiter,
}

impl HostState {
    pub fn new(limits: &Limits) -> Self {
        Self {
            limiter: MemoryLimiter {
                max_memory_bytes: limits.max_memory_bytes,
                high_water: 0,
                refused: false,
            },
        }
    }
}

/// Bounds a guest's linear memory and remembers whether it hit the bound.
///
/// `refused` is what turns a subsequent trap into a `Limit` rather than a
/// `Backend` failure: a Rust guest whose allocation fails aborts, and an
/// abort is an `unreachable` trap indistinguishable from any other — except
/// that this flag says the host refused a growth first.
pub struct MemoryLimiter {
    max_memory_bytes: usize,
    pub high_water: usize,
    pub refused: bool,
}

/// Tables hold function references, not data; a guest has no reason to grow
/// one past this.
const MAX_TABLE_ELEMENTS: usize = 100_000;

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.max_memory_bytes {
            self.refused = true;
            return Ok(false);
        }
        self.high_water = self.high_water.max(desired);
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > MAX_TABLE_ELEMENTS {
            self.refused = true;
            return Ok(false);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PluginOverride;

    #[test]
    fn an_override_narrows_and_never_widens() {
        let mut config = PluginsConfig::default();
        config.overrides.push(PluginOverride {
            id: "acme.codec".to_string(),
            timeout_ms: Some(100),
            max_memory_bytes: Some(1 << 20),
            max_concurrency: Some(4),
            max_request_bytes: None,
            max_response_bytes: Some(4096),
        });
        let l = Limits::effective(&config, "acme.codec");
        assert_eq!(l.timeout, Duration::from_millis(100));
        assert_eq!(l.max_memory_bytes, 1 << 20);
        assert_eq!(l.max_concurrency, 4);
        assert_eq!(l.max_request_bytes, config.max_request_bytes);
        assert_eq!(l.max_response_bytes, 4096);
        assert_eq!(l.fuel, config.fuel_backstop);

        let host = Limits::effective(&config, "some.other");
        assert_eq!(host.timeout, Duration::from_millis(config.max_timeout_ms));
        assert_eq!(host.max_memory_bytes, config.max_memory_bytes);
    }

    #[test]
    fn the_limiter_refuses_past_the_bound_and_remembers_it() {
        let limits = Limits::effective(&PluginsConfig::default(), "x");
        let mut state = HostState::new(&limits);
        assert!(state.limiter.memory_growing(0, 1 << 20, None).expect("ok"));
        assert_eq!(state.limiter.high_water, 1 << 20);
        assert!(!state.limiter.refused);
        assert!(
            !state
                .limiter
                .memory_growing(1 << 20, limits.max_memory_bytes + 1, None)
                .expect("ok")
        );
        assert!(state.limiter.refused);
        assert_eq!(
            state.limiter.high_water,
            1 << 20,
            "a refused growth is not observed"
        );
    }
}

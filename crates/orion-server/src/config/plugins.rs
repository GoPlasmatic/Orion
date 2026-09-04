//! `[plugins]`: the WebAssembly plugin sandbox and the ceilings an operator
//! sets on it.
//!
//! A plugin requests nothing. Every limit below is the host's, and a
//! per-plugin override may only reduce one — an author who needs more asks
//! the operator, which is the conversation these numbers exist to force.
//! Off by default: `enabled = false` preserves the pre-plugin behaviour
//! exactly, and a stored plugin row on a node with plugins disabled
//! quarantines the workflows that name its functions rather than aborting.

use serde::{Deserialize, Serialize};

use super::validation::require_nonzero;
use crate::errors::OrionError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginsConfig {
    /// Whether this node loads and runs plugins at all.
    pub enabled: bool,
    /// Reserved for an on-disk cache of compiled artifacts. Not supported
    /// yet: a non-empty value is refused at startup, so nothing is silently
    /// ignored (design decision 4 in `plugin-plan.md`).
    pub cache_dir: String,
    /// Largest component an upload may carry.
    pub max_component_bytes: usize,
    /// Linear memory ceiling per invocation. Also sizes the pooling
    /// allocator's per-instance reservation, so the process reserves
    /// `max_live_instances × max_memory_bytes` of *virtual* address space at
    /// startup — count it when a container limits virtual memory.
    pub max_memory_bytes: usize,
    /// Largest evaluated `function.input`, serialised, handed to a guest.
    pub max_request_bytes: usize,
    /// Largest JSON a guest may return.
    pub max_response_bytes: usize,
    /// Wall-clock ceiling per invocation. The task's own deadline applies
    /// too; the shorter wins.
    pub max_timeout_ms: u64,
    /// Invocations of one function that may run at once. Beyond it a task
    /// waits for a permit until its deadline and then fails as a limit.
    pub max_concurrency_per_function: u32,
    /// Instances the pooling allocator can hold at once, across every
    /// function. Under-sizing it surfaces as instantiation failures under
    /// load, so it must be at least `max_concurrency_per_function`.
    pub max_live_instances: u32,
    /// Fuel per invocation — a backstop against a guest that spins without
    /// touching the clock, not a contract: fuel cost moves between Wasmtime
    /// versions, so operators reason in `max_timeout_ms`. Sized well above
    /// what `max_timeout_ms` admits (a tight loop burns roughly 5e9 units a
    /// second), so the deadline is what stops a runaway guest and fuel only
    /// catches one the clock somehow missed.
    pub fuel_backstop: u64,
    pub trust: PluginTrustConfig,
    /// Per-plugin ceilings, each at most the host's.
    pub overrides: Vec<PluginOverride>,
}

/// Optional hardening: when `public_keys` is non-empty, an upload must carry
/// a signature over the component digest by one of them, verified again at
/// every load.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginTrustConfig {
    pub public_keys: Vec<String>,
}

/// A ceiling lowered for one plugin. Any field left unset keeps the host's.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginOverride {
    pub id: String,
    pub timeout_ms: Option<u64>,
    pub max_memory_bytes: Option<usize>,
    pub max_concurrency: Option<u32>,
    pub max_request_bytes: Option<usize>,
    pub max_response_bytes: Option<usize>,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cache_dir: String::new(),
            max_component_bytes: 16 * 1024 * 1024,
            max_memory_bytes: 64 * 1024 * 1024,
            max_request_bytes: 1024 * 1024,
            max_response_bytes: 1024 * 1024,
            max_timeout_ms: 5_000,
            max_concurrency_per_function: 64,
            max_live_instances: 256,
            fuel_backstop: 100_000_000_000,
            trust: PluginTrustConfig::default(),
            overrides: Vec::new(),
        }
    }
}

impl PluginsConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        if !self.cache_dir.is_empty() {
            return Err(OrionError::Config {
                message: "plugins.cache_dir is reserved and not supported yet: leave it empty"
                    .to_string(),
            });
        }
        require_nonzero(
            self.max_component_bytes as u64,
            "plugins.max_component_bytes",
        )?;
        require_nonzero(self.max_memory_bytes as u64, "plugins.max_memory_bytes")?;
        require_nonzero(self.max_request_bytes as u64, "plugins.max_request_bytes")?;
        require_nonzero(self.max_response_bytes as u64, "plugins.max_response_bytes")?;
        require_nonzero(self.max_timeout_ms, "plugins.max_timeout_ms")?;
        require_nonzero(
            u64::from(self.max_concurrency_per_function),
            "plugins.max_concurrency_per_function",
        )?;
        require_nonzero(
            u64::from(self.max_live_instances),
            "plugins.max_live_instances",
        )?;
        require_nonzero(self.fuel_backstop, "plugins.fuel_backstop")?;
        if self.max_live_instances < self.max_concurrency_per_function {
            return Err(OrionError::Config {
                message: format!(
                    "plugins.max_live_instances ({}) must be at least \
                     plugins.max_concurrency_per_function ({}): one function alone could \
                     otherwise exhaust the instance pool",
                    self.max_live_instances, self.max_concurrency_per_function
                ),
            });
        }
        let mut seen: Vec<&str> = Vec::new();
        for (i, o) in self.overrides.iter().enumerate() {
            let at = format!("plugins.overrides[{i}]");
            if o.id.trim().is_empty() {
                return Err(OrionError::Config {
                    message: format!("{at}.id must name a plugin"),
                });
            }
            if seen.contains(&o.id.as_str()) {
                return Err(OrionError::Config {
                    message: format!("{at}.id '{}' appears twice", o.id),
                });
            }
            seen.push(&o.id);
            reduce_only(&at, "timeout_ms", o.timeout_ms, self.max_timeout_ms)?;
            reduce_only(
                &at,
                "max_memory_bytes",
                o.max_memory_bytes.map(|v| v as u64),
                self.max_memory_bytes as u64,
            )?;
            reduce_only(
                &at,
                "max_concurrency",
                o.max_concurrency.map(u64::from),
                u64::from(self.max_concurrency_per_function),
            )?;
            reduce_only(
                &at,
                "max_request_bytes",
                o.max_request_bytes.map(|v| v as u64),
                self.max_request_bytes as u64,
            )?;
            reduce_only(
                &at,
                "max_response_bytes",
                o.max_response_bytes.map(|v| v as u64),
                self.max_response_bytes as u64,
            )?;
        }
        Ok(())
    }

    /// The override for `plugin_id`, if the operator wrote one.
    pub fn override_for(&self, plugin_id: &str) -> Option<&PluginOverride> {
        self.overrides.iter().find(|o| o.id == plugin_id)
    }
}

/// An override is a ceiling lowered, never raised, and never zero.
fn reduce_only(at: &str, field: &str, value: Option<u64>, ceiling: u64) -> Result<(), OrionError> {
    match value {
        None => Ok(()),
        Some(0) => Err(OrionError::Config {
            message: format!("{at}.{field} must be non-zero"),
        }),
        Some(v) if v > ceiling => Err(OrionError::Config {
            message: format!(
                "{at}.{field} ({v}) exceeds the host ceiling ({ceiling}): an override may only \
                 reduce a limit"
            ),
        }),
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_validate() {
        PluginsConfig::default()
            .validate()
            .expect("defaults are valid");
    }

    #[test]
    fn the_cache_dir_is_refused_until_it_exists() {
        let c = PluginsConfig {
            cache_dir: "/var/cache/orion".to_string(),
            ..PluginsConfig::default()
        };
        let err = c.validate().expect_err("reserved");
        assert!(err.to_string().contains("cache_dir"));
    }

    #[test]
    fn a_pool_smaller_than_one_functions_concurrency_is_refused() {
        let c = PluginsConfig {
            max_live_instances: 8,
            ..PluginsConfig::default()
        };
        let err = c.validate().expect_err("pool");
        assert!(err.to_string().contains("max_live_instances"));
    }

    #[test]
    fn an_override_may_only_reduce() {
        let mut c = PluginsConfig::default();
        c.overrides.push(PluginOverride {
            id: "acme.codec".to_string(),
            timeout_ms: Some(100),
            ..Default::default()
        });
        c.validate().expect("a lower timeout is fine");
        c.overrides[0].timeout_ms = Some(6_000);
        let err = c.validate().expect_err("raised");
        assert!(
            err.to_string().contains("exceeds the host ceiling"),
            "{err}"
        );
        c.overrides[0].timeout_ms = Some(0);
        assert!(c.validate().is_err());
        c.overrides[0].timeout_ms = None;
        c.overrides.push(PluginOverride {
            id: "acme.codec".to_string(),
            ..Default::default()
        });
        let err = c.validate().expect_err("dup");
        assert!(err.to_string().contains("appears twice"));
    }
}

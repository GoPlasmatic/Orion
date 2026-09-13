//! What one inference may consume: the host ceiling from `[models]` narrowed
//! by the model's override, computed once per model at load.

use std::time::Duration;

use crate::config::ModelsConfig;

/// The effective ceilings for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Elements one inference may hand the model, summed over its inputs.
    pub max_input_elements: usize,
    /// Elements one inference may take back, summed over its outputs.
    pub max_output_elements: usize,
    /// Wall-clock ceiling per inference; the task's own deadline applies too.
    pub timeout: Duration,
    /// Inferences of this model that may run at once.
    pub max_concurrency: u32,
}

impl Limits {
    /// The host ceiling narrowed by `model_id`'s override. An override can
    /// only lower a ceiling — `ModelsConfig::validate` refused anything else
    /// at startup — so `min` here is documentation, not a second gate.
    pub fn effective(config: &ModelsConfig, model_id: &str) -> Self {
        let o = config.override_for(model_id);
        let pick_usize =
            |ceiling: usize, over: Option<usize>| over.map_or(ceiling, |v| v.min(ceiling));
        Self {
            max_input_elements: pick_usize(
                config.max_input_elements,
                o.and_then(|o| o.max_input_elements),
            ),
            max_output_elements: pick_usize(
                config.max_output_elements,
                o.and_then(|o| o.max_output_elements),
            ),
            timeout: Duration::from_millis(
                o.and_then(|o| o.timeout_ms)
                    .map_or(config.max_timeout_ms, |v| v.min(config.max_timeout_ms)),
            ),
            max_concurrency: o
                .and_then(|o| o.max_concurrency)
                .map_or(config.max_concurrency_per_model, |v| {
                    v.min(config.max_concurrency_per_model)
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelOverride;

    #[test]
    fn an_override_narrows_and_never_widens() {
        let mut config = ModelsConfig::default();
        config.overrides.push(ModelOverride {
            id: "ada.c4-tiny".to_string(),
            timeout_ms: Some(100),
            max_concurrency: Some(4),
            max_input_elements: None,
            max_output_elements: Some(64),
        });
        let l = Limits::effective(&config, "ada.c4-tiny");
        assert_eq!(l.timeout, Duration::from_millis(100));
        assert_eq!(l.max_concurrency, 4);
        assert_eq!(l.max_input_elements, config.max_input_elements);
        assert_eq!(l.max_output_elements, 64);

        let host = Limits::effective(&config, "some.other");
        assert_eq!(host.timeout, Duration::from_millis(config.max_timeout_ms));
        assert_eq!(host.max_concurrency, config.max_concurrency_per_model);
        assert_eq!(host.max_input_elements, config.max_input_elements);
        assert_eq!(host.max_output_elements, config.max_output_elements);

        // A raised override cannot reach here past validation, but `min`
        // keeps the ceiling regardless.
        config.overrides[0].timeout_ms = Some(u64::MAX);
        assert_eq!(
            Limits::effective(&config, "ada.c4-tiny").timeout,
            Duration::from_millis(config.max_timeout_ms)
        );
    }
}

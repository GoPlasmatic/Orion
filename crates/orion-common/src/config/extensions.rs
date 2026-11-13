//! `[extensions.<id>]`: settings an edition's own extensions read.
//!
//! An edition composes drivers and providers Community does not know, and
//! some want node-wide settings of their own (a Kafka driver's lag poll
//! interval, an entitlement file's path). Community cannot type them, and the
//! rest of [`super::AppConfig`] refuses unknown keys, so they get one
//! free-form table keyed by the extension's id. Community stores it and never
//! interprets it; the edition reads its section before it composes the node
//! ([`ExtensionsConfig::section`]) and holds it to its own type.
//!
//! Like `[vars]` and `[secrets]`, the table does not fit the
//! `ORION_SECTION__KEY` scheme, so there is no per-key env override: write
//! `${VAR}` in a value instead. A credential belongs behind a reference
//! (`env://…`, `vault://…`) the edition resolves, or in `[secrets]`, never
//! as a literal here: `validate-config` prints this table as written.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Every `[extensions.<id>]` table, by id.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct ExtensionsConfig(pub BTreeMap<String, toml::Value>);

impl ExtensionsConfig {
    /// The raw `[extensions.<id>]` table, when the config declares one.
    pub fn get(&self, id: &str) -> Option<&toml::Value> {
        self.0.get(id)
    }

    /// `[extensions.<id>]` as `T`, `None` when the config declares no such
    /// table. `T` decides how strict to be: one with `deny_unknown_fields`
    /// refuses a misspelled key the way the rest of the config does.
    pub fn section<T: DeserializeOwned>(&self, id: &str) -> Result<Option<T>, String> {
        let Some(value) = self.0.get(id) else {
            return Ok(None);
        };
        T::deserialize(value.clone())
            .map(Some)
            .map_err(|e| format!("[extensions.{id}]: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Kafka {
        lag_poll_interval_secs: u64,
    }

    #[test]
    fn an_edition_reads_its_own_section_as_strictly_as_it_likes() {
        let config: crate::config::AppConfig =
            toml::from_str("[extensions.kafka]\nlag_poll_interval_secs = 15\n").expect("parses");
        assert_eq!(
            config.extensions.section::<Kafka>("kafka").expect("typed"),
            Some(Kafka {
                lag_poll_interval_secs: 15
            })
        );
        assert_eq!(
            config.extensions.section::<Kafka>("redis").expect("absent"),
            None
        );

        let misspelled: crate::config::AppConfig =
            toml::from_str("[extensions.kafka]\nlag_poll_interval = 15\n").expect("parses");
        let err = misspelled
            .extensions
            .section::<Kafka>("kafka")
            .expect_err("the edition's type refuses it");
        assert!(err.starts_with("[extensions.kafka]"), "{err}");
    }
}

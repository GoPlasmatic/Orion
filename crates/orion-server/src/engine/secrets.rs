//! The engine's secret store: `[secrets]` resolved once, published to every
//! engine Orion builds, and reachable only through `{"secret": "name"}`.
//!
//! The guarantee this exists to preserve belongs to dataflow-rs, not to Orion:
//! a secret is held by the `Engine`, not by the `Message`, so it cannot appear
//! in `Serialize for Message`, in an `ExecutionTrace` snapshot, in a `map`
//! mapping clone, or in anything Orion derives from a message — there is
//! nothing to exclude. Orion's half is deciding *which* secrets exist, so a
//! workflow reaches what the operator published rather than whatever the
//! process environment happens to hold.
//!
//! This type is deliberately hard to leak from. It implements neither
//! `Serialize` nor `Clone`, its `Debug` prints names with the values masked,
//! and the only accessor that yields values is `pub(crate)` and used at
//! exactly one call site — the `EngineBuilder::with_secrets_json` in
//! [`crate::engine::operators::with_orion_engine_defaults`].

use std::fmt;

use serde_json::{Map, Value};

use crate::config::SecretsConfig;
use crate::errors::OrionError;

/// `[secrets]` with every reference resolved to its value.
pub struct ResolvedSecrets {
    /// Always a JSON object. dataflow-rs refuses anything else.
    root: Value,
}

impl ResolvedSecrets {
    /// A store with nothing in it — what every offline surface and every test
    /// that declares no secrets carries. A `{"secret": …}` against it is a
    /// build error, not a null.
    pub fn empty() -> Self {
        Self {
            root: Value::Object(Map::new()),
        }
    }

    /// Resolve every reference in `declared`.
    ///
    /// Failure is fatal to startup by design: a reference that cannot be
    /// resolved must never be handed onward as its own literal text, and the
    /// alternative — booting with a workflow whose signing key is the string
    /// `env://PARTNER_HMAC_KEY` — is the defect the reference syntax exists to
    /// prevent. The error names the entry and never its value.
    pub async fn resolve(declared: &SecretsConfig) -> Result<Self, OrionError> {
        // One resolver registry for the whole section. `resolve_secret_string`
        // would build its own per call — two `env::var` reads and a client
        // clone each — and this runs before readiness, once per declared entry.
        // The loop itself stays per-entry because the error has to name the
        // entry (`secrets.partner_hmac`), not the reference it holds.
        let resolvers = crate::connector::secrets::default_resolvers();
        let mut root = Map::with_capacity(declared.0.len());
        for (name, reference) in declared.iter() {
            let mut value = Value::String(reference.clone());
            crate::connector::secrets::resolve_in_place(
                &mut value,
                &resolvers,
                &format!("secrets.{name}"),
            )
            .await?;
            root.insert(name.clone(), value);
        }
        Ok(Self {
            root: Value::Object(root),
        })
    }

    /// Build one directly from already-resolved values. The offline surfaces
    /// (`dry-run --secrets`, a `*.case.json` `secrets` block) use this: their
    /// values are stand-ins supplied by the author, not references.
    pub fn from_values(values: Map<String, Value>) -> Self {
        Self {
            root: Value::Object(values),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.names().next().is_none()
    }

    /// Declared names, never values.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.root
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, _)| name.as_str())
    }

    /// The one accessor that yields values, for the single hand-off to
    /// `EngineBuilder::with_secrets_json`.
    pub(crate) fn as_json(&self) -> &Value {
        &self.root
    }
}

impl fmt::Debug for ResolvedSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("ResolvedSecrets");
        for name in self.names() {
            s.field(name, &"******");
        }
        s.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> ResolvedSecrets {
        let mut map = Map::new();
        map.insert("partner_hmac".into(), json!("hmac-value-1a2b"));
        ResolvedSecrets::from_values(map)
    }

    #[test]
    fn debug_names_the_keys_and_masks_every_value() {
        let rendered = format!("{:?}", store());
        assert!(rendered.contains("partner_hmac"), "{rendered}");
        assert!(!rendered.contains("hmac-value-1a2b"), "{rendered}");
    }

    #[test]
    fn an_empty_store_declares_nothing() {
        assert!(ResolvedSecrets::empty().is_empty());
        assert_eq!(ResolvedSecrets::empty().names().count(), 0);
    }

    #[tokio::test]
    async fn an_unset_reference_fails_rather_than_resolving_to_its_own_text() {
        let declared = SecretsConfig(
            [(
                "partner_hmac".to_string(),
                "env://ORION_SECRET_DEFINITELY_NOT_SET".to_string(),
            )]
            .into_iter()
            .collect(),
        );
        let err = ResolvedSecrets::resolve(&declared)
            .await
            .expect_err("an unset variable must not boot");
        let text = err.to_string();
        assert!(text.contains("secrets.partner_hmac"), "{text}");
    }

    #[tokio::test]
    async fn nothing_declared_resolves_to_an_empty_store() {
        // The end-to-end path — a declared reference resolving to its value and
        // reaching a workflow — is `secrets_and_vars_test.rs`, which can set the
        // variable pre-main. Mutating the environment from a test body cannot be
        // done soundly here.
        let store = ResolvedSecrets::resolve(&SecretsConfig::default())
            .await
            .expect("an empty section resolves");
        assert!(store.is_empty());
    }
}

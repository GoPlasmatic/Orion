//! `[vars]` and `[secrets]` — the two ways an operator declares a value that
//! workflow expressions may read.
//!
//! They exist for the same reason: a definition is promoted between instances
//! unchanged, so anything that differs per environment — a topic prefix, a
//! partner's base URL, a signing key — cannot be written into the definition
//! itself. They differ in exactly one respect, and it is the one that decides
//! which section a value belongs in:
//!
//! | | Read as | Recorded |
//! |---|---|---|
//! | `[vars]` | `{"var": "metadata.vars.name"}` | **Yes** — stamped into every message's metadata, so it appears in traces |
//! | `[secrets]` | `{"secret": "name"}` | **No** — held by the engine, never part of a message |
//!
//! A var is deployment configuration, and an operator debugging "which topic
//! did this run publish to?" needs it in the trace. A secret is key material,
//! and the whole point is that it is nowhere a trace can reach.
//!
//! Values reach both sections the same way: `${VAR}` placeholders are
//! substituted into the config text before it is parsed (see
//! [`super::env_substitute`]), so both sections read from the process
//! environment without either one naming a resolver.
//!
//! The difference resurfaces in what a value may *be*. A secret must be a
//! `env://` / `vault://` reference resolved at startup, never a literal — a
//! key pasted into a config file is a key in the deployment's file tree. A var
//! must be the opposite: a literal, because nothing resolves a reference on
//! its way into metadata, so an `env://` there would reach a workflow as the
//! nine characters `env://` and its name.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::errors::OrionError;

/// Deployment values stamped into `metadata.vars` on every message.
///
/// Free-form, so there is no `ORION_VARS__…` override: the values are named by
/// the operator, and `${VAR}` in the config text already covers reading them
/// from the environment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct VarsConfig(pub BTreeMap<String, toml::Value>);

/// Secret references resolved once at startup and published to the engine,
/// where `{"secret": "name"}` reaches them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct SecretsConfig(pub BTreeMap<String, String>);

impl VarsConfig {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The whole section as one JSON object — what gets stamped into
    /// `metadata.vars`. Built once at startup, cloned per message.
    ///
    /// Returns `None` for an empty section, which is the signal to stamp
    /// nothing at all rather than an empty object: a workflow reading
    /// `metadata.vars.x` on an instance that declares no vars should see the
    /// same missing value either way, and an empty object in every trace is
    /// noise.
    pub fn to_json(&self) -> Option<serde_json::Value> {
        if self.is_empty() {
            return None;
        }
        // Infallible for the value kinds `validate` admits — every `toml::Value`
        // has a JSON form, and the one whose form is a nonsense object
        // (`Datetime`) is refused before it can get here.
        serde_json::to_value(&self.0).ok()
    }

    pub(super) fn validate(&self) -> Result<(), OrionError> {
        for (name, value) in &self.0 {
            validate_name(name, "vars")?;
            check_var_value(name, value)?;
        }
        Ok(())
    }
}

impl SecretsConfig {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }

    pub(super) fn validate(&self) -> Result<(), OrionError> {
        for (name, reference) in &self.0 {
            validate_name(name, "secrets")?;
            if !crate::connector::secrets::is_resolvable_reference(reference) {
                return Err(OrionError::Config {
                    message: format!(
                        "secrets.{name} must be a secret reference such as \
                         \"env://SOME_VAR\" or \"vault://path#key\", not a literal value \
                         (a key written into a config file is a key in the deployment's \
                         file tree)"
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Names are the path a workflow types, so they have to be typable: an
/// identifier, and nothing else.
///
/// A dot is refused for a reason beyond tidiness — `{"secret": "a.b"}` walks
/// into a nested object, so a flat key literally named `a.b` would be
/// unreachable, and `{"var": "metadata.vars.a.b"}` has the same problem.
fn validate_name(name: &str, section: &str) -> Result<(), OrionError> {
    let ok = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(OrionError::Config {
            message: format!(
                "[{section}] name '{name}' is not an identifier — names may hold \
                 ASCII letters, digits and underscores, and may not start with a digit"
            ),
        });
    }
    Ok(())
}

/// A var value must be JSON-representable and must not be a secret reference.
fn check_var_value(name: &str, value: &toml::Value) -> Result<(), OrionError> {
    match value {
        toml::Value::String(s) => {
            if crate::connector::secrets::is_resolvable_reference(s) {
                return Err(OrionError::Config {
                    message: format!(
                        "vars.{name} is a secret reference, and nothing resolves one on its \
                         way into metadata — a workflow would read the literal text '{s}'. \
                         Declare it under [secrets] and read it with \
                         {{\"secret\": \"{name}\"}}, or inline the value here"
                    ),
                });
            }
            Ok(())
        }
        toml::Value::Integer(_) | toml::Value::Float(_) | toml::Value::Boolean(_) => Ok(()),
        toml::Value::Array(items) => items
            .iter()
            .try_for_each(|item| check_var_value(name, item)),
        toml::Value::Table(table) => table
            .values()
            .try_for_each(|item| check_var_value(name, item)),
        toml::Value::Datetime(_) => Err(OrionError::Config {
            message: format!(
                "vars.{name} is a TOML datetime, which has no JSON form — write it as a \
                 quoted string"
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(toml_text: &str) -> VarsConfig {
        VarsConfig(toml::from_str(toml_text).expect("test fixture parses"))
    }

    fn secrets(toml_text: &str) -> SecretsConfig {
        SecretsConfig(toml::from_str(toml_text).expect("test fixture parses"))
    }

    #[test]
    fn a_var_keeps_the_type_it_was_written_as() {
        let json = vars("prefix = \"eu\"\nretries = 3\nverbose = true")
            .to_json()
            .expect("non-empty");
        assert_eq!(json["prefix"], serde_json::json!("eu"));
        assert_eq!(json["retries"], serde_json::json!(3));
        assert_eq!(json["verbose"], serde_json::json!(true));
    }

    #[test]
    fn an_empty_section_stamps_nothing() {
        assert!(vars("").to_json().is_none());
    }

    #[test]
    fn a_secret_reference_in_vars_is_refused() {
        let err = vars("token = \"env://PARTNER_TOKEN\"")
            .validate()
            .expect_err("a reference in vars reaches the workflow as literal text");
        assert!(err.to_string().contains("[secrets]"), "{err}");
    }

    #[test]
    fn a_literal_in_secrets_is_refused() {
        let err = secrets("token = \"sk-live-abc\"")
            .validate()
            .expect_err("a literal key in a config file is a key on disk");
        assert!(err.to_string().contains("env://"), "{err}");
        secrets("token = \"env://PARTNER_TOKEN\"")
            .validate()
            .expect("a reference is the whole point");
    }

    #[test]
    fn names_must_be_identifiers() {
        for bad in ["", "a.b", "2fast", "with space", "dash-ed"] {
            let mut map = BTreeMap::new();
            map.insert(bad.to_string(), toml::Value::String("x".into()));
            VarsConfig(map)
                .validate()
                .expect_err("'{bad}' is not a typable path segment");
        }
        vars("ok_name_2 = \"x\"")
            .validate()
            .expect("an identifier is fine");
    }

    #[test]
    fn a_datetime_var_is_refused_rather_than_silently_reshaped() {
        vars("cutover = 1979-05-27T07:32:00Z")
            .validate()
            .expect_err("TOML datetimes have no JSON form");
    }
}

//! `env://NAME` references in the config file (#311).
//!
//! A connector naming a Postgres database takes a reference and resolves it
//! (`"connection_string": "env://SOMA_DB_URL"`); the server's own `[storage]`
//! URL could not, so a deployment ended up with the same credential in two
//! different shapes — one referenced, one interpolated — and an entrypoint
//! script rendering one line of the config file at boot to work around it.
//!
//! This closes that asymmetry for every value in the file rather than for
//! `storage.url` alone. A per-field allowlist would have made
//! `kafka.auth.sasl_password`, `cluster.redis_url` and
//! `storage.connector_encryption_key` each their own request.
//!
//! ## Why `env://` only, and why this is synchronous
//!
//! [`crate::connector::secrets`] also registers `vault://` and reserves three
//! cloud backends. Those are deliberately **not** available here, and the
//! reason is a bootstrap ordering one rather than a policy: resolving them is
//! an HTTP call, and the config is what tells the process how to make HTTP
//! calls. `[secrets]` resolves them because it runs at *bootstrap*, once the
//! runtime is up — this runs before there is a runtime at all. A reserved
//! scheme in the config file is refused by name so the difference is stated
//! rather than discovered.
//!
//! ## Strict, like `${VAR}`
//!
//! An unset variable is a hard error, exactly as
//! [`super::env_substitute`] already treats `${VAR}` with no `:-default`. A
//! config file is per-deployment by nature, so "validates here, cannot boot
//! there" is the outcome worth preventing — and it is what
//! `orion-server validate-config` exists to catch.
//!
//! This is the opposite of `[secrets]`, whose `validate` accepts an unresolved
//! reference on purpose: a *definition bundle* has to validate on a host that
//! holds none of the production secrets. A config file has no such life.

use std::collections::BTreeSet;

use crate::errors::OrionError;

/// Sections whose reference semantics are their own.
///
/// `[vars]` **refuses** a resolvable reference (`config/vars.rs`): a var is
/// stamped into metadata verbatim, so nothing would resolve it on the way, and
/// the error tells the author to use `[secrets]` instead. Resolving here would
/// silently grant what that check exists to refuse.
///
/// `[secrets]` **requires** one, and resolves it at bootstrap through the full
/// resolver set — including the `vault://` this module cannot reach.
const OWN_SEMANTICS: &[&str] = &["vars", "secrets"];

/// Resolve every `env://NAME` in `doc`, in place.
///
/// `referenced` collects the variable names consumed, for the same reason
/// [`super::env_substitute::referenced_vars`] does: a file saying
/// `url = "env://ORION_STATE_DB_URL"` makes that variable one Orion genuinely
/// reads, and the unknown-`ORION_*` guard (C4d) would otherwise refuse it as a
/// misspelled setting.
pub(super) fn resolve_env_references(
    doc: &mut toml::Value,
    referenced: &mut BTreeSet<String>,
) -> Result<(), OrionError> {
    let toml::Value::Table(root) = doc else {
        return Ok(());
    };
    for (section, value) in root.iter_mut() {
        if OWN_SEMANTICS.contains(&section.as_str()) {
            continue;
        }
        walk(value, section, referenced)?;
    }
    Ok(())
}

fn walk(
    value: &mut toml::Value,
    path: &str,
    referenced: &mut BTreeSet<String>,
) -> Result<(), OrionError> {
    match value {
        toml::Value::String(s) => {
            if let Some(resolved) = resolve_one(s, path, referenced)? {
                *s = resolved;
            }
        }
        toml::Value::Table(t) => {
            for (k, v) in t.iter_mut() {
                walk(v, &format!("{path}.{k}"), referenced)?;
            }
        }
        toml::Value::Array(items) => {
            for (i, v) in items.iter_mut().enumerate() {
                walk(v, &format!("{path}[{i}]"), referenced)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `Some(value)` when `s` was a reference this module resolves, `None` when it
/// is an ordinary string.
fn resolve_one(
    s: &str,
    path: &str,
    referenced: &mut BTreeSet<String>,
) -> Result<Option<String>, OrionError> {
    let Some(name) = s.strip_prefix("env://") else {
        // A reserved scheme is refused rather than passed through: reaching the
        // remote system as its own literal text is exactly what
        // `connector::secrets` refuses it for, and a URL that reads
        // `vault://secret/db` would fail far from here.
        if let Some(scheme) = crate::connector::secrets::RESERVED_SCHEMES
            .iter()
            .find(|scheme| s.starts_with(&format!("{scheme}://")))
        {
            return Err(OrionError::Config {
                message: format!(
                    "{path} uses '{scheme}://', which the config file cannot resolve — \
                     it is read before the process has a runtime to reach a secret \
                     backend with. Use 'env://NAME' here, or declare the value under \
                     [secrets], which resolves at startup through every scheme."
                ),
            });
        }
        return Ok(None);
    };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(OrionError::Config {
            // `[A-Za-z0-9_]`, matching the guard above. It used to read
            // `[A-Z0-9_]`, which sent an operator debugging `env://my.db.url`
            // looking at the case of the name when the `.` was the problem —
            // and lowercase names are accepted, reaching
            // `referenced_by_config_file` where the `ORION_*` guard reasons
            // about them.
            message: format!("Invalid env-var name '{name}' in {path} (allowed: [A-Za-z0-9_])"),
        });
    }
    referenced.insert(name.to_string());
    std::env::var(name)
        .map(Some)
        .map_err(|_| OrionError::Config {
            message: format!(
                "Required environment variable '{name}' is not set (referenced as \
                 'env://{name}' by {path})."
            ),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(input: &str) -> Result<toml::Value, OrionError> {
        let mut doc: toml::Value = toml::from_str(input).expect("valid toml");
        let mut referenced = BTreeSet::new();
        resolve_env_references(&mut doc, &mut referenced)?;
        Ok(doc)
    }

    #[test]
    fn a_reference_resolves_wherever_it_sits() {
        // SAFETY: single-threaded test; the value is read back immediately.
        unsafe { std::env::set_var("ORION_TEST_REF_URL", "postgres://u:p@h/db") };
        let doc = resolve("[storage]\nurl = \"env://ORION_TEST_REF_URL\"\n").expect("resolves");
        assert_eq!(doc["storage"]["url"].as_str(), Some("postgres://u:p@h/db"));
    }

    /// Strict, like `${VAR}`: a config that cannot boot must not validate.
    #[test]
    fn an_unset_variable_is_named() {
        let err =
            resolve("[storage]\nurl = \"env://ORION_TEST_REF_ABSENT\"\n").expect_err("must fail");
        let message = err.to_string();
        assert!(message.contains("ORION_TEST_REF_ABSENT"), "{message}");
        assert!(message.contains("storage.url"), "{message}");
    }

    /// The two sections that own their reference semantics keep them.
    #[test]
    fn vars_and_secrets_are_left_alone() {
        let doc =
            resolve("[vars]\na = \"env://ORION_TEST_REF_ABSENT\"\n[secrets]\nb = \"vault://x\"\n")
                .expect("neither section is walked");
        assert_eq!(
            doc["vars"]["a"].as_str(),
            Some("env://ORION_TEST_REF_ABSENT")
        );
        assert_eq!(doc["secrets"]["b"].as_str(), Some("vault://x"));
    }

    /// A scheme this module cannot reach is named, not passed through as text.
    #[test]
    fn a_reserved_scheme_is_refused_with_the_reason() {
        let err = resolve("[storage]\nurl = \"vault://secret/db\"\n").expect_err("must fail");
        let message = err.to_string();
        assert!(message.contains("vault://"), "{message}");
        assert!(message.contains("[secrets]"), "{message}");
    }

    #[test]
    fn an_ordinary_string_is_untouched() {
        let doc = resolve("[storage]\nurl = \"sqlite:orion.db\"\n").expect("resolves");
        assert_eq!(doc["storage"]["url"].as_str(), Some("sqlite:orion.db"));
    }
}

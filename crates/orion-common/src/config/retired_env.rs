//! Environment-variable names retired pre-1.0 whose shape the generic
//! unknown-variable scan (`unknown_env`) cannot distinguish from unrelated
//! host variables.
//!
//! Lives in its own module so `config_docs_drift_test`'s scraper — which
//! harvests every `"ORION_…"` literal out of `env_overrides.rs` — cannot
//! mistake a retired name for a live override.

use crate::errors::OrionError;

/// Separator-less retired variable names paired with their replacement.
///
/// Double-underscore variables (`ORION_<SECTION>__<KEY>`) are refused by
/// `unknown_env::reject_unknown_env_vars`. `ORION_ENV` has no `__` separator,
/// and falling back to `development` relaxes the production safety checks
/// (CORS wildcard rejection among them), so a silent downgrade would be a
/// security regression. Refuse to start instead.
const RETIRED_ENV_VARS: &[(&str, &str)] = &[("ORION_ENV", "ORION_ENVIRONMENT")];

/// Every retired variable name.
///
/// Exposed for `config_docs_drift_test`, which checks that every `ORION_*`
/// token written down anywhere in the repository is a name Orion accounts for
/// — a live override, a retired one, or explicitly not a server setting.
pub fn retired_env_names() -> std::collections::BTreeSet<String> {
    RETIRED_ENV_VARS
        .iter()
        .map(|(old, _)| (*old).to_string())
        .collect()
}

/// Whether `name` is a retired variable.
///
/// The unknown-variable scan (C4d) skips these so [`reject_retired_env_vars`]
/// keeps ownership of them.
pub(super) fn is_retired(name: &str) -> bool {
    RETIRED_ENV_VARS.iter().any(|(old, _)| *old == name)
}

/// Refuse to start when a retired `ORION_*` name is still set. Reports every
/// offender at once so one restart fixes the whole deployment manifest.
pub(super) fn reject_retired_env_vars<F>(env_var: F) -> Result<(), OrionError>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    let found: Vec<String> = RETIRED_ENV_VARS
        .iter()
        .filter(|(old, _)| env_var(old).is_ok())
        .map(|(old, new)| format!("  {old} -> {new}"))
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    Err(OrionError::Config {
        message: format!(
            "these environment variables were renamed or removed in 1.0 and are no \
             longer read (see https://docs.goplasmatic.io/operate/upgrading-to-1.0.html):\n{}",
            found.join("\n")
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn reader(
        env: HashMap<&'static str, &'static str>,
    ) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        move |key: &str| {
            env.get(key)
                .map(|v| v.to_string())
                .ok_or(std::env::VarError::NotPresent)
        }
    }

    #[test]
    fn is_retired_answers_for_the_whole_table() {
        for (old, _) in RETIRED_ENV_VARS {
            assert!(is_retired(old), "{old} is in the table");
        }
        assert!(!is_retired("ORION_SERVER__PORT"));
        assert!(!is_retired("ORION_QUEUE__WORKERS_TYPO"));
    }

    #[test]
    fn a_clean_environment_passes() {
        assert!(
            reject_retired_env_vars(reader(HashMap::from([("ORION_SERVER__PORT", "8080")])))
                .is_ok()
        );
    }

    #[test]
    fn orion_env_is_refused_rather_than_ignored() {
        // The security-relevant one: silently ignoring it would fall back to
        // `development`, which turns the production CORS and admin-auth checks
        // from startup errors back into warnings.
        let err = reject_retired_env_vars(reader(HashMap::from([("ORION_ENV", "production")])))
            .expect_err("ORION_ENV must be refused");
        let message = err.to_string();
        assert!(message.contains("ORION_ENV"), "{message}");
        assert!(message.contains("ORION_ENVIRONMENT"), "{message}");
    }

    #[test]
    fn no_retired_entry_points_at_a_retired_name() {
        let olds: Vec<&str> = RETIRED_ENV_VARS.iter().map(|(old, _)| *old).collect();
        assert!(olds.contains(&"ORION_ENV"));
        for (_, new) in RETIRED_ENV_VARS {
            let target = new.split_whitespace().next().unwrap_or(new);
            assert!(!olds.contains(&target), "{new} points at a retired name");
        }
    }
}

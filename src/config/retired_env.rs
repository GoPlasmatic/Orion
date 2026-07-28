//! Environment-variable names retired by the C22 config renames.
//!
//! Lives in its own module so `config_docs_drift_test`'s scraper — which
//! harvests every `"ORION_…"` literal out of `env_overrides.rs` — cannot
//! mistake a retired name for a live override.

use crate::errors::OrionError;

/// Every variable name C22 retired, paired with its replacement.
///
/// Env overrides are matched by name, not deserialised, so `deny_unknown_fields`
/// cannot see them: a renamed section would leave `ORION_QUEUE__WORKERS` set and
/// silently ignored. For `ORION_ENV` that is worse than an inconvenience —
/// falling back to `development` relaxes the production safety checks (CORS
/// wildcard rejection among them), so a silent downgrade is a security
/// regression. Refuse to start instead.
const RETIRED_ENV_VARS: &[(&str, &str)] = &[
    ("ORION_ENV", "ORION_ENVIRONMENT"),
    ("ORION_QUEUE__WORKERS", "ORION_TRACE_QUEUE__WORKERS"),
    ("ORION_QUEUE__BUFFER_SIZE", "ORION_TRACE_QUEUE__BUFFER_SIZE"),
    (
        "ORION_QUEUE__SHUTDOWN_TIMEOUT_SECS",
        "ORION_TRACE_QUEUE__SHUTDOWN_TIMEOUT_SECS",
    ),
    (
        "ORION_QUEUE__TRACE_RETENTION_HOURS",
        "ORION_TRACE_QUEUE__RETENTION_HOURS",
    ),
    (
        "ORION_QUEUE__TRACE_CLEANUP_INTERVAL_SECS",
        "ORION_TRACE_QUEUE__CLEANUP_INTERVAL_SECS (audit cleanup now has its own \
         ORION_AUDIT__CLEANUP_INTERVAL_SECS)",
    ),
    (
        "ORION_QUEUE__AUDIT_RETENTION_DAYS",
        "ORION_AUDIT__RETENTION_DAYS",
    ),
    (
        "ORION_QUEUE__PROCESSING_TIMEOUT_MS",
        "ORION_TRACE_QUEUE__PROCESSING_TIMEOUT_MS",
    ),
    (
        "ORION_QUEUE__MAX_RESULT_SIZE_BYTES",
        "ORION_TRACE_QUEUE__MAX_RESULT_SIZE_BYTES",
    ),
    (
        "ORION_QUEUE__MAX_QUEUE_MEMORY_BYTES",
        "ORION_TRACE_QUEUE__MAX_QUEUE_MEMORY_BYTES",
    ),
    (
        "ORION_QUEUE__DLQ_RETRY_ENABLED",
        "ORION_TRACE_QUEUE__DLQ_RETRY_ENABLED",
    ),
    (
        "ORION_QUEUE__DLQ_MAX_RETRIES",
        "ORION_TRACE_QUEUE__DLQ_MAX_RETRIES",
    ),
    (
        "ORION_QUEUE__DLQ_POLL_INTERVAL_SECS",
        "ORION_TRACE_QUEUE__DLQ_POLL_INTERVAL_SECS",
    ),
    (
        "ORION_QUEUE__DLQ_BATCH_SIZE",
        "ORION_TRACE_QUEUE__DLQ_BATCH_SIZE",
    ),
    (
        "ORION_QUEUE__DLQ_LEASE_SECS",
        "ORION_TRACE_QUEUE__DLQ_LEASE_SECS",
    ),
    ("ORION_CHANNELS__INCLUDE", "ORION_CHANNEL_FILTER__INCLUDE"),
    ("ORION_CHANNELS__EXCLUDE", "ORION_CHANNEL_FILTER__EXCLUDE"),
    ("ORION_TRACING__STORAGE__MODE", "ORION_TRACE_STORAGE__MODE"),
    (
        "ORION_TRACING__STORAGE__SAMPLE_RATE",
        "ORION_TRACE_STORAGE__SAMPLE_RATE",
    ),
    (
        "ORION_TRACING__STORAGE__ERRORS_ONLY",
        "ORION_TRACE_STORAGE__ERRORS_ONLY",
    ),
    (
        "ORION_TRACING__STORAGE__MAX_PENDING",
        "ORION_TRACE_STORAGE__MAX_PENDING",
    ),
    (
        "ORION_TRACING__STORAGE__ASYNC_ON_OVERFLOW",
        "ORION_TRACE_STORAGE__ASYNC_ON_OVERFLOW",
    ),
    (
        "ORION_TRACING__STORAGE__OVERFLOW_BLOCK_TIMEOUT_MS",
        "ORION_TRACE_STORAGE__OVERFLOW_BLOCK_TIMEOUT_MS",
    ),
    (
        "ORION_TRACING__STORAGE__ASYNC_WORKERS",
        "ORION_TRACE_STORAGE__ASYNC_WORKERS",
    ),
    (
        "ORION_TRACING__STORAGE__BATCH_SIZE",
        "ORION_TRACE_STORAGE__BATCH_SIZE",
    ),
    (
        "ORION_TRACING__STORAGE__BATCH_FLUSH_INTERVAL_MS",
        "ORION_TRACE_STORAGE__BATCH_FLUSH_INTERVAL_MS",
    ),
    (
        "ORION_TRACING__STORAGE__BATCH_WORKERS",
        "ORION_TRACE_STORAGE__BATCH_WORKERS",
    ),
];

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
            "these environment variables were renamed in 1.0 and are no longer read \
             (see docs/src/getting-started/upgrading.md):\n{}",
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
    fn every_offender_is_reported_in_one_pass() {
        let err = reject_retired_env_vars(reader(HashMap::from([
            ("ORION_QUEUE__WORKERS", "8"),
            ("ORION_CHANNELS__INCLUDE", "orders-*"),
            ("ORION_TRACING__STORAGE__MODE", "batch"),
        ])))
        .expect_err("all three are retired");
        let message = err.to_string();
        for expected in [
            "ORION_TRACE_QUEUE__WORKERS",
            "ORION_CHANNEL_FILTER__INCLUDE",
            "ORION_TRACE_STORAGE__MODE",
        ] {
            assert!(
                message.contains(expected),
                "missing {expected} in: {message}"
            );
        }
    }

    /// The table is only useful if it covers every name the renames retired.
    /// A new rename that forgets its entry silently un-sets that override.
    #[test]
    fn the_table_covers_every_renamed_section() {
        let olds: Vec<&str> = RETIRED_ENV_VARS.iter().map(|(old, _)| *old).collect();
        assert_eq!(
            olds.iter()
                .filter(|k| k.starts_with("ORION_QUEUE__"))
                .count(),
            14,
            "one entry per pre-1.0 [queue] key"
        );
        assert_eq!(
            olds.iter()
                .filter(|k| k.starts_with("ORION_TRACING__STORAGE__"))
                .count(),
            10,
            "one entry per pre-1.0 [tracing.storage] key"
        );
        assert!(olds.contains(&"ORION_ENV"));
        assert!(olds.contains(&"ORION_CHANNELS__INCLUDE"));
        assert!(olds.contains(&"ORION_CHANNELS__EXCLUDE"));
        // No entry may point at a name that is itself retired.
        for (_, new) in RETIRED_ENV_VARS {
            let target = new.split_whitespace().next().unwrap_or(new);
            assert!(!olds.contains(&target), "{new} points at a retired name");
        }
    }
}

//! Reading key material out of a task input.
//!
//! A field that carries a signing key accepts two spellings, and they answer
//! different questions:
//!
//! * `{"secret": "name"}` reads the **engine's store** — what the operator
//!   published in the `[secrets]` config section, resolved once at startup.
//!   The workflow names a value the operator chose to expose, and reaches
//!   nothing else.
//! * A **string** is a literal, or an `env://` / `vault://` reference resolved
//!   here, at execution. It reads whatever the process environment holds under
//!   that name.
//!
//! Prefer the first. It is an allowlist rather than a lookup, it costs no
//! resolution per call, and `Engine::check_workflow` catches a misspelled name
//! before the workflow ever runs — whereas an `env://` typo surfaces as a
//! failing task in production. The string form stays because it predates the
//! store and because a definition promoted from an older instance must keep
//! working.
//!
//! Only fields marked `secret` in [`super::schema`] read this way. That is the
//! same set `validation::secret_reference_errors` exempts from the
//! `UNRESOLVED_SECRET_REF` check, so "where a reference resolves" has one
//! answer and one list behind it.

use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;

/// The name in a `{"secret": "name"}` node — the reserved operator dataflow-rs
/// registers on every engine — or `None` for anything else.
///
/// A single-key object is the shape datalogic compiles as an operator call, so
/// this recognises exactly what the engine would.
pub fn secret_name(value: &Value) -> Option<&str> {
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    object.get("secret")?.as_str()
}

/// Resolve one key-material field to its value.
///
/// The error text names the field and, for a store miss, the key — never a
/// value.
pub async fn resolve_key_material(
    value: &Value,
    field: &str,
    ctx: &TaskContext<'_>,
) -> Result<String, String> {
    if let Some(name) = secret_name(value) {
        let Some(secret) = ctx.secret(name) else {
            return Err(format!(
                "'{field}' names secret '{name}', which this instance does not declare \
                 — add it to the [secrets] section of the config file"
            ));
        };
        return secret
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("'{field}': secret '{name}' is not a string"));
    }
    match value.as_str() {
        Some(text) => crate::connector::secrets::resolve_secret_string(text, field).await,
        None => Err(format!(
            "'{field}' must be a string (a literal or an env:// reference) or \
             {{\"secret\": \"name\"}}"
        )),
    }
}

/// [`resolve_key_material`] returning a `DataflowError`, for the handlers.
pub async fn key_material(
    value: &Value,
    field: &str,
    ctx: &TaskContext<'_>,
) -> Result<String, DataflowError> {
    resolve_key_material(value, field, ctx)
        .await
        .map_err(DataflowError::Validation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_a_single_key_secret_object_is_a_reference() {
        assert_eq!(secret_name(&json!({"secret": "k"})), Some("k"));
        // Two keys is not the shape datalogic compiles as an operator call.
        assert_eq!(secret_name(&json!({"secret": "k", "other": 1})), None);
        assert_eq!(secret_name(&json!({"var": "data.k"})), None);
        assert_eq!(secret_name(&json!("k")), None);
        assert_eq!(secret_name(&json!({"secret": 7})), None);
    }
}

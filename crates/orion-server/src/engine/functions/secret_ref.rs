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
use dataflow_rs::engine::secrets::SECRET_OPERATOR;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;

/// The name in a `{"secret": "name"}` node — the reserved operator dataflow-rs
/// registers on every engine — or `None` for anything else.
///
/// A single-key object is the shape datalogic compiles as an operator call, so
/// this recognises exactly what the engine would. **Both** argument spellings
/// count: datalogic normalises a one-element array to a single argument, so
/// `{"secret": ["name"]}` resolves at runtime exactly as the string form does,
/// and dataflow-rs's own authoring check reads it the same way. Recognising
/// only the string form would leave the array form resolving in a condition,
/// failing in a handler field, and missing from `lint`'s `[secrets]`
/// inventory — three surfaces disagreeing about one node.
pub fn secret_name(value: &Value) -> Option<&str> {
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    // The operator's own name, from the engine that registers it, so the two
    // cannot drift apart.
    match object.get(SECRET_OPERATOR)? {
        Value::String(name) => Some(name.as_str()),
        Value::Array(items) if items.len() == 1 => items[0].as_str(),
        _ => None,
    }
}

/// Resolve one key-material field to its value.
///
/// The error text names the field and, for a store miss, the key — never a
/// value.
async fn resolve_key_material(
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

/// Resolve one key-material field to its value, as a `DataflowError`.
///
/// The only entry point: the `String`-error form behind it is private, so no
/// handler can reach it and skip this mapping.
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

    /// datalogic normalises a one-element array argument to a single argument,
    /// so the engine resolves this spelling; every Orion surface that reads a
    /// secret node has to agree with it.
    #[test]
    fn the_one_element_array_spelling_is_the_same_reference() {
        assert_eq!(secret_name(&json!({"secret": ["k"]})), Some("k"));
        // More than one argument is an error at the operator, not a name.
        assert_eq!(secret_name(&json!({"secret": ["k", "j"]})), None);
        assert_eq!(secret_name(&json!({"secret": []})), None);
        assert_eq!(secret_name(&json!({"secret": [7]})), None);
    }
}

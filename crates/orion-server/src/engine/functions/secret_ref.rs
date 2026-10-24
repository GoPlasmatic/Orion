//! Recognising a store read in a task input.
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

use serde_json::Value;

/// The operator dataflow-rs reserves for a store read. Its own constant is
/// crate-private; `the_secret_operator_is_the_engines` pins this one to the
/// operator the engine registers.
pub const SECRET_OPERATOR: &str = "secret";

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
    match object.get(SECRET_OPERATOR)? {
        Value::String(name) => Some(name.as_str()),
        Value::Array(items) if items.len() == 1 => items[0].as_str(),
        _ => None,
    }
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

    /// The store read is the operator Orion's expressions register under
    /// this name.
    #[test]
    fn the_secret_operator_is_the_engines() {
        let exprs = crate::engine::kit::test_exprs();
        assert!(
            exprs.operators().iter().any(|op| op.name == SECRET_OPERATOR
                && op.family == dataflow_rs::expr::OperatorFamily::Secret),
            "no `{SECRET_OPERATOR}` operator in the secret family"
        );
    }
}

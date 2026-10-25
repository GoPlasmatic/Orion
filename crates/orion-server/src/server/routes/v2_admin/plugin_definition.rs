//! The check a plugin definition passes before it is stored.
//!
//! The server never trusts a digest a client sent: it decodes the component,
//! hashes it, verifies the signature over that digest where the node has
//! keys, compiles it in the sandbox and probes every declared function — so a
//! stored plugin definition is already known to load.

use base64::Engine as _;
use serde_json::Value;

use crate::errors::{FieldError, OrionError};
use crate::plugin::{Limits, Manifest, WasmRuntime};
use crate::server::state::AppState;

fn refused(message: &str, details: Vec<FieldError>) -> OrionError {
    OrionError::Validation {
        code: orion_api::error::codes::VALIDATION_ERROR,
        message: message.to_string(),
        details,
    }
}

/// A manifest's field-error code, as the `'static` the constructor wants.
/// The manifest module emits a closed set, all of which the registry names.
fn leak_code(code: &str) -> &'static str {
    match code {
        "REQUIRED" => "REQUIRED",
        "DUPLICATE_FIELD" => "DUPLICATE_FIELD",
        "TYPE_MISMATCH" => "TYPE_MISMATCH",
        _ => "INVALID",
    }
}

/// Check `manifest` (the `plugin.toml` text or the manifest as a JSON
/// object), `component` (base64) and `signature` the way a node loads them,
/// answering the manifest as the parsed JSON the loader reads.
pub(crate) async fn check(
    state: &AppState,
    manifest: &Value,
    component: &str,
    signature: Option<&str>,
) -> Result<Value, OrionError> {
    let config = &state.config.plugins;
    let parsed = match manifest {
        Value::String(text) => Manifest::parse(text),
        Value::Object(_) => match serde_json::from_value::<Manifest>(manifest.clone()) {
            Ok(m) => m.validated(),
            Err(e) => Err(vec![FieldError::new("manifest", "INVALID", e.to_string())]),
        },
        _ => Err(vec![FieldError::new(
            "manifest",
            "TYPE_MISMATCH",
            "manifest must be the TOML text (a string) or the manifest as a JSON object",
        )]),
    };
    let parsed = parsed.map_err(|details| {
        refused(
            "Plugin manifest is invalid",
            details
                .into_iter()
                .map(|d| {
                    FieldError::new(
                        format!("manifest.{}", d.path),
                        leak_code(&d.code),
                        d.message,
                    )
                })
                .collect(),
        )
    })?;
    let manifest_json = serde_json::to_value(&parsed)?;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(component.trim())
        .map_err(|e| {
            refused(
                "component is not valid base64",
                vec![FieldError::new("component", "INVALID", e.to_string())],
            )
        })?;
    if bytes.len() > config.max_component_bytes {
        return Err(refused(
            "component is too large",
            vec![FieldError::new(
                "component",
                "TOO_LONG",
                format!(
                    "component is {} bytes, over plugins.max_component_bytes ({})",
                    bytes.len(),
                    config.max_component_bytes
                ),
            )],
        ));
    }
    let digest = WasmRuntime::digest(&bytes);
    // The signature is over the digest the server just computed, so it is
    // checked after it, and only where the node has keys to check it against.
    // Its absence is a missing field, a bad one is invalid; both name
    // `signature`.
    if let Err(reason) = crate::plugin::trust::verify(&config.trust.public_keys, &digest, signature)
    {
        return Err(refused(
            "the component signature does not verify",
            vec![FieldError::new(
                "signature",
                if signature.is_none() {
                    "REQUIRED"
                } else {
                    "INVALID"
                },
                reason,
            )],
        ));
    }

    let Some(runtime) = &state.plugins else {
        return Err(OrionError::validation(
            "plugins are disabled on this node (plugins.enabled = false), so a component \
             cannot be compiled or checked here",
        ));
    };
    let loaded = match runtime.cached(&digest) {
        Some(loaded) => loaded,
        None => runtime.load(bytes).await.map_err(|e| match e {
            // The node was busy, not the component wrong: a 503 to retry.
            crate::plugin::LoadError::Capacity(_) => OrionError::ServiceUnavailable {
                reason: crate::errors::Unavailable::AtCapacity,
                message: e.to_string(),
            },
            _ => refused(
                "component failed to load",
                vec![FieldError::new("component", "INVALID", e.to_string())],
            ),
        })?,
    };
    let limits = Limits::effective(config, &parsed.name);
    let names: Vec<&str> = parsed.function_names().collect();
    runtime
        .self_test(&loaded, &limits, &names)
        .await
        .map_err(|e| {
            refused(
                "component failed its self-test",
                vec![FieldError::new("component", "INVALID", e.to_string())],
            )
        })?;
    Ok(manifest_json)
}

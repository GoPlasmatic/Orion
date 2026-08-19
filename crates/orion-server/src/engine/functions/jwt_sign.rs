//! `jwt_sign` — mint a JWS in a workflow (#267 Part B).
//!
//! Issuance is workflow logic (login, refresh, RFC 7523 client assertions);
//! verification config lives on the channel. Same core as both verify
//! surfaces, driven in reverse. Self-contained like `crypto`: no connector,
//! and dry-run executes it for real. The signing key resolves through the
//! secret registry per call and never enters context, errors, or traces.

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::connector_helpers::{apply_output, resolve_value};
use super::schema::{FieldKind, FieldSchema};
use super::storage_presign::parse_duration_secs;

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "jwt_sign";

/// Ten years: past this, "expiring" is a fiction and the author should say
/// what they mean with an explicit `exp` claim.
const MAX_EXPIRES_SECS: u64 = 315_360_000;

/// Workflow function handler that signs JWTs.
pub struct JwtSignHandler;

#[async_trait]
impl AsyncFunctionHandler for JwtSignHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // Literal prologue (F58): the message-independent refusals first.
        let algorithm = match input.get("algorithm").and_then(Value::as_str) {
            Some(name) => crate::jwt::parse_algorithm(name).map_err(|e| validation(&e))?,
            None => return Err(validation("requires 'algorithm'")),
        };
        let Some(key_ref) = input.get("key").and_then(Value::as_str) else {
            return Err(validation(
                "requires 'key' (a literal or a secret reference like env://NAME)",
            ));
        };
        let key_encoding = input.get("key_encoding").and_then(Value::as_str);
        let kid = input.get("kid").and_then(Value::as_str).map(str::to_string);
        let output = input
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("data");

        // Claims: a resolvable object — {"var": ..} nodes fold, like every
        // connector function; computed claims are composed with `map` first.
        let claims_input = input
            .get("claims")
            .cloned()
            .unwrap_or(Value::Object(serde_json::Map::new()));
        let mut claims = match resolve_value(&claims_input, ctx) {
            Value::Object(map) => map,
            _ => return Err(validation("'claims' must resolve to an object")),
        };

        let now = chrono::Utc::now().timestamp();
        // Registered-claim conveniences; an explicit field wins over a claims
        // entry of the same name — it is the more specific statement.
        if let Some(iss) = input.get("issuer").and_then(Value::as_str) {
            claims.insert("iss".to_string(), Value::String(iss.to_string()));
        }
        if let Some(aud) = input.get("audience")
            && !aud.is_null()
        {
            claims.insert("aud".to_string(), resolve_value(aud, ctx));
        }
        if let Some(nbf) = input.get("not_before") {
            let offset = duration_field(nbf, ctx, "not_before")?;
            claims.insert("nbf".to_string(), Value::from(now + offset as i64));
        }
        claims.insert("iat".to_string(), Value::from(now));
        match input.get("expires_in") {
            Some(raw) if !raw.is_null() => {
                let secs = duration_field(raw, ctx, "expires_in")?;
                if secs == 0 || secs > MAX_EXPIRES_SECS {
                    return Err(validation(&format!(
                        "'expires_in' must be between 1 second and {MAX_EXPIRES_SECS} \
                         seconds (10 years)"
                    )));
                }
                claims.insert("exp".to_string(), Value::from(now + secs as i64));
            }
            // A token without an expiry must be a stated decision, not an
            // omission: either expires_in, or an explicit exp claim.
            _ if claims.contains_key("exp") => {}
            _ => {
                return Err(validation(
                    "requires 'expires_in' (or an explicit 'exp' claim — non-expiring \
                     tokens must be deliberate)",
                ));
            }
        }

        // The key resolves last and lives only inside this call.
        let material = crate::connector::secrets::resolve_secret_string(key_ref, "jwt_sign.key")
            .await
            .map_err(|e| validation(&e))?;
        let key = crate::jwt::encoding_key(algorithm, &material, key_encoding)
            .map_err(|e| validation(&e))?;
        let token = crate::jwt::sign(algorithm, &key, kid, &Value::Object(claims))
            .map_err(|e| DataflowError::function_execution(format!("{NAME}: {e}"), None))?;

        apply_output(ctx, output, Value::String(token));
        Ok(TaskOutcome::Success)
    }
}

fn validation(msg: &str) -> DataflowError {
    DataflowError::Validation(format!("{NAME}: {msg}"))
}

/// Seconds from an integer or a `"<n>s|m|h|d"` string (resolvable).
fn duration_field(raw: &Value, ctx: &TaskContext<'_>, field: &str) -> Result<u64, DataflowError> {
    match resolve_value(raw, ctx) {
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| validation(&format!("'{field}' must be a positive integer"))),
        Value::String(s) => {
            parse_duration_secs(&s).map_err(|e| validation(&format!("'{field}': {e}")))
        }
        _ => Err(validation(&format!(
            "'{field}' must be seconds (integer) or a duration like \"24h\""
        ))),
    }
}

// -- Authoring-time validation (shared with schema::validate_input) --

pub(super) fn validate_static_input(
    obj: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    let mut errors: Vec<(&'static str, &'static str, String)> = Vec::new();

    match obj.get("algorithm").and_then(Value::as_str) {
        None => {
            if obj.get("algorithm").is_none() {
                // The required-field loop reports the absence; a non-string is
                // its TYPE_MISMATCH.
            }
        }
        Some(name) => {
            if let Err(e) = crate::jwt::parse_algorithm(name) {
                errors.push(("algorithm", "INVALID", e));
            }
        }
    }
    if obj.get("expires_in").is_none_or(Value::is_null) {
        let has_exp = obj
            .get("claims")
            .and_then(Value::as_object)
            .is_some_and(|c| c.contains_key("exp"));
        if !has_exp {
            errors.push((
                "expires_in",
                "REQUIRED",
                "jwt_sign requires 'expires_in' (or an explicit 'exp' claim — \
                 non-expiring tokens must be deliberate)"
                    .to_string(),
            ));
        }
    } else if let Some(Value::String(s)) = obj.get("expires_in")
        && let Err(e) = parse_duration_secs(s)
    {
        errors.push(("expires_in", "INVALID", e));
    }
    errors
}

// -- Input schema (F53) --

pub(super) const JWT_SIGN_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "algorithm",
        description: "Signing algorithm: HS/RS/PS 256-512, ES256/384, or EdDSA.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "key",
        description: "HS secret or RS/ES/Ed private-key PEM; a literal or a secret \
                      reference (env://NAME). Never appears in traces or errors.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "key_encoding",
        description: "How an HS secret becomes bytes: utf8 (default), base64, hex.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "claims",
        description: "The claims object; values fold {\"var\": ..} nodes (compose \
                      computed claims with a map task first). iat is stamped \
                      automatically.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "expires_in",
        description: "Token lifetime: integer seconds or \"<n>s|m|h|d\"; sets exp from \
                      now. Required unless claims carries an explicit exp.",
        kind: FieldKind::Any,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "issuer",
        description: "Convenience for the iss claim.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "audience",
        description: "Convenience for the aud claim (string or array; resolvable).",
        kind: FieldKind::Any,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "not_before",
        description: "Offset from now for the nbf claim: integer seconds or \
                      \"<n>s|m|h|d\".",
        kind: FieldKind::Any,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "kid",
        description: "Key id stamped into the token header, for rotation-aware \
                      verifiers.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the compact JWS (string) is stored. Defaults \
                      to \"data\".",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
];

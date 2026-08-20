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

use super::connector_helpers::{
    apply_output, parse_duration_secs, resolve_duration_secs, resolve_value,
};
use super::schema::{FieldKind, FieldSchema};

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
        let mut claims = match input.get("claims") {
            None => serde_json::Map::new(),
            Some(raw) => match resolve_value(raw, ctx) {
                Value::Object(map) => map,
                _ => return Err(validation("'claims' must resolve to an object")),
            },
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
            let offset = resolve_duration_secs(nbf, ctx, NAME, "not_before")?;
            claims.insert("nbf".to_string(), Value::from(now + offset as i64));
        }
        // `iat` is stamped only when the claims object does not supply one.
        //
        // Unlike `iss`/`aud`/`nbf`/`exp`, `iat` has no dedicated input field —
        // so there is nothing "more specific" to beat a claims entry, and the
        // entry should simply win. Overwriting it with an ambient value made
        // two things impossible: a revocation-pivot scheme cannot forward-date
        // a token minted in the same second as the pivot it must survive, and
        // no offline `*.case.json` case can assert a minted token, because the
        // bytes moved every run.
        //
        // `iat` is not security-bearing in Orion — nothing in the runtime makes
        // a trust decision on it — so author control costs no check. (`exp`,
        // which is, has been author-settable since #267 for the same reason:
        // a non-expiring token must be a deliberate, visible choice.)
        if let Some(iat) = claims.get("iat") {
            require_numeric_date(iat, "iat")?;
        } else {
            claims.insert("iat".to_string(), Value::from(now));
        }
        match input.get("expires_in") {
            Some(raw) if !raw.is_null() => {
                let secs = resolve_duration_secs(raw, ctx, NAME, "expires_in")?;
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
            _ if claims.contains_key("exp") => {
                // Same rule as `iat`: a non-numeric registered date mints a
                // token every verifier rejects, so refuse it here rather than
                // ship it.
                require_numeric_date(&claims["exp"], "exp")?;
            }
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

/// A registered date claim must be a JSON number — NumericDate, RFC 7519 §2.
///
/// A string or object here mints a token that every verifier rejects later,
/// which surfaces as an opaque failure at the far end of an integration. Refuse
/// it at sign time instead, where the message can name the claim.
fn require_numeric_date(value: &Value, claim: &'static str) -> Result<(), DataflowError> {
    if value.is_number() {
        return Ok(());
    }
    Err(validation(&format!(
        "claims.{claim} must be a number of seconds since the Unix epoch \
         (NumericDate, RFC 7519 §2), got {}",
        match value {
            Value::Null => "null",
            Value::Bool(_) => "a boolean",
            Value::String(_) => "a string",
            Value::Array(_) => "an array",
            Value::Object(_) => "an object",
            Value::Number(_) => unreachable!("handled above"),
        }
    )))
}

// -- Authoring-time validation (shared with schema::validate_input) --

pub(super) fn validate_static_input(
    obj: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    let mut errors: Vec<(&'static str, &'static str, String)> = Vec::new();

    // A missing or non-string `algorithm` is the field loop's REQUIRED /
    // TYPE_MISMATCH; only a present string is judged here.
    if let Some(name) = obj.get("algorithm").and_then(Value::as_str)
        && let Err(e) = crate::jwt::parse_algorithm(name)
    {
        errors.push(("algorithm", "INVALID", e));
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

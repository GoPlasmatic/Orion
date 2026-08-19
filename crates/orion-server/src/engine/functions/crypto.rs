//! `crypto` — digests, MACs and password hashing as one operation envelope
//! (#259 a+b).
//!
//! One registry name whose capabilities grow by table, not by API surface —
//! the same trade `data_write`'s envelope made. Self-contained: no connector,
//! no egress, deterministic given its inputs, which is why dry-run and
//! `orion-server test` run it for real instead of stubbing it.
//!
//! The op × algorithm capability tables below are shared with
//! `schema::validate_input` (via [`validate_static_input`]), so execution and
//! authoring-time validation cannot drift: an op×algorithm pair outside the
//! table — `password_hash` with `sha256`, `hash` with `argon2id` — is
//! unrepresentable, rejected at create/validate/lint, and refused here for
//! definitions that bypassed validation.
//!
//! Secret hygiene: `key` accepts a literal or a secret reference resolved by
//! the same registry connector configs use (`env://`, `vault://` when
//! configured). The resolved key and the submitted password exist only inside
//! this handler — never written to context, never quoted in an error.

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use hmac::{Hmac, KeyInit, Mac};
use md5::Md5;
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use super::connector_helpers::{apply_output, resolve_required_str, resolve_value};
use super::schema::{FieldKind, FieldSchema};
use crate::engine::operators::{Codec, decode_bytes, encode_bytes};

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "crypto";

// -- Capability tables (op × algorithm) --
//
// Server-side data, not schema enums: a new algorithm is a row here (plus its
// dispatch arm), touching no input schema and no workflow shape.

pub(crate) const OPS: &[&str] = &[
    "hash",
    "hmac",
    "hmac_verify",
    "password_hash",
    "password_verify",
];
/// `sha1`/`md5` are legacy-interop only — documented as such, never defaults.
pub(crate) const HASH_ALGORITHMS: &[&str] = &["sha256", "sha512", "sha1", "md5"];
pub(crate) const HMAC_ALGORITHMS: &[&str] = &["sha256", "sha512", "sha1"];
pub(crate) const PASSWORD_ALGORITHMS: &[&str] = &["argon2id", "bcrypt"];

// -- password_hash cost bounds --
//
// Bounded so a workflow cannot configure a DoS on itself; defaults follow
// current OWASP guidance (argon2id m=19456 KiB, t=2, p=1; bcrypt cost 12).

const ARGON2_DEFAULT_MEMORY_KIB: u32 = 19_456;
const ARGON2_DEFAULT_ITERATIONS: u32 = 2;
const ARGON2_DEFAULT_PARALLELISM: u32 = 1;
const ARGON2_MEMORY_KIB_RANGE: std::ops::RangeInclusive<u64> = 8_192..=131_072;
const ARGON2_ITERATIONS_RANGE: std::ops::RangeInclusive<u64> = 1..=10;
const ARGON2_PARALLELISM_RANGE: std::ops::RangeInclusive<u64> = 1..=4;
const BCRYPT_DEFAULT_COST: u32 = 12;
const BCRYPT_COST_RANGE: std::ops::RangeInclusive<u64> = 10..=14;

/// Workflow function handler for the `crypto` operation envelope. Stateless:
/// secret references resolve through the process-wide resolver registry.
pub struct CryptoHandler;

#[async_trait]
impl AsyncFunctionHandler for CryptoHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        let op = match input.get("op").and_then(Value::as_str) {
            Some(op) => op,
            None => return Err(validation("requires 'op' (string)")),
        };
        let output = input
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("data");

        let result = match op {
            "hash" => hash_op(input, ctx)?,
            "hmac" => hmac_op(input, ctx, HmacMode::Compute).await?,
            "hmac_verify" => hmac_op(input, ctx, HmacMode::Verify).await?,
            "password_hash" => password_hash_op(input, ctx).await?,
            "password_verify" => password_verify_op(input, ctx).await?,
            other => {
                return Err(validation(&format!(
                    "unknown op '{other}' — expected one of {}",
                    OPS.join(", ")
                )));
            }
        };

        apply_output(ctx, output, result);
        Ok(TaskOutcome::Success)
    }
}

fn validation(msg: &str) -> DataflowError {
    DataflowError::Validation(format!("{NAME}: {msg}"))
}

/// An optional literal string field; a present non-string is an error.
fn literal_str<'i>(input: &'i Value, field: &str) -> Result<Option<&'i str>, DataflowError> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(validation(&format!("'{field}' must be a string"))),
    }
}

/// The op's `algorithm`, defaulted and checked against its capability table.
fn algorithm<'i>(
    input: &'i Value,
    allowed: &[&str],
    default: &'i str,
) -> Result<&'i str, DataflowError> {
    match literal_str(input, "algorithm")? {
        None => Ok(default),
        Some(a) if allowed.contains(&a) => Ok(a),
        Some(other) => Err(validation(&format!(
            "algorithm '{other}' is not valid here — expected one of {}",
            allowed.join(", ")
        ))),
    }
}

/// The output `encoding` for `hash`/`hmac` results (default hex).
fn output_codec(input: &Value) -> Result<Codec, DataflowError> {
    match literal_str(input, "encoding")? {
        None | Some("hex") => Ok(Codec::Hex),
        Some("base64") => Ok(Codec::Base64),
        Some("base64url") => Ok(Codec::Base64Url),
        Some(other) => Err(validation(&format!(
            "unknown encoding '{other}' — expected one of hex, base64, base64url"
        ))),
    }
}

/// How a *string* value becomes bytes (`input_encoding` / `key_encoding`):
/// UTF-8 by default, or decoded from hex/base64 for binary material.
fn byte_codec(input: &Value, field: &str) -> Result<Option<Codec>, DataflowError> {
    match literal_str(input, field)? {
        None | Some("utf8") => Ok(None),
        Some("hex") => Ok(Some(Codec::Hex)),
        Some("base64") => Ok(Some(Codec::Base64)),
        Some(other) => Err(validation(&format!(
            "unknown {field} '{other}' — expected one of utf8, hex, base64"
        ))),
    }
}

/// The byte model of #259, applied to `data`: strings per `input_encoding`
/// (UTF-8 default), any other JSON value as its compact serialization — key
/// order preserved — so "sign this payload" is first-class and deterministic.
fn data_bytes(input: &Value, ctx: &TaskContext<'_>) -> Result<Vec<u8>, DataflowError> {
    let Some(raw) = input.get("data") else {
        return Err(validation("this op requires 'data'"));
    };
    let codec = byte_codec(input, "input_encoding")?;
    match resolve_value(raw, ctx) {
        Value::String(s) => match codec {
            None => Ok(s.into_bytes()),
            Some(c) => {
                decode_bytes(c, &s).map_err(|e| validation(&format!("'data' does not decode: {e}")))
            }
        },
        Value::Null => Err(validation(
            "'data' resolved to null — check the referenced path",
        )),
        other => {
            if codec.is_some() {
                return Err(validation(
                    "input_encoding applies to string data only — this data is JSON and \
                     would be hashed as its compact serialization",
                ));
            }
            serde_json::to_string(&other)
                .map(String::into_bytes)
                .map_err(|e| validation(&format!("'data' failed to serialize: {e}")))
        }
    }
}

fn digest_bytes(algorithm: &str, data: &[u8]) -> Vec<u8> {
    match algorithm {
        "sha256" => Sha256::digest(data).to_vec(),
        "sha512" => Sha512::digest(data).to_vec(),
        "sha1" => Sha1::digest(data).to_vec(),
        "md5" => Md5::digest(data).to_vec(),
        // The capability table gates every caller.
        other => unreachable!("hash algorithm '{other}' passed validation"),
    }
}

fn hash_op(input: &Value, ctx: &TaskContext<'_>) -> Result<Value, DataflowError> {
    let algorithm = algorithm(input, HASH_ALGORITHMS, "sha256")?;
    let codec = output_codec(input)?;
    let data = data_bytes(input, ctx)?;
    Ok(Value::String(encode_bytes(
        codec,
        &digest_bytes(algorithm, &data),
    )))
}

enum HmacMode {
    Compute,
    Verify,
}

/// The op's `key`, resolved (literal or secret reference) and decoded per
/// `key_encoding`. The resolved bytes never leave this call chain.
async fn hmac_key(input: &Value) -> Result<Vec<u8>, DataflowError> {
    let Some(key) = literal_str(input, "key")? else {
        return Err(validation(
            "this op requires 'key' (a literal or a secret reference like env://NAME)",
        ));
    };
    let resolved = crate::connector::secrets::resolve_secret_string(key, "crypto.key")
        .await
        .map_err(|e| validation(&e))?;
    if resolved.is_empty() {
        return Err(validation("'key' resolved to an empty value"));
    }
    match byte_codec(input, "key_encoding")? {
        None => Ok(resolved.into_bytes()),
        Some(c) => decode_bytes(c, &resolved)
            .map_err(|e| validation(&format!("'key' does not decode per key_encoding: {e}"))),
    }
}

fn mac_compute<M: Mac + KeyInit>(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = M::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn mac_verify<M: Mac + KeyInit>(key: &[u8], data: &[u8], signature: &[u8]) -> bool {
    let mut mac = M::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    // Constant-time and length-checked — the reason `hmac_verify` exists at
    // all: without it the obvious spelling is `==` on the computed MAC.
    mac.verify_slice(signature).is_ok()
}

/// The presented MAC of an `hmac_verify`: hex or base64 (either form),
/// auto-detected — the same rule channel auth applies to inbound signatures.
fn decode_presented_signature(s: &str) -> Result<Vec<u8>, DataflowError> {
    decode_bytes(Codec::Hex, s)
        .or_else(|_| decode_bytes(Codec::Base64, s))
        .or_else(|_| decode_bytes(Codec::Base64Url, s))
        .map_err(|_| validation("'signature' is neither hex nor base64"))
}

async fn hmac_op(
    input: &Value,
    ctx: &TaskContext<'_>,
    mode: HmacMode,
) -> Result<Value, DataflowError> {
    // F58 ordering: the message-independent refusals (algorithm, key) come
    // before anything the message can change (data, signature).
    let algorithm = algorithm(input, HMAC_ALGORITHMS, "sha256")?;
    let key = hmac_key(input).await?;
    let data = data_bytes(input, ctx)?;

    match mode {
        HmacMode::Compute => {
            let codec = output_codec(input)?;
            let mac = match algorithm {
                "sha256" => mac_compute::<Hmac<Sha256>>(&key, &data),
                "sha512" => mac_compute::<Hmac<Sha512>>(&key, &data),
                "sha1" => mac_compute::<Hmac<Sha1>>(&key, &data),
                other => unreachable!("hmac algorithm '{other}' passed validation"),
            };
            Ok(Value::String(encode_bytes(codec, &mac)))
        }
        HmacMode::Verify => {
            let signature = resolve_required_str(input, "signature", NAME, ctx)?;
            let signature = decode_presented_signature(&signature)?;
            let ok = match algorithm {
                "sha256" => mac_verify::<Hmac<Sha256>>(&key, &data, &signature),
                "sha512" => mac_verify::<Hmac<Sha512>>(&key, &data, &signature),
                "sha1" => mac_verify::<Hmac<Sha1>>(&key, &data, &signature),
                other => unreachable!("hmac algorithm '{other}' passed validation"),
            };
            Ok(Value::Bool(ok))
        }
    }
}

/// A `params` cost knob: defaulted, must be an integer inside `range`.
fn cost_param(
    params: Option<&Value>,
    key: &str,
    default: u32,
    range: &std::ops::RangeInclusive<u64>,
) -> Result<u32, DataflowError> {
    let Some(v) = params.and_then(|p| p.get(key)) else {
        return Ok(default);
    };
    match v.as_u64() {
        Some(n) if range.contains(&n) => Ok(n as u32),
        _ => Err(validation(&format!(
            "params.{key} must be an integer between {} and {}",
            range.start(),
            range.end()
        ))),
    }
}

/// Reject `params` keys the algorithm does not take — a typoed cost knob must
/// not silently mean "use the default".
fn check_param_keys(params: Option<&Value>, allowed: &[&str]) -> Result<(), DataflowError> {
    let Some(Value::Object(map)) = params else {
        if params.is_some_and(|p| !p.is_null()) {
            return Err(validation("'params' must be an object"));
        }
        return Ok(());
    };
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(validation(&format!(
                "unknown params key '{key}' — this algorithm takes {}",
                allowed.join(", ")
            )));
        }
    }
    Ok(())
}

async fn password_hash_op(input: &Value, ctx: &TaskContext<'_>) -> Result<Value, DataflowError> {
    let algorithm = algorithm(input, PASSWORD_ALGORITHMS, "argon2id")?;
    let password = resolve_required_str(input, "password", NAME, ctx)?;
    let params = input.get("params");

    match algorithm {
        "argon2id" => {
            check_param_keys(params, &["memory_kib", "iterations", "parallelism"])?;
            let memory = cost_param(
                params,
                "memory_kib",
                ARGON2_DEFAULT_MEMORY_KIB,
                &ARGON2_MEMORY_KIB_RANGE,
            )?;
            let iterations = cost_param(
                params,
                "iterations",
                ARGON2_DEFAULT_ITERATIONS,
                &ARGON2_ITERATIONS_RANGE,
            )?;
            let parallelism = cost_param(
                params,
                "parallelism",
                ARGON2_DEFAULT_PARALLELISM,
                &ARGON2_PARALLELISM_RANGE,
            )?;
            // Memory-hard by design, so off the async worker (W: ~20 MiB and
            // tens of milliseconds per call at the defaults).
            spawn_hashing(move || {
                use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};
                use argon2::{Algorithm, Argon2, Params, Version};
                let params = Params::new(memory, iterations, parallelism, None)
                    .map_err(|e| format!("argon2 parameters were rejected: {e}"))?;
                let salt = SaltString::generate(&mut OsRng);
                Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
                    .hash_password(password.as_bytes(), &salt)
                    .map(|h| Value::String(h.to_string()))
                    .map_err(|e| format!("argon2 hashing failed: {e}"))
            })
            .await
        }
        "bcrypt" => {
            check_param_keys(params, &["cost"])?;
            let cost = cost_param(params, "cost", BCRYPT_DEFAULT_COST, &BCRYPT_COST_RANGE)?;
            spawn_hashing(move || {
                bcrypt::hash(&password, cost)
                    .map(Value::String)
                    .map_err(|e| format!("bcrypt hashing failed: {e}"))
            })
            .await
        }
        other => unreachable!("password algorithm '{other}' passed validation"),
    }
}

async fn password_verify_op(input: &Value, ctx: &TaskContext<'_>) -> Result<Value, DataflowError> {
    let password = resolve_required_str(input, "password", NAME, ctx)?;
    let hash = resolve_required_str(input, "hash", NAME, ctx)?;

    // Scheme auto-detected from the stored hash, which is what turns a
    // bcrypt → argon2id migration into a verify-then-rehash pair inside one
    // login workflow. Wrong password → false; a *malformed* stored hash is a
    // task error — silent-false would make data corruption indistinguishable
    // from a bad credential.
    spawn_hashing(move || {
        if hash.starts_with("$argon2") {
            use argon2::password_hash::{Error, PasswordHash, PasswordVerifier};
            let parsed = PasswordHash::new(&hash)
                .map_err(|e| format!("stored hash is not a valid PHC string: {e}"))?;
            match argon2::Argon2::default().verify_password(password.as_bytes(), &parsed) {
                Ok(()) => Ok(Value::Bool(true)),
                Err(Error::Password) => Ok(Value::Bool(false)),
                Err(e) => Err(format!("stored hash could not be checked: {e}")),
            }
        } else if hash.starts_with("$2") {
            bcrypt::verify(&password, &hash)
                .map(Value::Bool)
                .map_err(|e| format!("stored bcrypt hash is malformed: {e}"))
        } else {
            Err(
                "unrecognized password hash scheme — expected an $argon2*$ PHC string or a \
                 $2*$ bcrypt hash"
                    .to_string(),
            )
        }
    })
    .await
}

/// Run a CPU-bound hashing closure on the blocking pool. Password KDFs are
/// deliberately slow; running them inline would stall an async worker for
/// tens of milliseconds per login.
async fn spawn_hashing<F>(f: F) -> Result<Value, DataflowError>
where
    F: FnOnce() -> Result<Value, String> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| {
            DataflowError::function_execution(format!("{NAME}: hashing task failed: {e}"), None)
        })?
        .map_err(|e| validation(&e))
}

// -- Authoring-time validation (shared with schema::validate_input) --

/// Cross-field checks over a *static* `crypto` input: op membership, the
/// op × algorithm table, per-op required fields, encoding vocabularies, and
/// `params` bounds. Returns `(path-suffix, code, message)` triples; an empty
/// suffix addresses the input object itself.
///
/// Fields produced by `{"var": ..}` only exist per message; their shape is
/// checked at execution by the same functions this file runs there.
pub(super) fn validate_static_input(
    obj: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    let mut errors: Vec<(&'static str, &'static str, String)> = Vec::new();
    let input = Value::Object(obj.clone());

    // A missing or mistyped `op` is already reported by the field loop.
    let Some(op) = obj.get("op").and_then(Value::as_str) else {
        return errors;
    };
    if !OPS.contains(&op) {
        errors.push((
            "op",
            "INVALID",
            format!("unknown op '{op}' — expected one of {}", OPS.join(", ")),
        ));
        return errors;
    }

    // Which fields this op takes (beyond op/output), and which it requires.
    // A field outside the op's set is flagged: `password_hash` with a `key`,
    // or `hash` with a `signature`, is a misunderstanding worth naming.
    let (required, optional): (&[&str], &[&str]) = match op {
        "hash" => (&["data"], &["algorithm", "input_encoding", "encoding"]),
        "hmac" => (
            &["data", "key"],
            &["algorithm", "input_encoding", "encoding", "key_encoding"],
        ),
        "hmac_verify" => (
            &["data", "key", "signature"],
            &["algorithm", "input_encoding", "key_encoding"],
        ),
        "password_hash" => (&["password"], &["algorithm", "params"]),
        "password_verify" => (&["password", "hash"], &[]),
        _ => unreachable!("op membership checked above"),
    };
    for field in required {
        if obj.get(*field).is_none_or(Value::is_null) {
            // Leak the constant name back out of the loop via a static match
            // so the tuple can stay &'static str.
            errors.push((
                field_name(field),
                "REQUIRED",
                format!("op '{op}' requires '{field}'"),
            ));
        }
    }
    for (key, _) in obj {
        let known = key == "op"
            || key == "output"
            || required.contains(&key.as_str())
            || optional.contains(&key.as_str());
        if !known && field_exists(key) {
            errors.push((
                field_name(key),
                "INVALID",
                format!("'{key}' does not apply to op '{op}'"),
            ));
        }
    }

    // Value vocabularies — reuse the exact execution-path parsers.
    let allowed_algorithms: &[&str] = match op {
        "hash" => HASH_ALGORITHMS,
        "hmac" | "hmac_verify" => HMAC_ALGORITHMS,
        "password_hash" => PASSWORD_ALGORITHMS,
        // password_verify auto-detects; `algorithm` is flagged above as
        // not-applicable.
        _ => &[],
    };
    if !allowed_algorithms.is_empty()
        && let Err(e) = algorithm(&input, allowed_algorithms, allowed_algorithms[0])
    {
        errors.push(("algorithm", "INVALID", strip_name(&e)));
    }
    if optional.contains(&"encoding")
        && let Err(e) = output_codec(&input)
    {
        errors.push(("encoding", "INVALID", strip_name(&e)));
    }
    for enc_field in ["input_encoding", "key_encoding"] {
        if optional.contains(&enc_field)
            && let Err(e) = byte_codec(&input, enc_field)
        {
            errors.push((field_name(enc_field), "INVALID", strip_name(&e)));
        }
    }

    // A static `data` that is not a string cannot take an `input_encoding`.
    if matches!(op, "hash" | "hmac" | "hmac_verify")
        && obj.get("input_encoding").is_some()
        && obj
            .get("data")
            .is_some_and(|d| !d.is_string() && !d.is_null() && !is_resolvable(d))
    {
        errors.push((
            "data",
            "INVALID",
            "input_encoding applies to string data only — this static data is JSON and \
             would be hashed as its compact serialization"
                .to_string(),
        ));
    }

    // `params` keys and bounds, per the (defaulted) algorithm.
    if op == "password_hash" {
        let params = obj.get("params");
        let algorithm = obj
            .get("algorithm")
            .and_then(Value::as_str)
            .unwrap_or("argon2id");
        let check: Result<(), DataflowError> = (|| {
            match algorithm {
                "argon2id" => {
                    check_param_keys(params, &["memory_kib", "iterations", "parallelism"])?;
                    cost_param(
                        params,
                        "memory_kib",
                        ARGON2_DEFAULT_MEMORY_KIB,
                        &ARGON2_MEMORY_KIB_RANGE,
                    )?;
                    cost_param(
                        params,
                        "iterations",
                        ARGON2_DEFAULT_ITERATIONS,
                        &ARGON2_ITERATIONS_RANGE,
                    )?;
                    cost_param(
                        params,
                        "parallelism",
                        ARGON2_DEFAULT_PARALLELISM,
                        &ARGON2_PARALLELISM_RANGE,
                    )?;
                }
                "bcrypt" => {
                    check_param_keys(params, &["cost"])?;
                    cost_param(params, "cost", BCRYPT_DEFAULT_COST, &BCRYPT_COST_RANGE)?;
                }
                // Unknown algorithm already reported above.
                _ => {}
            }
            Ok(())
        })();
        if let Err(e) = check {
            errors.push(("params", "INVALID", strip_name(&e)));
        }
    }

    errors
}

/// Whether an authored value is a `{"var": ..}` node the handler will resolve
/// per message (so its static shape cannot be judged here).
fn is_resolvable(v: &Value) -> bool {
    v.as_object().is_some_and(|o| o.contains_key("var"))
}

/// Map a field key to its `&'static str` — every key this function can flag
/// is in [`CRYPTO_FIELDS`], so the fallback is unreachable for real inputs
/// and merely safe for arbitrary ones.
fn field_name(key: &str) -> &'static str {
    CRYPTO_FIELDS
        .iter()
        .map(|f| f.name)
        .find(|n| *n == key)
        .unwrap_or("op")
}

fn field_exists(key: &str) -> bool {
    CRYPTO_FIELDS.iter().any(|f| f.name == key)
}

/// Drop the `"crypto: "` prefix [`validation`] adds — as a `FieldError`
/// message the field path already carries the context.
fn strip_name(e: &DataflowError) -> String {
    let s = e.to_string();
    match s.split_once(&format!("{NAME}: ")) {
        Some((_, msg)) => msg.to_string(),
        None => s,
    }
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes, like every other function's.

pub(super) const CRYPTO_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "op",
        description: "Operation: hash, hmac, hmac_verify, password_hash, or password_verify.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "algorithm",
        description: "Algorithm within the op's capability table (e.g. sha256, sha512, \
                      argon2id). Per-op default; password_verify auto-detects from the \
                      stored hash.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "data",
        description: "Bytes to digest (hash/hmac/hmac_verify). A string is used as UTF-8 \
                      (see input_encoding); any other JSON value is hashed as its compact \
                      serialization.",
        kind: FieldKind::Any,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "input_encoding",
        description: "How a string 'data' becomes bytes: utf8 (default), hex, or base64.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "key",
        description: "HMAC key (hmac/hmac_verify): a literal or a secret reference like \
                      env://NAME, resolved like connector secrets. Never appears in \
                      traces or errors.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "key_encoding",
        description: "How the resolved key becomes bytes: utf8 (default), hex, or base64 \
                      — for APIs that issue binary signing keys.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "signature",
        description: "The presented MAC to check (hmac_verify); hex or base64, \
                      auto-detected. Compared in constant time.",
        kind: FieldKind::String,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "password",
        description: "The submitted password (password_hash/password_verify).",
        kind: FieldKind::String,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "hash",
        description: "The stored password hash to verify against (password_verify); \
                      scheme auto-detected from its $argon2*$/$2*$ prefix.",
        kind: FieldKind::String,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "encoding",
        description: "Output encoding for hash/hmac results: hex (default), base64, or \
                      base64url (unpadded).",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "params",
        description: "password_hash cost tuning: argon2id takes memory_kib/iterations/\
                      parallelism, bcrypt takes cost. Safe defaults; bounded ranges.",
        kind: FieldKind::Object,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the result is stored. Defaults to \"data\". \
                      String for hash/hmac/password_hash; boolean for \
                      hmac_verify/password_verify.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build an engine with one crypto task and run one message through it.
    async fn run(input: Value, data: Value) -> Result<Value, String> {
        let workflow: dataflow_rs::Workflow = serde_json::from_value(json!({
            "id": "w", "name": "w", "condition": true,
            "tasks": [{"id": "t", "name": "t",
                       "function": {"name": "crypto", "input": input}}]
        }))
        .map_err(|e| e.to_string())?;
        let mut fns: std::collections::HashMap<String, dataflow_rs::BoxedFunctionHandler> =
            Default::default();
        fns.insert("crypto".to_string(), Box::new(CryptoHandler));
        let engine = dataflow_rs::Engine::new(vec![workflow], fns).map_err(|e| e.to_string())?;
        let mut message = dataflow_rs::Message::from_value(&json!({}));
        dataflow_rs::engine::utils::set_nested_value(
            &mut message.context,
            "data",
            dataflow_rs::datavalue::OwnedDataValue::from(&data),
        );
        engine
            .process_message(&mut message)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(err) = message.errors().first() {
            return Err(format!("{err:?}"));
        }
        Ok(message.data().into())
    }

    #[tokio::test]
    async fn hash_matches_the_nist_vectors() {
        // FIPS 180 "abc" vectors (and RFC 1321 for md5) — wrong bytes or a
        // wrong algorithm dispatch cannot produce these.
        for (algorithm, expected) in [
            (
                "sha256",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                "sha512",
                "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
                 2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
            ),
            ("sha1", "a9993e364706816aba3e25717850c26c9cd0d89d"),
            ("md5", "900150983cd24fb0d6963f7d28e17f72"),
        ] {
            let out = run(
                json!({"op": "hash", "algorithm": algorithm,
                       "data": "abc", "output": "data.digest"}),
                json!({}),
            )
            .await
            .expect(algorithm);
            assert_eq!(out["digest"], json!(expected), "{algorithm}");
        }
    }

    #[tokio::test]
    async fn hmac_matches_rfc_4231_and_rfc_2202() {
        // Test case 2 of both RFCs: key "Jefe", data "what do ya want for
        // nothing?".
        for (algorithm, expected) in [
            (
                "sha256",
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                "sha512",
                "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea250554\
                 9758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737",
            ),
            ("sha1", "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"),
        ] {
            let out = run(
                json!({"op": "hmac", "algorithm": algorithm, "key": "Jefe",
                       "data": {"var": "data.challenge"}, "output": "data.mac"}),
                json!({"challenge": "what do ya want for nothing?"}),
            )
            .await
            .expect(algorithm);
            assert_eq!(out["mac"], json!(expected), "{algorithm}");
        }
    }

    #[tokio::test]
    async fn hmac_output_encodings_and_binary_key() {
        // RFC 4231 case 1: 20 bytes of 0x0b as the key — only expressible
        // through key_encoding — data "Hi There".
        let expected_hex = "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
        let out = run(
            json!({"op": "hmac", "key": "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b",
                   "key_encoding": "hex", "data": "Hi There",
                   "encoding": "base64", "output": "data.mac"}),
            json!({}),
        )
        .await
        .expect("test");
        let expected_b64 = crate::engine::operators::encode_bytes(
            Codec::Base64,
            &hex::decode(expected_hex).expect("test"),
        );
        assert_eq!(out["mac"], json!(expected_b64));
    }

    #[tokio::test]
    async fn hmac_verify_is_a_boolean_with_auto_detected_encoding() {
        let mac_hex = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
        let mac_b64 = crate::engine::operators::encode_bytes(
            Codec::Base64,
            &hex::decode(mac_hex).expect("test"),
        );
        // The same MAC verifies presented as hex or as base64.
        for presented in [mac_hex.to_string(), mac_b64] {
            let out = run(
                json!({"op": "hmac_verify", "key": "Jefe",
                       "data": "what do ya want for nothing?",
                       "signature": {"var": "data.sig"}, "output": "data.ok"}),
                json!({"sig": presented}),
            )
            .await
            .expect("test");
            assert_eq!(out["ok"], json!(true));
        }
        // A wrong MAC of the right shape is false, not an error.
        let out = run(
            json!({"op": "hmac_verify", "key": "Jefe",
                   "data": "what do ya want for nothing?",
                   "signature": "00".repeat(32), "output": "data.ok"}),
            json!({}),
        )
        .await
        .expect("test");
        assert_eq!(out["ok"], json!(false));
        // Garbage that decodes as nothing is an error, not false.
        let err = run(
            json!({"op": "hmac_verify", "key": "Jefe", "data": "x",
                   "signature": "!!not-an-encoding!!", "output": "data.ok"}),
            json!({}),
        )
        .await
        .expect_err("test");
        assert!(err.contains("signature"), "{err}");
    }

    #[tokio::test]
    async fn non_string_data_is_hashed_as_compact_json() {
        // The byte model: an object digests as its compact serialization with
        // authored key order.
        let out = run(
            json!({"op": "hash", "data": {"var": "data.payload"}, "output": "data.digest"}),
            json!({"payload": {"b": 1, "a": 2}}),
        )
        .await
        .expect("test");
        let expected = hex::encode(Sha256::digest(br#"{"b":1,"a":2}"#));
        assert_eq!(out["digest"], json!(expected));
    }

    #[tokio::test]
    async fn input_encoding_decodes_binary_payloads() {
        // sha256 of the two raw bytes 0xff 0xfe — unreachable through UTF-8.
        let out = run(
            json!({"op": "hash", "data": "fffe", "input_encoding": "hex",
                   "output": "data.digest"}),
            json!({}),
        )
        .await
        .expect("test");
        assert_eq!(
            out["digest"],
            json!(hex::encode(Sha256::digest([0xffu8, 0xfe])))
        );
    }

    #[tokio::test]
    async fn password_hash_and_verify_round_trip() {
        // argon2id default → PHC string → verify true / false.
        let out = run(
            json!({"op": "password_hash",
                   // Minimum-cost params keep the test fast; bounds are
                   // asserted separately.
                   "params": {"memory_kib": 8192, "iterations": 1},
                   "password": "correct horse", "output": "data.phc"}),
            json!({}),
        )
        .await
        .expect("test");
        let phc = out["phc"].as_str().expect("test").to_string();
        assert!(phc.starts_with("$argon2id$"), "{phc}");

        for (candidate, expected) in [("correct horse", true), ("battery staple", false)] {
            let out = run(
                json!({"op": "password_verify", "password": candidate,
                       "hash": {"var": "data.stored"}, "output": "data.ok"}),
                json!({"stored": phc}),
            )
            .await
            .expect("test");
            assert_eq!(out["ok"], json!(expected), "{candidate}");
        }
    }

    #[tokio::test]
    async fn bcrypt_hashes_verify_and_support_rehash_detection() {
        let out = run(
            json!({"op": "password_hash", "algorithm": "bcrypt",
                   "params": {"cost": 10},
                   "password": "legacy pw", "output": "data.hash"}),
            json!({}),
        )
        .await
        .expect("test");
        let stored = out["hash"].as_str().expect("test").to_string();
        // `$2*$` prefix is the rehash-on-login discriminator the issue
        // documents (`starts_with` in workflow logic).
        assert!(stored.starts_with("$2"), "{stored}");

        let out = run(
            json!({"op": "password_verify", "password": "legacy pw",
                   "hash": {"var": "data.stored"}, "output": "data.ok"}),
            json!({"stored": stored}),
        )
        .await
        .expect("test");
        assert_eq!(out["ok"], json!(true));
    }

    #[tokio::test]
    async fn malformed_stored_hash_is_an_error_not_false() {
        // Data corruption must be distinguishable from a wrong password.
        let err = run(
            json!({"op": "password_verify", "password": "x",
                   "hash": "plainly-not-a-hash", "output": "data.ok"}),
            json!({}),
        )
        .await
        .expect_err("test");
        assert!(err.contains("hash scheme"), "{err}");
    }

    #[tokio::test]
    async fn table_violations_are_refused_at_execution_too() {
        // The runtime guard behind the authoring-time table — for
        // definitions that bypassed validation.
        for input in [
            json!({"op": "password_hash", "algorithm": "sha256", "password": "x"}),
            json!({"op": "hash", "algorithm": "argon2id", "data": "x"}),
            json!({"op": "melt", "data": "x"}),
            json!({"op": "hmac", "data": "x"}), // key missing
            json!({"op": "password_hash", "password": "x", "params": {"cost": 4}}),
            json!({"op": "password_hash", "password": "x",
                   "params": {"memory_mib": 64}}), // typoed knob
        ] {
            let err = run(input.clone(), json!({})).await.expect_err("test");
            assert!(err.contains("crypto"), "{input}: {err}");
        }
    }

    #[test]
    fn static_validation_reads_the_same_tables() {
        // Spot checks; the full matrix lives in the schema tests.
        let obj = json!({"op": "hmac", "data": "x"});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(
            errs.iter().any(|(f, c, _)| *f == "key" && *c == "REQUIRED"),
            "{errs:?}"
        );

        let obj = json!({"op": "password_hash", "password": "x", "key": "why"});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(
            errs.iter().any(|(f, c, _)| *f == "key" && *c == "INVALID"),
            "{errs:?}"
        );

        let obj = json!({"op": "hash", "data": "x", "encoding": "base32"});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(
            errs.iter()
                .any(|(f, c, _)| *f == "encoding" && *c == "INVALID"),
            "{errs:?}"
        );
    }
}

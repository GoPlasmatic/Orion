//! `crypto` — digests, MACs and password hashing as one operation envelope
//! (#259 a+b).
//!
//! One registry name whose capabilities grow by table, not by API surface —
//! the same trade `data_write`'s envelope made. Self-contained: no connector,
//! no egress, deterministic given its inputs, which is why dry-run and
//! `orion-cli test` run it for real instead of stubbing it.
//!
//! The op × algorithm capability tables below are shared with
//! `schema::validate_input` (via `validate_static_input`), so execution and
//! authoring-time validation cannot drift: an op×algorithm pair outside the
//! table — `password_hash` with `sha256`, `hash` with `argon2id` — is
//! unrepresentable, rejected at create/validate/lint, and refused here for
//! definitions that bypassed validation.
//!
//! Secret hygiene: `key` accepts a literal or a secret reference resolved by
//! the same registry connector configs use (`env://`, `vault://` when
//! configured). The resolved key and the submitted password exist only inside
//! this handler — never written to context, never quoted in an error.

use super::definition::{Definition, FieldOverlay, NoConnector, OrionFunction};
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::{Ctx, Expr, Function, Issue, Outcome};
use hmac::Hmac;
use md5::Md5;
use serde_json::{Map, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use super::schema::{FieldKind, RetrySafety};
use crate::crypto::{Codec, decode_bytes, encode_bytes, mac_compute, mac_verify};
use crate::kit::{self, KeyMaterial, read};
use crate::{ErrorClass, HandlerError};

/// This handler's name in metrics, profiles and error messages (F48).
pub const NAME: &str = "crypto";

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

/// Default maximum concurrent password KDF operations (`TD-04`).
const DEFAULT_KDF_MAX_CONCURRENCY: usize = 16;
/// Default maximum aggregate Argon2/bcrypt memory in flight (512 MiB in KiB) (`TD-04`).
const DEFAULT_KDF_MAX_MEMORY_KIB: u32 = 524_288;
/// Default maximum queue wait time before rejecting a KDF request under load (`TD-04`).
const DEFAULT_KDF_QUEUE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Nominal memory footprint attributed to one bcrypt operation (4 KiB).
const BCRYPT_MEMORY_KIB: u32 = 4;

/// Process-wide KDF admission controller bounding concurrent blocking-pool
/// threads and aggregate memory-hard KDF allocation (`TD-04`).
struct KdfAdmission {
    concurrency: std::sync::Arc<tokio::sync::Semaphore>,
    memory_kib: std::sync::Arc<tokio::sync::Semaphore>,
    max_memory_kib: u32,
    queue_timeout: std::time::Duration,
}

struct KdfPermit {
    _concurrency: tokio::sync::OwnedSemaphorePermit,
    _memory_kib: tokio::sync::OwnedSemaphorePermit,
}

impl Default for KdfAdmission {
    fn default() -> Self {
        Self::new(
            DEFAULT_KDF_MAX_CONCURRENCY,
            DEFAULT_KDF_MAX_MEMORY_KIB,
            DEFAULT_KDF_QUEUE_TIMEOUT,
        )
    }
}

impl KdfAdmission {
    fn new(
        max_concurrency: usize,
        max_memory_kib: u32,
        queue_timeout: std::time::Duration,
    ) -> Self {
        Self {
            concurrency: std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrency.max(1))),
            memory_kib: std::sync::Arc::new(tokio::sync::Semaphore::new(
                (max_memory_kib.max(1)) as usize,
            )),
            max_memory_kib: max_memory_kib.max(1),
            queue_timeout,
        }
    }

    async fn acquire(&self, requested_memory_kib: u32) -> Result<KdfPermit, HandlerError> {
        let mem_units = requested_memory_kib.clamp(1, self.max_memory_kib);
        let conc_sem = std::sync::Arc::clone(&self.concurrency);
        let mem_sem = std::sync::Arc::clone(&self.memory_kib);

        let acquire_fut = async move {
            let conc_permit = conc_sem.acquire_owned().await.map_err(|_| {
                HandlerError::new(
                    ErrorClass::Backend,
                    "password KDF admission closed".to_string(),
                )
            })?;
            let mem_permit = mem_sem.acquire_many_owned(mem_units).await.map_err(|_| {
                HandlerError::new(
                    ErrorClass::Backend,
                    "password KDF memory admission closed".to_string(),
                )
            })?;
            Ok::<KdfPermit, HandlerError>(KdfPermit {
                _concurrency: conc_permit,
                _memory_kib: mem_permit,
            })
        };

        tokio::time::timeout(self.queue_timeout, acquire_fut)
            .await
            .map_err(|_| {
                HandlerError::new(
                    ErrorClass::Timeout,
                    format!(
                        "password KDF queue timed out after {}ms waiting for admission",
                        self.queue_timeout.as_millis()
                    ),
                )
            })?
    }
}

static KDF_ADMISSION: std::sync::LazyLock<KdfAdmission> =
    std::sync::LazyLock::new(KdfAdmission::default);

/// An optional literal string field; a present non-string is an error.
fn literal_str<'i>(input: &'i Value, field: &str) -> Result<Option<&'i str>, HandlerError> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(HandlerError::caller_input(format!(
            "'{field}' must be a string"
        ))),
    }
}

/// The op's `algorithm`, defaulted and checked against its capability table.
fn algorithm<'i>(
    input: &'i Value,
    allowed: &[&str],
    default: &'i str,
) -> Result<&'i str, HandlerError> {
    algorithm_named(literal_str(input, "algorithm")?, allowed, default)
}

/// [`algorithm`] over the name the task gave, if any: shared by both
/// engines' paths.
fn algorithm_named<'i>(
    name: Option<&'i str>,
    allowed: &[&str],
    default: &'i str,
) -> Result<&'i str, HandlerError> {
    match name {
        None => Ok(default),
        Some(a) if allowed.contains(&a) => Ok(a),
        Some(other) => Err(HandlerError::caller_input(format!(
            "algorithm '{other}' is not valid here — expected one of {}",
            allowed.join(", ")
        ))),
    }
}

/// The output `encoding` for `hash`/`hmac` results (default hex).
fn output_codec(input: &Value) -> Result<Codec, HandlerError> {
    codec_named(literal_str(input, "encoding")?)
}

/// [`output_codec`] over the name the task gave, if any.
fn codec_named(name: Option<&str>) -> Result<Codec, HandlerError> {
    match name {
        None => Ok(Codec::Hex),
        Some(name) => Codec::parse(name).ok_or_else(|| {
            HandlerError::caller_input(format!(
                "unknown encoding '{name}' — expected one of hex, base64, base64url"
            ))
        }),
    }
}

/// How a *string* value becomes bytes (`input_encoding` / `key_encoding`):
/// UTF-8 by default, or decoded from hex/base64 for binary material.
fn byte_codec(input: &Value, field: &str) -> Result<Option<Codec>, HandlerError> {
    byte_codec_named(literal_str(input, field)?, field)
}

/// [`byte_codec`] over the name the task gave, if any.
fn byte_codec_named(name: Option<&str>, field: &str) -> Result<Option<Codec>, HandlerError> {
    match name {
        None | Some("utf8") => Ok(None),
        Some("hex") => Ok(Some(Codec::Hex)),
        Some("base64") => Ok(Some(Codec::Base64)),
        Some(other) => Err(HandlerError::caller_input(format!(
            "unknown {field} '{other}' — expected one of utf8, hex, base64"
        ))),
    }
}

/// The byte model over the value `data` resolved to.
fn bytes_of(data: Value, codec: Option<Codec>) -> Result<Vec<u8>, HandlerError> {
    match data {
        Value::String(s) => match codec {
            None => Ok(s.into_bytes()),
            Some(c) => decode_bytes(c, &s)
                .map_err(|e| HandlerError::caller_input(format!("'data' does not decode: {e}"))),
        },
        Value::Null => Err(HandlerError::caller_input(
            "'data' resolved to null — check the referenced path",
        )),
        other => {
            if codec.is_some() {
                return Err(HandlerError::caller_input(
                    "input_encoding applies to string data only — this data is JSON and \
                     would be hashed as its compact serialization",
                ));
            }
            serde_json::to_string(&other)
                .map(String::into_bytes)
                .map_err(|e| HandlerError::caller_input(format!("'data' failed to serialize: {e}")))
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

enum HmacMode {
    Compute,
    Verify,
}

/// The resolved key's bytes, by its `key_encoding`'s codec: an empty key is
/// refused before the codec is judged, as it always was. Shared by both
/// engines' paths.
fn key_bytes(
    resolved: String,
    codec: Result<Option<Codec>, HandlerError>,
) -> Result<Vec<u8>, HandlerError> {
    if resolved.is_empty() {
        return Err(HandlerError::caller_input(
            "'key' resolved to an empty value",
        ));
    }
    match codec? {
        None => Ok(resolved.into_bytes()),
        Some(c) => decode_bytes(c, &resolved).map_err(|e| {
            HandlerError::caller_input(format!("'key' does not decode per key_encoding: {e}"))
        }),
    }
}

/// The MAC of `data` under `key`, by a table algorithm.
fn mac(algorithm: &str, key: &[u8], data: &[u8]) -> Vec<u8> {
    match algorithm {
        "sha256" => mac_compute::<Hmac<Sha256>>(key, data),
        "sha512" => mac_compute::<Hmac<Sha512>>(key, data),
        "sha1" => mac_compute::<Hmac<Sha1>>(key, data),
        other => unreachable!("hmac algorithm '{other}' passed validation"),
    }
}

/// Whether `signature` is the MAC of `data` under `key`, compared in
/// constant time.
fn mac_matches(algorithm: &str, key: &[u8], data: &[u8], signature: &[u8]) -> bool {
    match algorithm {
        "sha256" => mac_verify::<Hmac<Sha256>>(key, data, signature),
        "sha512" => mac_verify::<Hmac<Sha512>>(key, data, signature),
        "sha1" => mac_verify::<Hmac<Sha1>>(key, data, signature),
        other => unreachable!("hmac algorithm '{other}' passed validation"),
    }
}

/// The presented MAC of an `hmac_verify`: hex, base64, or base64url,
/// auto-detected. (Channel HMAC auth's auto-detection is deliberately
/// narrower — hex then standard base64, its pre-#264 behaviour — so the two
/// rules are related but not identical.)
fn decode_presented_signature(s: &str) -> Result<Vec<u8>, HandlerError> {
    decode_bytes(Codec::Hex, s)
        .or_else(|_| decode_bytes(Codec::Base64, s))
        .or_else(|_| decode_bytes(Codec::Base64Url, s))
        .map_err(|_| HandlerError::caller_input("'signature' is not hex, base64, or base64url"))
}

/// A `params` cost knob: defaulted, must be an integer inside `range`.
fn cost_param(
    params: Option<&Value>,
    key: &str,
    default: u32,
    range: &std::ops::RangeInclusive<u64>,
) -> Result<u32, HandlerError> {
    let Some(v) = params.and_then(|p| p.get(key)) else {
        return Ok(default);
    };
    match v.as_u64() {
        Some(n) if range.contains(&n) => Ok(n as u32),
        _ => Err(HandlerError::caller_input(format!(
            "params.{key} must be an integer between {} and {}",
            range.start(),
            range.end()
        ))),
    }
}

/// Reject `params` keys the algorithm does not take — a typoed cost knob must
/// not silently mean "use the default".
fn check_param_keys(params: Option<&Value>, allowed: &[&str]) -> Result<(), HandlerError> {
    let Some(Value::Object(map)) = params else {
        if params.is_some_and(|p| !p.is_null()) {
            return Err(HandlerError::caller_input("'params' must be an object"));
        }
        return Ok(());
    };
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(HandlerError::caller_input(format!(
                "unknown params key '{key}' — this algorithm takes {}",
                allowed.join(", ")
            )));
        }
    }
    Ok(())
}

/// The effective (bounds-checked, defaulted) cost knobs of one
/// `password_hash` call.
enum PasswordParams {
    Argon2id {
        memory_kib: u32,
        iterations: u32,
        parallelism: u32,
    },
    Bcrypt {
        cost: u32,
    },
}

/// Check `params` for `algorithm` — key allowlist, defaults, bounds — and
/// return the effective values. One function feeding both the execution path
/// (which uses the values) and authoring-time validation (which discards
/// them), so the two surfaces cannot drift. `algorithm` must already have
/// passed the capability table.
fn password_params(
    algorithm: &str,
    params: Option<&Value>,
) -> Result<PasswordParams, HandlerError> {
    match algorithm {
        "argon2id" => {
            check_param_keys(params, &["memory_kib", "iterations", "parallelism"])?;
            Ok(PasswordParams::Argon2id {
                memory_kib: cost_param(
                    params,
                    "memory_kib",
                    ARGON2_DEFAULT_MEMORY_KIB,
                    &ARGON2_MEMORY_KIB_RANGE,
                )?,
                iterations: cost_param(
                    params,
                    "iterations",
                    ARGON2_DEFAULT_ITERATIONS,
                    &ARGON2_ITERATIONS_RANGE,
                )?,
                parallelism: cost_param(
                    params,
                    "parallelism",
                    ARGON2_DEFAULT_PARALLELISM,
                    &ARGON2_PARALLELISM_RANGE,
                )?,
            })
        }
        "bcrypt" => {
            check_param_keys(params, &["cost"])?;
            Ok(PasswordParams::Bcrypt {
                cost: cost_param(params, "cost", BCRYPT_DEFAULT_COST, &BCRYPT_COST_RANGE)?,
            })
        }
        other => unreachable!("password algorithm '{other}' passed validation"),
    }
}

/// `password` hashed by the effective `params`, on the blocking pool.
async fn hash_password(params: PasswordParams, password: String) -> Result<Value, HandlerError> {
    match params {
        PasswordParams::Argon2id {
            memory_kib,
            iterations,
            parallelism,
        } => {
            // Memory-hard by design, so off the async worker (W: ~20 MiB and
            // tens of milliseconds per call at the defaults) and gated by
            // KdfAdmission (`TD-04`).
            spawn_hashing(memory_kib, move || {
                use argon2::password_hash::PasswordHasher;
                use argon2::{Algorithm, Argon2, Params, Version};
                let params = Params::new(memory_kib, iterations, parallelism, None)
                    .map_err(|e| format!("argon2 parameters were rejected: {e}"))?;
                // `hash_password` draws its own salt from the OS RNG
                // (password-hash 0.6's `getrandom` feature, on by default).
                // Generating one by hand is the same bytes from the same
                // source, so there is nothing to keep by spelling it out.
                Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
                    .hash_password(password.as_bytes())
                    .map(|h| Value::String(h.to_string()))
                    .map_err(|e| format!("argon2 hashing failed: {e}"))
            })
            .await
        }
        PasswordParams::Bcrypt { cost } => {
            spawn_hashing(BCRYPT_MEMORY_KIB, move || {
                bcrypt::hash(&password, cost)
                    .map(Value::String)
                    .map_err(|e| format!("bcrypt hashing failed: {e}"))
            })
            .await
        }
    }
}

/// Validate the scheme and encoded cost parameters of a stored password hash
/// *before* acquiring KDF admission or spawning onto the blocking pool (`TD-04`).
/// Returns the memory footprint in KiB to charge against [`KdfAdmission`].
fn preflight_verify_hash(hash: &str) -> Result<u32, HandlerError> {
    if hash.starts_with("$argon2") {
        use argon2::Params;
        use argon2::password_hash::phc::PasswordHash;
        let parsed = PasswordHash::new(hash).map_err(|e| {
            HandlerError::caller_input(format!("stored hash is not a valid PHC string: {e}"))
        })?;
        let params = Params::try_from(&parsed).map_err(|e| {
            HandlerError::caller_input(format!("stored argon2 hash has invalid parameters: {e}"))
        })?;
        let m_cost = u64::from(params.m_cost());
        if !ARGON2_MEMORY_KIB_RANGE.contains(&m_cost) {
            return Err(HandlerError::caller_input(format!(
                "stored argon2 hash memory_kib ({m_cost}) is outside allowed range {}..={}",
                ARGON2_MEMORY_KIB_RANGE.start(),
                ARGON2_MEMORY_KIB_RANGE.end()
            )));
        }
        let t_cost = u64::from(params.t_cost());
        if !ARGON2_ITERATIONS_RANGE.contains(&t_cost) {
            return Err(HandlerError::caller_input(format!(
                "stored argon2 hash iterations ({t_cost}) is outside allowed range {}..={}",
                ARGON2_ITERATIONS_RANGE.start(),
                ARGON2_ITERATIONS_RANGE.end()
            )));
        }
        let p_cost = u64::from(params.p_cost());
        if !ARGON2_PARALLELISM_RANGE.contains(&p_cost) {
            return Err(HandlerError::caller_input(format!(
                "stored argon2 hash parallelism ({p_cost}) is outside allowed range {}..={}",
                ARGON2_PARALLELISM_RANGE.start(),
                ARGON2_PARALLELISM_RANGE.end()
            )));
        }
        Ok(params.m_cost())
    } else if hash.starts_with("$2") {
        // Standard modular crypt format: `$2a$<cost>$<22-char salt><31-char hash>`
        let parts: Vec<&str> = hash.split('$').collect();
        if parts.len() != 4 || !matches!(parts[1], "2a" | "2b" | "2x" | "2y") {
            return Err(HandlerError::caller_input(
                "stored bcrypt hash is malformed: invalid version or segment count",
            ));
        }
        let cost: u64 = parts[2].parse().map_err(|_| {
            HandlerError::caller_input("stored bcrypt hash is malformed: invalid cost parameter")
        })?;
        if !BCRYPT_COST_RANGE.contains(&cost) {
            return Err(HandlerError::caller_input(format!(
                "stored bcrypt hash cost ({cost}) is outside allowed range {}..={}",
                BCRYPT_COST_RANGE.start(),
                BCRYPT_COST_RANGE.end()
            )));
        }
        Ok(BCRYPT_MEMORY_KIB)
    } else {
        Err(HandlerError::caller_input(
            "unrecognized password hash scheme — expected an $argon2*$ PHC string or a \
             $2*$ bcrypt hash",
        ))
    }
}

/// Whether `password` matches the stored `hash`, on the blocking pool.
async fn verify_password(password: String, hash: String) -> Result<Value, HandlerError> {
    // Scheme auto-detected from the stored hash, which is what turns a
    // bcrypt → argon2id migration into a verify-then-rehash pair inside one
    // login workflow. Wrong password → false; a *malformed* stored hash or an
    // encoded cost outside policy bounds is a task error — validated before
    // spawning onto the blocking pool (`TD-04`).
    let memory_kib = preflight_verify_hash(&hash)?;
    spawn_hashing(memory_kib, move || {
        if hash.starts_with("$argon2") {
            use argon2::password_hash::{Error, PasswordVerifier, phc::PasswordHash};
            let parsed = PasswordHash::new(&hash)
                .map_err(|e| format!("stored hash is not a valid PHC string: {e}"))?;
            match argon2::Argon2::default().verify_password(password.as_bytes(), &parsed) {
                Ok(()) => Ok(Value::Bool(true)),
                Err(Error::PasswordInvalid) => Ok(Value::Bool(false)),
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

/// Run a CPU-bound hashing closure on the blocking pool under [`KDF_ADMISSION`].
/// Password KDFs are deliberately slow; running them inline would stall an
/// async worker for tens of milliseconds per login, and running them without
/// admission would let concurrent logins exhaust the blocking pool or memory.
async fn spawn_hashing<F>(memory_kib: u32, f: F) -> Result<Value, HandlerError>
where
    F: FnOnce() -> Result<Value, String> + Send + 'static,
{
    spawn_hashing_with_admission(&KDF_ADMISSION, memory_kib, f).await
}

async fn spawn_hashing_with_admission<F>(
    admission: &KdfAdmission,
    memory_kib: u32,
    f: F,
) -> Result<Value, HandlerError>
where
    F: FnOnce() -> Result<Value, String> + Send + 'static,
{
    let permit = admission.acquire(memory_kib).await?;
    tokio::task::spawn_blocking(move || {
        // Hold the KDF permit inside the blocking closure so an abandoned
        // caller cannot release concurrency/memory permits while the CPU/memory
        // work is still executing on the blocking thread (`TD-04`).
        let _permit = permit;
        f()
    })
    .await
    .map_err(|e| HandlerError::new(ErrorClass::Backend, format!("hashing task failed: {e}")))?
    .map_err(|e| HandlerError::caller_input(&e))
}

// -- dataflow-rs 4 --

/// I185's Defect for a literal root: no op's result is an object (a digest,
/// a MAC or a password hash is a string; a verification is a boolean), so
/// a context root, which takes only an object (break 21), refuses it on
/// every call.
const ROOT_OUTPUT: &str = "`crypto` writes a string or a boolean, and a context root takes \
    only an object (break 21): set `output` to a path below it, such as `data.digest`";

/// `crypto`'s input on dataflow-rs 4, in its field table's order (I179),
/// each field in the position 1.12 executed it in (I183). The op, the
/// algorithm, the three encodings and `params` are literals, read as
/// written, as 1.12 read them; `data`, `signature`, `password` and `hash`
/// are expressions, read by `kit::read`'s twin of `resolve_required_str`
/// where 1.12 read them with it; `key` is key material. A computed `output`
/// is evaluated before the op, where 1.12 never evaluated it and wrote at
/// `data` (a change O11b's CHANGELOG names). A task missing `op` is refused
/// at load (I183), and its `check` is 1.12's authoring-time rules.
#[derive(Debug, FunctionConfig)]
#[config(check = check)]
pub struct CryptoConfig {
    /// Operation: hash, hmac, hmac_verify, password_hash, or password_verify.
    op: String,
    /// Algorithm within the op's capability table (e.g. sha256, sha512, argon2id). Per-op
    /// default; password_verify auto-detects from the stored hash.
    algorithm: Option<String>,
    /// Bytes to digest (hash/hmac/hmac_verify). A string is used as UTF-8 (see input_encoding);
    /// any other JSON value is hashed as its compact serialization.
    data: Option<Expr<OwnedDataValue>>,
    /// How a string 'data' becomes bytes: utf8 (default), hex, or base64.
    input_encoding: Option<String>,
    /// HMAC key (hmac/hmac_verify): {"secret": "name"} reads the engine's [secrets] store; a
    /// string is a literal or a reference like env://NAME. Never appears in traces or errors.
    key: Option<KeyMaterial>,
    /// How the resolved key becomes bytes: utf8 (default), hex, or base64 — for APIs that issue binary signing keys.
    key_encoding: Option<String>,
    /// The presented MAC to check (hmac_verify); hex, base64, or base64url, auto-detected.
    /// Compared in constant time.
    signature: Option<Expr<OwnedDataValue>>,
    /// The submitted password (password_hash/password_verify).
    password: Option<Expr<OwnedDataValue>>,
    /// The stored password hash to verify against (password_verify); scheme auto-detected from
    /// its $argon2*$/$2*$ prefix.
    hash: Option<Expr<OwnedDataValue>>,
    /// Output encoding for hash/hmac results: hex (default), base64, or base64url (unpadded).
    encoding: Option<String>,
    /// password_hash cost tuning: argon2id takes memory_kib/iterations/parallelism, bcrypt
    /// takes cost. Safe defaults; bounded ranges.
    params: Option<Value>,
    /// Dotted path where the result is stored. Defaults to "data". String for
    /// hash/hmac/password_hash; boolean for hmac_verify/password_verify.
    #[output(default = "data")]
    output: Output,
}

impl CryptoConfig {
    /// The task's input as 1.12's authoring check judged it: every field
    /// the task sets, as written, in the field table's order. A literal
    /// `null` in a string field, 1.12's `TYPE_MISMATCH` at authoring, is
    /// unset here, so it is not judged again.
    fn authored(&self) -> Map<String, Value> {
        let mut input = Map::new();
        input.insert("op".to_string(), Value::String(self.op.clone()));
        let mut text = |field: &str, value: &Option<String>| {
            if let Some(value) = value {
                input.insert(field.to_string(), Value::String(value.clone()));
            }
        };
        text("algorithm", &self.algorithm);
        text("input_encoding", &self.input_encoding);
        text("key_encoding", &self.key_encoding);
        text("encoding", &self.encoding);
        for (field, expr) in [
            ("data", &self.data),
            ("signature", &self.signature),
            ("password", &self.password),
            ("hash", &self.hash),
        ] {
            if let Some(expr) = expr {
                input.insert(field.to_string(), expr.raw().clone());
            }
        }
        match &self.key {
            Some(KeyMaterial::Text(text)) => {
                input.insert("key".to_string(), Value::String(text.clone()));
            }
            Some(KeyMaterial::Expr(expr)) => {
                input.insert("key".to_string(), expr.raw().clone());
            }
            None => {}
        }
        if let Some(params) = &self.params {
            input.insert("params".to_string(), params.clone());
        }
        input
    }
}

/// 1.12's authoring-time rules for `crypto` ([`validate_static_input`], by
/// which create, update, import, validate and `lint` refused a task), on
/// the config at load: the op's membership, its algorithm table, its
/// required fields and the fields it does not take, the encodings and the
/// `params` bounds, each refused at the field 1.12 named, in its words.
fn check(cfg: &CryptoConfig, cx: &mut CompileCx<'_>) {
    super::report_static(validate_static_input(&cfg.authored()), cx);
}

/// `crypto` as a function: the op, each field read in 1.12's order, the
/// hashing on the blocking pool.
pub struct CryptoFunction;

impl OrionFunction for CryptoFunction {
    type Connector = NoConnector;

    const DEFINITION: Definition = Definition {
        name: NAME,
        description: "Digests, HMAC compute/verify, and password hashing — a self-contained operation envelope.",
        category: "utility",
        retry_safety: RetrySafety::Pure,
        fields: &[
            FieldOverlay {
                name: "key",
                secret_at: &[""],
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "signature",
                kind: Some(FieldKind::String),
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "password",
                kind: Some(FieldKind::String),
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "hash",
                kind: Some(FieldKind::String),
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "params",
                kind: Some(FieldKind::Object),
                ..FieldOverlay::DEFAULT
            },
        ],
        validate_static: Some(validate_static_input),
        ..Definition::DEFAULT
    };

    fn from_deps(_deps: &crate::HandlerDeps<'_>) -> Self {
        Self
    }
}

impl Function for CryptoFunction {
    type Config = CryptoConfig;

    fn spec(&self) -> FunctionSpec {
        super::definition::spec::<Self>()
    }

    /// I185: no op's result is an object, so an `output` that is a literal
    /// context root, the default `data` included, is a Defect at `output`.
    /// A computed one is judged per call, when its path is known.
    fn prepare(&self, cfg: &mut CryptoConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        kit::root_output_defect(&cfg.output, ROOT_OUTPUT)
    }

    /// The output, where 1.12 read it, before the op; the op, its refusals
    /// named once with the handler's name, as 1.12 named them; then the one
    /// write.
    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &CryptoConfig) -> dataflow_rs::Result<Outcome> {
        let output = Self::DEFINITION.output_path(ctx, &cfg.output)?;
        let result = compute(ctx, cfg).await.map_err(|e| e.prefixed(NAME))?;
        ctx.set(&output, OwnedDataValue::from(&result))?;
        Ok(Outcome::Ok)
    }
}

/// The op's result.
async fn compute(ctx: &Ctx<'_>, cfg: &CryptoConfig) -> Result<Value, HandlerError> {
    match cfg.op.as_str() {
        "hash" => hash_v4(ctx, cfg),
        "hmac" => hmac_v4(ctx, cfg, HmacMode::Compute).await,
        "hmac_verify" => hmac_v4(ctx, cfg, HmacMode::Verify).await,
        "password_hash" => password_hash_v4(ctx, cfg).await,
        "password_verify" => password_verify_v4(ctx, cfg).await,
        // Refused at load by the check; kept for the call's own guard.
        other => Err(HandlerError::caller_input(format!(
            "unknown op '{other}' — expected one of {}",
            OPS.join(", ")
        ))),
    }
}

/// A required string field for this call, by `resolve_required_str`'s
/// reading.
fn required_v4(
    ctx: &Ctx<'_>,
    field: &Option<Expr<OwnedDataValue>>,
    name: &str,
) -> Result<String, HandlerError> {
    let value = field.as_ref().map(|expr| ctx.eval(expr)).transpose()?;
    Ok(read::required_str(value.as_deref(), name, NAME)?.into_owned())
}

/// `data`'s bytes for this call: `data` evaluated, then the codec judged,
/// then the evaluation's error, in that order, as 1.12 judged them.
fn data_bytes_v4(ctx: &Ctx<'_>, cfg: &CryptoConfig) -> Result<Vec<u8>, HandlerError> {
    let Some(expr) = &cfg.data else {
        return Err(HandlerError::caller_input("this op requires 'data'"));
    };
    let data = ctx.eval(expr).map(|value| Value::from(&*value));
    let codec = byte_codec_named(cfg.input_encoding.as_deref(), "input_encoding")?;
    bytes_of(data?, codec)
}

/// [`hash_op`] for this call.
fn hash_v4(ctx: &Ctx<'_>, cfg: &CryptoConfig) -> Result<Value, HandlerError> {
    let algorithm = algorithm_named(cfg.algorithm.as_deref(), HASH_ALGORITHMS, "sha256")?;
    let codec = codec_named(cfg.encoding.as_deref())?;
    let data = data_bytes_v4(ctx, cfg)?;
    Ok(Value::String(encode_bytes(
        codec,
        &digest_bytes(algorithm, &data),
    )))
}

/// [`hmac_op`] for this call, in its order: the algorithm, the key, the
/// data, then the encoding or the signature.
async fn hmac_v4(ctx: &Ctx<'_>, cfg: &CryptoConfig, mode: HmacMode) -> Result<Value, HandlerError> {
    let algorithm = algorithm_named(cfg.algorithm.as_deref(), HMAC_ALGORITHMS, "sha256")?;
    let resolved = kit::key_material(cfg.key.as_ref(), ctx, "key", NAME).await?;
    let key = key_bytes(
        resolved,
        byte_codec_named(cfg.key_encoding.as_deref(), "key_encoding"),
    )?;
    let data = data_bytes_v4(ctx, cfg)?;
    match mode {
        HmacMode::Compute => {
            let codec = codec_named(cfg.encoding.as_deref())?;
            Ok(Value::String(encode_bytes(
                codec,
                &mac(algorithm, &key, &data),
            )))
        }
        HmacMode::Verify => {
            let signature = required_v4(ctx, &cfg.signature, "signature")?;
            let signature = decode_presented_signature(&signature)?;
            Ok(Value::Bool(mac_matches(algorithm, &key, &data, &signature)))
        }
    }
}

/// [`password_hash_op`] for this call.
async fn password_hash_v4(ctx: &Ctx<'_>, cfg: &CryptoConfig) -> Result<Value, HandlerError> {
    let algorithm = algorithm_named(cfg.algorithm.as_deref(), PASSWORD_ALGORITHMS, "argon2id")?;
    let password = required_v4(ctx, &cfg.password, "password")?;
    hash_password(password_params(algorithm, cfg.params.as_ref())?, password).await
}

/// [`password_verify_op`] for this call.
async fn password_verify_v4(ctx: &Ctx<'_>, cfg: &CryptoConfig) -> Result<Value, HandlerError> {
    let password = required_v4(ctx, &cfg.password, "password")?;
    let hash = required_v4(ctx, &cfg.hash, "hash")?;
    verify_password(password, hash).await
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
        errors.push(("algorithm", "INVALID", e.msg));
    }
    if optional.contains(&"encoding")
        && let Err(e) = output_codec(&input)
    {
        errors.push(("encoding", "INVALID", e.msg));
    }
    for enc_field in ["input_encoding", "key_encoding"] {
        if optional.contains(&enc_field)
            && let Err(e) = byte_codec(&input, enc_field)
        {
            errors.push((field_name(enc_field), "INVALID", e.msg));
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

    // `params` keys and bounds, per the (defaulted) algorithm — the same
    // `password_params` the execution path runs. An unknown algorithm is
    // already reported above and has no params table to check.
    if op == "password_hash" {
        let algorithm = obj
            .get("algorithm")
            .and_then(Value::as_str)
            .unwrap_or("argon2id");
        if PASSWORD_ALGORITHMS.contains(&algorithm)
            && let Err(e) = password_params(algorithm, obj.get("params"))
        {
            errors.push(("params", "INVALID", e.msg));
        }
    }

    errors
}

/// Whether an authored value is a `{"var": ..}` node the handler will resolve
/// per message (so its static shape cannot be judged here).
fn is_resolvable(v: &Value) -> bool {
    v.as_object().is_some_and(|o| o.contains_key("var"))
}

fn field_name(key: &str) -> &'static str {
    super::schema::static_field_name(CryptoConfig::fields(), key, "op")
}

fn field_exists(key: &str) -> bool {
    CryptoConfig::fields().iter().any(|f| f.name == key)
}

#[cfg(test)]
#[path = "crypto_tests.rs"]
mod tests;

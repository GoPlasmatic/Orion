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

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::{Ctx, Expr, Function, Issue, Outcome};
use hmac::Hmac;
use md5::Md5;
use serde_json::{Map, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use super::schema::{FieldKind, FieldSchema};
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

/// A caller-fixable problem with the task's input, message bare. See the shell
/// on `execute` for where the handler's name is added.
fn validation(msg: &str) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, msg)
}

/// An optional literal string field; a present non-string is an error.
fn literal_str<'i>(input: &'i Value, field: &str) -> Result<Option<&'i str>, HandlerError> {
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
        Some(other) => Err(validation(&format!(
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
            validation(&format!(
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
        Some(other) => Err(validation(&format!(
            "unknown {field} '{other}' — expected one of utf8, hex, base64"
        ))),
    }
}

/// The byte model over the value `data` resolved to.
fn bytes_of(data: Value, codec: Option<Codec>) -> Result<Vec<u8>, HandlerError> {
    match data {
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
        return Err(validation("'key' resolved to an empty value"));
    }
    match codec? {
        None => Ok(resolved.into_bytes()),
        Some(c) => decode_bytes(c, &resolved)
            .map_err(|e| validation(&format!("'key' does not decode per key_encoding: {e}"))),
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
        .map_err(|_| validation("'signature' is not hex, base64, or base64url"))
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
        _ => Err(validation(&format!(
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
        let parsed = PasswordHash::new(hash)
            .map_err(|e| validation(&format!("stored hash is not a valid PHC string: {e}")))?;
        let params = Params::try_from(&parsed)
            .map_err(|e| validation(&format!("stored argon2 hash has invalid parameters: {e}")))?;
        let m_cost = u64::from(params.m_cost());
        if !ARGON2_MEMORY_KIB_RANGE.contains(&m_cost) {
            return Err(validation(&format!(
                "stored argon2 hash memory_kib ({m_cost}) is outside allowed range {}..={}",
                ARGON2_MEMORY_KIB_RANGE.start(),
                ARGON2_MEMORY_KIB_RANGE.end()
            )));
        }
        let t_cost = u64::from(params.t_cost());
        if !ARGON2_ITERATIONS_RANGE.contains(&t_cost) {
            return Err(validation(&format!(
                "stored argon2 hash iterations ({t_cost}) is outside allowed range {}..={}",
                ARGON2_ITERATIONS_RANGE.start(),
                ARGON2_ITERATIONS_RANGE.end()
            )));
        }
        let p_cost = u64::from(params.p_cost());
        if !ARGON2_PARALLELISM_RANGE.contains(&p_cost) {
            return Err(validation(&format!(
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
            return Err(validation(
                "stored bcrypt hash is malformed: invalid version or segment count",
            ));
        }
        let cost: u64 = parts[2]
            .parse()
            .map_err(|_| validation("stored bcrypt hash is malformed: invalid cost parameter"))?;
        if !BCRYPT_COST_RANGE.contains(&cost) {
            return Err(validation(&format!(
                "stored bcrypt hash cost ({cost}) is outside allowed range {}..={}",
                BCRYPT_COST_RANGE.start(),
                BCRYPT_COST_RANGE.end()
            )));
        }
        Ok(BCRYPT_MEMORY_KIB)
    } else {
        Err(validation(
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
    .map_err(|e| validation(&e))
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
    /// The operation: hash, hmac, hmac_verify, password_hash or
    /// password_verify.
    op: String,
    /// The algorithm within the op's capability table; per-op default.
    algorithm: Option<String>,
    /// The bytes digested: a string per `input_encoding`, anything else its
    /// compact JSON.
    data: Option<Expr<OwnedDataValue>>,
    /// How a string `data` becomes bytes: utf8, hex or base64.
    input_encoding: Option<String>,
    /// The HMAC key.
    key: Option<KeyMaterial>,
    /// How the resolved key becomes bytes: utf8, hex or base64.
    key_encoding: Option<String>,
    /// The presented MAC an `hmac_verify` checks.
    signature: Option<Expr<OwnedDataValue>>,
    /// The submitted password.
    password: Option<Expr<OwnedDataValue>>,
    /// The stored password hash a `password_verify` checks against.
    hash: Option<Expr<OwnedDataValue>>,
    /// The encoding of a hash or MAC result: hex, base64 or base64url.
    encoding: Option<String>,
    /// A `password_hash`'s cost knobs.
    params: Option<Value>,
    /// Where the result is written.
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

impl Function for CryptoFunction {
    type Config = CryptoConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
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
        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
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
        other => Err(validation(&format!(
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
        return Err(validation("this op requires 'data'"));
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
    super::schema::static_field_name(CRYPTO_FIELDS, key, "op")
}

fn field_exists(key: &str) -> bool {
    CRYPTO_FIELDS.iter().any(|f| f.name == key)
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
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "algorithm",
        description: "Algorithm within the op's capability table (e.g. sha256, sha512, \
                      argon2id). Per-op default; password_verify auto-detects from the \
                      stored hash.",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "data",
        description: "Bytes to digest (hash/hmac/hmac_verify). A string is used as UTF-8 \
                      (see input_encoding); any other JSON value is hashed as its compact \
                      serialization.",
        kind: FieldKind::Any,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "input_encoding",
        description: "How a string 'data' becomes bytes: utf8 (default), hex, or base64.",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key",
        description: "HMAC key (hmac/hmac_verify): {\"secret\": \"name\"} reads the \
                      engine's [secrets] store; a string is a literal or a reference \
                      like env://NAME. Never appears in traces or errors.",
        kind: FieldKind::String,
        secret_at: &[""],
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key_encoding",
        description: "How the resolved key becomes bytes: utf8 (default), hex, or base64 \
                      — for APIs that issue binary signing keys.",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "signature",
        description: "The presented MAC to check (hmac_verify); hex, base64, or \
                      base64url, auto-detected. Compared in constant time.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "password",
        description: "The submitted password (password_hash/password_verify).",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "hash",
        description: "The stored password hash to verify against (password_verify); \
                      scheme auto-detected from its $argon2*$/$2*$ prefix.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "encoding",
        description: "Output encoding for hash/hmac results: hex (default), base64, or \
                      base64url (unpadded).",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "params",
        description: "password_hash cost tuning: argon2id takes memory_kib/iterations/\
                      parallelism, bcrypt takes cost. Safe defaults; bounded ranges.",
        kind: FieldKind::Object,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the result is stored. Defaults to \"data\". \
                      String for hash/hmac/password_hash; boolean for \
                      hmac_verify/password_verify.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::json;

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

    // -- the config, then calls against 1.12's records --

    fn config_v4(input: Value) -> Result<CryptoConfig, Vec<dataflow_rs::Issue>> {
        testing::config_with(&kit::test_exprs(), input)
    }

    /// The Rejected issues of a config that does not load, as
    /// `(path, message)`.
    fn refusals(input: Value) -> Vec<(String, String)> {
        config_v4(input.clone())
            .map(|_| ())
            .expect_err(&format!("{input} is refused at load"))
            .into_iter()
            .filter(|i| i.severity == Severity::Rejected)
            .map(|i| (i.path.to_string(), i.message))
            .collect()
    }

    /// What 1.12's authoring check refused in `input`, as `(path, message)`.
    fn authoring(input: &Value) -> Vec<(String, String)> {
        validate_static_input(input.as_object().expect("an object"))
            .into_iter()
            .map(|(field, _code, message)| (field.to_string(), message))
            .collect()
    }

    /// One task over `input` and `data`, which must be what 1.12 answered
    /// for them; the answer is returned. Only for deterministic answers: a
    /// salted hash is checked by verifying it instead.
    async fn agreed(input: Value, data: Value) -> Seen {
        let v4 = twin::v4(NAME, CryptoFunction, input.clone(), data.clone()).await;
        twin::pinned(&v4, format!("{input} {data}"));
        v4
    }

    /// Declaration order is the field table's (I179).
    #[test]
    fn the_config_declares_the_field_table_in_its_order() {
        let fields: Vec<&str> = CryptoConfig::fields().iter().map(|f| &*f.name).collect();
        let table: Vec<&str> = CRYPTO_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);
    }

    /// Each field compiles in the position 1.12 executed it in (I183): the
    /// op required, the literals typed, key material as text or an
    /// expression, a `null` expression a value, as 1.12 read it.
    #[test]
    fn the_fields_compile_in_1_12s_positions() {
        let cfg = config_v4(json!({
            "op": "hmac", "algorithm": "sha512", "data": {"var": "data.body"},
            "key": "env://K", "key_encoding": "hex", "encoding": "base64",
            "output": "data.mac"
        }))
        .expect("compiles");
        assert_eq!(cfg.op, "hmac");
        assert_eq!(cfg.algorithm.as_deref(), Some("sha512"));
        assert!(matches!(&cfg.key, Some(KeyMaterial::Text(t)) if t == "env://K"));
        assert!(cfg.data.as_ref().is_some_and(|d| d.constant().is_none()));
        let literal = |cfg: &CryptoConfig| cfg.output.constant().map(ToString::to_string);
        assert_eq!(literal(&cfg).as_deref(), Some("data.mac"));

        let cfg = config_v4(json!({"op": "hmac", "data": "x", "key": {"secret": "k"},
                                   "output": {"var": "data.where"}}))
        .expect("compiles");
        assert!(matches!(&cfg.key, Some(KeyMaterial::Expr(_))));
        // A computed output, evaluated per call.
        assert_eq!(literal(&cfg), None);
        let cfg = config_v4(json!({"op": "password_hash", "password": "x", "params": null}))
            .expect("compiles");
        assert_eq!(cfg.params, Some(Value::Null));
        assert_eq!(literal(&cfg).as_deref(), Some("data"));
        // A `null` expression is a value, which the check refuses where 1.12
        // refused it (`data` required): see `the_check_is_validate_static_input`.

        // A missing op, and a non-string literal, which 1.12's field loop
        // refused at authoring (TYPE_MISMATCH), are refused at load.
        assert_eq!(refusals(json!({"data": "x"}))[0].0, "op");
        for field in ["algorithm", "input_encoding", "key_encoding", "encoding"] {
            let mut input = json!({"op": "hash", "data": "x"});
            input[field] = json!(7);
            assert_eq!(refusals(input)[0].0, field);
        }
        // `crypto` was `deny_unknown` in 1.12, so an unknown key stays
        // refused.
        let issues = config_v4(json!({"op": "hash", "data": "x", "outputs": "data.d"}))
            .map(|_| ())
            .expect_err("an unknown key");
        assert_eq!(issues[0].path.to_string(), "outputs");
        assert_eq!(issues[0].code, IssueCode::LikelyTypo);
    }

    /// The check is 1.12's authoring-time rules: each refusal at the field
    /// and in the words `validate_static_input` gives, computed from it; an
    /// input it accepts loads.
    #[test]
    fn the_check_is_validate_static_input() {
        for input in [
            json!({"op": "melt", "data": "x"}),
            json!({"op": "hmac", "data": "x"}),
            json!({"op": "hmac_verify", "data": "x", "key": "k"}),
            json!({"op": "password_hash", "password": "x", "key": "why"}),
            json!({"op": "hash", "data": "x", "encoding": "base32"}),
            json!({"op": "hash", "data": "x", "algorithm": "argon2id"}),
            json!({"op": "password_hash", "algorithm": "sha256", "password": "x"}),
            json!({"op": "hash", "data": {"a": 1}, "input_encoding": "hex"}),
            json!({"op": "hash", "data": "x", "input_encoding": "rot13"}),
            json!({"op": "hmac", "data": "x", "key": "k", "key_encoding": "base32"}),
            json!({"op": "hmac_verify", "data": "x", "key": "k", "signature": "s",
                   "encoding": "hex"}),
            json!({"op": "password_hash", "password": "x", "params": {"cost": 4}}),
            json!({"op": "password_hash", "password": "x", "params": {"memory_mib": 64}}),
            json!({"op": "password_hash", "algorithm": "bcrypt", "password": "x",
                   "params": {"cost": 4}}),
            json!({"op": "password_hash", "password": "x", "params": [1]}),
            json!({"op": "password_verify", "password": "x", "hash": "h",
                   "algorithm": "bcrypt"}),
            json!({"op": "password_verify", "password": "x"}),
            json!({"op": "hash", "data": null}),
            json!({"op": "hmac", "data": "x", "key": null}),
        ] {
            let expected = authoring(&input);
            assert!(!expected.is_empty(), "{input} is a refusal");
            assert_eq!(refusals(input.clone()), expected, "{input}");
        }
        for input in [
            json!({"op": "hash", "data": "x"}),
            json!({"op": "hash", "data": {"var": "data.obj"}, "input_encoding": "hex"}),
            json!({"op": "hmac_verify", "data": "x", "key": {"secret": "k"},
                   "signature": {"var": "data.sig"}}),
            json!({"op": "password_hash", "algorithm": "bcrypt", "password": "x",
                   "params": {"cost": 10}}),
            json!({"op": "password_verify", "password": "x", "hash": "h"}),
        ] {
            assert!(authoring(&input).is_empty(), "{input}");
            assert!(config_v4(input.clone()).is_ok(), "{input}");
        }
    }

    /// I185: no op's result is an object, so a literal root `output` (the
    /// default `data` included, and an expression that folds to one) is a
    /// Defect at `output`, reported by `prepare`, which only a compile
    /// through the registry runs. A path below a root is not, folded or
    /// written. The call still runs, and a root's write is refused (break
    /// 21), where 1.12 replaced the root with the result.
    #[tokio::test]
    async fn a_literal_root_output_is_i185s_defect() {
        for op in [
            json!({"op": "hash", "data": "x"}),
            json!({"op": "hmac_verify", "data": "x", "key": "Jefe", "signature": "00"}),
        ] {
            for (output, defect) in [
                (None, Some(ROOT_OUTPUT)),
                (Some(json!("data")), Some(ROOT_OUTPUT)),
                (Some(json!("temp_data")), Some(ROOT_OUTPUT)),
                (Some(Value::Null), Some(ROOT_OUTPUT)),
                (Some(json!({"cat": ["data"]})), Some(ROOT_OUTPUT)),
                (Some(json!("data.digest")), None),
                (Some(json!("temp_data.d")), None),
                (Some(json!({"cat": ["data", ".x"]})), None),
            ] {
                let mut input = op.clone();
                if let Some(output) = &output {
                    input["output"] = output.clone();
                }
                let called = testing::call(CryptoFunction, input)
                    .exprs(kit::test_exprs())
                    .name(NAME)
                    .await;
                let defects: Vec<_> = called
                    .issues
                    .iter()
                    .filter(|i| i.severity == Severity::Defect)
                    .map(|i| (i.path.to_string(), i.code, i.message.as_str()))
                    .collect();
                match defect {
                    Some(message) => {
                        assert_eq!(
                            defects,
                            [("output".to_string(), IssueCode::InputParse, message)],
                            "{op} at {output:?}"
                        );
                        let error = called.outcome.expect_err("no root takes the result");
                        assert_eq!(error.code(), "CONFIG_ERROR", "{op} at {output:?}");
                    }
                    None => {
                        assert!(defects.is_empty(), "{op} at {output:?}: {defects:?}");
                        assert_eq!(called.outcome.expect("computed"), Outcome::Ok);
                    }
                }
            }
        }
    }

    /// A computed `output` is evaluated before the op, by 1.12's
    /// `resolve_output_path` rule, and judged per call: a path below a root
    /// is written, a root or a `null` result (which is `data`) is refused
    /// at the write (break 21, `CONFIG_ERROR`), and a result that is no path
    /// is refused before the op runs, in 1.12's words. 1.12 never evaluated
    /// `crypto`'s output: it wrote the result at `data` whatever the
    /// expression said (a change O11b's CHANGELOG names).
    #[tokio::test]
    async fn a_computed_output_is_evaluated_before_the_op_and_judged_per_call() {
        let input = json!({"op": "hash", "data": {"var": "data.payload"},
                           "output": {"var": "data.where"}});
        let expected = hex::encode(Sha256::digest(b"abc"));
        let out = twin::v4(
            NAME,
            CryptoFunction,
            input.clone(),
            json!({"payload": "abc", "where": "data.digest"}),
        )
        .await
        .expect("written where it evaluates");
        assert_eq!(out["digest"], json!(expected));
        for at_a_root in [
            json!({"where": "data"}),
            json!({"where": "temp_data"}),
            json!({}),
        ] {
            let mut data = at_a_root.clone();
            data["payload"] = json!("abc");
            let called = testing::call(CryptoFunction, input.clone())
                .exprs(kit::test_exprs())
                .name(NAME)
                .message(dataflow_rs::Message::from_parts(
                    OwnedDataValue::Null,
                    OwnedDataValue::from(&data),
                    OwnedDataValue::Null,
                ))
                .await;
            assert!(
                called.issues.iter().all(|i| i.severity != Severity::Defect),
                "{:?}",
                called.issues
            );
            let error = called.outcome.expect_err("a root takes no string");
            assert_eq!(error.code(), "CONFIG_ERROR", "{at_a_root}");
        }
        // No `payload`: the op would refuse its `null` data, and the
        // output's refusal comes first.
        for not_a_path in [json!(5), json!({"a": 1}), json!(["data.d"])] {
            let failure = twin::v4(
                NAME,
                CryptoFunction,
                input.clone(),
                json!({"where": not_a_path}),
            )
            .await
            .expect_err("no path");
            twin::pinned(&Err(failure), format!("output_refusal/{not_a_path}"));
        }
    }

    #[tokio::test]
    async fn hash_matches_the_nist_vectors() {
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
            let out = agreed(
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
            let out = agreed(
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
        let expected_hex = "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
        let out = agreed(
            json!({"op": "hmac", "key": "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b",
                   "key_encoding": "hex", "data": "Hi There",
                   "encoding": "base64", "output": "data.mac"}),
            json!({}),
        )
        .await
        .expect("test");
        let expected_b64 = crate::operators::encode_bytes(
            Codec::Base64,
            &hex::decode(expected_hex).expect("test"),
        );
        assert_eq!(out["mac"], json!(expected_b64));
    }

    #[tokio::test]
    async fn hmac_verify_is_a_boolean_with_auto_detected_encoding() {
        let mac_hex = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
        let mac_b64 =
            crate::operators::encode_bytes(Codec::Base64, &hex::decode(mac_hex).expect("test"));
        for presented in [mac_hex.to_string(), mac_b64] {
            let out = agreed(
                json!({"op": "hmac_verify", "key": "Jefe",
                       "data": "what do ya want for nothing?",
                       "signature": {"var": "data.sig"}, "output": "data.ok"}),
                json!({"sig": presented}),
            )
            .await
            .expect("test");
            assert_eq!(out["ok"], json!(true));
        }
        let out = agreed(
            json!({"op": "hmac_verify", "key": "Jefe",
                   "data": "what do ya want for nothing?",
                   "signature": "00".repeat(32), "output": "data.ok"}),
            json!({}),
        )
        .await
        .expect("test");
        assert_eq!(out["ok"], json!(false));
        let err = agreed(
            json!({"op": "hmac_verify", "key": "Jefe", "data": "x",
                   "signature": "!!not-an-encoding!!", "output": "data.ok"}),
            json!({}),
        )
        .await
        .expect_err("test");
        assert!(err.message.contains("signature"), "{err:?}");
    }

    #[tokio::test]
    async fn non_string_data_is_hashed_as_compact_json() {
        let out = agreed(
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
        let out = agreed(
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

    /// A salted hash differs per call, so it is checked by verifying it.
    #[tokio::test]
    async fn password_hash_and_verify_round_trip() {
        let hash = json!({"op": "password_hash",
                          "params": {"memory_kib": 8192, "iterations": 1},
                          "password": "correct horse", "output": "data.phc"});
        let out = twin::v4(NAME, CryptoFunction, hash, json!({}))
            .await
            .expect("hashed");
        let phc = out["phc"].as_str().expect("a PHC string");
        assert!(phc.starts_with("$argon2id$"), "{phc}");
        for (candidate, expected) in [("correct horse", true), ("battery staple", false)] {
            let out = twin::v4(
                NAME,
                CryptoFunction,
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
        let out = twin::v4(
            NAME,
            CryptoFunction,
            json!({"op": "password_hash", "algorithm": "bcrypt",
                   "params": {"cost": 10},
                   "password": "legacy pw", "output": "data.hash"}),
            json!({}),
        )
        .await
        .expect("test");
        let stored = out["hash"].as_str().expect("test").to_string();
        assert!(stored.starts_with("$2"), "{stored}");
        let out = twin::v4(
            NAME,
            CryptoFunction,
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
        let err = agreed(
            json!({"op": "password_verify", "password": "x",
                   "hash": "plainly-not-a-hash", "output": "data.ok"}),
            json!({}),
        )
        .await
        .expect_err("test");
        assert!(err.message.contains("hash scheme"), "{err:?}");
        assert_eq!(err.code, "VALIDATION_ERROR");
    }

    /// Inputs the field tables refuse: 1.12 refused each at authoring and
    /// at execution; each is now refused at load, in the authoring check's
    /// words, so none reaches a call.
    #[tokio::test]
    async fn table_violations_are_refused_at_load() {
        for input in [
            json!({"op": "password_hash", "algorithm": "sha256", "password": "x"}),
            json!({"op": "hash", "algorithm": "argon2id", "data": "x"}),
            json!({"op": "melt", "data": "x"}),
            json!({"op": "hmac", "data": "x"}),
            json!({"op": "password_hash", "password": "x", "params": {"cost": 4}}),
            json!({"op": "password_hash", "password": "x",
                   "params": {"memory_mib": 64}}),
        ] {
            assert_eq!(refusals(input.clone()), authoring(&input), "{input}");
            let called = testing::call(CryptoFunction, input.clone())
                .exprs(kit::test_exprs())
                .name(NAME)
                .await;
            let error = called.outcome.expect_err("refused before the call");
            assert_eq!(*error.kind(), dataflow_rs::ErrorKind::Compile, "{input}");
        }
    }

    /// What reaches a call and is refused there keeps 1.12's code, text and
    /// the handler's name, in 1.12's order of refusal.
    #[tokio::test]
    async fn call_time_refusals_keep_1_12s_text_and_name() {
        let data = json!({"obj": {"a": 1}, "empty": "", "n": 5, "s": "ok"});
        for input in [
            json!({"op": "hash", "data": {"var": "data.missing"}, "output": "data.d"}),
            json!({"op": "hash", "data": {"var": "data.obj"}, "input_encoding": "hex",
                   "output": "data.d"}),
            json!({"op": "hash", "data": "zz", "input_encoding": "hex", "output": "data.d"}),
            json!({"op": "hash", "data": {"throw": "boom"}, "output": "data.d"}),
            json!({"op": "hmac", "data": "x", "key": {"var": "data.empty"},
                   "output": "data.d"}),
            json!({"op": "hmac", "data": "x", "key": "zz", "key_encoding": "hex",
                   "output": "data.d"}),
            json!({"op": "hmac", "data": "x", "key": {"var": "data.n"}, "output": "data.d"}),
            json!({"op": "hmac", "data": "x", "key": "env://ORION_CRYPTO_SURELY_UNSET_4K",
                   "output": "data.d"}),
            // The key is judged before the data, as 1.12 judged it (F58).
            json!({"op": "hmac", "data": {"var": "data.missing"}, "key": {"var": "data.empty"},
                   "output": "data.d"}),
            json!({"op": "hmac_verify", "data": "x", "key": "Jefe",
                   "signature": {"var": "data.obj"}, "output": "data.d"}),
            json!({"op": "password_hash", "password": {"var": "data.obj"},
                   "output": "data.d"}),
            json!({"op": "password_verify", "password": {"var": "data.missing"},
                   "hash": "$2b$10$x", "output": "data.d"}),
            json!({"op": "password_verify", "password": "x", "hash": {"var": "data.n"},
                   "output": "data.d"}),
        ] {
            let failure = agreed(input.clone(), data.clone())
                .await
                .expect_err(&format!("{input} is refused"));
            assert!(failure.message.contains("crypto: "), "{input}: {failure:?}");
        }
    }

    /// A key read from the store reaches the MAC and nothing else: the
    /// input a layer reads writes it `null`, and the call's changes hold
    /// the MAC alone.
    #[tokio::test]
    async fn a_key_from_the_store_never_reaches_the_input_or_the_changes() {
        use dataflow_rs::function::{BoxFuture, CallLayer, Next};
        use std::sync::{Arc, Mutex};

        /// The input each call it wraps was given, as a layer reads it.
        struct Inputs(Arc<Mutex<Vec<Value>>>);
        impl CallLayer for Inputs {
            fn call<'a>(
                &'a self,
                ctx: &'a mut Ctx<'_>,
                next: Next<'a>,
            ) -> BoxFuture<'a, dataflow_rs::Result<Outcome>> {
                let input = ctx.input().map(|v| Value::from(&v)).expect("an input");
                self.0.lock().expect("unpoisoned").push(input);
                next.run(ctx)
            }
        }

        const SIGNING: &str = "s3cr3t-hmac-key-from-the-store";
        let mut values = Map::new();
        values.insert("signing".into(), json!(SIGNING));
        let exprs = crate::operators::orion_exprs(&crate::ResolvedSecrets::from_values(values), 0)
            .expect("Orion's expressions build");
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let called = testing::call(
            CryptoFunction,
            json!({"op": "hmac", "key": {"secret": "signing"}, "data": "abc",
                   "output": "data.mac"}),
        )
        .exprs(exprs)
        .name(NAME)
        .layer(Inputs(Arc::clone(&inputs)))
        .await;
        assert!(called.issues.is_empty(), "{:?}", called.issues);
        assert_eq!(called.outcome.expect("signed"), Outcome::Ok);
        let literal = twin::v4(
            NAME,
            CryptoFunction,
            json!({"op": "hmac", "key": SIGNING, "data": "abc", "output": "data.mac"}),
            json!({}),
        )
        .await
        .expect("signed");
        assert_eq!(Value::from(called.message.context().data()), literal);
        let inputs = inputs.lock().expect("unpoisoned");
        assert_eq!(inputs[0]["key"], Value::Null, "{inputs:?}");
        assert!(!inputs[0].to_string().contains(SIGNING));
        assert!(!format!("{:?}", called.changes).contains(SIGNING));
    }

    #[tokio::test]
    async fn password_verify_rejects_excessive_encoded_argon2_and_bcrypt_costs_before_kdf() {
        // Argon2 hash with m=1048576 (1 GiB, above 131_072 KiB max)
        let excessive_argon2_m =
            "$argon2id$v=19$m=1048576,t=2,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG";
        let err = agreed(
            json!({"op": "password_verify", "password": "pw",
                   "hash": excessive_argon2_m, "output": "data.ok"}),
            json!({}),
        )
        .await
        .expect_err("excessive argon2 memory_kib must be rejected");
        assert!(
            err.message.contains("memory_kib") && err.message.contains("outside allowed range"),
            "{err:?}"
        );

        // Argon2 hash with t=99 (above 10 max)
        let excessive_argon2_t =
            "$argon2id$v=19$m=19456,t=99,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG";
        let err = agreed(
            json!({"op": "password_verify", "password": "pw",
                   "hash": excessive_argon2_t, "output": "data.ok"}),
            json!({}),
        )
        .await
        .expect_err("excessive argon2 iterations must be rejected");
        assert!(
            err.message.contains("iterations") && err.message.contains("outside allowed range"),
            "{err:?}"
        );

        // Bcrypt hash with cost=31 (above 14 max)
        let excessive_bcrypt = "$2b$31$abcdefghijklmnopqrstuuABCDEFGHIJKLMNOPQRSTUVWXYZ01234";
        let err = agreed(
            json!({"op": "password_verify", "password": "pw",
                   "hash": excessive_bcrypt, "output": "data.ok"}),
            json!({}),
        )
        .await
        .expect_err("excessive bcrypt cost must be rejected");
        assert!(
            err.message.contains("cost (31)") && err.message.contains("outside allowed range"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn kdf_admission_enforces_queue_deadline_and_retains_permit_until_blocking_task_finishes()
    {
        use std::sync::{Arc, Barrier};
        use std::time::Duration;

        let admission = Arc::new(KdfAdmission::new(1, 16_384, Duration::from_millis(30)));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let release_barrier = Arc::new(Barrier::new(2));
        let barrier_in_worker = Arc::clone(&release_barrier);
        let admission_first = Arc::clone(&admission);

        // Spawn a KDF job that acquires the single permit and blocks on `release_barrier`,
        // then immediately abort the caller future once the blocking closure has started.
        let first_handle = tokio::spawn(async move {
            spawn_hashing_with_admission(&admission_first, 8_192, move || {
                let _ = started_tx.send(());
                barrier_in_worker.wait();
                Ok(Value::Bool(true))
            })
            .await
        });

        started_rx.await.expect("blocking task started");
        // Abandon the caller while the blocking closure is still running!
        first_handle.abort();
        let _ = first_handle.await;

        // While the blocking closure is still blocked on `release_barrier`,
        // a second caller must NOT be able to acquire the permit and must time out.
        let second_err = spawn_hashing_with_admission(&admission, 8_192, || Ok(Value::Bool(true)))
            .await
            .expect_err("second KDF request must time out while first blocking task still runs");
        assert!(
            second_err.msg.contains("timed out"),
            "expected queue timeout error, got: {second_err:?}"
        );

        // Now release the first blocking closure and verify the permit becomes available again.
        tokio::task::spawn_blocking(move || {
            release_barrier.wait();
        })
        .await
        .expect("barrier wait finishes");

        // Wait briefly for the blocking closure to drop its permit.
        let retry_admission = KdfAdmission {
            concurrency: Arc::clone(&admission.concurrency),
            memory_kib: Arc::clone(&admission.memory_kib),
            max_memory_kib: admission.max_memory_kib,
            queue_timeout: Duration::from_millis(500),
        };
        let third_ok =
            spawn_hashing_with_admission(&retry_admission, 8_192, || Ok(Value::Bool(true)))
                .await
                .expect("permit must be released once blocking task finishes");
        assert_eq!(third_ok, Value::Bool(true));
    }
}

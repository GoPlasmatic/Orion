//! Orion's custom JSONLogic operators — the encoding glue of #259(c), the
//! `random` generator of #260, and the string operators of #276.
//!
//! Eight pure encoding operators (`base64_encode`/`base64_decode`,
//! `base64url_encode`/`base64url_decode`, `hex_encode`/`hex_decode`,
//! `url_encode`/`url_decode`) plus the CSPRNG-backed `random` and `join`,
//! registered on every engine that evaluates authored
//! expressions, so they compose inside conditions, `map` logic, `body_logic`
//! — anywhere JSONLogic runs. They are Orion's
//! vocabulary, not datalogic-rs's: the implementations live here and enter
//! each engine through dataflow-rs 4's `ExprEngineBuilder` ([`orion_exprs`],
//! the one expression set every engine the process builds compiles on), or
//! through `datalogic_rs`'s own `add_operator` for the bare engines Orion
//! builds directly. Built-in names always win over custom registrations, so these
//! live in namespaces no JSONLogic built-in uses; the operator vocabulary
//! test pins each one against the live engine, which would also surface any
//! future upstream collision.
//!
//! Byte/text model (shared with the `crypto` function's spec in #259):
//!
//! - Encoders take text: a string encodes as its UTF-8 bytes; a non-string
//!   value encodes as its compact-JSON text (deterministic — key order is
//!   preserved end to end), so `{"base64url_encode": {"var": "data.claims"}}`
//!   is expressible. `null` is an error, not `"null"` — it is almost always a
//!   missing variable, and failing loud beats signing the wrong bytes.
//! - Decoders are strict: input must be a string in the expected alphabet
//!   (base64 accepts padded and unpadded), and the decoded bytes must be
//!   valid UTF-8 — JSONLogic values are strings, so binary payloads belong to
//!   the `crypto` function's `input_encoding` path instead. Invalid input is
//!   an evaluation error, never a lossy guess.
//!
//! Being the one home of the byte/text model, this module also hosts the
//! shared HMAC primitives (`mac_compute`/`mac_verify`) that the `crypto`
//! function, channel HMAC auth, and SigV4 signing all spell identically.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::OnceLock;

use dataflow_rs::datalogic_rs::operator::EvalContext;
use dataflow_rs::datalogic_rs::{self as datalogic, ArenaExt, CustomOperator, DataValue, Error};
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{ExprEngine, Operator, Secrets};

/// Every JSONLogic operator this build evaluates — datalogic-rs's core and
/// extension sets (selected by [`orion_exprs`]) plus
/// Orion's own registrations above and the engine's `secret`.
///
/// Authoring-time checks read this to tell an operator apart from a data key:
/// the engine runs datalogic in templating mode, where an unknown operator is
/// not an error but an object that echoes back as literal data, so membership
/// here is the only thing separating a live call from inert JSON.
///
/// It is asked of a built engine rather than hand-maintained, backed by
/// datalogic's own opcode table — so the vocabulary is derived from what this
/// build actually dispatches rather than from what someone remembered to
/// type. The distinction is not
/// academic: enabling an extension family turns names on, and any crate in the
/// dependency graph enabling a family Orion exposes no feature for would make
/// operators live that a typed list would have called typos.
///
/// Read from [`ExprEngine::operators`] on an [`orion_exprs`] engine, which
/// lists the built-ins this build's datalogic reports, Orion's ten and
/// `secret`. The names do not depend on the secrets or the budget, so the engine asked is one with
/// neither. Built once and cached.
pub fn operator_names() -> &'static BTreeSet<String> {
    static NAMES: OnceLock<BTreeSet<String>> = OnceLock::new();
    NAMES.get_or_init(|| match orion_exprs(&crate::ResolvedSecrets::empty(), 0) {
        Ok(exprs) => exprs
            .operators()
            .iter()
            .map(|op| op.name.to_string())
            .collect(),
        // Unreachable in practice — Orion's names are distinct and none is
        // a built-in, which the tests below pin — but a lint is the wrong
        // place to panic. An empty vocabulary makes `is_operator` answer
        // `false` throughout, which downgrades the advisory to silence
        // rather than rejecting valid workflows.
        Err(e) => {
            tracing::error!(
                error = %e,
                "could not enumerate the JSONLogic operator vocabulary; \
                 authoring-time operator advisories are disabled"
            );
            BTreeSet::new()
        }
    })
}

/// The encoding table and the MAC helpers live in [`crate::crypto`] — a
/// primitive four modules outside the engine need does not belong in the
/// JSONLogic operator registry, which is where it grew. Re-exported here
/// because the operators below are its heaviest user.
pub(crate) use crate::crypto::{Codec, decode_bytes, encode_bytes};

/// Whether `name` is an operator this build evaluates, rather than a data key.
pub fn is_operator(name: &str) -> bool {
    operator_names().contains(name)
}

/// Byte chunk size for custom-operator budget charging, matching
/// `datalogic-rs`'s `STRING_UNIT` (64 bytes, one cache line) so custom string
/// and encoding operators price byte movement at the same scale as built-in
/// string operators while keeping short strings (< 64 bytes) free of extra
/// per-byte charges.
const STRING_UNIT: usize = 64;

/// Charge one operation per whole [`STRING_UNIT`] bytes touched.
#[inline]
fn charge_bytes(ctx: &mut EvalContext<'_, '_>, bytes: usize) -> datalogic::Result<()> {
    if bytes >= STRING_UNIT {
        ctx.charge((bytes / STRING_UNIT) as u64)?;
    }
    Ok(())
}

/// Charge for walking a composite value before serializing it to compact JSON.
fn charge_composite_work(
    ctx: &mut EvalContext<'_, '_>,
    value: &DataValue<'_>,
) -> datalogic::Result<()> {
    if let Some(items) = value.as_array() {
        ctx.charge(items.len() as u64)?;
        for item in items {
            charge_composite_work(ctx, item)?;
        }
    } else if let Some(entries) = value.as_object() {
        ctx.charge(entries.len() as u64)?;
        for (key, val) in entries {
            charge_bytes(ctx, key.len())?;
            charge_composite_work(ctx, val)?;
        }
    } else if let Some(s) = value.as_str() {
        charge_bytes(ctx, s.len())?;
    }
    Ok(())
}

/// The text an encoder operates on: strings as-is (borrowed — no copy on the
/// common case), other values as their compact-JSON form, `null`/absent as an
/// error naming the operator.
fn encode_text<'v>(
    op: &'static str,
    args: &[&'v DataValue<'v>],
    ctx: &mut EvalContext<'_, 'v>,
) -> Result<Cow<'v, str>, Error> {
    let Some(v) = args.first() else {
        return Err(Error::custom_message(format!("{op} requires one argument")));
    };
    match v.as_str() {
        Some(s) => Ok(Cow::Borrowed(s)),
        None if v.is_null() => Err(Error::custom_message(format!(
            "{op} of null — the argument is missing or resolved to nothing"
        ))),
        None => {
            charge_composite_work(ctx, v)?;
            Ok(Cow::Owned(v.to_string()))
        }
    }
}

/// The string a decoder operates on. Strict: no coercion.
fn decode_text<'v>(op: &'static str, args: &[&'v DataValue<'v>]) -> Result<&'v str, Error> {
    args.first().and_then(|v| v.as_str()).ok_or_else(|| {
        Error::custom_message(format!("{op} requires one string argument to decode"))
    })
}

struct Encode {
    name: &'static str,
    codec: Codec,
}

impl CustomOperator for Encode {
    fn evaluate<'a>(
        &self,
        args: &[&'a DataValue<'a>],
        ctx: &mut EvalContext<'_, 'a>,
        arena: &'a datalogic::bumpalo::Bump,
    ) -> datalogic::Result<&'a DataValue<'a>> {
        let text = encode_text(self.name, args, ctx)?;
        charge_bytes(ctx, text.len())?;
        Ok(arena.string(&encode_bytes(self.codec, text.as_bytes())))
    }
}

struct Decode {
    name: &'static str,
    codec: Codec,
}

impl CustomOperator for Decode {
    fn evaluate<'a>(
        &self,
        args: &[&'a DataValue<'a>],
        ctx: &mut EvalContext<'_, 'a>,
        arena: &'a datalogic::bumpalo::Bump,
    ) -> datalogic::Result<&'a DataValue<'a>> {
        let input = decode_text(self.name, args)?;
        charge_bytes(ctx, input.len())?;
        let bytes = decode_bytes(self.codec, input)
            .map_err(|e| Error::custom_message(format!("{}: invalid input: {e}", self.name)))?;
        let text = String::from_utf8(bytes).map_err(|_| {
            Error::custom_message(format!(
                "{}: decoded bytes are not valid UTF-8 — binary payloads belong to the \
                 crypto function's input_encoding, not a JSONLogic string",
                self.name
            ))
        })?;
        Ok(arena.string(&text))
    }
}

/// The generator kinds `random` accepts — the extensible axis of #260. A new
/// generator is a row here (plus its arm), never a new operator name.
const RANDOM_KINDS: &[&str] = &["uuid", "digits", "int", "string", "bytes"];

/// Bounds that keep a workflow from configuring a DoS on itself.
const MAX_STRING_LENGTH: i64 = 1024;
const MAX_BYTES: i64 = 1024;
const MAX_DIGITS: i64 = 64;
/// JSON's exactly-representable integer range (±2⁵³−1): `int` bounds live
/// inside it so every consumer — including JavaScript ones — sees the exact
/// value.
const MAX_SAFE_INT: i64 = 9_007_199_254_740_991;

/// `random` — CSPRNG-backed value generation, kind-selected (#260).
///
/// Never constant-folded and never CSE-memoized: datalogic's optimizers
/// classify every custom operator as opaque and impure (the `now` treatment),
/// so a fresh value per evaluation is structural. The CSPRNG is `rand`'s
/// ThreadRng (OS-seeded); there is deliberately no seed parameter, and range
/// and alphabet sampling go through `rand`'s uniform machinery — no modulo
/// bias, which is a real property for OTPs.
///
/// A custom operator has no compile-time specialisation hook, so an unknown
/// kind or an out-of-bounds argument is an evaluation-time error naming what
/// is allowed — the trade #260 records.
struct Random;

impl CustomOperator for Random {
    fn evaluate<'a>(
        &self,
        args: &[&'a DataValue<'a>],
        ctx: &mut EvalContext<'_, 'a>,
        arena: &'a datalogic::bumpalo::Bump,
    ) -> datalogic::Result<&'a DataValue<'a>> {
        use rand::RngExt;

        let Some(kind) = args.first().and_then(|v| v.as_str()) else {
            return Err(Error::custom_message(format!(
                "random requires a generator kind as its first argument — one of {}",
                RANDOM_KINDS.join(", ")
            )));
        };
        let mut rng = rand::rng();
        match kind {
            "uuid" => {
                let id = match args.get(1).and_then(|v| v.as_str()) {
                    None | Some("v4") => uuid::Uuid::new_v4(),
                    // Time-ordered for index-friendly ids; embeds the creation
                    // instant, which is why v4 stays the default.
                    Some("v7") => uuid::Uuid::now_v7(),
                    Some(other) => {
                        return Err(Error::custom_message(format!(
                            "random uuid version '{other}' is not supported — 'v4' (default) or 'v7'"
                        )));
                    }
                };
                Ok(arena.string(&id.to_string()))
            }
            "digits" => {
                let n = int_arg(args, 1, "digits count", 1, MAX_DIGITS)?;
                charge_bytes(ctx, n as usize)?;
                let code: String = (0..n)
                    .map(|_| char::from(b'0' + rng.random_range(0..10u8)))
                    .collect();
                Ok(arena.string(&code))
            }
            "int" => {
                let min = int_arg(args, 1, "min", -MAX_SAFE_INT, MAX_SAFE_INT)?;
                let max = int_arg(args, 2, "max", -MAX_SAFE_INT, MAX_SAFE_INT)?;
                if min > max {
                    return Err(Error::custom_message(format!(
                        "random int bounds are inverted: min {min} > max {max}"
                    )));
                }
                Ok(arena.i64(rng.random_range(min..=max)))
            }
            "string" => {
                let length = int_arg(args, 1, "length", 1, MAX_STRING_LENGTH)?;
                let alphabet = alphabet_chars(args.get(2).copied())?;
                charge_bytes(ctx, length as usize)?;
                let s: String = (0..length)
                    .map(|_| alphabet[rng.random_range(0..alphabet.len())])
                    .collect();
                Ok(arena.string(&s))
            }
            "bytes" => {
                let n = int_arg(args, 1, "byte count", 1, MAX_BYTES)?;
                let codec = match args.get(2).and_then(|v| v.as_str()) {
                    None => Codec::Hex,
                    Some(name) => Codec::parse(name).ok_or_else(|| {
                        Error::custom_message(format!(
                            "random bytes encoding '{name}' is not supported — \
                             hex (default), base64, base64url"
                        ))
                    })?,
                };
                charge_bytes(ctx, n as usize)?;
                let mut buf = vec![0u8; n as usize];
                rng.fill(buf.as_mut_slice());
                Ok(arena.string(&encode_bytes(codec, &buf)))
            }
            other => Err(Error::custom_message(format!(
                "random kind '{other}' is not supported — one of {}",
                RANDOM_KINDS.join(", ")
            ))),
        }
    }
}

/// A required integer argument inside `[min, max]`, named plainly on failure.
fn int_arg(
    args: &[&DataValue<'_>],
    index: usize,
    what: &str,
    min: i64,
    max: i64,
) -> Result<i64, Error> {
    let Some(n) = args.get(index).and_then(|v| v.as_i64()) else {
        return Err(Error::custom_message(format!(
            "random requires {what} (an integer) as argument {index}"
        )));
    };
    if n < min || n > max {
        return Err(Error::custom_message(format!(
            "random {what} must be between {min} and {max}, got {n}"
        )));
    }
    Ok(n)
}

/// The named alphabets, materialized once — `random string` samples on every
/// evaluation and must not rebuild its character set each time.
static ALPHANUMERIC_CHARS: std::sync::LazyLock<Vec<char>> =
    std::sync::LazyLock::new(|| ALPHANUMERIC.chars().collect());
static HEX_CHARS: std::sync::LazyLock<Vec<char>> =
    std::sync::LazyLock::new(|| "0123456789abcdef".chars().collect());
static NUMERIC_CHARS: std::sync::LazyLock<Vec<char>> =
    std::sync::LazyLock::new(|| "0123456789".chars().collect());
static URL_SAFE_CHARS: std::sync::LazyLock<Vec<char>> =
    std::sync::LazyLock::new(|| ALPHANUMERIC.chars().chain(['-', '_']).collect());

const ALPHANUMERIC: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// The character set for `random string`: a named alphabet, or a custom one
/// given as a literal string of **distinct** characters — a duplicate would
/// silently bias sampling toward it, so it is rejected instead.
fn alphabet_chars(arg: Option<&DataValue<'_>>) -> Result<Cow<'static, [char]>, Error> {
    let spec = match arg.map(|v| v.as_str()) {
        None => None,
        Some(Some(s)) => Some(s),
        Some(None) => {
            return Err(Error::custom_message(
                "random string alphabet must be a string — a named set or the characters themselves",
            ));
        }
    };
    let chars: Cow<'static, [char]> = match spec {
        None | Some("alphanumeric") => Cow::Borrowed(ALPHANUMERIC_CHARS.as_slice()),
        Some("hex") => Cow::Borrowed(HEX_CHARS.as_slice()),
        Some("numeric") => Cow::Borrowed(NUMERIC_CHARS.as_slice()),
        Some("url-safe") => Cow::Borrowed(URL_SAFE_CHARS.as_slice()),
        Some(custom) => {
            let chars: Vec<char> = custom.chars().collect();
            let mut seen = std::collections::BTreeSet::new();
            if let Some(dup) = chars.iter().find(|c| !seen.insert(**c)) {
                return Err(Error::custom_message(format!(
                    "random string alphabet repeats '{dup}' — duplicates would bias \
                     sampling, so every character must be distinct"
                )));
            }
            if chars.len() < 2 || chars.len() > 256 {
                return Err(Error::custom_message(format!(
                    "random string alphabet must have 2–256 distinct characters, got {}",
                    chars.len()
                )));
            }
            Cow::Owned(chars)
        }
    };
    Ok(chars)
}

/// Percent-encode text for a URL, RFC 3986 style.
///
/// There is no percent-encoder reachable from JSONLogic otherwise, and
/// `http_call` has no `query` field — an outbound query string is assembled
/// with `cat` into `path`/`path_logic`. Interpolating an unescaped value there
/// silently restructures the URL: a `&` or `#` in the value ends the parameter
/// or starts a fragment, and a `+` is decoded as a space by any form-decoding
/// server. That is a correctness and injection hole with no other mitigation.
struct UrlEncode;

impl CustomOperator for UrlEncode {
    fn evaluate<'a>(
        &self,
        args: &[&'a DataValue<'a>],
        ctx: &mut EvalContext<'_, 'a>,
        arena: &'a datalogic::bumpalo::Bump,
    ) -> datalogic::Result<&'a DataValue<'a>> {
        // Same text model as the encoders, `null` included: silently encoding
        // a missing variable into an empty query value is the failure class
        // the codec operators already reject.
        let text = encode_text("url_encode", args, ctx)?;
        charge_bytes(ctx, text.len())?;
        Ok(arena.string(&crate::crypto::uri_encode(&text, false)))
    }
}

/// The inverse of [`UrlEncode`]. Strict, like the other decoders: an invalid
/// `%XX` sequence or a non-UTF-8 result is an error, never a lossy guess.
struct UrlDecode;

impl CustomOperator for UrlDecode {
    fn evaluate<'a>(
        &self,
        args: &[&'a DataValue<'a>],
        ctx: &mut EvalContext<'_, 'a>,
        arena: &'a datalogic::bumpalo::Bump,
    ) -> datalogic::Result<&'a DataValue<'a>> {
        let input = decode_text("url_decode", args)?;
        charge_bytes(ctx, input.len())?;
        // Shared with route-parameter decoding, which returns `None` on both
        // failure modes. Note this does **not** treat `+` as a space: that is
        // form-encoding, and the correct inverse of RFC 3986 leaves it alone.
        let decoded = orion_common::text::percent_decode_segment(input).ok_or_else(|| {
            Error::custom_message(
                "url_decode: invalid input — a malformed %XX sequence, or bytes that are not \
                 valid UTF-8"
                    .to_string(),
            )
        })?;
        Ok(arena.string(&decoded))
    }
}

/// Join an array's elements with a separator.
///
/// `cat` flattens one array level, so `{"cat": [arr]}` is already
/// `join(arr, "")` — but a non-empty separator has no correct spelling. The
/// reduce-with-sentinel idiom cannot distinguish "first element" from "first
/// element is empty" (`["", "b"]` yields `"b"`, not `", b"`), and an author
/// cannot fix that without a second sentinel that is itself unrepresentable.
struct Join;

impl CustomOperator for Join {
    fn evaluate<'a>(
        &self,
        args: &[&'a DataValue<'a>],
        ctx: &mut EvalContext<'_, 'a>,
        arena: &'a datalogic::bumpalo::Bump,
    ) -> datalogic::Result<&'a DataValue<'a>> {
        let Some(first) = args.first() else {
            return Err(Error::custom_message(
                "join requires an array and a separator".to_string(),
            ));
        };
        let Some(items) = first.as_array() else {
            // `cat` already handles scalars; a scalar here is a mistake worth
            // naming rather than silently stringifying.
            return Err(Error::custom_message(format!(
                "join: first argument must be an array, got {}",
                type_name(first)
            )));
        };
        // A missing separator is an error, not an implicit "": `join(arr, "")`
        // is spelled explicitly, and it is exactly `cat(arr)`.
        let Some(sep) = args.get(1).and_then(|v| v.as_str()) else {
            return Err(Error::custom_message(
                "join requires a string separator as its second argument".to_string(),
            ));
        };

        ctx.charge(items.len() as u64)?;
        let mut out = String::new();
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                charge_bytes(ctx, sep.len())?;
                out.push_str(sep);
            }
            let piece = render_element(item, ctx)?;
            charge_bytes(ctx, piece.len())?;
            out.push_str(&piece);
        }
        Ok(arena.string(&out))
    }
}

/// Render one array element exactly as `cat` renders it, so
/// `join(arr, "") == cat(arr)` — one answer per question.
///
/// Note `null` becomes `""` here rather than an error. The null-is-an-error
/// rule governs an operator's *own* argument, where null means a missing
/// variable; inside an array being joined it is a value, and `cat` already
/// renders it empty.
fn render_element<'v>(
    v: &'v DataValue<'v>,
    ctx: &mut EvalContext<'_, 'v>,
) -> Result<Cow<'v, str>, Error> {
    match v.as_str() {
        Some(s) => Ok(Cow::Borrowed(s)),
        None if v.is_null() => Ok(Cow::Borrowed("")),
        None => {
            charge_composite_work(ctx, v)?;
            Ok(Cow::Owned(v.to_string()))
        }
    }
}

/// A value's JSON type name, for error messages.
fn type_name(v: &DataValue<'_>) -> &'static str {
    if v.is_null() {
        "null"
    } else if v.as_bool().is_some() {
        "boolean"
    } else if v.as_str().is_some() {
        "string"
    } else if v.as_array().is_some() {
        "array"
    } else if v.as_object().is_some() {
        "object"
    } else {
        "number"
    }
}

/// Every operator Orion registers, name first. One list so the two
/// registration paths below (and the vocabulary test) cannot drift.
fn all() -> impl Iterator<Item = (&'static str, OrionOperator)> {
    const CODECS: [(&str, &str, Codec); 3] = [
        ("base64_encode", "base64_decode", Codec::Base64),
        ("base64url_encode", "base64url_decode", Codec::Base64Url),
        ("hex_encode", "hex_decode", Codec::Hex),
    ];
    CODECS
        .into_iter()
        .flat_map(|(enc, dec, codec)| {
            [
                (enc, OrionOperator::Encode(Encode { name: enc, codec })),
                (dec, OrionOperator::Decode(Decode { name: dec, codec })),
            ]
        })
        .chain([
            ("random", OrionOperator::Random(Random)),
            ("url_encode", OrionOperator::UrlEncode(UrlEncode)),
            ("url_decode", OrionOperator::UrlDecode(UrlDecode)),
            // Plain `join`, not `str_join`: datalogic's compile walker tries
            // `OpCode` first and only falls through to the custom registry, so
            // a future `ext-string` `join` would shadow this one. The
            // vocabulary test asserts on **values**, not compile success, so
            // such a shadow fails CI rather than silently changing semantics.
            ("join", OrionOperator::Join(Join)),
        ])
}

/// The concrete operator behind one registered name.
enum OrionOperator {
    Encode(Encode),
    Decode(Decode),
    Random(Random),
    UrlEncode(UrlEncode),
    UrlDecode(UrlDecode),
    Join(Join),
}

impl OrionOperator {
    /// Whether the same arguments always give the same result: every one but
    /// `random`, whose whole job is a fresh value per evaluation (#260).
    fn deterministic(&self) -> bool {
        !matches!(self, OrionOperator::Random(_))
    }
}

impl CustomOperator for OrionOperator {
    fn evaluate<'a>(
        &self,
        args: &[&'a DataValue<'a>],
        ctx: &mut EvalContext<'_, 'a>,
        arena: &'a datalogic::bumpalo::Bump,
    ) -> datalogic::Result<&'a DataValue<'a>> {
        match self {
            OrionOperator::Encode(op) => op.evaluate(args, ctx, arena),
            OrionOperator::Decode(op) => op.evaluate(args, ctx, arena),
            OrionOperator::Random(op) => op.evaluate(args, ctx, arena),
            OrionOperator::UrlEncode(op) => op.evaluate(args, ctx, arena),
            OrionOperator::UrlDecode(op) => op.evaluate(args, ctx, arena),
            OrionOperator::Join(op) => op.evaluate(args, ctx, arena),
        }
    }
}

/// The one expression engine the process evaluates with outside a workflow
/// (I188): channel guards, channel auth, cron and the admin `/validate`
/// endpoint compile and evaluate on it. Built once at boot, held on
/// `AppState`, and kept across every reload.
///
/// Every workflow engine compiles on the same set (`serving_registry`), so an
/// expression means the same everywhere it runs: Orion's ten operators, the
/// nine pure ones declared deterministic and `random` left at the default
/// (not deterministic), the engine's secret store, and the operation budget.
/// `secrets` is a parameter because it is part of what an expression can
/// read: a surface that built its expressions without it would report
/// `UNKNOWN_SECRET` for a workflow the serving engine runs happily, so pass
/// [`ResolvedSecrets::empty`](crate::ResolvedSecrets::empty) where
/// there is genuinely no store. `ops_budget` is configuration
/// (`engine.ops_budget`), and `0` means no ceiling rather than a budget of
/// zero, which every expression would cross. The failed-task error context
/// is a workflow engine's setting and has no place here.
///
/// # Errors
///
/// What [`ExprEngineBuilder::build`](dataflow_rs::expr::ExprEngineBuilder::build)
/// refuses — an operator registered twice or named like a built-in, which the
/// tests below rule out — and a store that is not an object, which
/// [`ResolvedSecrets`](crate::ResolvedSecrets) never is.
pub fn orion_exprs(
    secrets: &crate::ResolvedSecrets,
    ops_budget: u64,
) -> dataflow_rs::Result<ExprEngine> {
    let mut builder = ExprEngine::builder().all_operators();
    for (name, op) in all() {
        builder = if op.deterministic() {
            builder.operator_with(Operator::custom(name).deterministic(true), op)
        } else {
            builder.operator(name, op)
        };
    }
    builder = builder.secrets(Secrets::new(OwnedDataValue::from(secrets.as_json()))?);
    // `0` means "do not set a ceiling".
    if ops_budget > 0 {
        builder = builder.ops_budget(ops_budget);
    }
    builder.build()
}

/// An expression's result as the `serde_json::Value` 1.12 handed back for it
/// (`session().eval_into::<Value>()`), for the readers that act on that
/// JSON: a rate-limit key's text and a response-cache key's bytes.
///
/// Built by datalogic's own conversion over the value's arena view, the one
/// `eval_into` used, so it is that JSON exactly: a datetime is its
/// `{"datetime": …}` sentinel and a non-finite number is `null`.
///
/// The view's arena is one per thread, reset after each call rather than
/// built and dropped per call: the rate-limit and cache-key paths call this
/// once per request, and a composite key (an array, an object) allocates its
/// spine there. The result owns its data, so nothing borrows the arena past
/// the reset. An arena that a large result grew past `ARENA_KEEP_BYTES` is
/// dropped instead of kept, so one outsized key does not pin its memory to the
/// thread for good.
pub fn result_json(value: &OwnedDataValue) -> serde_json::Value {
    thread_local! {
        static ARENA: std::cell::RefCell<datalogic::bumpalo::Bump> =
            std::cell::RefCell::new(datalogic::bumpalo::Bump::new());
    }
    let convert = |arena: &datalogic::bumpalo::Bump| {
        <serde_json::Value as datalogic::FromDataValue>::from_arena(&value.to_arena(arena))
            // Infallible for this output: the conversion only walks the tree.
            .unwrap_or(serde_json::Value::Null)
    };
    ARENA
        .try_with(|cell| match cell.try_borrow_mut() {
            Ok(mut arena) => {
                let json = convert(&arena);
                if arena.allocated_bytes() > ARENA_KEEP_BYTES {
                    *arena = datalogic::bumpalo::Bump::new();
                } else {
                    arena.reset();
                }
                json
            }
            // Re-entered from inside a conversion, which nothing does today:
            // fall back to an arena of its own rather than panic.
            Err(_) => convert(&datalogic::bumpalo::Bump::new()),
        })
        // The thread is being torn down; same fallback.
        .unwrap_or_else(|_| convert(&datalogic::bumpalo::Bump::new()))
}

/// The most a thread's [`result_json`] arena keeps between calls.
const ARENA_KEEP_BYTES: usize = 64 * 1024;

/// Orion's operators, for the datalogic engines Orion builds directly.
///
/// It cannot carry `secret` across: that operator is registered by dataflow-rs
/// on the engines *it* builds, and its implementation is crate-private. So an
/// engine built this way refuses `{"secret": …}` at compile.
///
/// **This is not how the serving instance builds its expression engine.**
/// Secrets are start-time config, resolved before any channel loads, so there
/// is no reason a `validation_logic` should see fewer of them than a workflow
/// does: the guards, auth, cron and `/validate` run on
/// [`orion_exprs`], whose `secret` is dataflow-rs's own operator over the real
/// store. One implementation, so the surfaces cannot disagree about what a
/// secret name resolves to.
///
/// What is left for this function is the bare engines with no store to offer:
/// the authoring-time compile checks, and the analysis paths that have not
/// moved to [`orion_exprs`] yet.
pub fn add_to_datalogic(mut builder: datalogic::EngineBuilder) -> datalogic::EngineBuilder {
    for (name, op) in all() {
        builder = builder.add_operator(name, op);
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(super) fn eval(logic: serde_json::Value) -> Result<serde_json::Value, String> {
        // Templating mode, like every engine Orion runs expressions on.
        let e = add_to_datalogic(datalogic::Engine::builder().with_templating(true)).build();
        let out = e
            .eval_str(&logic.to_string(), "{}")
            .map_err(|err| err.to_string())?;
        serde_json::from_str(&out).map_err(|err| err.to_string())
    }

    #[test]
    fn encode_decode_round_trips() {
        for (enc, dec, encoded) in [
            ("base64_encode", "base64_decode", "aGVsbG8gd29ybGQ="),
            ("base64url_encode", "base64url_decode", "aGVsbG8gd29ybGQ"),
            ("hex_encode", "hex_decode", "68656c6c6f20776f726c64"),
        ] {
            assert_eq!(
                eval(json!({enc: ["hello world"]})).expect("test"),
                json!(encoded),
                "{enc}"
            );
            assert_eq!(
                eval(json!({dec: [encoded]})).expect("test"),
                json!("hello world"),
                "{dec}"
            );
        }
    }

    #[test]
    fn base64url_is_unpadded_and_url_safe() {
        // 0xfb 0xef in the input forces '-'/'_' vs '+'/'/' to differ, and the
        // length forces padding in the standard form.
        assert_eq!(
            eval(json!({"base64url_encode": ["sign?me>"]})).expect("test"),
            json!("c2lnbj9tZT4")
        );
        assert_eq!(
            eval(json!({"base64_encode": ["sign?me>"]})).expect("test"),
            json!("c2lnbj9tZT4=")
        );
    }

    #[test]
    fn url_encode_uses_the_rfc_3986_unreserved_set() {
        // Unreserved characters survive; everything else is uppercase %XX.
        assert_eq!(
            eval(json!({"url_encode": ["AZaz09-_.~"]})).expect("test"),
            json!("AZaz09-_.~")
        );
        // The three that silently corrupt a query string when interpolated
        // raw: `&` ends the parameter, `#` starts a fragment, `+` is decoded
        // as a space by a form-decoding server.
        assert_eq!(
            eval(json!({"url_encode": ["a b+c&d#e"]})).expect("test"),
            json!("a%20b%2Bc%26d%23e")
        );
        // Space is %20, never `+` — this is RFC 3986, not form-encoding.
        assert_eq!(
            eval(json!({"url_encode": [" "]})).expect("test"),
            json!("%20")
        );
        // Stricter than JavaScript's encodeURIComponent, which leaves !'()*
        // literal. Pinned because it is the comparison an author will make.
        assert_eq!(
            eval(json!({"url_encode": ["!'()*"]})).expect("test"),
            json!("%21%27%28%29%2A")
        );
        // Multi-byte input encodes per UTF-8 byte.
        assert_eq!(
            eval(json!({"url_encode": ["é"]})).expect("test"),
            json!("%C3%A9")
        );
    }

    #[test]
    fn url_encode_round_trips_and_decodes_strictly() {
        for text in ["hello world", "a&b=c", "é@ü", "100%"] {
            let encoded = eval(json!({"url_encode": [text]})).expect("test");
            assert_eq!(
                eval(json!({"url_decode": [encoded]})).expect("test"),
                json!(text),
                "round trip of {text:?}"
            );
        }
        // `+` is left alone: the correct RFC 3986 inverse, not form-decoding.
        assert_eq!(
            eval(json!({"url_decode": ["a+b"]})).expect("test"),
            json!("a+b")
        );
        // Strict: a malformed escape is an error, never a lossy guess.
        for bad in ["%", "%2", "%ZZ", "%C3%28"] {
            let err = eval(json!({"url_decode": [bad]})).expect_err(bad);
            assert!(err.contains("url_decode"), "{bad}: {err}");
        }
    }

    #[test]
    fn url_encode_of_null_is_an_error() {
        // Encoding a missing variable into an empty query value is the
        // failure class the codec operators already refuse.
        let err = eval(json!({"url_encode": [{"var": "nope"}]})).expect_err("test");
        assert!(err.contains("url_encode"), "{err}");
        // And a non-string coerces to its compact JSON, like the codecs.
        assert_eq!(
            eval(json!({"url_encode": [42]})).expect("test"),
            json!("42")
        );
    }

    #[test]
    fn join_inserts_the_separator_between_elements() {
        assert_eq!(
            eval(json!({"join": [["a", "b", "c"], ", "]})).expect("test"),
            json!("a, b, c")
        );
        assert_eq!(eval(json!({"join": [[], "-"]})).expect("test"), json!(""));
        assert_eq!(
            eval(json!({"join": [["solo"], "-"]})).expect("test"),
            json!("solo")
        );
    }

    /// The defect that makes `join` worth having: the only idiom available
    /// today — reduce with an `accumulator == ""` sentinel — cannot tell
    /// "first element" from "first element is empty", so `["", "b"]` joins to
    /// `"b"` instead of `", b"`. An author cannot fix that without a second
    /// sentinel that is itself unrepresentable.
    #[test]
    fn join_preserves_a_leading_empty_element() {
        assert_eq!(
            eval(json!({"join": [["", "b"], ", "]})).expect("test"),
            json!(", b")
        );
        assert_eq!(
            eval(json!({"join": [["", "", "x"], "|"]})).expect("test"),
            json!("||x")
        );
    }

    /// One answer per question: `join(arr, "")` must be exactly `cat(arr)`,
    /// including how each element renders. This is the guard that would fail
    /// if the two ever disagreed about numbers, booleans or nulls.
    #[test]
    fn join_with_an_empty_separator_equals_cat() {
        for arr in [
            json!(["a", "b", "c"]),
            json!([1, 2, 3]),
            json!([1.5, true, false]),
            json!(["a", null, "b"]),
            json!([{"k": 1}, [2, 3]]),
        ] {
            assert_eq!(
                eval(json!({"join": [arr.clone(), ""]})).expect("test"),
                eval(json!({"cat": [arr.clone()]})).expect("test"),
                "join(arr, \"\") must equal cat(arr) for {arr}"
            );
        }
    }

    #[test]
    fn join_refuses_a_non_array_or_a_missing_separator() {
        let err = eval(json!({"join": ["not-an-array", "-"]})).expect_err("test");
        assert!(err.contains("must be an array"), "{err}");
        assert!(
            err.contains("string"),
            "the message names the type it got: {err}"
        );

        let err = eval(json!({"join": [["a", "b"]]})).expect_err("test");
        assert!(err.contains("separator"), "{err}");
    }

    /// Once `join` exists, `replace(s, from, to)` is `join(split(s, from), to)`
    /// — which is why no `replace` operator is registered. Pinned so the
    /// documented idiom cannot rot.
    #[test]
    fn replace_is_expressible_as_join_of_split() {
        assert_eq!(
            eval(json!({"join": [{"split": ["a-b-c", "-"]}, "+"]})).expect("test"),
            json!("a+b+c")
        );
    }

    #[test]
    fn decoders_accept_padded_and_unpadded() {
        for input in ["aGk=", "aGk"] {
            assert_eq!(
                eval(json!({"base64_decode": [input]})).expect("test"),
                json!("hi")
            );
            assert_eq!(
                eval(json!({"base64url_decode": [input]})).expect("test"),
                json!("hi")
            );
        }
    }

    #[test]
    fn non_string_encodes_as_compact_json() {
        // The crypto function's byte model, mirrored: objects/numbers encode
        // as their compact-JSON text.
        // In templating mode a non-operator object echoes as itself, so it
        // reaches the encoder as an object argument.
        assert_eq!(
            eval(json!({"hex_encode": [{"a": 1, "b": "x"}]})).expect("test"),
            json!(hex::encode(r#"{"a":1,"b":"x"}"#))
        );
        assert_eq!(
            eval(json!({"base64_encode": [42]})).expect("test"),
            json!("NDI=")
        );
    }

    #[test]
    fn decode_failures_are_strict_errors() {
        // Wrong alphabet.
        assert!(eval(json!({"hex_decode": ["zz"]})).is_err());
        assert!(eval(json!({"base64_decode": ["!!!"]})).is_err());
        // Valid base64, but the bytes are not UTF-8 (0xff 0xfe).
        let err = eval(json!({"base64_decode": ["//4="]})).expect_err("test");
        assert!(err.contains("not valid UTF-8"), "{err}");
        // Decoding a non-string is refused, not coerced.
        assert!(eval(json!({"base64_decode": [42]})).is_err());
    }

    #[test]
    fn encoding_null_is_an_error_not_the_string_null() {
        let err = eval(json!({"base64_encode": [{"var": "data.absent"}]})).expect_err("test");
        assert!(err.contains("null"), "{err}");
    }

    // ---- The process's expression engine (I188) -------------------------

    use crate::ResolvedSecrets;
    use dataflow_rs::expr::OperatorFamily;

    /// The ten names, in registration order: the table `all()` is.
    const ORION_OPERATORS: [&str; 10] = [
        "base64_encode",
        "base64_decode",
        "base64url_encode",
        "base64url_decode",
        "hex_encode",
        "hex_decode",
        "random",
        "url_encode",
        "url_decode",
        "join",
    ];

    fn exprs() -> ExprEngine {
        orion_exprs(&ResolvedSecrets::empty(), 0).expect("Orion's expression engine builds")
    }

    /// `logic` over `context` on the process's engine, as the JSON 1.12 read.
    fn on_v4(
        exprs: &ExprEngine,
        logic: &serde_json::Value,
        context: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let expr = exprs
            .compile::<OwnedDataValue>(logic)
            .map_err(|e| e.to_string())?;
        exprs
            .eval(&expr, context)
            .map(|evaluated| result_json(&evaluated.value))
            .map_err(|e| e.to_string())
    }

    #[test]
    fn orion_exprs_lists_the_ten_operators() {
        let exprs = exprs();
        let custom: Vec<&Operator> = exprs
            .operators()
            .iter()
            .filter(|op| op.family == OperatorFamily::Custom)
            .collect();
        let names: Vec<&str> = custom.iter().map(|op| &*op.name).collect();
        assert_eq!(names, ORION_OPERATORS, "the ten, in registration order");
        assert_eq!(
            names,
            all().map(|(name, _)| name).collect::<Vec<_>>(),
            "and they are the table's"
        );
        for op in custom {
            assert_eq!(
                op.deterministic,
                op.name != "random",
                "`{}`: only `random` gives a fresh value per evaluation",
                op.name
            );
            assert!(!op.scoping, "`{}` binds no element", op.name);
        }
    }

    #[test]
    fn operator_names_are_the_engines() {
        let exprs = exprs();
        let v4: BTreeSet<String> = exprs
            .operators()
            .iter()
            .map(|op| op.name.to_string())
            .collect();
        assert_eq!(exprs.operators().len(), v4.len(), "no name listed twice");
        assert_eq!(operator_names(), &v4, "the one `operator_names` serves");
        assert!(v4.contains("secret") && v4.contains("var"));
    }

    /// Each of the ten evaluates on the process's engine, answering or
    /// refusing by its arguments. `random` is held to its degenerate range
    /// and its refusals, the only answers that do not draw.
    #[test]
    fn every_operator_evaluates_on_the_process_engine() {
        let context = json!({
            "data": {"s": "Widget", "n": 4, "obj": {"a": 1, "b": "x"}, "xs": ["a", "", "b"]},
            "metadata": {},
            "temp_data": {},
        });
        let cases = [
            json!({"base64_encode": ["hello world"]}),
            json!({"base64_encode": [{"var": "data.s"}]}),
            json!({"base64_encode": [42]}),
            json!({"base64_encode": [{"var": "data.obj"}]}),
            json!({"base64_encode": [{"var": "data.absent"}]}),
            json!({"base64_encode": []}),
            json!({"base64_decode": ["aGVsbG8gd29ybGQ="]}),
            json!({"base64_decode": ["aGk"]}),
            json!({"base64_decode": ["!!!"]}),
            json!({"base64_decode": ["//4="]}),
            json!({"base64_decode": [42]}),
            json!({"base64url_encode": ["sign?me>"]}),
            json!({"base64url_decode": ["c2lnbj9tZT4"]}),
            json!({"base64url_decode": ["+/"]}),
            json!({"hex_encode": [{"var": "data.s"}]}),
            json!({"hex_decode": ["68656c6c6f"]}),
            json!({"hex_decode": ["zz"]}),
            json!({"url_encode": ["a b+c&d#e"]}),
            json!({"url_encode": [{"var": "data.n"}]}),
            json!({"url_encode": [{"var": "data.nope"}]}),
            json!({"url_decode": ["a%20b+c"]}),
            json!({"url_decode": ["%ZZ"]}),
            json!({"join": [{"var": "data.xs"}, ", "]}),
            json!({"join": [[1, null, true, {"k": 1}], "|"]}),
            json!({"join": ["not-an-array", "-"]}),
            json!({"join": [["a"]]}),
            json!({"random": ["int", 5, 5]}),
            json!({"random": ["melt"]}),
            json!({"random": ["uuid", "v9"]}),
            json!({"random": ["int", 9, 3]}),
            json!({"random": ["digits", 0]}),
            json!({"random": ["string", 8, "ABBA"]}),
            json!({"random": []}),
        ];
        let covered: BTreeSet<&str> = cases
            .iter()
            .filter_map(|case| case.as_object()?.keys().next().map(String::as_str))
            .collect();
        assert_eq!(
            covered,
            ORION_OPERATORS.into_iter().collect::<BTreeSet<_>>(),
            "every operator has a case"
        );

        let v4 = exprs();
        let (mut answered, mut refused) = (0, 0);
        for logic in &cases {
            if on_v4(&v4, logic, &context).is_ok() {
                answered += 1;
            } else {
                refused += 1;
            }
        }
        assert!(answered > 0 && refused > 0, "both arms are exercised");
        assert_eq!(
            on_v4(
                &v4,
                &json!({"base64_encode": [{"var": "data.s"}]}),
                &context
            ),
            Ok(json!("V2lkZ2V0"))
        );
        assert_eq!(
            on_v4(&v4, &json!({"random": ["int", 5, 5]}), &context),
            Ok(json!(5))
        );
        assert!(on_v4(&v4, &json!({"hex_decode": ["zz"]}), &context).is_err());
    }

    /// The engine's store and its ceiling, as the configuration sets them.
    #[test]
    fn orion_exprs_reads_its_secrets_and_holds_its_budget() {
        let mut values = serde_json::Map::new();
        values.insert("partner_hmac".into(), json!("hmac-value-1a2b"));
        let secrets = ResolvedSecrets::from_values(values);
        let v4 = orion_exprs(&secrets, 0).expect("builds");
        let empty = json!({});
        assert_eq!(
            on_v4(&v4, &json!({"secret": "partner_hmac"}), &empty),
            Ok(json!("hmac-value-1a2b"))
        );
        assert_eq!(
            on_v4(
                &v4,
                &json!({"==": [{"secret": "partner_hmac"}, "hmac-value-1a2b"]}),
                &empty
            ),
            Ok(json!(true))
        );
        for logic in [
            json!({"secret": "undeclared"}),
            json!({"secret": ""}),
            json!({"secret": 7}),
        ] {
            assert!(on_v4(&v4, &logic, &empty).is_err(), "{logic}");
        }

        let heavy = json!({"map": [{"var": "data.xs"}, {"+": [{"var": ""}, 1]}]});
        let context = json!({"data": {"xs": (0..100).collect::<Vec<u32>>()}});
        let bounded = orion_exprs(&ResolvedSecrets::empty(), 10).expect("builds");
        let expr = bounded.compile::<OwnedDataValue>(&heavy).expect("compiles");
        let err = bounded
            .eval(&expr, &context)
            .expect_err("crosses the ceiling");
        assert_eq!(err.kind(), &dataflow_rs::ErrorKind::Budget);
        // `0` sets none.
        assert!(on_v4(&exprs(), &heavy, &context).is_ok());
    }

    #[test]
    fn custom_operators_charge_proportional_to_variable_work() {
        let long_text = "x".repeat(64 * 10);
        let encoded_b64 = crate::crypto::encode_bytes(Codec::Base64, long_text.as_bytes());
        let encoded_url = crate::crypto::uri_encode(&long_text, false);
        let items: Vec<String> = (0..20).map(|i| format!("item-{i}")).collect();
        let context = json!({
            "data": {
                "long": long_text,
                "b64": encoded_b64,
                "url": encoded_url,
                "items": items,
            }
        });

        let bounded = orion_exprs(&ResolvedSecrets::empty(), 5).expect("builds");
        for logic in [
            json!({"base64_encode": [{"var": "data.long"}]}),
            json!({"base64_decode": [{"var": "data.b64"}]}),
            json!({"url_encode": [{"var": "data.long"}]}),
            json!({"url_decode": [{"var": "data.url"}]}),
            json!({"join": [{"var": "data.items"}, ", "]}),
            json!({"random": ["string", 512]}),
            json!({"random": ["bytes", 512]}),
        ] {
            let expr = bounded.compile::<OwnedDataValue>(&logic).expect("compiles");
            let err = bounded
                .eval(&expr, &context)
                .expect_err("variable-sized work crosses budget ceiling of 5");
            assert_eq!(err.kind(), &dataflow_rs::ErrorKind::Budget, "{logic}");
        }
    }
}

#[cfg(test)]
mod random_tests {
    use super::tests::eval;
    use serde_json::json;

    #[test]
    fn degenerate_int_range_is_deterministic() {
        // min == max pins the value — the same case the operator vocabulary
        // table uses, so registration is asserted without asserting entropy.
        assert_eq!(
            eval(json!({"random": ["int", 5, 5]})).expect("test"),
            json!(5)
        );
    }

    #[test]
    fn int_stays_inside_inclusive_bounds() {
        for _ in 0..200 {
            let v = eval(json!({"random": ["int", -3, 3]})).expect("test");
            let n = v.as_i64().expect("integer result");
            assert!((-3..=3).contains(&n), "{n}");
        }
    }

    #[test]
    fn uuid_versions_have_their_version_nibble() {
        for (args, nibble) in [
            (json!(["uuid"]), '4'),
            (json!(["uuid", "v4"]), '4'),
            (json!(["uuid", "v7"]), '7'),
        ] {
            let v = eval(json!({"random": args})).expect("test");
            let s = v.as_str().expect("string result");
            assert_eq!(s.len(), 36, "{s}");
            assert_eq!(s.chars().nth(14), Some(nibble), "{s}");
        }
        // Two draws differ — the "is it actually random" smoke test.
        assert_ne!(
            eval(json!({"random": ["uuid"]})).expect("test"),
            eval(json!({"random": ["uuid"]})).expect("test"),
        );
    }

    #[test]
    fn digits_keep_length_and_charset() {
        for _ in 0..50 {
            let v = eval(json!({"random": ["digits", 6]})).expect("test");
            let s = v.as_str().expect("string result");
            assert_eq!(s.len(), 6, "{s}");
            assert!(s.chars().all(|c| c.is_ascii_digit()), "{s}");
        }
    }

    #[test]
    fn string_kinds_respect_their_alphabets() {
        let v = eval(json!({"random": ["string", 24]})).expect("test");
        let s = v.as_str().expect("test");
        assert_eq!(s.len(), 24);
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric()), "{s}");

        let v = eval(json!({"random": ["string", 32, "hex"]})).expect("test");
        assert!(
            v.as_str()
                .expect("test")
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
            "{v}"
        );

        // Custom alphabet: only its characters appear.
        let v = eval(json!({"random": ["string", 40, "AB"]})).expect("test");
        assert!(
            v.as_str()
                .expect("test")
                .chars()
                .all(|c| c == 'A' || c == 'B'),
            "{v}"
        );
    }

    #[test]
    fn bytes_encode_per_the_pinned_table() {
        let v = eval(json!({"random": ["bytes", 8]})).expect("test");
        let s = v.as_str().expect("test");
        assert_eq!(s.len(), 16, "hex doubles: {s}");
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()), "{s}");

        // base64url of 32 bytes is 43 chars, unpadded — the JWS form.
        let v = eval(json!({"random": ["bytes", 32, "base64url"]})).expect("test");
        let s = v.as_str().expect("test");
        assert_eq!(s.len(), 43, "{s}");
        assert!(!s.ends_with('='), "{s}");
    }

    #[test]
    fn bad_arguments_are_named_evaluation_errors() {
        for (logic, expected) in [
            (
                json!({"random": ["melt"]}),
                "one of uuid, digits, int, string, bytes",
            ),
            (json!({"random": ["uuid", "v9"]}), "'v4' (default) or 'v7'"),
            (json!({"random": ["int", 9, 3]}), "inverted"),
            (json!({"random": ["int", 1]}), "max"),
            (json!({"random": ["digits", 0]}), "between 1 and 64"),
            (json!({"random": ["string", 2000]}), "between 1 and 1024"),
            (json!({"random": ["string", 8, "ABBA"]}), "repeats 'B'"),
            (json!({"random": ["string", 8, "A"]}), "2–256"),
            (
                json!({"random": ["bytes", 16, "base32"]}),
                "hex (default), base64, base64url",
            ),
            (json!({"random": []}), "generator kind"),
        ] {
            let err = eval(logic.clone()).expect_err("test");
            assert!(err.contains(expected), "{logic}: {err}");
        }
    }
}

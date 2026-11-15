//! Orion's custom JSONLogic operators — the encoding glue, the `random`
//! generator, and the string operators.
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
//! Byte/text model (shared with the `crypto` function's spec):
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
//! function and channel HMAC auth spell identically.

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

/// The generator kinds `random` accepts — the extensible axis. A new
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

/// `random` — CSPRNG-backed value generation, kind-selected.
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
/// is allowed — the trade this operator makes.
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
    /// `random`, whose whole job is a fresh value per evaluation.
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

/// The one expression engine the process evaluates with outside a workflow:
/// channel guards, channel auth, cron and the admin `/validate`
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

/// An expression's result as a `serde_json::Value`, for the readers that act
/// on JSON: a rate-limit key's text and a response-cache key's bytes.
///
/// Built by datalogic's own conversion over the value's arena view, the one
/// `eval_into` uses, so it is that JSON exactly: a datetime is its
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
#[path = "operators_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "operators_random_tests.rs"]
mod random_tests;

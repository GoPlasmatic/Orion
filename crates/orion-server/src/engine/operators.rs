//! Orion's custom JSONLogic operators — the encoding glue of #259(c) and the
//! `random` generator of #260.
//!
//! Six pure encoding operators (`base64_encode`/`base64_decode`,
//! `base64url_encode`/`base64url_decode`, `hex_encode`/`hex_decode`) plus the
//! CSPRNG-backed `random`, registered on every engine that evaluates authored
//! expressions, so they compose inside conditions, `map` logic, `body_logic`
//! — anywhere JSONLogic runs. They are Orion's
//! vocabulary, not datalogic-rs's: the implementations live here and enter
//! each engine through `dataflow_rs`'s `with_datalogic_operator` passthrough
//! (or `datalogic_rs`'s own `add_operator` for the engines Orion builds
//! directly). Built-in names always win over custom registrations, so these
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

use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use dataflow_rs::datalogic_rs::operator::EvalContext;
use dataflow_rs::datalogic_rs::{self as datalogic, ArenaExt, CustomOperator, DataValue, Error};

/// Standard-alphabet decoder that accepts padded and unpadded input.
/// Encoding always uses the canonical [`base64::engine::general_purpose::STANDARD`]
/// (padded); this leniency is decode-only.
const B64_STD_LENIENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// URL-safe-alphabet decoder that accepts padded and unpadded input.
/// Encoding always uses [`base64::engine::general_purpose::URL_SAFE_NO_PAD`] —
/// the unpadded RFC 4648 §5 form JWS uses, per the #259 encoding table.
const B64_URL_LENIENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Which alphabet an [`Encode`]/[`Decode`] instance speaks. Shared with the
/// `crypto` function so the two features implement the #259 encoding table
/// exactly once.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Codec {
    Base64,
    Base64Url,
    Hex,
}

/// Canonical encoding of `bytes` per the #259 table: hex lowercase, base64
/// standard padded, base64url unpadded (the JWS form).
pub(crate) fn encode_bytes(codec: Codec, bytes: &[u8]) -> String {
    match codec {
        Codec::Base64 => base64::engine::general_purpose::STANDARD.encode(bytes),
        Codec::Base64Url => base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        Codec::Hex => hex::encode(bytes),
    }
}

/// Strict decode per the same table; the base64 forms tolerate padded and
/// unpadded input.
pub(crate) fn decode_bytes(codec: Codec, s: &str) -> Result<Vec<u8>, String> {
    match codec {
        Codec::Base64 => B64_STD_LENIENT.decode(s).map_err(|e| e.to_string()),
        Codec::Base64Url => B64_URL_LENIENT.decode(s).map_err(|e| e.to_string()),
        Codec::Hex => hex::decode(s).map_err(|e| e.to_string()),
    }
}

/// The text an encoder operates on: strings as-is, other values as their
/// compact-JSON form, `null`/absent as an error naming the operator.
fn encode_text(op: &'static str, args: &[&DataValue<'_>]) -> Result<String, Error> {
    let Some(v) = args.first() else {
        return Err(Error::custom_message(format!("{op} requires one argument")));
    };
    match v.as_str() {
        Some(s) => Ok(s.to_string()),
        None if v.is_null() => Err(Error::custom_message(format!(
            "{op} of null — the argument is missing or resolved to nothing"
        ))),
        None => Ok(v.to_string()),
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
        _ctx: &mut EvalContext<'_, 'a>,
        arena: &'a datalogic::bumpalo::Bump,
    ) -> datalogic::Result<&'a DataValue<'a>> {
        let text = encode_text(self.name, args)?;
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
        _ctx: &mut EvalContext<'_, 'a>,
        arena: &'a datalogic::bumpalo::Bump,
    ) -> datalogic::Result<&'a DataValue<'a>> {
        let input = decode_text(self.name, args)?;
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
        _ctx: &mut EvalContext<'_, 'a>,
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
                let s: String = (0..length)
                    .map(|_| alphabet[rng.random_range(0..alphabet.len())])
                    .collect();
                Ok(arena.string(&s))
            }
            "bytes" => {
                let n = int_arg(args, 1, "byte count", 1, MAX_BYTES)?;
                let codec = match args.get(2).and_then(|v| v.as_str()) {
                    None | Some("hex") => Codec::Hex,
                    Some("base64") => Codec::Base64,
                    Some("base64url") => Codec::Base64Url,
                    Some(other) => {
                        return Err(Error::custom_message(format!(
                            "random bytes encoding '{other}' is not supported — \
                             hex (default), base64, base64url"
                        )));
                    }
                };
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

/// The character set for `random string`: a named alphabet, or a custom one
/// given as a literal string of **distinct** characters — a duplicate would
/// silently bias sampling toward it, so it is rejected instead.
fn alphabet_chars(arg: Option<&DataValue<'_>>) -> Result<Vec<char>, Error> {
    const ALPHANUMERIC: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let spec = match arg.map(|v| v.as_str()) {
        None => None,
        Some(Some(s)) => Some(s),
        Some(None) => {
            return Err(Error::custom_message(
                "random string alphabet must be a string — a named set or the characters themselves",
            ));
        }
    };
    let chars: Vec<char> = match spec {
        None | Some("alphanumeric") => ALPHANUMERIC.chars().collect(),
        Some("hex") => "0123456789abcdef".chars().collect(),
        Some("numeric") => "0123456789".chars().collect(),
        Some("url-safe") => ALPHANUMERIC.chars().chain(['-', '_']).collect(),
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
            chars
        }
    };
    Ok(chars)
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
        .chain([("random", OrionOperator::Random(Random))])
}

/// The concrete operator behind one registered name.
enum OrionOperator {
    Encode(Encode),
    Decode(Decode),
    Random(Random),
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
        }
    }
}

/// Register Orion's operators on a dataflow-rs engine builder. Every place
/// that builds a workflow engine — bootstrap, reload, dry-run, the workflow
/// test endpoint — goes through this, so the operator vocabulary is identical
/// everywhere expressions run.
pub fn with_orion_operators(
    mut builder: dataflow_rs::engine::EngineBuilder,
) -> dataflow_rs::engine::EngineBuilder {
    for (name, op) in all() {
        builder = builder.with_datalogic_operator(name, op);
    }
    builder
}

/// As [`with_orion_operators`], for the datalogic engines Orion builds
/// directly (channel-guard logic, the loader's compile parity check).
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

//! Orion's custom JSONLogic operators — the encoding glue of #259(c).
//!
//! Six pure operators (`base64_encode`/`base64_decode`, `base64url_encode`/
//! `base64url_decode`, `hex_encode`/`hex_decode`) registered on every engine
//! that evaluates authored expressions, so they compose inside conditions,
//! `map` logic, `body_logic` — anywhere JSONLogic runs. They are Orion's
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
#[derive(Clone, Copy)]
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

/// Every operator Orion registers, name first. One list so the two
/// registration paths below (and the vocabulary test) cannot drift.
fn all() -> impl Iterator<Item = (&'static str, OrionOperator)> {
    const CODECS: [(&str, &str, Codec); 3] = [
        ("base64_encode", "base64_decode", Codec::Base64),
        ("base64url_encode", "base64url_decode", Codec::Base64Url),
        ("hex_encode", "hex_decode", Codec::Hex),
    ];
    CODECS.into_iter().flat_map(|(enc, dec, codec)| {
        [
            (enc, OrionOperator::Encode(Encode { name: enc, codec })),
            (dec, OrionOperator::Decode(Decode { name: dec, codec })),
        ]
    })
}

/// The concrete operator behind one registered name.
enum OrionOperator {
    Encode(Encode),
    Decode(Decode),
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

    fn eval(logic: serde_json::Value) -> Result<serde_json::Value, String> {
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

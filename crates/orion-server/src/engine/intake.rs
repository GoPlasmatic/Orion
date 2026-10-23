//! Strict bounded JSON intake for Orion v2 host boundaries (`OR-02`, `DEC-007`).
//!
//! Wraps `datavalue-rs` 0.3.3's single-pass [`DataValue::from_str_with_options`]
//! and [`OwnedDataValue::from_json_with_options`] with Orion's schema-profile
//! resource ceilings and RFC 7493 (I-JSON) safe-integer numeric policy:
//!
//! - Input byte length, container nesting depth, and total JSON value node
//!   count are bounded **before** allocation or scanning exceeds the configured
//!   profile ceiling.
//! - Duplicate decoded object keys (including `\uXXXX`-escaped spellings) are
//!   rejected before parsing the duplicate member's value subtree, preventing
//!   ambiguity between ordered-pair (`DataValue`) and map-backed
//!   (`serde_json::Value`) representations.
//! - Numeric literals are restricted to [`NumberPolicy::SafeIntegers`]:
//!   integers and mathematically integral float/exponent literals must lie in
//!   `[MIN_SAFE_INTEGER, MAX_SAFE_INTEGER]` (`±(2^53 - 1)` = `±9_007_199_254_740_991`),
//!   non-finite floats (`±infinity`) are rejected, and non-zero literals that
//!   underflow binary64 to `±0.0` are rejected.

use std::sync::LazyLock;

use dataflow_rs::datalogic_rs::bumpalo::Bump;
pub use dataflow_rs::datavalue::{
    DataValue, MAX_SAFE_INTEGER, MAX_SUPPORTED_DEPTH, MIN_SAFE_INTEGER, NumberPolicy,
    OwnedDataValue, ParseLimits, ParseOptions, ParseOptionsError, StrictParseError,
    StrictParseErrorKind,
};

/// Default maximum UTF-8 input byte length for untrusted JSON documents (64 KiB),
/// matching `design-docs/v2/schemas/schema-profile.json` (`instance_bytes`).
pub const DEFAULT_MAX_INPUT_BYTES: usize = 65_536;

/// Default maximum container nesting depth for untrusted JSON documents (`64`),
/// matching `design-docs/v2/schemas/schema-profile.json` (`instance_depth`).
pub const DEFAULT_MAX_DEPTH: u16 = 64;

/// Default maximum total JSON value nodes for untrusted JSON documents (`4_096`),
/// matching `design-docs/v2/schemas/schema-profile.json` (`instance_nodes`).
pub const DEFAULT_MAX_NODES: usize = 4_096;

/// Default resource ceilings for strict JSON intake (`65_536` bytes, depth `64`,
/// `4_096` value nodes).
pub const DEFAULT_PARSE_LIMITS: ParseLimits = ParseLimits {
    max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
    max_depth: DEFAULT_MAX_DEPTH,
    max_nodes: DEFAULT_MAX_NODES,
};

/// Pre-validated strict [`ParseOptions`] using [`DEFAULT_PARSE_LIMITS`],
/// duplicate-key rejection, and [`NumberPolicy::SafeIntegers`].
pub static DEFAULT_STRICT_OPTIONS: LazyLock<ParseOptions> = LazyLock::new(|| {
    strict_parse_options(DEFAULT_PARSE_LIMITS)
        .expect("DEFAULT_PARSE_LIMITS is within MAX_SUPPORTED_DEPTH")
});

/// Build strict [`ParseOptions`] for the given resource ceilings with
/// duplicate-key rejection and [`NumberPolicy::SafeIntegers`].
#[inline]
pub fn strict_parse_options(limits: ParseLimits) -> Result<ParseOptions, ParseOptionsError> {
    ParseOptions::strict(limits)
        .number_policy(NumberPolicy::SafeIntegers)
        .allow_duplicate_keys(false)
        .build()
}

/// Parse a JSON document into `arena` under [`DEFAULT_STRICT_OPTIONS`].
///
/// If parsing fails after partial arena allocations, callers reusing `arena`
/// across requests should reset or drop it before the next document.
#[inline]
pub fn parse_strict_arena<'a>(
    input: &'a str,
    arena: &'a Bump,
) -> Result<DataValue<'a>, StrictParseError> {
    DataValue::from_str_with_options(input, arena, &DEFAULT_STRICT_OPTIONS)
}

/// Parse a JSON document into `arena` under caller-supplied [`ParseOptions`].
#[inline]
pub fn parse_strict_arena_with<'a>(
    input: &'a str,
    arena: &'a Bump,
    options: &ParseOptions,
) -> Result<DataValue<'a>, StrictParseError> {
    DataValue::from_str_with_options(input, arena, options)
}

/// Parse a JSON document into an [`OwnedDataValue`] under [`DEFAULT_STRICT_OPTIONS`].
#[inline]
pub fn parse_strict_owned(input: &str) -> Result<OwnedDataValue, StrictParseError> {
    OwnedDataValue::from_json_with_options(input, &DEFAULT_STRICT_OPTIONS)
}

/// Parse a JSON document into an [`OwnedDataValue`] under caller-supplied [`ParseOptions`].
#[inline]
pub fn parse_strict_owned_with(
    input: &str,
    options: &ParseOptions,
) -> Result<OwnedDataValue, StrictParseError> {
    OwnedDataValue::from_json_with_options(input, options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_json_and_safe_integer_boundaries() {
        let arena = Bump::new();
        let max_str = MAX_SAFE_INTEGER.to_string();
        let min_str = MIN_SAFE_INTEGER.to_string();
        let v_max = parse_strict_arena(&max_str, &arena).expect("MAX_SAFE_INTEGER is valid");
        assert_eq!(v_max.as_i64(), Some(MAX_SAFE_INTEGER));

        let v_min = parse_strict_owned(&min_str).expect("MIN_SAFE_INTEGER is valid");
        assert_eq!(v_min.as_i64(), Some(MIN_SAFE_INTEGER));

        let doc = r#"{"ok":true,"n":-0.0,"frac":12.5,"arr":[1,2,3]}"#;
        let parsed = parse_strict_owned(doc).expect("valid document");
        assert_eq!(parsed["ok"].as_bool(), Some(true));
        assert_eq!(parsed["frac"].as_f64(), Some(12.5));
    }

    #[test]
    fn rejects_duplicate_keys_including_escaped_spellings() {
        let arena = Bump::new();
        for input in [r#"{"k":1,"k":2}"#, r#"{"k":1,"\u006b":2}"#] {
            let err = parse_strict_arena(input, &arena).expect_err("duplicate key must fail");
            assert!(
                matches!(err.kind, StrictParseErrorKind::DuplicateObjectKey { .. }),
                "unexpected error for {input}: {err:?}"
            );
        }
    }

    #[test]
    fn rejects_integers_and_integral_floats_outside_rfc7493_safe_range() {
        for input in [
            "9007199254740992",
            "-9007199254740992",
            "9223372036854775807",
            "9223372036854775809",
            "9007199254740993.0",
            "1e20",
        ] {
            let err = parse_strict_owned(input).expect_err(input);
            assert_eq!(
                err.kind,
                StrictParseErrorKind::SafeIntegerRangeExceeded,
                "input {input} should fail with SafeIntegerRangeExceeded"
            );
        }
    }

    #[test]
    fn rejects_nonfinite_overflow_and_nonzero_underflow() {
        let overflow = parse_strict_owned("1e400").expect_err("1e400 overflows f64");
        assert_eq!(overflow.kind, StrictParseErrorKind::NonFiniteNumber);

        let underflow = parse_strict_owned("1e-400").expect_err("1e-400 underflows f64");
        assert_eq!(underflow.kind, StrictParseErrorKind::NumberUnderflow);
    }

    #[test]
    fn enforces_byte_depth_and_node_ceilings() {
        let opts = strict_parse_options(ParseLimits {
            max_input_bytes: 32,
            max_depth: 4,
            max_nodes: 5,
        })
        .expect("valid limits");

        let too_long = format!("\"{}\"", "a".repeat(40));
        let byte_err = parse_strict_owned_with(&too_long, &opts).expect_err("exceeds byte limit");
        assert_eq!(
            byte_err.kind,
            StrictParseErrorKind::InputByteLimitExceeded { limit: 32 }
        );

        let too_deep = "[[[[[0]]]]]";
        let depth_err = parse_strict_owned_with(too_deep, &opts).expect_err("exceeds depth 4");
        assert_eq!(
            depth_err.kind,
            StrictParseErrorKind::DepthLimitExceeded { limit: 4 }
        );

        // Array + 5 elements = 6 value nodes > 5.
        let too_many_nodes = "[1,2,3,4,5]";
        let node_err =
            parse_strict_owned_with(too_many_nodes, &opts).expect_err("exceeds 5 value nodes");
        assert_eq!(
            node_err.kind,
            StrictParseErrorKind::NodeLimitExceeded { limit: 5 }
        );
    }
}

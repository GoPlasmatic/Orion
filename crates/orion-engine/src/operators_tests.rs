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

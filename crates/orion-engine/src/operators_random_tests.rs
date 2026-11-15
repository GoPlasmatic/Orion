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

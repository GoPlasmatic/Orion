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
    let expected_b64 =
        crate::operators::encode_bytes(Codec::Base64, &hex::decode(expected_hex).expect("test"));
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
async fn kdf_admission_enforces_queue_deadline_and_retains_permit_until_blocking_task_finishes() {
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
    let third_ok = spawn_hashing_with_admission(&retry_admission, 8_192, || Ok(Value::Bool(true)))
        .await
        .expect("permit must be released once blocking task finishes");
    assert_eq!(third_ok, Value::Bool(true));
}

use super::*;
use crate::channel::config::ChannelAuthConfig;

fn api_key_config(keys: &[&str]) -> ChannelAuthConfig {
    ChannelAuthConfig {
        mode: AuthMode::ApiKey,
        keys: Some(keys.iter().map(|k| k.to_string()).collect()),
        ..Default::default()
    }
}

/// The process's expression engine, as boot builds it with no secrets and
/// no budget: what `authorization_logic` compiles and evaluates on.
fn dl() -> ExprEngine {
    crate::engine::operators::orion_exprs(&crate::engine::ResolvedSecrets::empty(), 0)
        .expect("Orion's expression engine builds")
}

fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a str> + 'a {
    move |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| *v)
    }
}

#[tokio::test]
async fn api_key_defaults_to_bearer_on_authorization() {
    let auth = CompiledAuth::compile(&api_key_config(&["s3cret"]), None, None)
        .await
        .expect("compiles");
    let headers = [("Authorization", "Bearer s3cret")];
    assert!(
        auth.authenticate(&lookup(&headers), None, &dl())
            .await
            .is_ok()
    );
}

/// The bare key without the scheme prefix is not accepted — otherwise the
/// prefix would be decorative.
#[tokio::test]
async fn api_key_requires_the_scheme_prefix() {
    let auth = CompiledAuth::compile(&api_key_config(&["s3cret"]), None, None)
        .await
        .expect("compiles");
    let headers = [("Authorization", "s3cret")];
    assert!(
        auth.authenticate(&lookup(&headers), None, &dl())
            .await
            .is_err()
    );
}

/// A custom header carries the bare key, with no prefix to strip.
#[tokio::test]
async fn a_custom_header_takes_a_bare_key() {
    let mut cfg = api_key_config(&["s3cret"]);
    cfg.header = Some("X-API-Key".to_string());
    let auth = CompiledAuth::compile(&cfg, None, None)
        .await
        .expect("compiles");
    let headers = [("X-API-Key", "s3cret")];
    assert!(
        auth.authenticate(&lookup(&headers), None, &dl())
            .await
            .is_ok()
    );
}

/// RFC 9110 §11.1 over the header value: a case-insensitive scheme, a run
/// of spaces, and the credential byte-exact after it.
#[test]
fn the_scheme_is_parsed_not_prefix_stripped() {
    use SchemeRefusal::{Mismatch, NoCredential};
    for (value, want) in [
        ("Bearer tok", Ok("tok")),
        ("bearer tok", Ok("tok")),
        ("BEARER tok", Ok("tok")),
        ("Bearer   tok", Ok("tok")),
        // Only the run after the scheme separates; the rest is the
        // credential, as presented.
        ("Bearer a b ", Ok("a b ")),
        ("Bearer<tok>", Err(Mismatch)),
        ("Bearertok", Err(Mismatch)),
        ("Bearerish tok", Err(Mismatch)),
        ("Basic tok", Err(Mismatch)),
        ("tok", Err(Mismatch)),
        ("", Err(Mismatch)),
        ("Bearer", Err(NoCredential)),
        ("Bearer ", Err(NoCredential)),
        ("bearer   ", Err(NoCredential)),
    ] {
        assert_eq!(scheme_credential(value, "Bearer"), want, "{value:?}");
    }
}

/// `"Bearer"` and the old prefix spelling `"Bearer "` are one scheme; an
/// empty value is none; a value that cannot be a name is refused by field.
#[test]
fn a_configured_scheme_is_normalised_to_its_name() {
    for raw in ["Bearer", "Bearer ", " Bearer  "] {
        assert_eq!(
            scheme_name("auth.scheme", raw),
            Ok(Some("Bearer".to_string())),
            "{raw:?}"
        );
    }
    assert_eq!(scheme_name("auth.scheme", ""), Ok(None));
    assert_eq!(scheme_name("auth.scheme", "  "), Ok(None));
    for raw in ["Key=", "Bearer:", "Two Words", "sha256="] {
        let err = scheme_name("auth.source.scheme", raw).expect_err(raw);
        assert!(err.starts_with("auth.source.scheme"), "{err}");
        assert!(err.contains(raw), "{err}");
    }
}

/// The #331 matrix on an `api_key` channel: conforming spellings admit,
/// whichever way the scheme was configured.
#[tokio::test]
async fn api_key_accepts_every_conforming_bearer_spelling() {
    for scheme in [None, Some("Bearer"), Some("Bearer "), Some("bearer")] {
        let mut cfg = api_key_config(&["s3cret"]);
        cfg.scheme = scheme.map(str::to_string);
        let auth = CompiledAuth::compile(&cfg, None, None)
            .await
            .expect("compiles");
        for value in [
            "Bearer s3cret",
            "bearer s3cret",
            "BEARER s3cret",
            "Bearer  s3cret",
        ] {
            assert!(
                auth.authenticate(&lookup(&[("Authorization", value)]), None, &dl())
                    .await
                    .is_ok(),
                "scheme {scheme:?}, header {value:?}"
            );
        }
        assert!(
            auth.authenticate(&lookup(&[("Authorization", "Bearers3cret")]), None, &dl())
                .await
                .is_err(),
            "scheme {scheme:?}: no separator is not the scheme"
        );
    }
}

/// An empty `scheme` on `Authorization` is no scheme — the bare key, which
/// is what it meant while the field was a prefix.
#[tokio::test]
async fn an_empty_scheme_takes_the_bare_key_on_authorization() {
    let mut cfg = api_key_config(&["s3cret"]);
    cfg.scheme = Some(String::new());
    let auth = CompiledAuth::compile(&cfg, None, None)
        .await
        .expect("compiles");
    assert!(
        auth.authenticate(&lookup(&[("Authorization", "s3cret")]), None, &dl())
            .await
            .is_ok()
    );
}

/// A scheme that can never match a name is refused by the structural
/// check (create, validate, lint) *and* at compile, so a stored one
/// quarantines its channel instead of refusing every caller silently.
#[tokio::test]
async fn a_scheme_that_cannot_be_a_name_is_refused() {
    let mut api_key = api_key_config(&["s3cret"]);
    api_key.scheme = Some("Key=".to_string());
    let mut jwt = jwt_config(&["HS512"], HS_SECRET);
    jwt.source = Some(JwtSource::Header {
        header: "Authorization".to_string(),
        scheme: Some("Bearer:".to_string()),
    });
    for (cfg, field) in [(api_key, "auth.scheme"), (jwt, "auth.source.scheme")] {
        let err = CompiledAuth::validate_config(&cfg).expect_err("structural");
        assert!(err.starts_with(field), "{err}");
        let err = CompiledAuth::compile(&cfg, Some(&dl()), None)
            .await
            .expect_err("compile");
        assert!(err.starts_with(field), "{err}");
    }
}

#[tokio::test]
async fn a_wrong_or_missing_key_is_refused() {
    let auth = CompiledAuth::compile(&api_key_config(&["s3cret"]), None, None)
        .await
        .expect("compiles");
    assert!(
        auth.authenticate(&lookup(&[("Authorization", "Bearer nope")]), None, &dl())
            .await
            .is_err()
    );
    assert!(auth.authenticate(&lookup(&[]), None, &dl()).await.is_err());
}

/// Several keys are accepted at once, which is what makes rotation
/// possible without a window of refusals.
#[tokio::test]
async fn any_configured_key_is_accepted() {
    let auth = CompiledAuth::compile(&api_key_config(&["old", "new"]), None, None)
        .await
        .expect("compiles");
    for key in ["old", "new"] {
        let headers = [("Authorization", format!("Bearer {key}"))];
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert!(
            auth.authenticate(&lookup(&pairs), None, &dl())
                .await
                .is_ok(),
            "{key}"
        );
    }
}

#[tokio::test]
async fn api_key_mode_requires_keys() {
    let mut cfg = api_key_config(&[]);
    cfg.keys = None;
    assert!(CompiledAuth::compile(&cfg, None, None).await.is_err());
    let empty = api_key_config(&[]);
    assert!(CompiledAuth::compile(&empty, None, None).await.is_err());
}

fn hmac_config(secret: &str, prefix: Option<&str>) -> ChannelAuthConfig {
    ChannelAuthConfig {
        mode: AuthMode::Hmac,
        header: Some("X-Signature".to_string()),
        secret: Some(secret.to_string()),
        signature_prefix: prefix.map(str::to_string),
        ..Default::default()
    }
}

fn sign_hex(secret: &str, body: &[u8]) -> String {
    hex::encode(sign::<Hmac<Sha256>>(secret.as_bytes(), body))
}

#[tokio::test]
async fn hmac_accepts_a_correct_hex_signature() {
    let auth = CompiledAuth::compile(&hmac_config("whsec", None), None, None)
        .await
        .expect("compiles");
    let body = br#"{"id":"evt_1","amount":2000}"#;
    let headers = [("X-Signature", sign_hex("whsec", body))];
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert!(
        auth.authenticate(&lookup(&pairs), Some(body), &dl())
            .await
            .is_ok()
    );
}

/// The GitHub spelling: `sha256=<hex>`.
#[tokio::test]
async fn hmac_strips_a_configured_signature_prefix() {
    let auth = CompiledAuth::compile(&hmac_config("whsec", Some("sha256=")), None, None)
        .await
        .expect("compiles");
    let body = br#"{"action":"opened"}"#;
    let headers = [("X-Signature", format!("sha256={}", sign_hex("whsec", body)))];
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert!(
        auth.authenticate(&lookup(&pairs), Some(body), &dl())
            .await
            .is_ok()
    );
}

/// The Shopify spelling: base64.
#[tokio::test]
async fn hmac_accepts_a_base64_signature() {
    let auth = CompiledAuth::compile(&hmac_config("whsec", None), None, None)
        .await
        .expect("compiles");
    let body = br#"{"order":1}"#;
    use base64::Engine;
    let sig =
        base64::engine::general_purpose::STANDARD.encode(sign::<Hmac<Sha256>>(b"whsec", body));
    let headers = [("X-Signature", sig)];
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert!(
        auth.authenticate(&lookup(&pairs), Some(body), &dl())
            .await
            .is_ok()
    );
}

/// The same webhook must not get two answers depending on whether
/// `encoding` happens to be set.
///
/// A SHA-256 MAC is 32 bytes, which base64 encodes to 44 characters with
/// one `=` of padding — so an unpadded sender is a real shape. The pinned
/// arm decodes through `B64_STD_LENIENT` (padding `Indifferent`) while the
/// auto-detect arm hand-rolled `general_purpose::STANDARD`, which requires
/// canonical padding: the unpadded signature verified with
/// `encoding = "base64"` configured and was refused as malformed without
/// it, on a field an operator may reasonably leave unset.
#[tokio::test]
async fn hmac_auto_detection_accepts_unpadded_base64_like_the_pinned_path() {
    use base64::Engine;
    let body = br#"{"order":1}"#;
    let padded =
        base64::engine::general_purpose::STANDARD.encode(sign::<Hmac<Sha256>>(b"whsec", body));
    let unpadded = padded.trim_end_matches('=').to_string();
    assert_ne!(padded, unpadded, "a SHA-256 MAC does carry padding");

    for (label, encoding) in [("auto-detected", None), ("pinned", Some("base64"))] {
        let mut cfg = hmac_config("whsec", None);
        cfg.encoding = encoding.map(str::to_string);
        let auth = CompiledAuth::compile(&cfg, None, None)
            .await
            .expect("compiles");

        for (shape, sig) in [("padded", &padded), ("unpadded", &unpadded)] {
            let headers = [("X-Signature", sig.clone())];
            let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            assert!(
                auth.authenticate(&lookup(&pairs), Some(body), &dl())
                    .await
                    .is_ok(),
                "{label} / {shape} must verify"
            );
        }
    }
}

/// Widening the decode cannot widen what verifies: the bytes still have to
/// match the MAC.
#[tokio::test]
async fn a_well_formed_but_wrong_base64_signature_is_still_refused() {
    use base64::Engine;
    let auth = CompiledAuth::compile(&hmac_config("whsec", None), None, None)
        .await
        .expect("compiles");
    let body = br#"{"order":1}"#;
    let wrong = base64::engine::general_purpose::STANDARD
        .encode(sign::<Hmac<Sha256>>(b"not-the-secret", body));
    let unpadded = wrong.trim_end_matches('=').to_string();
    let headers = [("X-Signature", unpadded)];
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert!(
        auth.authenticate(&lookup(&pairs), Some(body), &dl())
            .await
            .is_err()
    );
}

/// The property the whole mode exists for: one byte changed in the body
/// invalidates the signature.
#[tokio::test]
async fn hmac_refuses_a_tampered_body() {
    let auth = CompiledAuth::compile(&hmac_config("whsec", None), None, None)
        .await
        .expect("compiles");
    let signed = br#"{"amount":2000}"#;
    let tampered = br#"{"amount":9999}"#;
    let headers = [("X-Signature", sign_hex("whsec", signed))];
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert!(
        auth.authenticate(&lookup(&pairs), Some(tampered), &dl())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn hmac_refuses_a_signature_from_the_wrong_secret() {
    let auth = CompiledAuth::compile(&hmac_config("whsec", None), None, None)
        .await
        .expect("compiles");
    let body = br#"{"a":1}"#;
    let headers = [("X-Signature", sign_hex("attacker", body))];
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert!(
        auth.authenticate(&lookup(&pairs), Some(body), &dl())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn hmac_refuses_a_malformed_or_absent_signature() {
    let auth = CompiledAuth::compile(&hmac_config("whsec", None), None, None)
        .await
        .expect("compiles");
    let body = br#"{"a":1}"#;
    assert!(
        auth.authenticate(
            &lookup(&[("X-Signature", "not-a-signature!")]),
            Some(body),
            &dl()
        )
        .await
        .is_err()
    );
    assert!(
        auth.authenticate(&lookup(&[]), Some(body), &dl())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn hmac_mode_requires_a_secret() {
    let mut cfg = hmac_config("x", None);
    cfg.secret = None;
    assert!(CompiledAuth::compile(&cfg, None, None).await.is_err());
}

/// Every refusal reads the same, so a caller cannot learn which half of the
/// credential was right by comparing messages.
#[tokio::test]
async fn every_refusal_carries_the_same_message() {
    let auth = CompiledAuth::compile(&api_key_config(&["s3cret"]), None, None)
        .await
        .expect("compiles");
    let missing = auth
        .authenticate(&lookup(&[]), None, &dl())
        .await
        .expect_err("no header is a refusal");
    let wrong = auth
        .authenticate(&lookup(&[("Authorization", "Bearer nope")]), None, &dl())
        .await
        .expect_err("a wrong key is a refusal");
    assert_eq!(missing.to_string(), wrong.to_string());
}
// -- #264: templates, presets, rotation, replay windows --

use crate::crypto::mac_compute as sign;

fn preset_config(preset: &str, secret: &str) -> ChannelAuthConfig {
    ChannelAuthConfig {
        mode: AuthMode::Hmac,
        preset: Some(preset.to_string()),
        secret: Some(secret.to_string()),
        ..Default::default()
    }
}

async fn compiled_hmac(cfg: &ChannelAuthConfig) -> CompiledHmac {
    match CompiledAuth::compile(cfg, None, None)
        .await
        .expect("compiles")
    {
        CompiledAuth::Hmac(h) => h,
        CompiledAuth::ApiKey { .. } | CompiledAuth::Jwt(_) => {
            unreachable!("an hmac config compiles to Hmac")
        }
    }
}

#[tokio::test]
async fn zoom_preset_verifies_the_timestamped_template() {
    let auth = compiled_hmac(&preset_config("zoom", "zsecret")).await;
    let ts = 1_700_000_000i64;
    let body = br#"{"event":"meeting.started"}"#;
    let message = format!("v0:{ts}:{}", std::str::from_utf8(body).expect("test"));
    let sig = hex::encode(sign::<Hmac<Sha256>>(b"zsecret", message.as_bytes()));
    let ts_header = ts.to_string();
    let sig_header = format!("v0={sig}");
    let headers = [
        ("x-zm-signature", sig_header.as_str()),
        ("x-zm-request-timestamp", ts_header.as_str()),
    ];

    // Inside the window: accepted. Outside it (301 s later): refused,
    // with the same message as a bad MAC.
    assert!(
        auth.authenticate_at(&lookup(&headers), Some(body), ts + 10)
            .is_ok()
    );
    assert!(
        auth.authenticate_at(&lookup(&headers), Some(body), ts + 301)
            .is_err()
    );
    assert!(
        auth.authenticate_at(&lookup(&headers), Some(body), ts - 301)
            .is_err()
    );

    // Dropping the timestamp header must refuse, never sign "v0::body".
    let no_ts = [("x-zm-signature", sig_header.as_str())];
    assert!(
        auth.authenticate_at(&lookup(&no_ts), Some(body), ts)
            .is_err()
    );
}

#[tokio::test]
async fn stripe_preset_parses_the_packed_header() {
    let auth = compiled_hmac(&preset_config("stripe", "whsec_abc")).await;
    let ts = 1_700_000_000i64;
    let body = br#"{"id":"evt_1"}"#;
    let message = format!("{ts}.{}", std::str::from_utf8(body).expect("test"));
    let sig = hex::encode(sign::<Hmac<Sha256>>(b"whsec_abc", message.as_bytes()));

    // The second v1 is the valid one — Stripe sends two during provider-
    // side rotation, and every occurrence must be tried.
    let packed = format!("t={ts},v1={},v1={sig}", "0".repeat(64));
    let headers = [("stripe-signature", packed.as_str())];
    assert!(
        auth.authenticate_at(&lookup(&headers), Some(body), ts + 1)
            .is_ok()
    );

    // Only wrong signatures: refused.
    let bad = format!("t={ts},v1={}", "0".repeat(64));
    let headers = [("stripe-signature", bad.as_str())];
    assert!(
        auth.authenticate_at(&lookup(&headers), Some(body), ts + 1)
            .is_err()
    );
}

#[tokio::test]
async fn shopify_and_webex_presets_use_their_encodings_and_algorithms() {
    use base64::Engine as _;
    let body = br#"{"order":1}"#;

    // Shopify: SHA-256, base64 — and hex must NOT be accepted, because
    // the preset pins the encoding.
    let auth = CompiledAuth::compile(&preset_config("shopify", "shpss"), None, None)
        .await
        .expect("compiles");
    let raw = sign::<Hmac<Sha256>>(b"shpss", body);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
    assert!(
        auth.authenticate(
            &lookup(&[("x-shopify-hmac-sha256", b64.as_str())]),
            Some(body),
            &dl(),
        )
        .await
        .is_ok()
    );
    let hexed = hex::encode(&raw);
    assert!(
        auth.authenticate(
            &lookup(&[("x-shopify-hmac-sha256", hexed.as_str())]),
            Some(body),
            &dl(),
        )
        .await
        .is_err()
    );

    // Webex: SHA-1 over the raw body.
    let auth = CompiledAuth::compile(&preset_config("webex", "wxsecret"), None, None)
        .await
        .expect("compiles");
    let sha1_hex = hex::encode(sign::<Hmac<Sha1>>(b"wxsecret", body));
    assert!(
        auth.authenticate(
            &lookup(&[("x-spark-signature", sha1_hex.as_str())]),
            Some(body),
            &dl(),
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn github_preset_matches_the_documented_spelling() {
    let auth = CompiledAuth::compile(&preset_config("github", "ghs"), None, None)
        .await
        .expect("compiles");
    let body = br#"{"action":"opened"}"#;
    let sig = format!("sha256={}", hex::encode(sign::<Hmac<Sha256>>(b"ghs", body)));
    assert!(
        auth.authenticate(
            &lookup(&[("x-hub-signature-256", sig.as_str())]),
            Some(body),
            &dl(),
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn secret_rotation_accepts_any_listed_secret() {
    let cfg = ChannelAuthConfig {
        mode: AuthMode::Hmac,
        secret: Some("old".to_string()),
        secrets: Some(vec!["new".to_string()]),
        ..Default::default()
    };
    let auth = CompiledAuth::compile(&cfg, None, None)
        .await
        .expect("compiles");
    let body = b"payload";
    for secret in [b"old".as_slice(), b"new".as_slice()] {
        let sig = hex::encode(sign::<Hmac<Sha256>>(secret, body));
        assert!(
            auth.authenticate(&lookup(&[("X-Signature", sig.as_str())]), Some(body), &dl())
                .await
                .is_ok()
        );
    }
}

#[tokio::test]
async fn explicit_fields_override_their_preset_rows() {
    // Slack's window tightened from 300 to 60 without restating the rest.
    let mut cfg = preset_config("slack", "ssecret");
    cfg.tolerance_secs = Some(60);
    let auth = compiled_hmac(&cfg).await;
    let ts = 1_700_000_000i64;
    let body = b"{}";
    let message = format!("v0:{ts}:{}", std::str::from_utf8(body).expect("test"));
    let sig_header = format!(
        "v0={}",
        hex::encode(sign::<Hmac<Sha256>>(b"ssecret", message.as_bytes()))
    );
    let ts_header = ts.to_string();
    let headers = [
        ("x-slack-signature", sig_header.as_str()),
        ("x-slack-request-timestamp", ts_header.as_str()),
    ];
    assert!(
        auth.authenticate_at(&lookup(&headers), Some(body), ts + 59)
            .is_ok()
    );
    assert!(
        auth.authenticate_at(&lookup(&headers), Some(body), ts + 61)
            .is_err()
    );
}

/// The structural rules, exactly the ones create/validate enforce (#264's
/// activation-time fix). Every case would previously have been accepted
/// and quarantined at reload.
#[test]
fn structural_mistakes_are_named() {
    let base = || ChannelAuthConfig {
        mode: AuthMode::Hmac,
        secret: Some("s".to_string()),
        ..Default::default()
    };
    for (mutate, expected) in [
        (
            Box::new(|c: &mut ChannelAuthConfig| c.preset = Some("gitlab".to_string()))
                as Box<dyn Fn(&mut ChannelAuthConfig)>,
            "preset 'gitlab'",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| {
                c.signature_prefix = Some("v0=".to_string());
                c.signature_key = Some("v1".to_string());
            }),
            "mutually exclusive",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| c.tolerance_secs = Some(300)),
            "requires auth.timestamp",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| {
                c.timestamp = Some("x-ts".to_string());
            }),
            "requires auth.tolerance_secs",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| c.algorithm = Some("md5".to_string())),
            "algorithm 'md5'",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| c.encoding = Some("base32".to_string())),
            "encoding 'base32'",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| c.message = Some("v0:{ts}:{body}".to_string())),
            "placeholder '{ts}'",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| c.message = Some("{header:x}".to_string())),
            "must contain {body}",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| c.message = Some("v0:{body".to_string())),
            "unterminated",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| c.message = Some("v0}:{body}".to_string())),
            "stray '}'",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| c.secret = None),
            "requires auth.secret",
        ),
    ] {
        let mut cfg = base();
        mutate(&mut cfg);
        let err = CompiledAuth::validate_config(&cfg).expect_err("should be refused");
        assert!(err.contains(expected), "expected '{expected}' in: {err}");
    }

    // And the structural check never resolves secrets: an unset env://
    // reference passes validation (a bundle validates on hosts without
    // production secrets) even though compile would fail there.
    let mut cfg = base();
    cfg.secret = Some("env://UNSET_VAR_FOR_264_TEST".to_string());
    assert!(CompiledAuth::validate_config(&cfg).is_ok());

    // api_key structure is covered too — the pre-#264 quarantine case.
    let no_keys = ChannelAuthConfig {
        mode: AuthMode::ApiKey,
        ..Default::default()
    };
    assert!(CompiledAuth::validate_config(&no_keys).is_err());
}
// -- #267: the jwt mode --

fn jwt_config(algorithms: &[&str], secret: &str) -> ChannelAuthConfig {
    ChannelAuthConfig {
        mode: AuthMode::Jwt,
        algorithms: Some(algorithms.iter().map(|a| a.to_string()).collect()),
        jwt_keys: Some(vec![crate::channel::config::JwtKeyEntry {
            algorithm: algorithms[0].to_string(),
            key: secret.to_string(),
            kid: None,
            key_encoding: None,
        }]),
        ..Default::default()
    }
}

/// A 64-byte secret satisfying every HS variant's RFC 7518 floor.
const HS_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn mint(alg: jsonwebtoken::Algorithm, secret: &str, claims: serde_json::Value) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(alg),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .expect("test")
}

fn fresh_claims() -> serde_json::Value {
    serde_json::json!({
        "sub": "user-1",
        "roles": ["teacher"],
        "exp": chrono::Utc::now().timestamp() + 3600,
    })
}

fn bearer(token: &str) -> Vec<(&'static str, String)> {
    vec![("Authorization", format!("Bearer {token}"))]
}

async fn jwt_auth(cfg: &ChannelAuthConfig) -> CompiledAuth {
    CompiledAuth::compile(cfg, Some(&dl()), None)
        .await
        .expect("compiles")
}

#[tokio::test]
async fn jwt_verifies_and_exposes_claims() {
    let auth = jwt_auth(&jwt_config(&["HS512"], HS_SECRET)).await;
    let token = mint(jsonwebtoken::Algorithm::HS512, HS_SECRET, fresh_claims());
    let headers = bearer(&token);
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let outcome = auth
        .authenticate(&lookup(&pairs), None, &dl())
        .await
        .expect("verifies");
    let claims = outcome.claims.expect("claims exposed");
    assert_eq!(claims["sub"], "user-1");
    assert_eq!(claims["roles"][0], "teacher");
}

#[tokio::test]
async fn jwt_claims_filter_narrows_what_workflows_see() {
    let mut cfg = jwt_config(&["HS512"], HS_SECRET);
    cfg.claims_to_metadata = Some(vec!["sub".to_string()]);
    let auth = jwt_auth(&cfg).await;
    let token = mint(jsonwebtoken::Algorithm::HS512, HS_SECRET, fresh_claims());
    let headers = bearer(&token);
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let claims = auth
        .authenticate(&lookup(&pairs), None, &dl())
        .await
        .expect("verifies")
        .claims
        .expect("claims");
    assert_eq!(claims, serde_json::json!({"sub": "user-1"}));
}

/// The RFC 8725 rejections: `alg: none`, an allowlisted-out algorithm,
/// expiry (the one reason named on the wire), and identity mismatches.
#[tokio::test]
async fn jwt_rejections_are_typed_and_mostly_uniform() {
    let auth = jwt_auth(&jwt_config(&["HS512"], HS_SECRET)).await;
    let dl_engine = dl();

    // alg: none — hand-built, since no sane library signs it.
    use base64::Engine as _;
    let b64 = |v: &serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
    };
    let none_token = format!(
        "{}.{}.",
        b64(&serde_json::json!({"alg": "none", "typ": "JWT"})),
        b64(&fresh_claims()),
    );
    // HS256-signed against an HS512-only allowlist: a downgrade, refused
    // before any key is consulted.
    let downgraded = mint(jsonwebtoken::Algorithm::HS256, HS_SECRET, fresh_claims());
    // Wrong key.
    let forged = mint(
        jsonwebtoken::Algorithm::HS512,
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        fresh_claims(),
    );
    let mut expired_claims = fresh_claims();
    expired_claims["exp"] = serde_json::json!(chrono::Utc::now().timestamp() - 3600);
    let expired = mint(jsonwebtoken::Algorithm::HS512, HS_SECRET, expired_claims);
    // No exp at all (require_exp default).
    let unexpiring = mint(
        jsonwebtoken::Algorithm::HS512,
        HS_SECRET,
        serde_json::json!({"sub": "u"}),
    );

    for (token, expect_expired_hint) in [
        (none_token, false),
        (downgraded, false),
        (forged, false),
        (expired, true),
        (unexpiring, false),
        ("not-a-jwt".to_string(), false),
    ] {
        let headers = bearer(&token);
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let err = auth
            .authenticate(&lookup(&pairs), None, &dl_engine)
            .await
            .expect_err("must refuse");
        match err {
            OrionError::UnauthorizedToken { message, challenge } => {
                assert_eq!(message, "Channel authentication failed");
                assert_eq!(
                    challenge,
                    crate::errors::BearerChallenge::InvalidToken(
                        expect_expired_hint.then_some("token expired")
                    ),
                    "only expiry is named on the wire"
                );
            }
            other => unreachable!("expected UnauthorizedToken, got {other:?}"),
        }
    }

    // No bearer credential at all — no header, another scheme, or the
    // scheme glued to the token — is RFC 6750 §3.1's case: the bare
    // challenge, with no error code. The body is the same as above.
    for headers in [
        vec![],
        vec![("Authorization", "Basic dXNlcjpwYXNz".to_string())],
        vec![("Authorization", "Bearerx.y.z".to_string())],
    ] {
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        match auth.authenticate(&lookup(&pairs), None, &dl_engine).await {
            Err(OrionError::UnauthorizedToken { message, challenge }) => {
                assert_eq!(message, "Channel authentication failed");
                assert_eq!(challenge, crate::errors::BearerChallenge::Bare, "{pairs:?}");
            }
            other => unreachable!("expected UnauthorizedToken, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn jwt_issuer_and_audience_are_enforced_when_configured() {
    let mut cfg = jwt_config(&["HS512"], HS_SECRET);
    cfg.issuer = Some(crate::channel::config::StringOrVec::One(
        "good-iss".to_string(),
    ));
    cfg.audience = Some(crate::channel::config::StringOrVec::Many(vec![
        "app".to_string(),
    ]));
    let auth = jwt_auth(&cfg).await;
    let dl_engine = dl();

    let mut ok = fresh_claims();
    ok["iss"] = serde_json::json!("good-iss");
    ok["aud"] = serde_json::json!("app");
    let token = mint(jsonwebtoken::Algorithm::HS512, HS_SECRET, ok.clone());
    let headers = bearer(&token);
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert!(
        auth.authenticate(&lookup(&pairs), None, &dl_engine)
            .await
            .is_ok()
    );

    let mut bad = ok;
    bad["iss"] = serde_json::json!("evil-iss");
    let token = mint(jsonwebtoken::Algorithm::HS512, HS_SECRET, bad);
    let headers = bearer(&token);
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert!(
        auth.authenticate(&lookup(&pairs), None, &dl_engine)
            .await
            .is_err()
    );
}

/// Optional auth: a MISSING token proceeds identity-less; an INVALID one
/// is still rejected — Envoy's allow_missing, never allow_missing_or_failed.
#[tokio::test]
async fn jwt_optional_admits_missing_but_not_invalid() {
    let mut cfg = jwt_config(&["HS512"], HS_SECRET);
    cfg.required = Some(false);
    let auth = jwt_auth(&cfg).await;
    let dl_engine = dl();

    let outcome = auth
        .authenticate(&lookup(&[]), None, &dl_engine)
        .await
        .expect("token-less request proceeds");
    assert!(outcome.claims.is_none(), "no identity, no claims key");

    let headers = bearer("garbage.token.here");
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert!(
        auth.authenticate(&lookup(&pairs), None, &dl_engine)
            .await
            .is_err()
    );
}

/// Verified identity with insufficient rights answers 403, not 401 —
/// authorization_logic over the claims.
#[tokio::test]
async fn jwt_authorization_logic_answers_403() {
    let mut cfg = jwt_config(&["HS512"], HS_SECRET);
    cfg.authorization_logic = Some(serde_json::json!({"in": ["admin", {"var": "claims.roles"}]}));
    let auth = jwt_auth(&cfg).await;
    let dl_engine = dl();

    // roles = ["teacher"]: verified, not authorized.
    let token = mint(jsonwebtoken::Algorithm::HS512, HS_SECRET, fresh_claims());
    let headers = bearer(&token);
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    match auth.authenticate(&lookup(&pairs), None, &dl_engine).await {
        Err(OrionError::Http {
            status: orion_common::errors::StatusCode::FORBIDDEN,
            message: msg,
            ..
        }) => assert!(msg.contains("insufficient_scope"), "{msg}"),
        other => unreachable!("expected Forbidden, got {other:?}"),
    }

    let mut admin = fresh_claims();
    admin["roles"] = serde_json::json!(["admin"]);
    let token = mint(jsonwebtoken::Algorithm::HS512, HS_SECRET, admin.clone());
    let headers = bearer(&token);
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert!(
        auth.authenticate(&lookup(&pairs), None, &dl_engine)
            .await
            .is_ok()
    );

    // An authorization expression that errors at evaluation fails
    // CLOSED: 403, never a silent allow. The error is a decoder refusing
    // its input: the guards' engine runs in templating mode, where an
    // unknown operator is not an error but a one-key object (the case
    // used to name one, on a bare engine that is not the guards').
    let mut cfg = jwt_config(&["HS512"], HS_SECRET);
    cfg.authorization_logic = Some(serde_json::json!({"hex_decode": ["zz"]}));
    let auth = jwt_auth(&cfg).await;
    let token = mint(jsonwebtoken::Algorithm::HS512, HS_SECRET, admin);
    let headers = bearer(&token);
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert!(matches!(
        auth.authenticate(&lookup(&pairs), None, &dl_engine).await,
        Err(OrionError::Http {
            status: orion_common::errors::StatusCode::FORBIDDEN,
            ..
        })
    ));
}

/// The #331 matrix on a `jwt` channel, including its reproduction: a
/// source configured as `"scheme": "Bearer"` — no trailing space — used to
/// refuse every caller.
#[tokio::test]
async fn jwt_accepts_every_conforming_bearer_spelling() {
    let token = mint(jsonwebtoken::Algorithm::HS512, HS_SECRET, fresh_claims());
    for scheme in [None, Some("Bearer"), Some("Bearer ")] {
        let mut cfg = jwt_config(&["HS512"], HS_SECRET);
        if let Some(scheme) = scheme {
            cfg.source = Some(JwtSource::Header {
                header: "Authorization".to_string(),
                scheme: Some(scheme.to_string()),
            });
        }
        let auth = jwt_auth(&cfg).await;
        for spelling in ["Bearer", "bearer", "BEARER", "Bearer "] {
            let value = format!("{spelling} {token}");
            assert!(
                auth.authenticate(&lookup(&[("Authorization", value.as_str())]), None, &dl())
                    .await
                    .is_ok(),
                "scheme {scheme:?}, header {spelling:?} <token>"
            );
        }
    }
}

/// `required: false` admits a request with no header — not one whose
/// header carries another scheme, which is a bad presentation.
#[tokio::test]
async fn optional_jwt_still_refuses_a_foreign_scheme() {
    let mut cfg = jwt_config(&["HS512"], HS_SECRET);
    cfg.required = Some(false);
    let auth = jwt_auth(&cfg).await;
    assert!(
        auth.authenticate(&lookup(&[("Authorization", "Basic dXNlcg==")]), None, &dl())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn jwt_cookie_source_extracts_from_the_jar() {
    let mut cfg = jwt_config(&["HS512"], HS_SECRET);
    cfg.source = Some(JwtSource::Cookie {
        cookie: "session".to_string(),
    });
    let auth = jwt_auth(&cfg).await;
    let token = mint(jsonwebtoken::Algorithm::HS512, HS_SECRET, fresh_claims());
    let jar = format!("theme=dark; session={token}; lang=en");
    let headers = [("Cookie", jar.as_str())];
    assert!(
        auth.authenticate(&lookup(&headers), None, &dl())
            .await
            .is_ok()
    );
}

/// The activation-time matrix — every case a 400 at create, never a
/// reload-time quarantine (the #264 path, extended).
#[test]
fn jwt_structural_mistakes_are_named() {
    let base = || jwt_config(&["HS512"], HS_SECRET);
    for (mutate, expected) in [
        (
            Box::new(|c: &mut ChannelAuthConfig| c.algorithms = None)
                as Box<dyn Fn(&mut ChannelAuthConfig)>,
            "non-empty auth.algorithms",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| c.algorithms = Some(vec!["ES512".to_string()])),
            "ES512",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| c.jwt_keys = None),
            "auth.jwt_keys and/or auth.jwks_url",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| {
                c.jwks_url = Some("http://issuer.example.com/jwks".to_string())
            }),
            "HTTPS",
        ),
        (
            Box::new(|c: &mut ChannelAuthConfig| c.leeway_secs = Some(3600)),
            "leeway_secs",
        ),
        // Note: an *unknown operator* in authorization_logic compiles —
        // on the guards' engine, which runs in templating mode, it is a
        // one-key object — so it is not a structural case. An expression
        // that errors at request time fails CLOSED, pinned in
        // `jwt_authorization_logic_answers_403`.
    ] {
        let mut cfg = base();
        mutate(&mut cfg);
        let err = CompiledAuth::validate_config(&cfg).expect_err("must refuse");
        assert!(err.contains(expected), "expected '{expected}' in: {err}");
    }
}

/// The RFC 7518 §3.2 floor: an HS secret shorter than the hash length is
/// refused at compile, not silently accepted as a weak MAC.
#[tokio::test]
async fn jwt_short_hs_secret_is_refused() {
    let cfg = jwt_config(&["HS512"], "much-too-short");
    let err = CompiledAuth::compile(&cfg, Some(&dl()), None)
        .await
        .expect_err("test");
    assert!(err.contains("RFC 7518"), "{err}");
}

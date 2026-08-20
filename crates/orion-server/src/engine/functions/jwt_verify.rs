//! `jwt_verify` — verify a third-party (or own) JWS mid-workflow (#267
//! Part B): provider id_tokens for social login, refresh tokens, partner
//! assertions. The same shared core and JWKS cache as the channel mode, with
//! task-error semantics so workflows can branch on failure.

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::connector_helpers::{apply_output, resolve_required_str};
use super::schema::{FieldKind, FieldSchema};

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "jwt_verify";

/// Workflow function handler that verifies JWTs.
pub struct JwtVerifyHandler;

#[async_trait]
impl AsyncFunctionHandler for JwtVerifyHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // Literal prologue (F58).
        let algorithms = algorithms(input)?;
        let output = input
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("data");

        let token = resolve_required_str(input, "token", NAME, ctx)?;

        let mut static_keys = Vec::new();
        for (index, entry) in input
            .get("keys")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let field = |name: &str| entry.get(name).and_then(Value::as_str);
            let Some(algorithm) = field("algorithm") else {
                return Err(validation(&format!(
                    "'keys[{index}].algorithm' is required"
                )));
            };
            let algorithm = crate::jwt::parse_algorithm(algorithm).map_err(|e| validation(&e))?;
            let Some(key_ref) = field("key") else {
                return Err(validation(&format!("'keys[{index}].key' is required")));
            };
            let material =
                crate::connector::secrets::resolve_secret_string(key_ref, "jwt_verify.keys")
                    .await
                    .map_err(|e| validation(&e))?;
            let key = crate::jwt::decoding_key(algorithm, &material, field("key_encoding"))
                .map_err(|e| validation(&format!("'keys[{index}]': {e}")))?;
            static_keys.push(crate::jwt::StaticKey {
                kid: field("kid").map(str::to_string),
                algorithm,
                key,
            });
        }

        let jwks_url = match input.get("jwks_url").and_then(Value::as_str) {
            None => None,
            Some(url) => {
                crate::jwt::validate_jwks_url(url)
                    .map_err(|e| validation(&format!("'jwks_url' {e}")))?;
                Some(url.to_string())
            }
        };
        if static_keys.is_empty() && jwks_url.is_none() {
            return Err(validation("requires 'keys' and/or 'jwks_url'"));
        }

        let verifier = crate::jwt::Verifier {
            static_keys,
            jwks_url,
            algorithms,
            issuer: string_or_vec(input, "issuer", ctx).await?,
            audience: string_or_vec(input, "audience", ctx).await?,
            leeway_secs: input
                .get("leeway_secs")
                .and_then(Value::as_u64)
                .unwrap_or(crate::jwt::DEFAULT_LEEWAY_SECS)
                .min(crate::jwt::MAX_LEEWAY_SECS),
            require_exp: input
                .get("require_exp")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            max_token_bytes: crate::jwt::DEFAULT_MAX_TOKEN_BYTES,
            validations: std::sync::OnceLock::new(),
        };

        let claims = verifier.verify(&token).await.map_err(|reason| {
            // A typed, branchable task error — the reason, never the token.
            DataflowError::function_execution(
                format!("{NAME}: token rejected ({})", reason.as_str()),
                None,
            )
        })?;

        apply_output(ctx, output, claims);
        Ok(TaskOutcome::Success)
    }
}

fn validation(msg: &str) -> DataflowError {
    DataflowError::Validation(format!("{NAME}: {msg}"))
}

fn algorithms(input: &Value) -> Result<Vec<jsonwebtoken::Algorithm>, DataflowError> {
    let names = input
        .get("algorithms")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| validation("requires a non-empty 'algorithms' allowlist"))?;
    let mut algorithms = Vec::with_capacity(names.len());
    for name in names {
        let Some(name) = name.as_str() else {
            return Err(validation("'algorithms' entries must be strings"));
        };
        algorithms.push(crate::jwt::parse_algorithm(name).map_err(|e| validation(&e))?);
    }
    Ok(algorithms)
}

/// `issuer` / `audience`: a string or an array; string values pass through
/// the secret resolver so client ids can live as `env://` references (they
/// round-trip masked exports even though they are not secrets).
async fn string_or_vec(
    input: &Value,
    field: &str,
    ctx: &TaskContext<'_>,
) -> Result<Vec<String>, DataflowError> {
    let raw = match input.get(field) {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(raw) => super::connector_helpers::resolve_value(raw, ctx),
    };
    let items: Vec<String> = match raw {
        Value::String(s) => vec![s],
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => {
            return Err(validation(&format!(
                "'{field}' must be a string or an array of strings"
            )));
        }
    };
    let mut resolved = Vec::with_capacity(items.len());
    for item in items {
        resolved.push(
            crate::connector::secrets::resolve_secret_string(&item, field)
                .await
                .map_err(|e| validation(&e))?,
        );
    }
    Ok(resolved)
}

// -- Authoring-time validation (shared with schema::validate_input) --

pub(super) fn validate_static_input(
    obj: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    let mut errors: Vec<(&'static str, &'static str, String)> = Vec::new();
    if let Some(names) = obj.get("algorithms").and_then(Value::as_array) {
        if names.is_empty() {
            errors.push((
                "algorithms",
                "INVALID",
                "'algorithms' must be a non-empty allowlist".to_string(),
            ));
        }
        for name in names.iter().filter_map(Value::as_str) {
            if let Err(e) = crate::jwt::parse_algorithm(name) {
                errors.push(("algorithms", "INVALID", e));
            }
        }
    } else if obj.get("algorithms").is_none() {
        errors.push((
            "algorithms",
            "REQUIRED",
            "jwt_verify requires a non-empty 'algorithms' allowlist".to_string(),
        ));
    }
    if obj.get("keys").is_none_or(Value::is_null) && obj.get("jwks_url").is_none_or(Value::is_null)
    {
        errors.push((
            "",
            "REQUIRED",
            "jwt_verify requires 'keys' and/or 'jwks_url'".to_string(),
        ));
    }
    if let Some(url) = obj.get("jwks_url").and_then(Value::as_str)
        && let Err(e) = crate::jwt::validate_jwks_url(url)
    {
        errors.push(("jwks_url", "INVALID", format!("'jwks_url' {e}")));
    }
    errors
}

// -- Input schema (F53) --

pub(super) const JWT_VERIFY_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "token",
        description: "The compact JWS to verify.",
        kind: FieldKind::String,
        required: true,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "algorithms",
        description: "Mandatory non-empty allowlist (HS/RS/PS 256-512, ES256/384, \
                      EdDSA); alg: none is unrepresentable.",
        kind: FieldKind::Array,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "keys",
        description: "Static verification keys: [{algorithm, key, kid?, \
                      key_encoding?}]. At least one of keys/jwks_url.",
        kind: FieldKind::Array,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "jwks_url",
        description: "HTTPS JWKS URL; cached process-wide with single-flight refresh \
                      and stale-serve — the same cache as the jwt channel mode.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "issuer",
        description: "Accepted iss value(s); string or array. env:// references \
                      resolve.",
        kind: FieldKind::Any,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "audience",
        description: "Accepted aud value(s); string or array. env:// references \
                      resolve (OAuth client ids).",
        kind: FieldKind::Any,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "leeway_secs",
        description: "Clock-skew allowance for exp/nbf. Default 30, capped at 300.",
        kind: FieldKind::Number,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "require_exp",
        description: "Whether the token must carry exp. Default true (RFC 8725).",
        kind: FieldKind::Bool,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the verified claims object is stored. \
                      Defaults to \"data\". Rejections are typed task errors \
                      (continue_on_error branches on them).",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Run a two-task workflow — jwt_sign then jwt_verify — through a real
    /// engine, which is acceptance criterion 5's round-trip.
    async fn round_trip(
        sign_input: Value,
        verify_input: Value,
        data: Value,
    ) -> Result<Value, String> {
        let mut fns: std::collections::HashMap<String, dataflow_rs::BoxedFunctionHandler> =
            Default::default();
        fns.insert(
            "jwt_sign".to_string(),
            Box::new(super::super::jwt_sign::JwtSignHandler),
        );
        fns.insert("jwt_verify".to_string(), Box::new(JwtVerifyHandler));
        crate::engine::functions::run_test_tasks(
            fns,
            json!([
                {"id": "sign", "name": "sign",
                 "function": {"name": "jwt_sign", "input": sign_input}},
                {"id": "verify", "name": "verify",
                 "function": {"name": "jwt_verify", "input": verify_input}},
            ]),
            data,
        )
        .await
    }

    const HS_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Acceptance criterion 5: sign → verify round-trips for every family —
    /// HS, RS (PKCS), PS (PSS), ES, EdDSA.
    #[tokio::test]
    async fn every_family_round_trips() {
        use crate::jwt::testkeys;
        for (alg, sign_key, verify_key) in [
            ("HS256", HS_SECRET, HS_SECRET),
            ("HS512", HS_SECRET, HS_SECRET),
            ("RS256", testkeys::RSA_PRIVATE, testkeys::RSA_PUBLIC),
            ("PS256", testkeys::RSA_PRIVATE, testkeys::RSA_PUBLIC),
            ("ES256", testkeys::EC_PRIVATE, testkeys::EC_PUBLIC),
            ("EdDSA", testkeys::ED_PRIVATE, testkeys::ED_PUBLIC),
        ] {
            let out = round_trip(
                json!({"algorithm": alg, "key": sign_key,
                       "claims": {"sub": {"var": "data.user_id"}, "roles": ["teacher"]},
                       "expires_in": "24h", "issuer": "example-api",
                       "output": "data.token"}),
                json!({"token": {"var": "data.token"},
                       "algorithms": [alg],
                       "keys": [{"algorithm": alg, "key": verify_key}],
                       "issuer": "example-api",
                       "output": "data.claims"}),
                json!({"user_id": "user-7"}),
            )
            .await
            .unwrap_or_else(|e| unreachable!("{alg}: {e}"));
            assert_eq!(out["claims"]["sub"], "user-7", "{alg}");
            assert_eq!(out["claims"]["iss"], "example-api", "{alg}");
            assert!(out["claims"]["exp"].is_number(), "{alg}");
            assert!(out["claims"]["iat"].is_number(), "{alg}");
        }
    }

    /// #272: an author-supplied `iat` survives into the token instead of being
    /// overwritten by the ambient clock.
    ///
    /// `iat` is the only registered claim `jwt_sign` touches that has no
    /// dedicated input field, so nothing more specific exists to beat a claims
    /// entry — the entry should simply win, exactly as an explicit `exp` does.
    /// The round trip through `jwt_verify` proves nothing downstream rejects a
    /// back-dated token: neither `jwt_verify` nor the channel `jwt` auth mode
    /// inspects `iat`.
    #[tokio::test]
    async fn an_explicit_iat_claim_survives_signing() {
        let back_dated = chrono::Utc::now().timestamp() - 86_400;
        let out = round_trip(
            json!({"algorithm": "HS256", "key": HS_SECRET,
                   "claims": {"sub": "u1", "iat": back_dated},
                   "expires_in": 300, "output": "data.token"}),
            json!({"token": {"var": "data.token"},
                   "algorithms": ["HS256"],
                   "keys": [{"algorithm": "HS256", "key": HS_SECRET}],
                   "output": "data.claims"}),
            json!({}),
        )
        .await
        .expect("a back-dated iat must sign and verify");
        assert_eq!(
            out["claims"]["iat"], back_dated,
            "an explicit iat must not be overwritten by the ambient clock"
        );
    }

    /// The other half: with no `iat` supplied it is still stamped, so the
    /// change is additive and no existing workflow loses the claim.
    #[tokio::test]
    async fn an_absent_iat_is_still_stamped() {
        let before = chrono::Utc::now().timestamp();
        let out = round_trip(
            json!({"algorithm": "HS256", "key": HS_SECRET, "claims": {},
                   "expires_in": 300, "output": "data.token"}),
            json!({"token": {"var": "data.token"},
                   "algorithms": ["HS256"],
                   "keys": [{"algorithm": "HS256", "key": HS_SECRET}],
                   "output": "data.claims"}),
            json!({}),
        )
        .await
        .expect("sign");
        let iat = out["claims"]["iat"].as_i64().expect("iat is stamped");
        assert!(
            iat >= before && iat <= chrono::Utc::now().timestamp(),
            "an unsupplied iat is stamped to now, got {iat}"
        );
    }

    /// A non-numeric registered date mints a token every verifier rejects, so
    /// it is refused at sign time where the message can name the claim. `exp`
    /// takes the same rule — it previously got no check at all when supplied
    /// through `claims`.
    #[tokio::test]
    async fn a_non_numeric_registered_date_is_refused_at_signing() {
        for (claim, claims) in [
            (
                "iat",
                json!({"iat": "2026-08-20T00:00:00Z", "exp": 9_999_999_999i64}),
            ),
            ("exp", json!({"exp": "2026-08-20T00:00:00Z"})),
        ] {
            let err = round_trip(
                json!({"algorithm": "HS256", "key": HS_SECRET,
                       "claims": claims, "output": "data.token"}),
                json!({"token": "x", "algorithms": ["HS256"],
                       "keys": [{"algorithm": "HS256", "key": HS_SECRET}]}),
                json!({}),
            )
            .await
            .expect_err("a string date must be refused");
            assert!(
                err.contains(&format!("claims.{claim}")) && err.contains("NumericDate"),
                "{claim}: {err}"
            );
        }
    }

    /// Determinism is the structural win: with `iat` and `exp` both pinned, one
    /// input mints byte-identical tokens, so an offline `*.case.json` case can
    /// finally assert a `jwt_sign` result. Before this change `iat` moved every
    /// run and no golden test was possible.
    #[tokio::test]
    async fn a_fully_pinned_claim_set_mints_a_deterministic_token() {
        let sign = json!({"algorithm": "HS256", "key": HS_SECRET,
                          "claims": {"sub": "u1", "iat": 1_700_000_000i64,
                                     "exp": 1_900_000_000i64},
                          "output": "data.token"});
        let verify = json!({"token": {"var": "data.token"},
                            "algorithms": ["HS256"],
                            "keys": [{"algorithm": "HS256", "key": HS_SECRET}],
                            "output": "data.claims"});
        let first = round_trip(sign.clone(), verify.clone(), json!({}))
            .await
            .expect("sign");
        let second = round_trip(sign, verify, json!({})).await.expect("sign");
        assert_eq!(
            first["token"], second["token"],
            "a fully pinned claim set must mint the same bytes every run"
        );
    }

    /// Cross-family confusion (acceptance criterion 2): an HS-signed token
    /// against an RS allowlist — and the reverse — must die at the allowlist,
    /// as a typed task error naming the reason.
    #[tokio::test]
    async fn cross_family_tokens_are_rejected() {
        use crate::jwt::testkeys;
        let err = round_trip(
            json!({"algorithm": "HS256", "key": HS_SECRET,
                   "claims": {}, "expires_in": 300, "output": "data.token"}),
            json!({"token": {"var": "data.token"},
                   "algorithms": ["RS256"],
                   "keys": [{"algorithm": "RS256", "key": testkeys::RSA_PUBLIC}],
                   "output": "data.claims"}),
            json!({}),
        )
        .await
        .expect_err("HS token against RS allowlist");
        assert!(err.contains("alg_rejected"), "{err}");
    }

    #[tokio::test]
    async fn verify_failures_are_typed_task_errors() {
        // Expired token: signed with exp already past (explicit exp claim).
        let past = chrono::Utc::now().timestamp() - 3600;
        let err = round_trip(
            json!({"algorithm": "HS256", "key": HS_SECRET,
                   "claims": {"exp": past}, "output": "data.token"}),
            json!({"token": {"var": "data.token"},
                   "algorithms": ["HS256"],
                   "keys": [{"algorithm": "HS256", "key": HS_SECRET}],
                   "output": "data.claims"}),
            json!({}),
        )
        .await
        .expect_err("expired");
        assert!(err.contains("expired"), "{err}");

        // Wrong audience.
        let err = round_trip(
            json!({"algorithm": "HS256", "key": HS_SECRET, "claims": {},
                   "expires_in": 300, "audience": "other-app", "output": "data.token"}),
            json!({"token": {"var": "data.token"},
                   "algorithms": ["HS256"],
                   "keys": [{"algorithm": "HS256", "key": HS_SECRET}],
                   "audience": "this-app",
                   "output": "data.claims"}),
            json!({}),
        )
        .await
        .expect_err("audience mismatch");
        assert!(err.contains("audience_mismatch"), "{err}");
    }

    #[tokio::test]
    async fn sign_requires_a_deliberate_expiry_and_short_hs_keys_are_refused() {
        let err = round_trip(
            json!({"algorithm": "HS256", "key": HS_SECRET, "claims": {},
                   "output": "data.token"}),
            json!({"token": "x", "algorithms": ["HS256"],
                   "keys": [{"algorithm": "HS256", "key": HS_SECRET}]}),
            json!({}),
        )
        .await
        .expect_err("no expiry");
        assert!(err.contains("expires_in"), "{err}");

        let err = round_trip(
            json!({"algorithm": "HS256", "key": "short", "claims": {},
                   "expires_in": 300, "output": "data.token"}),
            json!({"token": "x", "algorithms": ["HS256"],
                   "keys": [{"algorithm": "HS256", "key": HS_SECRET}]}),
            json!({}),
        )
        .await
        .expect_err("short secret");
        assert!(err.contains("RFC 7518"), "{err}");
    }

    /// The JWKS path end to end against a live mock: fetch, kid routing, and
    /// the rotation refetch (acceptance criterion 1's JWKS half). Symmetric
    /// JWK (kty=oct) keeps the mock simple; the cache mechanics are identical
    /// for every family.
    #[tokio::test]
    async fn jwks_fetch_and_kid_rotation_work() {
        use base64::Engine as _;
        let k = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(HS_SECRET);
        // Serves kid "old" first, then kid "new" after rotation is flagged.
        let rotated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rotated_srv = rotated.clone();
        let app = axum::Router::new().route(
            "/jwks.json",
            axum::routing::get(move || {
                let rotated = rotated_srv.clone();
                let k = k.clone();
                async move {
                    let kid = if rotated.load(std::sync::atomic::Ordering::SeqCst) {
                        "new"
                    } else {
                        "old"
                    };
                    axum::Json(serde_json::json!({
                        "keys": [{"kty": "oct", "k": k, "kid": kid, "alg": "HS256"}]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test");
        let addr = listener.local_addr().expect("test");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test");
        });
        // The handler requires HTTPS for operator configs; the cache itself is
        // scheme-agnostic, so exercise it directly against the HTTP mock.
        let url = format!("http://{addr}/jwks.json");

        let verifier = |kid: &str| {
            let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
            header.kid = Some(kid.to_string());
            jsonwebtoken::encode(
                &header,
                &serde_json::json!({"sub": "u", "exp": chrono::Utc::now().timestamp() + 300}),
                &jsonwebtoken::EncodingKey::from_secret(HS_SECRET.as_bytes()),
            )
            .expect("test")
        };
        let core = crate::jwt::Verifier {
            static_keys: Vec::new(),
            jwks_url: Some(url),
            algorithms: vec![jsonwebtoken::Algorithm::HS256],
            issuer: Vec::new(),
            audience: Vec::new(),
            leeway_secs: 30,
            require_exp: true,
            max_token_bytes: 8192,
            validations: std::sync::OnceLock::new(),
        };

        // kid "old" verifies from the first fetch.
        core.verify(&verifier("old"))
            .await
            .expect("old kid verifies");
        // Rotate; a token with the unseen kid forces the refetch and passes.
        rotated.store(true, std::sync::atomic::Ordering::SeqCst);
        core.verify(&verifier("new"))
            .await
            .expect("rotation refetch");
    }
}

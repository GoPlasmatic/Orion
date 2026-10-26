//! `jwt_verify` — verify a third-party (or own) JWS mid-workflow (#267
//! Part B): provider id_tokens for social login, refresh tokens, partner
//! assertions. The same shared core and JWKS cache as the channel mode, with
//! task-error semantics so workflows can branch on failure.

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use serde_json::{Map, Value};

use super::schema::{FieldKind, FieldSchema};
use crate::kit::{self, KeyMaterialList, KeyRef, read};
use crate::{ErrorClass, HandlerError};

/// This handler's name in metrics, profiles and error messages (F48).
pub const NAME: &str = "jwt_verify";

/// A caller-fixable refusal, named: `jwt_verify`'s messages carry the
/// handler's name themselves.
fn invalid(msg: &str) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, format!("{NAME}: {msg}"))
}

/// A verification's refusal: a typed, branchable task error, the reason and
/// never the token.
fn rejected(reason: crate::jwt::RejectReason) -> HandlerError {
    HandlerError::new(
        ErrorClass::Backend,
        format!("{NAME}: token rejected ({})", reason.as_str()),
    )
}

// -- dataflow-rs 4 --

/// One static verification key, as 1.12's element reader read it: `kid` and
/// `key_encoding` read only when they are strings, anything else read as
/// unset, as 1.12 read it; `algorithm` a literal, required at load; `key` as
/// written, a string or a `{"secret": …}` node ([`KeyRef`]), never an
/// expression.
#[derive(Debug, FunctionConfig)]
pub struct JwtKey {
    /// The key id a token's header names it by, for rotation.
    kid: Option<Value>,
    /// The algorithm the key verifies.
    algorithm: String,
    /// The HS secret or the public-key PEM.
    key: KeyRef,
    /// How an HS secret becomes bytes: utf8, base64 or hex.
    key_encoding: Option<Value>,
}

/// `jwt_verify`'s input on dataflow-rs 4, in its field table's order
/// (I179), each field in the position 1.12 executed it in (I183). The
/// algorithms, the keys, the JWKS URL and `require_exp` are literals, read
/// as written; `token` and `leeway_secs` are expressions, read by
/// `kit::read`'s twins of 1.12's readers; `issuer` and `audience` are key
/// material lists, `string_or_vec`'s reading, K51 included. A computed
/// `output` is evaluated where 1.12 read `output`, before the token and any
/// JWKS fetch, where 1.12 never evaluated it and wrote at `data` (a change
/// O11b's CHANGELOG names). A task missing `token` or `algorithms` is
/// refused at load (I183), and its `check` is 1.12's authoring-time rules.
#[derive(Debug, FunctionConfig)]
#[config(check = check)]
pub struct JwtVerifyConfig {
    /// The compact JWS verified.
    token: Expr<OwnedDataValue>,
    /// The allowlist of algorithms a token may be signed with.
    algorithms: Vec<String>,
    /// The static verification keys.
    keys: Option<Vec<JwtKey>>,
    /// The HTTPS JWKS endpoint, cached with the channel mode's.
    jwks_url: Option<String>,
    /// The accepted `iss` values.
    issuer: Option<KeyMaterialList>,
    /// The accepted `aud` values.
    audience: Option<KeyMaterialList>,
    /// The clock-skew allowance for `exp` and `nbf`, capped.
    leeway_secs: Option<Expr<OwnedDataValue>>,
    /// Whether a token must carry `exp`.
    require_exp: Option<bool>,
    /// Where the verified claims are written.
    #[output(default = "data")]
    output: Output,
}

/// 1.12's authoring-time rules for `jwt_verify` ([`validate_static_input`],
/// by which create, update, import, validate and `lint` refused a task), on
/// the config at load: a non-empty allowlist of supported algorithms, keys
/// or a JWKS URL, and an HTTPS JWKS URL, each refused at the field 1.12
/// named, in its words. Only whether `keys` is set is judged, as 1.12
/// judged it.
fn check(cfg: &JwtVerifyConfig, cx: &mut CompileCx<'_>) {
    let mut input = Map::new();
    input.insert(
        "algorithms".to_string(),
        Value::Array(cfg.algorithms.iter().cloned().map(Value::String).collect()),
    );
    if cfg.keys.is_some() {
        input.insert("keys".to_string(), Value::Array(Vec::new()));
    }
    if let Some(url) = &cfg.jwks_url {
        input.insert("jwks_url".to_string(), Value::String(url.clone()));
    }
    super::report_static(validate_static_input(&input), cx);
}

/// `jwt_verify` as a function: the verification 1.12's handler ran, each
/// field read in the same order, a JWKS fetch inside the run's deadline.
pub struct JwtVerifyFunction {
    /// The instance's JWKS cache, shared with the channel `jwt` auth mode.
    pub jwks: std::sync::Arc<crate::jwt::jwks::JwksCache>,
}

impl Function for JwtVerifyFunction {
    type Config = JwtVerifyConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    // No `prepare`: the claims are an object, which any root takes.

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &JwtVerifyConfig) -> dataflow_rs::Result<Outcome> {
        let (output, claims) = self.claims(ctx, cfg).await?;
        ctx.set(&output, OwnedDataValue::from(&claims))?;
        Ok(Outcome::Ok)
    }
}

impl JwtVerifyFunction {
    /// Where the claims go and the verified claims, read in 1.12's order: the allowlist, the output, the
    /// token, each static key, the JWKS URL, the issuer and audience, the
    /// leeway, then the verification.
    async fn claims<'c>(
        &self,
        ctx: &Ctx<'_>,
        cfg: &'c JwtVerifyConfig,
    ) -> Result<(std::borrow::Cow<'c, dataflow_rs::Path>, Value), HandlerError> {
        let algorithms = algorithms_v4(&cfg.algorithms)?;
        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
        let token = ctx.eval(&cfg.token)?;
        let token = read::required_str(Some(&token), "token", NAME)?.into_owned();

        let mut static_keys = Vec::new();
        for (index, entry) in cfg.keys.iter().flatten().enumerate() {
            let algorithm =
                crate::jwt::parse_algorithm(&entry.algorithm).map_err(|e| invalid(&e))?;
            if entry.key.is_null() {
                return Err(invalid(&format!("'keys[{index}].key' is required")));
            }
            let material = entry.key.material(ctx, "jwt_verify.keys").await?;
            let key_encoding = entry.key_encoding.as_ref().and_then(Value::as_str);
            let key = crate::jwt::decoding_key(algorithm, &material, key_encoding)
                .map_err(|e| invalid(&format!("'keys[{index}]': {e}")))?;
            static_keys.push(crate::jwt::StaticKey {
                kid: entry
                    .kid
                    .as_ref()
                    .and_then(Value::as_str)
                    .map(str::to_string),
                algorithm,
                key,
            });
        }

        let jwks = match cfg.jwks_url.as_deref() {
            None => None,
            Some(url) => {
                crate::jwt::validate_jwks_url(url)
                    .map_err(|e| invalid(&format!("'jwks_url' {e}")))?;
                Some(crate::jwt::JwksSource {
                    url: url.to_string(),
                    cache: self.jwks.clone(),
                })
            }
        };
        if static_keys.is_empty() && jwks.is_none() {
            return Err(invalid("requires 'keys' and/or 'jwks_url'"));
        }

        let issuer = kit::key_material_list(cfg.issuer.as_ref(), ctx, "issuer", NAME).await?;
        let audience = kit::key_material_list(cfg.audience.as_ref(), ctx, "audience", NAME).await?;
        let leeway = cfg
            .leeway_secs
            .as_ref()
            .map(|expr| ctx.eval(expr))
            .transpose()?;
        let leeway_secs = read::optional_u64(leeway.as_deref(), "leeway_secs", NAME)?
            .unwrap_or(crate::jwt::DEFAULT_LEEWAY_SECS)
            .min(crate::jwt::MAX_LEEWAY_SECS);
        let verifier = crate::jwt::Verifier {
            static_keys,
            jwks,
            algorithms,
            issuer,
            audience,
            leeway_secs,
            require_exp: cfg.require_exp.unwrap_or(true),
            max_token_bytes: crate::jwt::DEFAULT_MAX_TOKEN_BYTES,
            validations: std::sync::OnceLock::new(),
        };
        // A JWKS fetch inside the run's deadline: its 5 s timer is armed only
        // when that comes first (`jwks::fetch_timeout`).
        let claims = verifier
            .verify_within(&token, ctx.deadline())
            .await
            .map_err(rejected)?;
        Ok((output, claims))
    }
}

/// The allowlist, as `algorithms` reads it: non-empty, every name
/// supported. A non-array or a non-string entry is refused at load.
fn algorithms_v4(names: &[String]) -> Result<Vec<jsonwebtoken::Algorithm>, HandlerError> {
    if names.is_empty() {
        return Err(invalid("requires a non-empty 'algorithms' allowlist"));
    }
    names
        .iter()
        .map(|name| crate::jwt::parse_algorithm(name).map_err(|e| invalid(&e)))
        .collect()
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
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "algorithms",
        description: "Mandatory non-empty allowlist (HS/RS/PS 256-512, ES256/384, \
                      EdDSA); alg: none is unrepresentable.",
        kind: FieldKind::Array,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "keys",
        description: "Static verification keys: [{algorithm, key, kid?, \
                      key_encoding?}]. Each entry's key takes {\"secret\": \"name\"} \
                      or a string; the field itself is always an array. At least one \
                      of keys/jwks_url.",
        kind: FieldKind::Array,
        // Each entry's `key`, and nothing else: `kid` and `key_encoding` are
        // read verbatim, so a reference in either is as stray as anywhere else.
        secret_at: &["[].key"],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "jwks_url",
        description: "HTTPS JWKS URL; cached process-wide with single-flight refresh \
                      and stale-serve — the same cache as the jwt channel mode.",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "issuer",
        description: "Accepted iss value(s); string or array. {\"secret\": \"name\"} \
                      and env:// references resolve.",
        kind: FieldKind::Any,
        template_at: &[""],
        secret_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "audience",
        description: "Accepted aud value(s); string or array. {\"secret\": \"name\"} \
                      and env:// references resolve (OAuth client ids).",
        kind: FieldKind::Any,
        template_at: &[""],
        secret_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "leeway_secs",
        description: "Clock-skew allowance for exp/nbf. Default 30, capped at 300.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "require_exp",
        description: "Whether the token must carry exp. Default true (RFC 8725).",
        kind: FieldKind::Bool,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the verified claims object is stored. \
                      Defaults to \"data\". Rejections are typed task errors \
                      (continue_on_error branches on them).",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::functions::twin::{self, Seen};
    use crate::jwt::jwks::JwksCache;
    use dataflow_rs::function::testing;
    use dataflow_rs::program::Severity;
    use serde_json::json;
    use std::sync::Arc;

    const HS_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

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
        // The mock listens on loopback, which the fetch-time private-address
        // check refuses — the same check `jwt.allow_private_jwks_urls` exists
        // to switch off for an in-cluster issuer. Exercising the cache against
        // a local mock is exactly that case.
        let cache = std::sync::Arc::new(crate::jwt::jwks::JwksCache::new(
            reqwest::Client::new(),
            true,
        ));
        let core = crate::jwt::Verifier {
            static_keys: Vec::new(),
            jwks: Some(crate::jwt::JwksSource { url, cache }),
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

    // -- the tasks, against 1.12's records --

    /// The store the tasks below read: the signing secret, a value that
    /// is not a string, and a client id.
    fn secrets() -> crate::ResolvedSecrets {
        let mut values = serde_json::Map::new();
        values.insert("signing".into(), json!(HS_SECRET));
        values.insert("count".into(), json!(7));
        values.insert("aud".into(), json!("partner-app"));
        crate::ResolvedSecrets::from_values(values)
    }

    /// Orion's expressions over [`secrets`].
    fn exprs() -> dataflow_rs::expr::ExprEngine {
        crate::operators::orion_exprs(&secrets(), 0).expect("Orion's expressions build")
    }

    /// A JWKS cache of its own, over `client`, that lets a loopback issuer
    /// through.
    fn cache(client: reqwest::Client) -> Arc<JwksCache> {
        Arc::new(JwksCache::new(client, true))
    }

    /// `steps`, each `(function, input)`, one call after another, each over
    /// the message the one before left, on [`exprs`], over a message whose
    /// `data` is `data`: the message's `data`, or the failure that stopped it.
    async fn steps_on(steps: &[(&str, Value)], data: Value, jwks: Arc<JwksCache>) -> Seen {
        let mut message = dataflow_rs::Message::from_parts(
            OwnedDataValue::Null,
            OwnedDataValue::from(&data),
            OwnedDataValue::Null,
        );
        for (name, input) in steps {
            let called = if *name == NAME {
                let f = JwtVerifyFunction {
                    jwks: Arc::clone(&jwks),
                };
                testing::call(f, input.clone())
                    .exprs(exprs())
                    .name(name)
                    .message(message)
                    .await
            } else {
                testing::call(super::super::jwt_sign::JwtSignFunction, input.clone())
                    .exprs(exprs())
                    .name(name)
                    .message(message)
                    .await
            };
            twin::seen(&called)?;
            message = called.message;
        }
        Ok(Value::from(message.context().data()))
    }

    /// `steps` with a JWKS cache of their own.
    async fn run(steps: &[(&str, Value)], data: Value) -> Seen {
        steps_on(steps, data, cache(reqwest::Client::new())).await
    }

    /// Sign then verify, the round trip: `jwt_sign` over `sign`, then
    /// `jwt_verify` over `verify`.
    async fn round_trip(sign: Value, verify: Value, data: Value) -> Seen {
        run(&[("jwt_sign", sign), (NAME, verify)], data).await
    }

    /// `seen` with what depends on the clock or on a signature's randomness
    /// spelled out: each numeric `iat`, `exp` and `nbf`, and each token.
    fn unclocked(seen: &Seen) -> Seen {
        fn strip(v: &mut Value) {
            match v {
                Value::Object(map) => {
                    for (key, value) in map.iter_mut() {
                        if matches!(key.as_str(), "iat" | "exp" | "nbf") && value.is_number() {
                            *value = json!("<clock>");
                        } else if key == "token" && value.is_string() {
                            *value = json!("<jws>");
                        } else {
                            strip(value);
                        }
                    }
                }
                Value::Array(items) => items.iter_mut().for_each(strip),
                _ => {}
            }
        }
        seen.clone().map(|mut data| {
            strip(&mut data);
            data
        })
    }

    /// An HS256 token for `kid`, expiring in five minutes.
    fn hs_token(kid: &str) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some(kid.to_string());
        jsonwebtoken::encode(
            &header,
            &json!({"sub": "u", "exp": chrono::Utc::now().timestamp() + 300}),
            &jsonwebtoken::EncodingKey::from_secret(HS_SECRET.as_bytes()),
        )
        .expect("test")
    }

    fn config_v4(input: Value) -> Result<JwtVerifyConfig, Vec<dataflow_rs::Issue>> {
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

    #[tokio::test]
    async fn every_family_round_trips() {
        use crate::jwt::testkeys;
        for (alg, sign_key, verify_key) in [
            ("HS256", HS_SECRET, HS_SECRET),
            ("HS512", HS_SECRET, HS_SECRET),
            (
                "RS256",
                testkeys::RSA.private.as_str(),
                testkeys::RSA.public.as_str(),
            ),
            (
                "PS256",
                testkeys::RSA.private.as_str(),
                testkeys::RSA.public.as_str(),
            ),
            (
                "ES256",
                testkeys::EC.private.as_str(),
                testkeys::EC.public.as_str(),
            ),
            (
                "EdDSA",
                testkeys::ED.private.as_str(),
                testkeys::ED.public.as_str(),
            ),
        ] {
            let v4 = round_trip(
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
            .await;
            twin::pinned(&unclocked(&v4), format!("every_family/{alg}"));
            let out = v4.unwrap_or_else(|e| unreachable!("{alg}: {e:?}"));
            assert_eq!(out["claims"]["sub"], "user-7", "{alg}");
            assert_eq!(out["claims"]["iss"], "example-api", "{alg}");
            assert!(out["claims"]["exp"].is_number(), "{alg}");
            assert!(out["claims"]["iat"].is_number(), "{alg}");
        }
    }

    #[tokio::test]
    async fn an_explicit_iat_claim_survives_signing() {
        let back_dated = chrono::Utc::now().timestamp() - 86_400;
        let v4 = round_trip(
            json!({"algorithm": "HS256", "key": HS_SECRET,
                   "claims": {"sub": "u1", "iat": back_dated},
                   "expires_in": 300, "output": "data.token"}),
            json!({"token": {"var": "data.token"},
                   "algorithms": ["HS256"],
                   "keys": [{"algorithm": "HS256", "key": HS_SECRET}],
                   "output": "data.claims"}),
            json!({}),
        )
        .await;
        twin::pinned(&unclocked(&v4), "explicit_iat");
        assert_eq!(v4.expect("verified")["claims"]["iat"], back_dated);
    }

    #[tokio::test]
    async fn an_absent_iat_is_still_stamped() {
        let before = chrono::Utc::now().timestamp();
        let v4 = round_trip(
            json!({"algorithm": "HS256", "key": HS_SECRET, "claims": {},
                   "expires_in": 300, "output": "data.token"}),
            json!({"token": {"var": "data.token"},
                   "algorithms": ["HS256"],
                   "keys": [{"algorithm": "HS256", "key": HS_SECRET}],
                   "output": "data.claims"}),
            json!({}),
        )
        .await;
        twin::pinned(&unclocked(&v4), "absent_iat");
        let iat = v4.expect("verified")["claims"]["iat"]
            .as_i64()
            .expect("iat is stamped");
        assert!(iat >= before && iat <= chrono::Utc::now().timestamp());
    }

    #[tokio::test]
    async fn a_non_numeric_registered_date_is_refused_at_signing() {
        for (claim, claims) in [
            (
                "iat",
                json!({"iat": "2026-08-20T00:00:00Z", "exp": 9_999_999_999i64}),
            ),
            ("exp", json!({"exp": "2026-08-20T00:00:00Z"})),
        ] {
            let v4 = round_trip(
                json!({"algorithm": "HS256", "key": HS_SECRET,
                       "claims": claims, "output": "data.token"}),
                json!({"token": "x", "algorithms": ["HS256"],
                       "keys": [{"algorithm": "HS256", "key": HS_SECRET}]}),
                json!({}),
            )
            .await;
            twin::pinned(&v4, format!("non_numeric_date/{claim}"));
            let err = v4.expect_err("a string date is refused");
            assert!(
                err.message.contains(&format!("claims.{claim}"))
                    && err.message.contains("NumericDate"),
                "{claim}: {err:?}"
            );
        }
    }

    /// With `iat` and `exp` pinned, the token is 1.12's bytes, verified to
    /// 1.12's claims.
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
        let v4 = round_trip(sign, verify, json!({})).await;
        twin::pinned(&v4, "fully_pinned");
        let out = v4.expect("verified");
        assert!(out["token"].is_string());
    }

    #[tokio::test]
    async fn cross_family_tokens_are_rejected() {
        use crate::jwt::testkeys;
        let v4 = round_trip(
            json!({"algorithm": "HS256", "key": HS_SECRET,
                   "claims": {}, "expires_in": 300, "output": "data.token"}),
            json!({"token": {"var": "data.token"},
                   "algorithms": ["RS256"],
                   "keys": [{"algorithm": "RS256", "key": testkeys::RSA.public.as_str()}],
                   "output": "data.claims"}),
            json!({}),
        )
        .await;
        twin::pinned(&v4, "cross_family");
        let err = v4.expect_err("HS token against RS allowlist");
        assert!(err.message.contains("alg_rejected"), "{err:?}");
        assert_eq!(err.code, "FUNCTION_ERROR");
    }

    #[tokio::test]
    async fn verify_failures_are_typed_task_errors() {
        let past = chrono::Utc::now().timestamp() - 3600;
        let v4 = round_trip(
            json!({"algorithm": "HS256", "key": HS_SECRET,
                   "claims": {"exp": past}, "output": "data.token"}),
            json!({"token": {"var": "data.token"},
                   "algorithms": ["HS256"],
                   "keys": [{"algorithm": "HS256", "key": HS_SECRET}],
                   "output": "data.claims"}),
            json!({}),
        )
        .await;
        twin::pinned(&v4, "verify_failures/expired");
        assert!(v4.expect_err("expired").message.contains("expired"));

        let v4 = round_trip(
            json!({"algorithm": "HS256", "key": HS_SECRET, "claims": {},
                   "expires_in": 300, "audience": "other-app", "output": "data.token"}),
            json!({"token": {"var": "data.token"},
                   "algorithms": ["HS256"],
                   "keys": [{"algorithm": "HS256", "key": HS_SECRET}],
                   "audience": "this-app",
                   "output": "data.claims"}),
            json!({}),
        )
        .await;
        twin::pinned(&v4, "verify_failures/audience");
        assert!(
            v4.expect_err("audience mismatch")
                .message
                .contains("audience_mismatch")
        );
    }

    /// A task with no expiry was refused at authoring by 1.12 and at its
    /// call; it is now refused at load, in the authoring check's words. A
    /// short HS secret is refused at the call, as 1.12 refused it.
    #[tokio::test]
    async fn sign_requires_a_deliberate_expiry_and_short_hs_keys_are_refused() {
        let no_expiry = json!({"algorithm": "HS256", "key": HS_SECRET, "claims": {},
                               "output": "data.token"});
        let issues = testing::config_with::<super::super::jwt_sign::JwtSignConfig>(
            &kit::test_exprs(),
            no_expiry.clone(),
        )
        .map(|_| ())
        .expect_err("refused at load");
        let authoring = super::super::jwt_sign::validate_static_input(
            no_expiry.as_object().expect("an object"),
        );
        assert_eq!(
            issues
                .iter()
                .map(|i| (i.path.to_string(), i.message.clone()))
                .collect::<Vec<_>>(),
            authoring
                .into_iter()
                .map(|(field, _, message)| (field.to_string(), message))
                .collect::<Vec<_>>()
        );

        let v4 = round_trip(
            json!({"algorithm": "HS256", "key": "short", "claims": {},
                   "expires_in": 300, "output": "data.token"}),
            json!({"token": "x", "algorithms": ["HS256"],
                   "keys": [{"algorithm": "HS256", "key": HS_SECRET}]}),
            json!({}),
        )
        .await;
        twin::pinned(&v4, "short_hs_key");
        assert!(v4.expect_err("short secret").message.contains("RFC 7518"));
    }

    /// An HTTPS JWKS endpoint on loopback with a self-signed certificate:
    /// the URL (HTTPS, as `jwt_verify` requires), a client that trusts the
    /// certificate, the switch that rotates kid `old` to `new`, and the
    /// count of fetches served.
    struct HttpsJwks {
        url: String,
        client: reqwest::Client,
        rotated: Arc<std::sync::atomic::AtomicBool>,
        hits: Arc<std::sync::atomic::AtomicUsize>,
    }

    async fn https_jwks() -> HttpsJwks {
        use base64::Engine as _;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        crate::crypto::ensure_provider();
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("a certificate");
        let cert = certified.cert.pem();
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
            cert.clone().into_bytes(),
            certified.signing_key.serialize_pem().into_bytes(),
        )
        .await
        .expect("a TLS config");
        let k = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(HS_SECRET);
        let rotated = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(AtomicUsize::new(0));
        let (rotated_srv, hits_srv) = (Arc::clone(&rotated), Arc::clone(&hits));
        let app = axum::Router::new().route(
            "/jwks.json",
            axum::routing::get(move || {
                let (rotated, hits, k) = (rotated_srv.clone(), hits_srv.clone(), k.clone());
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let kid = if rotated.load(Ordering::SeqCst) {
                        "new"
                    } else {
                        "old"
                    };
                    axum::Json(json!({
                        "keys": [{"kty": "oct", "k": k, "kid": kid, "alg": "HS256"}]
                    }))
                }
            }),
        );
        let handle: axum_server::Handle<std::net::SocketAddr> = axum_server::Handle::new();
        let server = axum_server::bind_rustls("127.0.0.1:0".parse().expect("an address"), tls)
            .handle(handle.clone());
        tokio::spawn(async move {
            server
                .serve(app.into_make_service())
                .await
                .expect("the mock serves");
        });
        let addr = handle.listening().await.expect("the mock listens");
        let client = reqwest::Client::builder()
            .add_root_certificate(
                reqwest::Certificate::from_pem(cert.as_bytes()).expect("the certificate"),
            )
            .build()
            .expect("a client");
        HttpsJwks {
            url: format!("https://localhost:{}/jwks.json", addr.port()),
            client,
            rotated,
            hits,
        }
    }

    /// `jwks_fetch_and_kid_rotation_work` through the task, against an
    /// HTTPS issuer (the task refuses any other): kid `old` verifies from the
    /// first fetch, and after rotation an unseen kid forces the refetch.
    #[tokio::test]
    async fn jwks_fetch_and_kid_rotation_work_through_the_task() {
        use std::sync::atomic::Ordering;
        let jwks = https_jwks().await;
        let verify = json!({"token": {"var": "data.token"}, "algorithms": ["HS256"],
                            "jwks_url": jwks.url, "output": "data.claims"});
        let jwks_cache = cache(jwks.client.clone());
        let step = |token: String| (json!({"token": token}), [(NAME, verify.clone())]);
        let (data, steps) = step(hs_token("old"));
        let v4 = steps_on(&steps, data, Arc::clone(&jwks_cache)).await;
        twin::pinned(&unclocked(&v4), "jwks_rotation/old");
        assert_eq!(v4.expect("old kid verifies")["claims"]["sub"], "u");
        assert_eq!(jwks.hits.load(Ordering::SeqCst), 1, "one fetch");

        jwks.rotated.store(true, Ordering::SeqCst);
        let (data, steps) = step(hs_token("new"));
        let v4 = steps_on(&steps, data, jwks_cache).await;
        twin::pinned(&unclocked(&v4), "jwks_rotation/new");
        assert_eq!(v4.expect("rotation refetch")["claims"]["sub"], "u");
        assert_eq!(jwks.hits.load(Ordering::SeqCst), 2, "one refetch");
    }

    /// An HTTPS URL on loopback whose listener accepts and never answers, so
    /// a fetch waits for its handshake until a timer ends it.
    async fn silent_jwks() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a listener");
        let port = listener.local_addr().expect("an address").port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        format!("https://127.0.0.1:{port}/jwks.json")
    }

    /// When the fetch's 5 s comes before the run's deadline, it fires as it
    /// did in 1.12: the fetch fails, nothing is cached, and the token is
    /// rejected `keys_unavailable`, 1.12's failure. The clock is paused, so the 5 s pass at once.
    #[tokio::test(start_paused = true)]
    async fn a_jwks_fetch_still_times_out_at_five_seconds_as_1_12s_did() {
        let url = silent_jwks().await;
        let input = json!({"token": {"var": "data.token"}, "algorithms": ["HS256"],
                           "jwks_url": url, "output": "data.claims"});
        let data = json!({"token": hs_token("old")});
        let started = tokio::time::Instant::now();
        let called = testing::call(
            JwtVerifyFunction {
                jwks: cache(reqwest::Client::new()),
            },
            input,
        )
        .exprs(exprs())
        .name(NAME)
        .message(dataflow_rs::Message::from_parts(
            OwnedDataValue::Null,
            OwnedDataValue::from(&data),
            OwnedDataValue::Null,
        ))
        .deadline(std::time::Instant::now() + std::time::Duration::from_secs(60))
        .await;
        let elapsed = started.elapsed();
        let err = twin::seen(&called).expect_err("the fetch timed out");
        assert!(err.message.contains("keys_unavailable"), "{err:?}");
        assert!(
            elapsed >= crate::jwt::jwks::FETCH_TIMEOUT
                && elapsed < std::time::Duration::from_secs(60),
            "the fetch's own timer ended it, at {elapsed:?}"
        );
    }

    /// When the run's deadline comes first, the fetch arms no timer of its
    /// own, and the engine's race ends the call `TimedOut` with nothing
    /// written, as 1.12's channel timeout dropped the run. A timer of the
    /// fetch's own at the deadline would win that race and answer
    /// `keys_unavailable`, the task's error.
    #[tokio::test]
    async fn a_run_deadline_before_the_fetch_timeout_ends_the_call_timed_out() {
        let url = silent_jwks().await;
        let input = json!({"token": {"var": "data.token"}, "algorithms": ["HS256"],
                           "jwks_url": url, "output": "data.claims"});
        let data = json!({"token": hs_token("old")});
        let started = std::time::Instant::now();
        let called = testing::call(
            JwtVerifyFunction {
                jwks: cache(reqwest::Client::new()),
            },
            input,
        )
        .exprs(exprs())
        .name(NAME)
        .message(dataflow_rs::Message::from_parts(
            OwnedDataValue::Null,
            OwnedDataValue::from(&data),
            OwnedDataValue::Null,
        ))
        .deadline(started + std::time::Duration::from_millis(300))
        .await;
        let error = called.outcome.expect_err("the run's deadline passed");
        assert_eq!(*error.kind(), dataflow_rs::ErrorKind::TimedOut, "{error}");
        assert!(called.writes.is_empty());
        assert!(started.elapsed() < crate::jwt::jwks::FETCH_TIMEOUT);
    }

    /// Declaration order is the field table's (I179), and a key's is the
    /// plan's: kid, algorithm, key, key_encoding.
    #[test]
    fn the_config_declares_the_field_table_in_its_order() {
        let fields: Vec<&str> = JwtVerifyConfig::fields().iter().map(|f| &*f.name).collect();
        let table: Vec<&str> = JWT_VERIFY_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);
        let key: Vec<&str> = JwtKey::fields().iter().map(|f| &*f.name).collect();
        assert_eq!(key, ["kid", "algorithm", "key", "key_encoding"]);
    }

    /// Each field compiles in the position 1.12 executed it in (I183): the
    /// token and the allowlist required, the literals typed where 1.12's
    /// field loop typed them, a key's `kid` and `key_encoding` anything (1.12
    /// read a non-string as unset), its algorithm and key required.
    #[test]
    fn the_fields_compile_in_1_12s_positions() {
        let cfg = config_v4(json!({
            "token": {"var": "data.token"}, "algorithms": ["HS256", "RS256"],
            "keys": [{"kid": 5, "algorithm": "HS256", "key": {"secret": "k"},
                      "key_encoding": true}],
            "issuer": ["a", {"var": "data.iss"}], "audience": {"secret": "aud"},
            "leeway_secs": 10, "require_exp": false, "output": "data.claims"
        }))
        .expect("compiles");
        assert_eq!(cfg.algorithms, ["HS256", "RS256"]);
        let key = &cfg.keys.as_ref().expect("keys")[0];
        assert_eq!(key.kid, Some(json!(5)));
        assert!(matches!(key.key, KeyRef::Secret(_)));
        assert_eq!(cfg.require_exp, Some(false));
        let cfg = config_v4(json!({"token": "t", "algorithms": ["HS256"], "keys": null,
                                   "issuer": null, "jwks_url": "https://issuer/jwks"}))
        .expect("compiles");
        assert!(cfg.keys.is_none() && cfg.issuer.is_none());

        let full = json!({"token": "t", "algorithms": ["HS256"],
                          "keys": [{"algorithm": "HS256", "key": HS_SECRET}]});
        for field in ["token", "algorithms"] {
            let mut input = full.clone();
            input.as_object_mut().expect("an object").remove(field);
            assert_eq!(refusals(input)[0].0, field);
        }
        // 1.12 failed every call on each of these, or its field loop
        // refused them at authoring.
        for (patch, at) in [
            (json!({"algorithms": "HS256"}), "algorithms"),
            (json!({"algorithms": ["HS256", 5]}), "algorithms[1]"),
            (json!({"keys": {"algorithm": "HS256"}}), "keys"),
            (json!({"keys": ["HS256"]}), "keys[0]"),
            (json!({"keys": [{"key": HS_SECRET}]}), "keys[0].algorithm"),
            (
                json!({"keys": [{"algorithm": 5, "key": HS_SECRET}]}),
                "keys[0].algorithm",
            ),
            (json!({"keys": [{"algorithm": "HS256"}]}), "keys[0].key"),
            (json!({"jwks_url": 5}), "jwks_url"),
            (json!({"require_exp": "yes"}), "require_exp"),
        ] {
            let mut input = full.clone();
            for (key, value) in patch.as_object().expect("an object") {
                input[key] = value.clone();
            }
            assert_eq!(refusals(input.clone())[0].0, at, "{input}");
        }
        // Break 2a: an unknown key in a key, which 1.12 ignored.
        let mut input = full;
        input["keys"][0]["use"] = json!("sig");
        let issues = refusals(input);
        assert_eq!(issues[0].0, "keys[0].use", "{issues:?}");
    }

    /// The check is 1.12's authoring-time rules: each refusal at the field
    /// and in the words `validate_static_input` gives, computed from it; an
    /// input it accepts loads.
    #[test]
    fn the_check_is_validate_static_input() {
        let authoring = |input: &Value| -> Vec<(String, String)> {
            validate_static_input(input.as_object().expect("an object"))
                .into_iter()
                .map(|(field, _code, message)| (field.to_string(), message))
                .collect()
        };
        let key = json!([{"algorithm": "HS256", "key": HS_SECRET}]);
        for input in [
            json!({"token": "t", "algorithms": [], "keys": key}),
            json!({"token": "t", "algorithms": ["HS999"], "keys": key}),
            json!({"token": "t", "algorithms": ["HS256", "none"], "keys": key}),
            json!({"token": "t", "algorithms": ["HS256"]}),
            json!({"token": "t", "algorithms": ["HS256"], "keys": null}),
            json!({"token": "t", "algorithms": ["HS256"], "jwks_url": "http://issuer/jwks"}),
        ] {
            let expected = authoring(&input);
            assert!(!expected.is_empty(), "{input} is a refusal");
            assert_eq!(refusals(input.clone()), expected, "{input}");
        }
        for input in [
            json!({"token": "t", "algorithms": ["HS256"], "keys": key}),
            json!({"token": "t", "algorithms": ["HS256"], "keys": []}),
            json!({"token": "t", "algorithms": ["RS256"], "jwks_url": "https://issuer/jwks"}),
        ] {
            assert!(authoring(&input).is_empty(), "{input}");
            assert!(config_v4(input.clone()).is_ok(), "{input}");
        }
    }

    /// A static key reads as 1.12's element reader read it: a non-string `kid` or `key_encoding` is unset, the key is a
    /// string or a `{"secret": …}` node read as written, and anything else,
    /// an expression included, is refused at the call in 1.12's words,
    /// never evaluated.
    #[tokio::test]
    async fn a_static_key_reads_as_1_12s_element_reader_did() {
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &json!({"sub": "u", "exp": chrono::Utc::now().timestamp() + 300}),
            &jsonwebtoken::EncodingKey::from_secret(HS_SECRET.as_bytes()),
        )
        .expect("test");
        let data = json!({"token": token, "k": HS_SECRET, "n": 5});
        let verify = |key: Value| {
            json!({"token": {"var": "data.token"}, "algorithms": ["HS256"],
                   "keys": [key], "output": "data.claims"})
        };
        for key in [
            json!({"algorithm": "HS256", "key": HS_SECRET, "kid": 5, "key_encoding": 7}),
            json!({"algorithm": "HS256", "key": {"secret": "signing"}}),
            json!({"algorithm": "HS256", "key": {"secret": ["signing"]}}),
            json!({"algorithm": "HS256", "key": {"secret": "count"}}),
            json!({"algorithm": "HS256", "key": {"var": "data.k"}}),
            json!({"algorithm": "HS256", "key": {"cat": [{"secret": "signing"}]}}),
            json!({"algorithm": "HS256", "key": {"secret": {"var": "data.k"}}}),
            json!({"algorithm": "HS256", "key": 7}),
            json!({"algorithm": "HS256", "key": null}),
            json!({"algorithm": "HS256", "key": "env://ORION_JWT_VERIFY_SURELY_UNSET_8P"}),
            json!({"algorithm": "HS999", "key": HS_SECRET}),
            json!({"algorithm": "HS256", "key": "short"}),
            json!({"algorithm": "HS256", "key": HS_SECRET, "key_encoding": "rot13"}),
        ] {
            let v4 = run(&[(NAME, verify(key.clone()))], data.clone()).await;
            twin::pinned(&unclocked(&v4), format!("static_key/{key}"));
        }
        // The key from the store verifies; the one a `var` would read from
        // the message is refused, though it is the right one.
        let v4 = run(
            &[(
                NAME,
                verify(json!({"algorithm": "HS256", "key": {"secret": "signing"}})),
            )],
            data.clone(),
        )
        .await;
        assert_eq!(v4.expect("verified")["claims"]["sub"], "u");
        let v4 = run(
            &[(
                NAME,
                verify(json!({"algorithm": "HS256", "key": {"var": "data.k"}})),
            )],
            data,
        )
        .await;
        let refusal = v4.expect_err("an expression is no key");
        assert_eq!(refusal.code, "VALIDATION_ERROR");
        assert!(
            refusal
                .message
                .contains("'jwt_verify.keys' must be a string")
        );
    }

    /// K51, kept as 1.12 had it (I183) and pinned, not fixed: `issuer` and
    /// `audience` read a value a `var` hole takes from the message as key
    /// material, so an `env://NAME` string in request data reads that
    /// environment variable, and a `{"secret": …}` object in request data
    /// reads the store. The owner's to decide; the register holds it.
    #[tokio::test]
    async fn k51_issuer_and_audience_read_key_material_from_the_message_as_1_12_did() {
        let path = std::env::var("PATH").expect("PATH is set");
        let sign = json!({"algorithm": "HS256", "key": HS_SECRET, "claims": {},
                          "issuer": path, "audience": "partner-app", "expires_in": 300,
                          "output": "data.token"});
        let verify = json!({"token": {"var": "data.token"}, "algorithms": ["HS256"],
                            "keys": [{"algorithm": "HS256", "key": HS_SECRET}],
                            "issuer": {"var": "data.iss"}, "audience": [{"var": "data.aud"}],
                            "output": "data.claims"});
        let data = json!({"iss": "env://PATH", "aud": {"secret": "aud"}});
        let v4 = round_trip(sign, verify, data).await;
        let claims = &v4.expect("the message's references resolved")["claims"];
        assert_eq!(claims["iss"], json!(path));
        assert_eq!(claims["aud"], json!("partner-app"));
    }

    /// A computed `output` writes the claims where it evaluates, by 1.12's
    /// `resolve_output_path` rule. 1.12 never evaluated `jwt_verify`'s
    /// output: it wrote the claims at `data` whatever the expression said,
    /// replacing the rest of `data`. The claims are 1.12's, only their place
    /// differs (a change O11b's CHANGELOG names). A `null` result is `data`,
    /// as 1.12 had it. A result that is no path is
    /// refused in 1.12's words before the token is read or any key fetched.
    #[tokio::test]
    async fn a_computed_output_writes_the_claims_where_it_evaluates() {
        use std::sync::atomic::Ordering;
        let sign = json!({"algorithm": "HS256", "key": HS_SECRET,
                          "claims": {"sub": "u1", "iat": 1_700_000_000i64,
                                     "exp": 1_900_000_000i64},
                          "output": "temp_data.token"});
        let verify = json!({"token": {"var": "temp_data.token"}, "algorithms": ["HS256"],
                            "keys": [{"algorithm": "HS256", "key": HS_SECRET}],
                            "output": {"var": "data.where"}});
        let claims = json!({"sub": "u1", "iat": 1_700_000_000i64, "exp": 1_900_000_000i64});
        let v4 = round_trip(
            sign.clone(),
            verify.clone(),
            json!({"where": "data.claims"}),
        )
        .await;
        assert_eq!(
            v4.expect("verified"),
            json!({"where": "data.claims", "claims": claims})
        );

        let v4 = round_trip(sign, verify, json!({"kept": false})).await;
        twin::pinned(&v4, "computed_output/null");
        assert_eq!(v4.expect("verified"), claims);

        let jwks = https_jwks().await;
        let input = json!({"token": {"var": "data.token"}, "algorithms": ["HS256"],
                           "jwks_url": jwks.url, "output": {"var": "data.where"}});
        for not_a_path in [json!(5), json!({"a": 1})] {
            let data = json!({"token": hs_token("old"), "where": not_a_path});
            let called = testing::call(
                JwtVerifyFunction {
                    jwks: cache(jwks.client.clone()),
                },
                input.clone(),
            )
            .exprs(exprs())
            .name(NAME)
            .message(dataflow_rs::Message::from_parts(
                OwnedDataValue::Null,
                OwnedDataValue::from(&data),
                OwnedDataValue::Null,
            ))
            .await;
            let failure = twin::seen(&called).expect_err("no path");
            twin::pinned(&Err(failure), format!("output_refusal/{not_a_path}"));
        }
        assert_eq!(jwks.hits.load(Ordering::SeqCst), 0, "no key was fetched");
    }

    /// A key read from the store signs and verifies, and is seen by nothing
    /// else: the input a layer reads writes it `null` on both calls, and
    /// neither call's changes hold it. The claims are 1.12's.
    #[tokio::test]
    async fn a_round_trip_with_a_key_from_the_store_never_shows_the_secret() {
        use dataflow_rs::function::{BoxFuture, CallLayer, Next};
        use std::sync::Mutex;

        /// The input each call it wraps was given, as a layer reads it.
        struct Inputs(Arc<Mutex<Vec<Value>>>);
        impl CallLayer for Inputs {
            fn call<'a>(
                &'a self,
                ctx: &'a mut dataflow_rs::Ctx<'_>,
                next: Next<'a>,
            ) -> BoxFuture<'a, dataflow_rs::Result<Outcome>> {
                let input = ctx.input().map(|v| Value::from(&v)).expect("an input");
                self.0.lock().expect("unpoisoned").push(input);
                next.run(ctx)
            }
        }

        let sign = json!({"algorithm": "HS256", "key": {"secret": "signing"},
                          "claims": {"sub": "u1"}, "expires_in": 300,
                          "output": "data.token"});
        let verify = json!({"token": {"var": "data.token"}, "algorithms": ["HS256"],
                            "keys": [{"algorithm": "HS256", "key": {"secret": "signing"}}],
                            "output": "data.claims"});
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let signed = testing::call(super::super::jwt_sign::JwtSignFunction, sign.clone())
            .exprs(exprs())
            .name("jwt_sign")
            .layer(Inputs(Arc::clone(&inputs)))
            .await;
        assert!(signed.issues.is_empty(), "{:?}", signed.issues);
        assert_eq!(signed.outcome.as_ref().expect("signed"), &Outcome::Ok);
        let sign_changes = format!("{:?}", signed.changes);
        let verified = testing::call(
            JwtVerifyFunction {
                jwks: cache(reqwest::Client::new()),
            },
            verify.clone(),
        )
        .exprs(exprs())
        .name(NAME)
        .layer(Inputs(Arc::clone(&inputs)))
        .message(signed.message)
        .await;
        assert!(verified.issues.is_empty(), "{:?}", verified.issues);
        let v4 = twin::seen(&verified);
        assert_eq!(v4.as_ref().expect("verified")["claims"]["sub"], "u1");

        let inputs = inputs.lock().expect("unpoisoned").clone();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0]["key"], Value::Null, "{inputs:?}");
        assert_eq!(inputs[1]["keys"][0]["key"], Value::Null, "{inputs:?}");
        for seen in [
            inputs[0].to_string(),
            inputs[1].to_string(),
            sign_changes,
            format!("{:?}", verified.changes),
        ] {
            assert!(!seen.contains(HS_SECRET), "the secret leaked: {seen}");
        }

        twin::pinned(&unclocked(&v4), "store_round_trip");
    }
}

//! `jwt_verify` — verify a third-party (or own) JWS mid-workflow (#267
//! Part B): provider id_tokens for social login, refresh tokens, partner
//! assertions. The same shared core and JWKS cache as the channel mode, with
//! task-error semantics so workflows can branch on failure.

use super::definition::{Definition, FieldOverlay, NoConnector, OrionFunction};
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use serde_json::{Map, Value};

use super::schema::{FieldKind, RetrySafety};
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
    /// The compact JWS to verify.
    token: Expr<OwnedDataValue>,
    /// Mandatory non-empty allowlist (HS/RS/PS 256-512, ES256/384, EdDSA); alg: none is unrepresentable.
    algorithms: Vec<String>,
    /// Static verification keys: [{algorithm, key, kid?, key_encoding?}]. Each entry's key
    /// takes {"secret": "name"} or a string; the field itself is always an array. At least one
    /// of keys/jwks_url.
    keys: Option<Vec<JwtKey>>,
    /// HTTPS JWKS URL; cached process-wide with single-flight refresh and stale-serve — the
    /// same cache as the jwt channel mode.
    jwks_url: Option<String>,
    /// Accepted iss value(s); string or array. {"secret": "name"} and env:// references resolve.
    issuer: Option<KeyMaterialList>,
    /// Accepted aud value(s); string or array. {"secret": "name"} and env:// references resolve (OAuth client ids).
    audience: Option<KeyMaterialList>,
    /// Clock-skew allowance for exp/nbf. Default 30, capped at 300.
    leeway_secs: Option<Expr<OwnedDataValue>>,
    /// Whether the token must carry exp. Default true (RFC 8725).
    require_exp: Option<bool>,
    /// Dotted path where the verified claims object is stored. Defaults to "data". Rejections
    /// are typed task errors (continue_on_error branches on them).
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

impl OrionFunction for JwtVerifyFunction {
    type Connector = NoConnector;

    const DEFINITION: Definition = Definition {
        name: NAME,
        description: "Verify a JWT mid-workflow (provider id_tokens, refresh tokens) against static keys or a JWKS.",
        category: "utility",
        retry_safety: RetrySafety::Read,
        fields: &[
            FieldOverlay {
                name: "token",
                kind: Some(FieldKind::String),
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "keys",
                secret_at: &["[].key"],
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "issuer",
                secret_at: &[""],
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "audience",
                secret_at: &[""],
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "leeway_secs",
                kind: Some(FieldKind::Number),
                ..FieldOverlay::DEFAULT
            },
        ],
        validate_static: Some(validate_static_input),
        ..Definition::DEFAULT
    };

    fn from_deps(deps: &crate::HandlerDeps<'_>) -> Self {
        Self {
            jwks: deps.jwks.clone(),
        }
    }
}

impl Function for JwtVerifyFunction {
    type Config = JwtVerifyConfig;

    fn spec(&self) -> FunctionSpec {
        super::definition::spec::<Self>()
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
        let output = Self::DEFINITION.output_path(ctx, &cfg.output)?;
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

#[cfg(test)]
#[path = "jwt_verify_tests.rs"]
mod tests;

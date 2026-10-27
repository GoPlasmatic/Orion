//! Per-channel authentication for the HTTP data plane.
//!
//! `admin_auth` guards `/api/v2/admin` and nothing else, so before this module
//! every data channel was open to anyone who could reach the port. The two
//! controls the documentation pointed at are not substitutes:
//! `origin_allow_list` reads a client-supplied header, and a `validation_logic`
//! header comparison puts the credential in the stored config in plain text and
//! compares it with an early exit.
//!
//! Configuration is [`ChannelAuthConfig`]; this module turns it into a
//! [`CompiledAuth`] once at channel load, so the request path never parses a
//! config, resolves an `env://` reference, or hashes a stored key. Enforcement
//! happens in `guards::apply_guards`, through the transport matrix, so every
//! HTTP ingress inherits it from one place.
//!
//! Two modes ship here. `api_key` covers the common "our other service calls
//! this" case; `hmac` covers inbound webhooks, whose signature is computed over
//! the **raw body** and so must be checked before the JSON is parsed.

// KeyInit is what carries `new_from_slice` from hmac 0.13 on: the constructor
// moved off the concrete Hmac type onto the crypto-common trait, so it has to
// be in scope to be called.
use hmac::Hmac;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use crate::channel::config::{AuthMode, ChannelAuthConfig, JwtSource};
use crate::channel::guards::HeaderLookup;
use crate::config::constant_time_eq;
use dataflow_rs::Expr;
use dataflow_rs::expr::ExprEngine;

use crate::crypto::{Codec, decode_bytes, mac_verify};
use crate::errors::OrionError;

/// A channel's authentication policy, resolved and pre-hashed at load time.
#[derive(Debug, Clone)]
pub enum CompiledAuth {
    ApiKey {
        header: String,
        /// The authentication scheme the header value must carry, e.g.
        /// `"Bearer"` — a name, parsed by `scheme_credential`, not a prefix.
        scheme: Option<String>,
        /// SHA-256 of each accepted key. Digests rather than keys so the
        /// comparison is over a fixed width and reveals neither the length nor
        /// the content of the expected value through timing — the same policy
        /// `admin_auth` applies (S11).
        digests: Vec<[u8; 32]>,
    },
    Hmac(CompiledHmac),
    Jwt(std::sync::Arc<CompiledJwt>),
}

/// What a successful authentication carries besides admission: the verified
/// claims a `jwt` channel exposes at `metadata.auth.claims`. `None` for the
/// party-level modes and for `required: false` requests without a token.
#[derive(Debug, Clone, Default)]
pub struct AuthOutcome {
    pub claims: Option<serde_json::Value>,
}

/// The compiled form of a `jwt` config (#267): the shared verifier plus the
/// channel-side policy (extraction source, optionality, claim filter,
/// authorization logic).
pub struct CompiledJwt {
    verifier: crate::jwt::Verifier,
    required: bool,
    source: JwtSource,
    claims_filter: Option<Vec<String>>,
    /// Compiled `authorization_logic`, evaluated over `{"claims": …}` after
    /// verification with the guards' engine (the process's `ExprEngine`, which
    /// compiled it); falsy → 403 `insufficient_scope`.
    authorization: Option<std::sync::Arc<Expr>>,
}

impl std::fmt::Debug for CompiledJwt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledJwt")
            .field("required", &self.required)
            .field("algorithms", &self.verifier.algorithms)
            .finish_non_exhaustive()
    }
}

/// The compiled form of an `hmac` config (#264): template, extraction rules,
/// algorithm, encoding, replay window, and the resolved secrets.
#[derive(Debug, Clone)]
pub struct CompiledHmac {
    /// Header carrying the presented MAC.
    header: String,
    /// How the MAC is pulled out of that header's value.
    extraction: SigExtraction,
    /// Pinned signature encoding; `None` keeps the pre-#264 auto-detection.
    encoding: Option<Codec>,
    algorithm: HmacAlgorithm,
    /// The signing-string template, compiled to segments.
    message: Vec<Segment>,
    /// Replay window: where the unix-seconds timestamp lives and how far from
    /// now it may be.
    timestamp: Option<TimestampSpec>,
    /// Every accepted secret — several during rotation — tried in constant
    /// time each.
    secrets: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
enum SigExtraction {
    /// The header value, minus an optional prefix (`sha256=`, `v0=`).
    Prefix(Option<String>),
    /// The value(s) of one key of a comma-separated `k=v` packed header —
    /// Stripe's `t=<ts>,v1=<sig>[,v1=<sig>]` shape. Every occurrence is tried,
    /// which is what makes provider-side signing-secret rotation work.
    Key(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HmacAlgorithm {
    Sha1,
    Sha256,
    Sha512,
}

/// One piece of the signing-string template.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Body,
    Header(String),
    /// `{header:<name>:<key>}` — the `<key>` value of a packed `k=v` header.
    HeaderPart(String, String),
}

/// `<header>` or `<header>:<key>` naming where the unix-seconds timestamp
/// lives, plus the accepted window.
#[derive(Debug, Clone)]
struct TimestampSpec {
    header: String,
    key: Option<String>,
    tolerance_secs: u64,
}

/// The refusal every failure path returns.
///
/// One message for every cause — wrong key, absent header, malformed
/// signature — because distinguishing them tells an unauthenticated caller
/// which half of the credential they got right.
pub(crate) fn refused() -> OrionError {
    OrionError::unauthorized("Channel authentication failed")
}

impl CompiledAuth {
    /// Resolve secrets and pre-hash keys. `Err` is a human-readable reason, and
    /// the caller turns it into a channel load issue: a channel whose auth
    /// cannot be built is quarantined rather than served without it (F35).
    /// `exprs` compiles `authorization_logic`; it must be the engine the
    /// guards evaluate with (the process's `ExprEngine`, built at boot). `None`
    /// is for callers that cannot evaluate logic — compiling a config that
    /// carries `authorization_logic` then fails loudly. `jwks` is the
    /// instance's key cache and follows the same rule for `auth.jwks_url`.
    pub async fn compile(
        cfg: &ChannelAuthConfig,
        exprs: Option<&ExprEngine>,
        jwks: Option<&std::sync::Arc<crate::jwt::jwks::JwksCache>>,
    ) -> Result<Self, String> {
        match cfg.mode {
            AuthMode::ApiKey => {
                let keys = cfg
                    .keys
                    .as_ref()
                    .filter(|k| !k.is_empty())
                    .ok_or("auth.mode = \"api_key\" requires a non-empty auth.keys")?;

                let header = cfg
                    .header
                    .clone()
                    .unwrap_or_else(|| "Authorization".to_string());
                let scheme = api_key_scheme(cfg, &header)?;

                let mut digests = Vec::with_capacity(keys.len());
                for key in keys {
                    let resolved = resolve_secret(key, "auth.keys").await?;
                    if resolved.is_empty() {
                        return Err("auth.keys contains an empty key".to_string());
                    }
                    digests.push(Sha256::digest(resolved.as_bytes()).into());
                }

                Ok(Self::ApiKey {
                    header,
                    scheme,
                    digests,
                })
            }
            AuthMode::Hmac => {
                let (plan, secret_refs) = hmac_plan(cfg)?;
                let mut secrets = Vec::with_capacity(secret_refs.len());
                for reference in &secret_refs {
                    let resolved = resolve_secret(reference, "auth.secret").await?;
                    if resolved.is_empty() {
                        return Err("auth.secret resolved to an empty value".to_string());
                    }
                    secrets.push(resolved.into_bytes());
                }
                Ok(Self::Hmac(CompiledHmac {
                    header: plan.header,
                    extraction: plan.extraction,
                    encoding: plan.encoding,
                    algorithm: plan.algorithm,
                    message: plan.message,
                    timestamp: plan.timestamp,
                    secrets,
                }))
            }
            AuthMode::Jwt => {
                let plan = jwt_plan(cfg)?;
                let mut static_keys = Vec::with_capacity(plan.keys.len());
                for entry in &plan.keys {
                    let material = resolve_secret(&entry.key, "auth.jwt_keys").await?;
                    let key = crate::jwt::decoding_key(
                        entry.algorithm,
                        &material,
                        entry.key_encoding.as_deref(),
                    )
                    .map_err(|e| format!("auth.jwt_keys: {e}"))?;
                    static_keys.push(crate::jwt::StaticKey {
                        kid: entry.kid.clone(),
                        algorithm: entry.algorithm,
                        key,
                    });
                }
                let authorization = match &cfg.authorization_logic {
                    None => None,
                    Some(logic) => {
                        let engine = exprs.ok_or(
                            "auth.authorization_logic needs the shared JSONLogic engine, \
                             which this caller cannot supply",
                        )?;
                        Some(std::sync::Arc::new(
                            engine
                                .compile(logic)
                                .map_err(|e| format!("auth.authorization_logic: {e}"))?,
                        ))
                    }
                };
                let jwks = match plan.jwks_url {
                    None => None,
                    Some(url) => {
                        let cache = jwks.ok_or(
                            "auth.jwks_url needs the shared JWKS cache, \
                             which this caller cannot supply",
                        )?;
                        Some(crate::jwt::JwksSource {
                            url,
                            cache: cache.clone(),
                        })
                    }
                };
                Ok(Self::Jwt(std::sync::Arc::new(CompiledJwt {
                    verifier: crate::jwt::Verifier {
                        static_keys,
                        jwks,
                        algorithms: plan.algorithms,
                        issuer: plan.issuer,
                        audience: plan.audience,
                        leeway_secs: plan.leeway_secs,
                        require_exp: plan.require_exp,
                        max_token_bytes: plan.max_token_bytes,
                        validations: std::sync::OnceLock::new(),
                    },
                    required: plan.required,
                    source: plan.source,
                    claims_filter: cfg.claims_to_metadata.clone(),
                    authorization,
                })))
            }
        }
    }

    /// The structural half of [`CompiledAuth::compile`] — everything except
    /// secret resolution, which stays load-time so a bundle validates on a
    /// host without production secrets. This is what create/update/validate/
    /// import run (the activation-time fix of #264): a broken auth config is
    /// a 400 naming the problem, not a reload-time quarantine.
    pub fn validate_config(cfg: &ChannelAuthConfig) -> Result<(), String> {
        match cfg.mode {
            AuthMode::ApiKey => {
                let keys = cfg
                    .keys
                    .as_ref()
                    .filter(|k| !k.is_empty())
                    .ok_or("auth.mode = \"api_key\" requires a non-empty auth.keys")?;
                if keys.iter().any(|k| k.trim().is_empty()) {
                    return Err("auth.keys contains an empty key".to_string());
                }
                api_key_scheme(cfg, cfg.header.as_deref().unwrap_or("Authorization"))?;
                Ok(())
            }
            AuthMode::Hmac => hmac_plan(cfg).map(|_| ()),
            AuthMode::Jwt => {
                let _ = jwt_plan(cfg)?;
                if let Some(logic) = &cfg.authorization_logic {
                    // Operator parity with the runtime guard engine, the same
                    // property `validate_channel_config_blob` checks for
                    // validation_logic.
                    let engine = crate::engine::operators::add_to_datalogic(
                        dataflow_rs::datalogic_rs::Engine::builder(),
                    )
                    .build();
                    engine
                        .compile(logic)
                        .map_err(|e| format!("auth.authorization_logic does not compile: {e}"))?;
                }
                Ok(())
            }
        }
    }

    /// Authenticate one request, returning what admission carries onward —
    /// for `jwt`, the verified claims.
    ///
    /// `raw_body` is the bytes exactly as received. HMAC is defined over them,
    /// so it must be checked before any parse: re-serializing parsed JSON
    /// reorders keys and normalises whitespace, and the signature would never
    /// match again. `exprs` evaluates `authorization_logic`; it is the
    /// same engine that compiled it.
    /// Whether a failure of this mode is evidence of *guessing*, and so worth
    /// counting against a client's backoff budget.
    ///
    /// `api_key` and `hmac` are: every failure is a wrong credential, and a
    /// legitimate client never produces one. `jwt` is deliberately not — an
    /// expired token is a routine, legitimate failure that a well-behaved
    /// client answers by refreshing (it is the one reason `RejectReason`
    /// surfaces on the wire for exactly that purpose), and locking a client
    /// out for it would punish the correct behaviour.
    pub fn failures_are_guesses(&self) -> bool {
        matches!(self, Self::ApiKey { .. } | Self::Hmac(_))
    }

    pub async fn authenticate(
        &self,
        header: HeaderLookup<'_, '_>,
        raw_body: Option<&[u8]>,
        exprs: &ExprEngine,
    ) -> Result<AuthOutcome, OrionError> {
        match self {
            Self::ApiKey {
                header: name,
                scheme,
                digests,
            } => {
                let presented = header(name).ok_or_else(refused)?;
                let credential = match scheme {
                    Some(scheme) => scheme_credential(presented, scheme).map_err(|_| refused())?,
                    None => presented,
                };
                let digest: [u8; 32] = Sha256::digest(credential.as_bytes()).into();
                if digests.iter().any(|d| constant_time_eq(&digest, d)) {
                    Ok(AuthOutcome::default())
                } else {
                    Err(refused())
                }
            }
            Self::Hmac(hmac) => hmac
                .authenticate_at(header, raw_body, chrono::Utc::now().timestamp())
                .map(|()| AuthOutcome::default()),
            Self::Jwt(jwt) => jwt.authenticate(header, exprs).await,
        }
    }
}

impl CompiledJwt {
    async fn authenticate(
        &self,
        header: HeaderLookup<'_, '_>,
        exprs: &ExprEngine,
    ) -> Result<AuthOutcome, OrionError> {
        use crate::jwt::RejectReason;

        // 1. Extract per `source`. RFC 6750: no query parameters, ever.
        let token: Option<String> = match &self.source {
            JwtSource::Header {
                header: name,
                scheme,
            } => match header(name) {
                None => None,
                Some(value) => match scheme {
                    Some(scheme) => {
                        // A present header with the wrong scheme is a bad
                        // presentation, not a missing token, so `required:
                        // false` does not admit it.
                        let credential = scheme_credential(value, scheme).map_err(|refusal| {
                            self.refuse(match refusal {
                                SchemeRefusal::Mismatch => RejectReason::SchemeMismatch,
                                SchemeRefusal::NoCredential => RejectReason::Malformed,
                            })
                        })?;
                        Some(credential.to_string())
                    }
                    None => Some(value.to_string()),
                },
            },
            // One RFC 6265 parser for the whole estate (#270), which fixes the
            // quoted-value and loose-whitespace defects the inline version
            // had. `HeaderLookup` yields a single value, so a jar split across
            // several `cookie` headers is still only searched in the first —
            // the transport behind this abstraction (channel_call) has no
            // multi-valued headers to justify widening it.
            JwtSource::Cookie { cookie } => {
                header("cookie").and_then(|jar| super::cookies::lookup([jar], cookie.as_str()))
            }
        };
        let Some(token) = token else {
            // "Optional" means a MISSING token proceeds identity-less; a
            // present-but-invalid one is still rejected below.
            if self.required {
                return Err(self.refuse(RejectReason::Missing));
            }
            return Ok(AuthOutcome { claims: None });
        };

        // 2-5. The shared pipeline: allowlist, key routing (JWKS included),
        // signature, temporal/identity claims.
        let claims = self
            .verifier
            .verify(&token)
            .await
            .map_err(|reason| self.refuse(reason))?;

        // 6. Authorization: verified identity, insufficient rights → 403
        // (RFC 6750 insufficient_scope), distinct from every 401 above. The
        // claims move into the eval context and back out afterwards — the
        // evaluation only borrows it, and `json!` would deep-copy the whole
        // claims object on every request.
        let claims = if let Some(logic) = &self.authorization {
            let mut context = serde_json::Map::with_capacity(1);
            context.insert("claims".to_string(), claims);
            let context = serde_json::Value::Object(context);
            let allowed = exprs
                .eval(logic, &context)
                .map(|v| super::guards::is_truthy(&v.value))
                .unwrap_or(false);
            if !allowed {
                return Err(OrionError::forbidden(
                    "insufficient_scope: the token's claims do not authorize this request"
                        .to_string(),
                ));
            }
            match context {
                serde_json::Value::Object(mut map) => map.remove("claims").expect("inserted above"),
                _ => unreachable!("built as an object above"),
            }
        } else {
            claims
        };

        // 7. Filter and expose. The token string dies here.
        let exposed = match &self.claims_filter {
            None => claims,
            Some(filter) => {
                let mut filtered = serde_json::Map::new();
                if let Some(obj) = claims.as_object() {
                    for name in filter {
                        if let Some(value) = obj.get(name) {
                            filtered.insert(name.clone(), value.clone());
                        }
                    }
                }
                serde_json::Value::Object(filtered)
            }
        };
        Ok(AuthOutcome {
            claims: Some(exposed),
        })
    }

    /// One uniform 401 body for every cause; the typed reason goes to metrics
    /// and the trace, and its RFC 6750 challenge to `WWW-Authenticate`.
    fn refuse(&self, reason: crate::jwt::RejectReason) -> OrionError {
        crate::metrics::record_jwt_rejection(reason.as_str());
        tracing::debug!(reason = reason.as_str(), "JWT rejected");
        OrionError::UnauthorizedToken {
            message: "Channel authentication failed".to_string(),
            challenge: reason.challenge(),
        }
    }
}

impl CompiledHmac {
    /// Verify one request at instant `now` (a parameter so the replay window
    /// is testable; production passes the clock).
    fn authenticate_at(
        &self,
        header: HeaderLookup<'_, '_>,
        raw_body: Option<&[u8]>,
        now: i64,
    ) -> Result<(), OrionError> {
        // 1. Extract the presented signature(s).
        let presented = header(&self.header).ok_or_else(refused)?;
        let candidates: Vec<&str> = match &self.extraction {
            SigExtraction::Prefix(prefix) => {
                let one = match prefix {
                    Some(p) => presented.strip_prefix(p.as_str()).ok_or_else(refused)?,
                    None => presented,
                };
                vec![one]
            }
            SigExtraction::Key(key) => {
                let values = packed_values(presented, key);
                if values.is_empty() {
                    return Err(refused());
                }
                values
            }
        };

        // 2. Replay window, before any MAC work. Same refusal as every other
        // failure — staleness must not be distinguishable from a bad MAC.
        if let Some(spec) = &self.timestamp {
            let raw = match &spec.key {
                None => header(&spec.header).ok_or_else(refused)?,
                Some(key) => {
                    let value = header(&spec.header).ok_or_else(refused)?;
                    packed_values(value, key)
                        .first()
                        .copied()
                        .ok_or_else(refused)?
                }
            };
            let ts: i64 = raw.trim().parse().map_err(|_| refused())?;
            if (now - ts).unsigned_abs() > spec.tolerance_secs {
                return Err(refused());
            }
        }

        // 3. Assemble the signing string. A template header missing from the
        // request refuses — never empty-string substitution, or an attacker
        // could shrink the signed message by dropping a header.
        //
        // An absent body is an empty one: a signed GET webhook signs zero
        // bytes, and treating that as "cannot verify" would refuse a
        // legitimately signed request.
        let body = raw_body.unwrap_or(&[]);
        let mut message: Vec<u8> = Vec::new();
        for segment in &self.message {
            match segment {
                Segment::Literal(text) => message.extend_from_slice(text.as_bytes()),
                Segment::Body => message.extend_from_slice(body),
                Segment::Header(name) => {
                    let value = header(name).ok_or_else(refused)?;
                    message.extend_from_slice(value.as_bytes());
                }
                Segment::HeaderPart(name, key) => {
                    let value = header(name).ok_or_else(refused)?;
                    let part = packed_values(value, key)
                        .first()
                        .copied()
                        .ok_or_else(refused)?;
                    message.extend_from_slice(part.as_bytes());
                }
            }
        }

        // 4. Verify every candidate against every accepted secret — several of
        // each during rotation. `verify_slice` is constant-time and
        // length-checked per attempt.
        for candidate in candidates {
            let Some(signature) = self.decode(candidate) else {
                continue;
            };
            for secret in &self.secrets {
                let ok = match self.algorithm {
                    HmacAlgorithm::Sha1 => mac_verify::<Hmac<Sha1>>(secret, &message, &signature),
                    HmacAlgorithm::Sha256 => {
                        mac_verify::<Hmac<Sha256>>(secret, &message, &signature)
                    }
                    HmacAlgorithm::Sha512 => {
                        mac_verify::<Hmac<Sha512>>(secret, &message, &signature)
                    }
                };
                if ok {
                    return Ok(());
                }
            }
        }
        Err(refused())
    }

    /// Decode one presented signature: the pinned encoding when configured,
    /// else auto-detection (hex first — unambiguous at MAC lengths — then
    /// standard base64).
    ///
    /// Both arms go through [`decode_bytes`], which is the point. A fallback
    /// decoding with `general_purpose::STANDARD` would require canonical
    /// padding while the pinned arm, through `B64_STD_LENIENT`
    /// (`DecodePaddingMode::Indifferent`), does not — so an **unpadded**
    /// standard-base64 signature would verify with `auth.encoding = "base64"`
    /// set and be refused as malformed without it: the same webhook, two
    /// answers, turning on a field an operator may reasonably leave unset.
    /// `crypto.rs`'s header names `channel::auth` as one of its consumers so
    /// there is one spelling of each primitive.
    ///
    /// Widening the decode cannot weaken anything: the bytes still have to
    /// match the HMAC, and a signature that decodes differently simply fails
    /// that comparison instead of this one.
    fn decode(&self, presented: &str) -> Option<Vec<u8>> {
        match self.encoding {
            Some(codec) => decode_bytes(codec, presented).ok(),
            None => hex::decode(presented)
                .ok()
                .or_else(|| decode_bytes(Codec::Base64, presented).ok()),
        }
    }
}

/// The value(s) of `key` in a comma-separated `k=v` packed header
/// (`t=1699999999,v1=abc,v1=def`). Whitespace around pairs is tolerated;
/// keys match exactly.
fn packed_values<'v>(header_value: &'v str, key: &str) -> Vec<&'v str> {
    header_value
        .split(',')
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| v)
        .collect()
}

/// The structural plan of an `hmac` config: everything compiled except the
/// secrets, which come back as unresolved references so [`CompiledAuth::
/// validate_config`] can run on hosts that hold none of them.
struct HmacPlan {
    header: String,
    extraction: SigExtraction,
    encoding: Option<Codec>,
    algorithm: HmacAlgorithm,
    message: Vec<Segment>,
    timestamp: Option<TimestampSpec>,
}

/// One preset row — pure data expanding to the explicit fields. An explicitly
/// set config field overrides its row, so `preset: "slack", tolerance_secs:
/// 60` tightens Slack's window without restating the rest.
struct PresetRow {
    header: &'static str,
    signature_prefix: Option<&'static str>,
    signature_key: Option<&'static str>,
    algorithm: &'static str,
    message: &'static str,
    encoding: Option<&'static str>,
    timestamp: Option<&'static str>,
    tolerance_secs: Option<u64>,
}

/// The provider presets. A new provider is one row here (and its line in
/// `channel-config.md`) — configuration, never code.
fn preset_row(name: &str) -> Option<PresetRow> {
    Some(match name {
        "zoom" => PresetRow {
            header: "x-zm-signature",
            signature_prefix: Some("v0="),
            signature_key: None,
            algorithm: "sha256",
            message: "v0:{header:x-zm-request-timestamp}:{body}",
            encoding: Some("hex"),
            timestamp: Some("x-zm-request-timestamp"),
            tolerance_secs: Some(300),
        },
        "slack" => PresetRow {
            header: "x-slack-signature",
            signature_prefix: Some("v0="),
            signature_key: None,
            algorithm: "sha256",
            message: "v0:{header:x-slack-request-timestamp}:{body}",
            encoding: Some("hex"),
            timestamp: Some("x-slack-request-timestamp"),
            tolerance_secs: Some(300),
        },
        "stripe" => PresetRow {
            header: "stripe-signature",
            signature_prefix: None,
            signature_key: Some("v1"),
            algorithm: "sha256",
            message: "{header:stripe-signature:t}.{body}",
            encoding: Some("hex"),
            timestamp: Some("stripe-signature:t"),
            tolerance_secs: Some(300),
        },
        "github" => PresetRow {
            header: "x-hub-signature-256",
            signature_prefix: Some("sha256="),
            signature_key: None,
            algorithm: "sha256",
            message: "{body}",
            encoding: Some("hex"),
            timestamp: None,
            tolerance_secs: None,
        },
        "shopify" => PresetRow {
            header: "x-shopify-hmac-sha256",
            signature_prefix: None,
            signature_key: None,
            algorithm: "sha256",
            message: "{body}",
            encoding: Some("base64"),
            timestamp: None,
            tolerance_secs: None,
        },
        "webex" => PresetRow {
            header: "x-spark-signature",
            signature_prefix: None,
            signature_key: None,
            algorithm: "sha1",
            message: "{body}",
            encoding: Some("hex"),
            timestamp: None,
            tolerance_secs: None,
        },
        _ => return None,
    })
}

const PRESET_NAMES: &[&str] = &["zoom", "slack", "stripe", "github", "shopify", "webex"];

/// Build the structural plan: preset expansion (explicit fields win), value
/// tables, template parse, and the cross-field rules. Returns the plan plus
/// the unresolved secret references.
fn hmac_plan(cfg: &ChannelAuthConfig) -> Result<(HmacPlan, Vec<String>), String> {
    let preset = match cfg.preset.as_deref() {
        None => None,
        Some(name) => Some(preset_row(name).ok_or_else(|| {
            format!(
                "auth.preset '{name}' is not known — one of {}",
                PRESET_NAMES.join(", ")
            )
        })?),
    };
    let row = |explicit: Option<&str>, from_preset: Option<&str>| {
        explicit
            .map(str::to_string)
            .or(from_preset.map(str::to_string))
    };

    let header = row(cfg.header.as_deref(), preset.as_ref().map(|p| p.header))
        .unwrap_or_else(|| "X-Signature".to_string());
    let signature_prefix = row(
        cfg.signature_prefix.as_deref(),
        preset.as_ref().and_then(|p| p.signature_prefix),
    );
    let signature_key = row(
        cfg.signature_key.as_deref(),
        preset.as_ref().and_then(|p| p.signature_key),
    );
    let extraction = match (signature_prefix, signature_key) {
        (Some(_), Some(_)) => {
            return Err(
                "auth.signature_prefix and auth.signature_key are mutually exclusive — \
                 a signature is either prefix-stripped or extracted from a packed header"
                    .to_string(),
            );
        }
        (prefix, None) => SigExtraction::Prefix(prefix),
        (None, Some(key)) => SigExtraction::Key(key),
    };

    let algorithm = match row(
        cfg.algorithm.as_deref(),
        preset.as_ref().map(|p| p.algorithm),
    )
    .as_deref()
    {
        None | Some("sha256") => HmacAlgorithm::Sha256,
        Some("sha1") => HmacAlgorithm::Sha1,
        Some("sha512") => HmacAlgorithm::Sha512,
        Some(other) => {
            return Err(format!(
                "auth.algorithm '{other}' is not supported — sha1, sha256, sha512"
            ));
        }
    };

    let encoding = match row(
        cfg.encoding.as_deref(),
        preset.as_ref().and_then(|p| p.encoding),
    )
    .as_deref()
    {
        None => None,
        Some(name) => Some(Codec::parse(name).ok_or_else(|| {
            format!(
                "auth.encoding '{name}' is not supported — hex, base64, base64url \
                 (omit it for auto-detection)"
            )
        })?),
    };

    let message = parse_template(
        &row(cfg.message.as_deref(), preset.as_ref().map(|p| p.message))
            .unwrap_or_else(|| "{body}".to_string()),
    )?;

    let timestamp_where = row(
        cfg.timestamp.as_deref(),
        preset.as_ref().and_then(|p| p.timestamp),
    );
    let tolerance = cfg
        .tolerance_secs
        .or(preset.as_ref().and_then(|p| p.tolerance_secs));
    let timestamp = match (timestamp_where, tolerance) {
        (None, None) => None,
        (Some(spec), Some(tolerance_secs)) => {
            if tolerance_secs == 0 {
                return Err("auth.tolerance_secs must be positive".to_string());
            }
            let (header, key) = match spec.split_once(':') {
                Some((h, k)) if !h.is_empty() && !k.is_empty() => {
                    (h.to_string(), Some(k.to_string()))
                }
                Some(_) => {
                    return Err("auth.timestamp must be '<header>' or '<header>:<key>'".to_string());
                }
                None if !spec.is_empty() => (spec, None),
                None => return Err("auth.timestamp must name a header".to_string()),
            };
            Some(TimestampSpec {
                header,
                key,
                tolerance_secs,
            })
        }
        // Half a replay guard is a config mistake, not a default.
        (Some(_), None) => {
            return Err("auth.timestamp requires auth.tolerance_secs".to_string());
        }
        (None, Some(_)) => {
            return Err("auth.tolerance_secs requires auth.timestamp".to_string());
        }
    };

    let mut secret_refs: Vec<String> = Vec::new();
    if let Some(secret) = &cfg.secret {
        secret_refs.push(secret.clone());
    }
    if let Some(more) = &cfg.secrets {
        secret_refs.extend(more.iter().cloned());
    }
    if secret_refs.is_empty() {
        return Err("auth.mode = \"hmac\" requires auth.secret (or auth.secrets)".to_string());
    }
    if secret_refs.iter().any(|s| s.trim().is_empty()) {
        return Err("auth.secrets contains an empty entry".to_string());
    }

    Ok((
        HmacPlan {
            header,
            extraction,
            encoding,
            algorithm,
            message,
            timestamp,
        },
        secret_refs,
    ))
}

/// Parse the signing-string template. Strict: a `{` always opens a
/// placeholder, `}` outside one is an error, and `{body}` must appear —
/// a template that never covers the payload verifies nothing about it.
fn parse_template(template: &str) -> Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if !literal.is_empty() {
                    segments.push(Segment::Literal(std::mem::take(&mut literal)));
                }
                let mut placeholder = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(inner) => placeholder.push(inner),
                        None => {
                            return Err(format!(
                                "auth.message has an unterminated '{{' before the end: \
                                 '{template}'"
                            ));
                        }
                    }
                }
                segments.push(parse_placeholder(&placeholder)?);
            }
            '}' => {
                return Err(format!(
                    "auth.message has a stray '}}' outside a placeholder: '{template}'"
                ));
            }
            other => literal.push(other),
        }
    }
    if !literal.is_empty() {
        segments.push(Segment::Literal(literal));
    }
    if !segments.contains(&Segment::Body) {
        return Err(
            "auth.message must contain {body} — a template that never covers the \
             payload verifies nothing about it"
                .to_string(),
        );
    }
    Ok(segments)
}

fn parse_placeholder(placeholder: &str) -> Result<Segment, String> {
    if placeholder == "body" {
        return Ok(Segment::Body);
    }
    if let Some(rest) = placeholder.strip_prefix("header:") {
        return match rest.split_once(':') {
            Some((name, key)) if !name.is_empty() && !key.is_empty() => {
                Ok(Segment::HeaderPart(name.to_string(), key.to_string()))
            }
            Some(_) => Err(format!(
                "auth.message placeholder '{{{placeholder}}}' is malformed — \
                 {{header:<name>}} or {{header:<name>:<key>}}"
            )),
            None if !rest.is_empty() => Ok(Segment::Header(rest.to_string())),
            None => Err("auth.message placeholder {header:} names no header".to_string()),
        };
    }
    Err(format!(
        "auth.message placeholder '{{{placeholder}}}' is not known — \
         {{body}}, {{header:<name>}}, or {{header:<name>:<key>}}"
    ))
}

/// The structural plan of a `jwt` config — everything except secret
/// resolution and `authorization_logic` compilation.
struct JwtPlan {
    keys: Vec<JwtKeyPlan>,
    jwks_url: Option<String>,
    algorithms: Vec<jsonwebtoken::Algorithm>,
    issuer: Vec<String>,
    audience: Vec<String>,
    leeway_secs: u64,
    require_exp: bool,
    required: bool,
    source: JwtSource,
    max_token_bytes: usize,
}

struct JwtKeyPlan {
    algorithm: jsonwebtoken::Algorithm,
    key: String,
    kid: Option<String>,
    key_encoding: Option<String>,
}

fn jwt_plan(cfg: &ChannelAuthConfig) -> Result<JwtPlan, String> {
    let algorithm_names = cfg
        .algorithms
        .as_ref()
        .filter(|a| !a.is_empty())
        .ok_or("auth.mode = \"jwt\" requires a non-empty auth.algorithms allowlist")?;
    let mut algorithms = Vec::with_capacity(algorithm_names.len());
    for name in algorithm_names {
        algorithms
            .push(crate::jwt::parse_algorithm(name).map_err(|e| format!("auth.algorithms: {e}"))?);
    }

    let mut keys = Vec::new();
    for entry in cfg.jwt_keys.as_deref().unwrap_or_default() {
        let algorithm = crate::jwt::parse_algorithm(&entry.algorithm)
            .map_err(|e| format!("auth.jwt_keys: {e}"))?;
        if entry.key.trim().is_empty() {
            return Err("auth.jwt_keys contains an empty key".to_string());
        }
        keys.push(JwtKeyPlan {
            algorithm,
            key: entry.key.clone(),
            kid: entry.kid.clone(),
            key_encoding: entry.key_encoding.clone(),
        });
    }

    let jwks_url = match &cfg.jwks_url {
        None => None,
        Some(url) => {
            crate::jwt::validate_jwks_url(url).map_err(|e| format!("auth.jwks_url {e}"))?;
            Some(url.clone())
        }
    };
    if keys.is_empty() && jwks_url.is_none() {
        return Err("auth.mode = \"jwt\" requires auth.jwt_keys and/or auth.jwks_url".to_string());
    }

    let leeway_secs = cfg.leeway_secs.unwrap_or(crate::jwt::DEFAULT_LEEWAY_SECS);
    if leeway_secs > crate::jwt::MAX_LEEWAY_SECS {
        return Err(format!(
            "auth.leeway_secs must be at most {}, got {leeway_secs}",
            crate::jwt::MAX_LEEWAY_SECS
        ));
    }

    let source = match cfg.source.clone() {
        None => JwtSource::Header {
            header: "authorization".to_string(),
            scheme: Some("Bearer".to_string()),
        },
        Some(JwtSource::Header { header, scheme }) => {
            if header.trim().is_empty() {
                return Err("auth.source.header must name a header".to_string());
            }
            let scheme = match scheme.as_deref() {
                Some(raw) => scheme_name("auth.source.scheme", raw)?,
                None => None,
            };
            JwtSource::Header { header, scheme }
        }
        Some(JwtSource::Cookie { cookie }) => {
            if cookie.trim().is_empty() {
                return Err("auth.source.cookie must name a cookie".to_string());
            }
            JwtSource::Cookie { cookie }
        }
    };

    Ok(JwtPlan {
        keys,
        jwks_url,
        algorithms,
        issuer: cfg
            .issuer
            .as_ref()
            .map(|v| v.into_vec())
            .unwrap_or_default(),
        audience: cfg
            .audience
            .as_ref()
            .map(|v| v.into_vec())
            .unwrap_or_default(),
        leeway_secs,
        require_exp: cfg.require_exp.unwrap_or(true),
        required: cfg.required.unwrap_or(true),
        source,
        max_token_bytes: cfg
            .max_token_bytes
            .unwrap_or(crate::jwt::DEFAULT_MAX_TOKEN_BYTES),
    })
}

/// Why a presented header yielded no credential under a scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemeRefusal {
    /// The value names another scheme, or none: `Basic …`, `Bearer<token>`.
    Mismatch,
    /// The scheme, with nothing after it.
    NoCredential,
}

/// The credential `value` presents under `scheme`, parsed as RFC 9110 §11.1
/// defines an `Authorization` value: `auth-scheme [ 1*SP ( token68 /
/// #auth-param ) ]`.
///
/// The scheme is an ABNF literal, so it compares case-insensitively, and any
/// run of spaces separates it from the credential. A byte `strip_prefix` over
/// a configured `"Bearer "` would refuse `bearer <tok>` and `Bearer  <tok>`
/// from spec-conforming clients and make the delimiter part of the configured
/// value (#331). The credential comes back byte-exact:
/// an `api_key` digest is over exactly what was sent.
pub fn scheme_credential<'v>(value: &'v str, scheme: &str) -> Result<&'v str, SchemeRefusal> {
    let (name, rest) = value.split_once(' ').unwrap_or((value, ""));
    if !name.eq_ignore_ascii_case(scheme) {
        return Err(SchemeRefusal::Mismatch);
    }
    match rest.trim_start_matches(' ') {
        "" => Err(SchemeRefusal::NoCredential),
        credential => Ok(credential),
    }
}

/// A configured scheme, normalised to the name [`scheme_credential`] compares.
///
/// Surrounding whitespace is dropped, so the prefix spelling `"Bearer "` and
/// the natural one, `"Bearer"`, are the same scheme — read as a byte prefix,
/// the second would refuse every caller and pass every offline check (#331).
/// An empty value is no scheme: the bare credential it always meant.
/// Anything else must be an RFC 9110 token, because a value like `"Key="` can
/// never match a scheme name and would otherwise refuse every request without
/// saying why.
fn scheme_name(field: &str, raw: &str) -> Result<Option<String>, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if !name.bytes().all(super::cookies::is_token_byte) {
        return Err(format!(
            "{field} '{raw}' is not an HTTP authentication scheme name — a scheme is a \
             token such as \"Bearer\", and the space before the credential is not part of it"
        ));
    }
    Ok(Some(name.to_string()))
}

/// The scheme an `api_key` header must carry: the configured one, else
/// `Bearer` on `Authorization` — the conventional spelling — and none on a
/// custom header like `X-API-Key`, which carries the bare key.
fn api_key_scheme(cfg: &ChannelAuthConfig, header: &str) -> Result<Option<String>, String> {
    match cfg.scheme.as_deref() {
        Some(raw) => scheme_name("auth.scheme", raw),
        None if header.eq_ignore_ascii_case("authorization") => Ok(Some("Bearer".to_string())),
        None => Ok(None),
    }
}

/// Resolve an `env://VAR` reference, or pass a literal through.
///
/// The same resolver connector secrets use, so an operator has one mechanism to
/// learn and production credentials never have to sit in a stored channel
/// config.
async fn resolve_secret(value: &str, field: &str) -> Result<String, String> {
    crate::connector::secrets::resolve_secret_string(value, field).await
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;

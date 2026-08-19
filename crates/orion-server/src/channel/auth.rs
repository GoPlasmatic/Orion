//! Per-channel authentication for the HTTP data plane.
//!
//! `admin_auth` guards `/api/v1/admin` and nothing else, so before this module
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
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use crate::channel::config::{AuthMode, ChannelAuthConfig};
use crate::channel::guards::HeaderLookup;
use crate::config::constant_time_eq;
use crate::engine::operators::{Codec, decode_bytes};
use crate::errors::OrionError;

/// A channel's authentication policy, resolved and pre-hashed at load time.
#[derive(Debug, Clone)]
pub enum CompiledAuth {
    ApiKey {
        header: String,
        /// Prefix stripped from the header value before comparison, e.g.
        /// `"Bearer "`.
        scheme: Option<String>,
        /// SHA-256 of each accepted key. Digests rather than keys so the
        /// comparison is over a fixed width and reveals neither the length nor
        /// the content of the expected value through timing — the same policy
        /// `admin_auth` applies (S11).
        digests: Vec<[u8; 32]>,
    },
    Hmac(CompiledHmac),
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
fn refused() -> OrionError {
    OrionError::Unauthorized("Channel authentication failed".into())
}

impl CompiledAuth {
    /// Resolve secrets and pre-hash keys. `Err` is a human-readable reason, and
    /// the caller turns it into a channel load issue: a channel whose auth
    /// cannot be built is quarantined rather than served without it (F35).
    pub async fn compile(cfg: &ChannelAuthConfig) -> Result<Self, String> {
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
                // `Authorization: Bearer <key>` is the conventional spelling and
                // the one an unset `scheme` should produce; a custom header like
                // `X-API-Key` carries the bare key.
                let scheme = match cfg.scheme {
                    Some(ref s) => Some(s.clone()),
                    None if header.eq_ignore_ascii_case("authorization") => {
                        Some("Bearer ".to_string())
                    }
                    None => None,
                };

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
                Ok(())
            }
            AuthMode::Hmac => hmac_plan(cfg).map(|_| ()),
        }
    }

    /// Authenticate one request.
    ///
    /// `raw_body` is the bytes exactly as received. HMAC is defined over them,
    /// so it must be checked before any parse: re-serializing parsed JSON
    /// reorders keys and normalises whitespace, and the signature would never
    /// match again.
    pub fn authenticate(
        &self,
        header: HeaderLookup<'_>,
        raw_body: Option<&[u8]>,
    ) -> Result<(), OrionError> {
        match self {
            Self::ApiKey {
                header: name,
                scheme,
                digests,
            } => {
                let presented = header(name).ok_or_else(refused)?;
                let presented = match scheme {
                    Some(prefix) => presented
                        .strip_prefix(prefix.as_str())
                        .ok_or_else(refused)?
                        .to_string(),
                    None => presented,
                };
                let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
                if digests.iter().any(|d| constant_time_eq(&digest, d)) {
                    Ok(())
                } else {
                    Err(refused())
                }
            }
            Self::Hmac(hmac) => {
                hmac.authenticate_at(header, raw_body, chrono::Utc::now().timestamp())
            }
        }
    }
}

impl CompiledHmac {
    /// Verify one request at instant `now` (a parameter so the replay window
    /// is testable; production passes the clock).
    fn authenticate_at(
        &self,
        header: HeaderLookup<'_>,
        raw_body: Option<&[u8]>,
        now: i64,
    ) -> Result<(), OrionError> {
        // 1. Extract the presented signature(s).
        let presented = header(&self.header).ok_or_else(refused)?;
        let candidates: Vec<&str> = match &self.extraction {
            SigExtraction::Prefix(prefix) => {
                let one = match prefix {
                    Some(p) => presented.strip_prefix(p.as_str()).ok_or_else(refused)?,
                    None => presented.as_str(),
                };
                vec![one]
            }
            SigExtraction::Key(key) => {
                let values = packed_values(&presented, key);
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
                    packed_values(&value, key)
                        .first()
                        .map(|v| v.to_string())
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
                    let part = packed_values(&value, key)
                        .first()
                        .map(|v| v.to_string())
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
                    HmacAlgorithm::Sha1 => verify_mac::<Hmac<Sha1>>(secret, &message, &signature),
                    HmacAlgorithm::Sha256 => {
                        verify_mac::<Hmac<Sha256>>(secret, &message, &signature)
                    }
                    HmacAlgorithm::Sha512 => {
                        verify_mac::<Hmac<Sha512>>(secret, &message, &signature)
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
    /// else the pre-#264 auto-detection (hex first — unambiguous at MAC
    /// lengths — then standard base64).
    fn decode(&self, presented: &str) -> Option<Vec<u8>> {
        match self.encoding {
            Some(codec) => decode_bytes(codec, presented).ok(),
            None => hex::decode(presented).ok().or_else(|| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(presented)
                    .ok()
            }),
        }
    }
}

fn verify_mac<M: Mac + KeyInit>(secret: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Ok(mut mac) = M::new_from_slice(secret) else {
        return false;
    };
    mac.update(message);
    mac.verify_slice(signature).is_ok()
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
                "auth.signature_prefix and auth.signature_key are mutually exclusive —                  a signature is either prefix-stripped or extracted from a packed header"
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
        Some("hex") => Some(Codec::Hex),
        Some("base64") => Some(Codec::Base64),
        Some("base64url") => Some(Codec::Base64Url),
        Some(other) => {
            return Err(format!(
                "auth.encoding '{other}' is not supported — hex, base64, base64url                  (omit it for auto-detection)"
            ));
        }
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
                                "auth.message has an unterminated '{{' before the end:                                  '{template}'"
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
        return Err("auth.message must contain {body} — a template that never covers the                     payload verifies nothing about it"
            .to_string());
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
                "auth.message placeholder '{{{placeholder}}}' is malformed —                  {{header:<name>}} or {{header:<name>:<key>}}"
            )),
            None if !rest.is_empty() => Ok(Segment::Header(rest.to_string())),
            None => Err("auth.message placeholder {header:} names no header".to_string()),
        };
    }
    Err(format!(
        "auth.message placeholder '{{{placeholder}}}' is not known —          {{body}}, {{header:<name>}}, or {{header:<name>:<key>}}"
    ))
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
mod tests {
    use super::*;
    use crate::channel::config::ChannelAuthConfig;

    fn api_key_config(keys: &[&str]) -> ChannelAuthConfig {
        ChannelAuthConfig {
            mode: AuthMode::ApiKey,
            keys: Some(keys.iter().map(|k| k.to_string()).collect()),
            ..Default::default()
        }
    }

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.to_string())
        }
    }

    #[tokio::test]
    async fn api_key_defaults_to_bearer_on_authorization() {
        let auth = CompiledAuth::compile(&api_key_config(&["s3cret"]))
            .await
            .expect("compiles");
        let headers = [("Authorization", "Bearer s3cret")];
        assert!(auth.authenticate(&lookup(&headers), None).is_ok());
    }

    /// The bare key without the scheme prefix is not accepted — otherwise the
    /// prefix would be decorative.
    #[tokio::test]
    async fn api_key_requires_the_scheme_prefix() {
        let auth = CompiledAuth::compile(&api_key_config(&["s3cret"]))
            .await
            .expect("compiles");
        let headers = [("Authorization", "s3cret")];
        assert!(auth.authenticate(&lookup(&headers), None).is_err());
    }

    /// A custom header carries the bare key, with no prefix to strip.
    #[tokio::test]
    async fn a_custom_header_takes_a_bare_key() {
        let mut cfg = api_key_config(&["s3cret"]);
        cfg.header = Some("X-API-Key".to_string());
        let auth = CompiledAuth::compile(&cfg).await.expect("compiles");
        let headers = [("X-API-Key", "s3cret")];
        assert!(auth.authenticate(&lookup(&headers), None).is_ok());
    }

    #[tokio::test]
    async fn a_wrong_or_missing_key_is_refused() {
        let auth = CompiledAuth::compile(&api_key_config(&["s3cret"]))
            .await
            .expect("compiles");
        assert!(
            auth.authenticate(&lookup(&[("Authorization", "Bearer nope")]), None)
                .is_err()
        );
        assert!(auth.authenticate(&lookup(&[]), None).is_err());
    }

    /// Several keys are accepted at once, which is what makes rotation
    /// possible without a window of refusals.
    #[tokio::test]
    async fn any_configured_key_is_accepted() {
        let auth = CompiledAuth::compile(&api_key_config(&["old", "new"]))
            .await
            .expect("compiles");
        for key in ["old", "new"] {
            let headers = [("Authorization", format!("Bearer {key}"))];
            let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            assert!(auth.authenticate(&lookup(&pairs), None).is_ok(), "{key}");
        }
    }

    #[tokio::test]
    async fn api_key_mode_requires_keys() {
        let mut cfg = api_key_config(&[]);
        cfg.keys = None;
        assert!(CompiledAuth::compile(&cfg).await.is_err());
        let empty = api_key_config(&[]);
        assert!(CompiledAuth::compile(&empty).await.is_err());
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
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[tokio::test]
    async fn hmac_accepts_a_correct_hex_signature() {
        let auth = CompiledAuth::compile(&hmac_config("whsec", None))
            .await
            .expect("compiles");
        let body = br#"{"id":"evt_1","amount":2000}"#;
        let headers = [("X-Signature", sign_hex("whsec", body))];
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert!(auth.authenticate(&lookup(&pairs), Some(body)).is_ok());
    }

    /// The GitHub spelling: `sha256=<hex>`.
    #[tokio::test]
    async fn hmac_strips_a_configured_signature_prefix() {
        let auth = CompiledAuth::compile(&hmac_config("whsec", Some("sha256=")))
            .await
            .expect("compiles");
        let body = br#"{"action":"opened"}"#;
        let headers = [("X-Signature", format!("sha256={}", sign_hex("whsec", body)))];
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert!(auth.authenticate(&lookup(&pairs), Some(body)).is_ok());
    }

    /// The Shopify spelling: base64.
    #[tokio::test]
    async fn hmac_accepts_a_base64_signature() {
        let auth = CompiledAuth::compile(&hmac_config("whsec", None))
            .await
            .expect("compiles");
        let body = br#"{"order":1}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec").expect("hmac key");
        mac.update(body);
        use base64::Engine;
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        let headers = [("X-Signature", sig)];
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert!(auth.authenticate(&lookup(&pairs), Some(body)).is_ok());
    }

    /// The property the whole mode exists for: one byte changed in the body
    /// invalidates the signature.
    #[tokio::test]
    async fn hmac_refuses_a_tampered_body() {
        let auth = CompiledAuth::compile(&hmac_config("whsec", None))
            .await
            .expect("compiles");
        let signed = br#"{"amount":2000}"#;
        let tampered = br#"{"amount":9999}"#;
        let headers = [("X-Signature", sign_hex("whsec", signed))];
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert!(auth.authenticate(&lookup(&pairs), Some(tampered)).is_err());
    }

    #[tokio::test]
    async fn hmac_refuses_a_signature_from_the_wrong_secret() {
        let auth = CompiledAuth::compile(&hmac_config("whsec", None))
            .await
            .expect("compiles");
        let body = br#"{"a":1}"#;
        let headers = [("X-Signature", sign_hex("attacker", body))];
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert!(auth.authenticate(&lookup(&pairs), Some(body)).is_err());
    }

    #[tokio::test]
    async fn hmac_refuses_a_malformed_or_absent_signature() {
        let auth = CompiledAuth::compile(&hmac_config("whsec", None))
            .await
            .expect("compiles");
        let body = br#"{"a":1}"#;
        assert!(
            auth.authenticate(&lookup(&[("X-Signature", "not-a-signature!")]), Some(body))
                .is_err()
        );
        assert!(auth.authenticate(&lookup(&[]), Some(body)).is_err());
    }

    #[tokio::test]
    async fn hmac_mode_requires_a_secret() {
        let mut cfg = hmac_config("x", None);
        cfg.secret = None;
        assert!(CompiledAuth::compile(&cfg).await.is_err());
    }

    /// Every refusal reads the same, so a caller cannot learn which half of the
    /// credential was right by comparing messages.
    #[tokio::test]
    async fn every_refusal_carries_the_same_message() {
        let auth = CompiledAuth::compile(&api_key_config(&["s3cret"]))
            .await
            .expect("compiles");
        let missing = auth
            .authenticate(&lookup(&[]), None)
            .expect_err("no header is a refusal");
        let wrong = auth
            .authenticate(&lookup(&[("Authorization", "Bearer nope")]), None)
            .expect_err("a wrong key is a refusal");
        assert_eq!(missing.to_string(), wrong.to_string());
    }
    // -- #264: templates, presets, rotation, replay windows --

    fn sign<M: Mac + KeyInit>(secret: &[u8], message: &[u8]) -> Vec<u8> {
        let mut mac = M::new_from_slice(secret).expect("hmac key");
        mac.update(message);
        mac.finalize().into_bytes().to_vec()
    }

    fn preset_config(preset: &str, secret: &str) -> ChannelAuthConfig {
        ChannelAuthConfig {
            mode: AuthMode::Hmac,
            preset: Some(preset.to_string()),
            secret: Some(secret.to_string()),
            ..Default::default()
        }
    }

    async fn compiled_hmac(cfg: &ChannelAuthConfig) -> CompiledHmac {
        match CompiledAuth::compile(cfg).await.expect("compiles") {
            CompiledAuth::Hmac(h) => h,
            CompiledAuth::ApiKey { .. } => unreachable!("an hmac config compiles to Hmac"),
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
        let auth = CompiledAuth::compile(&preset_config("shopify", "shpss"))
            .await
            .expect("compiles");
        let raw = sign::<Hmac<Sha256>>(b"shpss", body);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        assert!(
            auth.authenticate(
                &lookup(&[("x-shopify-hmac-sha256", b64.as_str())]),
                Some(body)
            )
            .is_ok()
        );
        let hexed = hex::encode(&raw);
        assert!(
            auth.authenticate(
                &lookup(&[("x-shopify-hmac-sha256", hexed.as_str())]),
                Some(body)
            )
            .is_err()
        );

        // Webex: SHA-1 over the raw body.
        let auth = CompiledAuth::compile(&preset_config("webex", "wxsecret"))
            .await
            .expect("compiles");
        let sha1_hex = hex::encode(sign::<Hmac<Sha1>>(b"wxsecret", body));
        assert!(
            auth.authenticate(
                &lookup(&[("x-spark-signature", sha1_hex.as_str())]),
                Some(body)
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn github_preset_matches_the_documented_spelling() {
        let auth = CompiledAuth::compile(&preset_config("github", "ghs"))
            .await
            .expect("compiles");
        let body = br#"{"action":"opened"}"#;
        let sig = format!("sha256={}", hex::encode(sign::<Hmac<Sha256>>(b"ghs", body)));
        assert!(
            auth.authenticate(
                &lookup(&[("x-hub-signature-256", sig.as_str())]),
                Some(body)
            )
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
        let auth = CompiledAuth::compile(&cfg).await.expect("compiles");
        let body = b"payload";
        for secret in [b"old".as_slice(), b"new".as_slice()] {
            let sig = hex::encode(sign::<Hmac<Sha256>>(secret, body));
            assert!(
                auth.authenticate(&lookup(&[("X-Signature", sig.as_str())]), Some(body))
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
                Box::new(|c: &mut ChannelAuthConfig| {
                    c.message = Some("v0:{ts}:{body}".to_string())
                }),
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
}

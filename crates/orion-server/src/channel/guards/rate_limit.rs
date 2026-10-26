//! The per-channel rate limit, and the bucket key it is counted against.
//!
//! Split out of `guards` as one concept: the limiter call and the key
//! derivation change together and nothing else reads either.

use std::sync::Arc;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::ExprEngine;
use serde_json::{Value, json};

use super::ChannelRuntimeConfig;
use super::*;
use crate::errors::OrionError;
use crate::metrics;

/// Check the channel's rate limit.
///
/// S15: this used to live in `server::rate_limit`, which pulls `AppState`
/// and resolves the target channel from the URI — so the limiter a channel
/// declared applied to HTTP ingress and to nothing else. `channel_call`
/// reached the workflow with the limit unenforced. Lifting it
/// here is what lets [`apply_guards`] run it on every transport.
///
/// `Err(RateLimited)` = over limit; `Err(ServiceUnavailable)` = the backend
/// could not answer and the channel is configured to fail closed (N7 —
/// deliberately not a `429`: the caller is not over any limit, the control is
/// unavailable).
pub(super) async fn check_rate_limit(
    channel: &str,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
    exprs: &ExprEngine,
    caller_identity: &str,
    header: HeaderLookup<'_>,
) -> Result<(), OrionError> {
    let Some(cfg) = channel_config else {
        return Ok(());
    };
    let Some(ref limiter) = cfg.rate_limiter else {
        return Ok(());
    };

    // Compute the bucket key from `key_logic`, defaulting to the transport's
    // caller identity.
    let key = if let Some(ref compiled) = cfg.rate_limit_key_logic {
        let context = rate_limit_context(
            caller_identity,
            channel,
            header,
            cfg.rate_limit_key_headers.as_deref(),
        );
        // N5: falling back to the caller identity here would silently
        // re-dimension the limit mid-flight. The key is part of the control,
        // so a request whose key cannot be computed is rejected rather than
        // counted in the wrong bucket.
        //
        // Its own variant, not `RateLimited`: over-limit is transient and
        // worth retrying, while a key expression that cannot be evaluated
        // against this message will fail against every copy of it. Both
        // answer `429` over HTTP; a transport that retries can tell from the
        // variant that only this one is terminal.
        let unavailable = |reason: &str| {
            tracing::warn!(
                channel = %channel,
                reason = %reason,
                "rate_limit.key_logic produced no usable key; rejecting request"
            );
            metrics::record_rate_limit_rejected(channel);
            metrics::record_rate_limit_key_unavailable(channel);
            OrionError::RateLimitKeyUnavailable("Too many requests".to_string())
        };
        match exprs.eval(compiled, &context) {
            // A missing path resolves to `null` in datalogic 5 rather than
            // erroring, and an absent header is exactly that. Serializing it
            // would make the key the literal string `"null"` for *every*
            // caller on the channel — one shared bucket, no warning, no
            // metric, and a rate limit that reads as enforced while enforcing
            // nothing. A key that resolved to nothing has not been computed,
            // so it takes the same path as an evaluation error. An
            // all-whitespace string is the same collapse by a different route
            // (a header present but empty). `key_text` reads both.
            Ok(evaluated) => key_text(evaluated.value.into_owned()).map_err(unavailable)?,
            Err(e) => {
                let err = unavailable(&format!("evaluation failed: {e}"));
                return Err(err);
            }
        }
    } else {
        caller_identity.to_string()
    };

    let policy = cfg
        .parsed_config
        .rate_limit
        .as_ref()
        .map(|rl| rl.on_backend_error)
        .unwrap_or_default();
    match check_limiter(limiter.as_ref(), key).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            metrics::record_rate_limit_rejected(channel);
            Err(OrionError::RateLimited("Too many requests".to_string()))
        }
        Err(e) => {
            metrics::record_error("rate_limit_backend");
            match policy {
                crate::channel::BackendErrorPolicy::Allow => {
                    tracing::warn!(
                        channel = %channel,
                        error = %e,
                        "Rate-limit backend error; failing open (request allowed)"
                    );
                    Ok(())
                }
                crate::channel::BackendErrorPolicy::Deny => {
                    tracing::warn!(
                        channel = %channel,
                        error = %e,
                        "Rate-limit backend error; failing closed (request refused)"
                    );
                    metrics::record_rate_limit_rejected(channel);
                    Err(OrionError::unavailable(
                        crate::errors::Unavailable::GuardBackend,
                        format!(
                            "Channel '{channel}' cannot check its rate limit: the backend \
                             is unavailable and the channel is configured to fail closed"
                        ),
                    ))
                }
            }
        }
    }
}

/// The bucket key an evaluated `key_logic` names, read as 1.12 read the JSON
/// the expression gave ([`result_json`](crate::engine::operators::result_json)):
/// `null` — a number JSON cannot spell included, which it was — and an
/// all-whitespace string are no key, and say which; a string is itself;
/// anything else is its compact JSON.
pub(super) fn key_text(value: OwnedDataValue) -> Result<String, &'static str> {
    match value {
        OwnedDataValue::String(s) if s.trim().is_empty() => {
            Err("expression resolved to an empty string")
        }
        OwnedDataValue::String(s) => Ok(s),
        other => match crate::engine::operators::result_json(&other) {
            Value::Null => Err("expression resolved to null"),
            json => Ok(serde_json::to_string(&json).unwrap_or_default()),
        },
    }
}

/// The headers every `rate_limit.key_logic` can read without declaring them.
///
/// A closed default rather than "every header": a key referencing two names
/// must not cost an allocation per header on the request path. A channel that
/// needs another name declares it in `rate_limit.key_headers`, which is
/// *merged* with this list — so no stored `key_logic` changes meaning when a
/// channel starts declaring headers.
pub(crate) const COMMON_KEY_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "x-forwarded-for",
    "x-real-ip",
    "user-agent",
    "content-type",
    "origin",
    "x-tenant-id",
];

/// Build the context object `rate_limit.key_logic` is evaluated against.
///
/// `client_ip` is the transport's caller identity — the HTTP client IP or
/// the calling channel — and keeps its historical name so
/// stored `key_logic` expressions keep resolving. Headers are limited to
/// [`COMMON_KEY_HEADERS`] plus whatever the channel declared, so a request does
/// not pay an allocation per header for a key that references two of them.
pub(super) fn rate_limit_context(
    caller_identity: &str,
    channel: &str,
    header: HeaderLookup<'_>,
    declared: Option<&[String]>,
) -> Value {
    let declared = declared.unwrap_or(&[]);
    let mut headers = serde_json::Map::with_capacity(COMMON_KEY_HEADERS.len() + declared.len());
    for &name in COMMON_KEY_HEADERS {
        if let Some(value) = header(name) {
            headers.insert(name.to_string(), Value::String(value));
        }
    }
    // Declared names are already lowercased at load. A redundant declaration
    // of a built-in is a no-op rather than a second lookup.
    for name in declared {
        if COMMON_KEY_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if let Some(value) = header(name) {
            headers.insert(name.clone(), Value::String(value));
        }
    }
    json!({
        "client_ip": caller_identity,
        "channel": channel,
        "headers": headers,
    })
}

/// Collect the `headers.<name>` paths a `key_logic` expression reads, when they
/// are statically visible.
///
/// Used at channel load to warn about a name no context will ever carry — the
/// typo that used to collapse every caller into one bucket. Only literal `var`
/// paths are recognised; a dynamically-composed path (`{"var": {"cat": […]}}`)
/// is skipped rather than guessed, so this never produces a false positive.
pub(crate) fn key_logic_header_paths(logic: &Value) -> Vec<String> {
    fn literal_path(arg: &Value) -> Option<&str> {
        match arg {
            // `{"var": "headers.x"}`
            Value::String(s) => Some(s.as_str()),
            // `{"var": ["headers.x", <default>]}` — the default does not
            // change which header is read.
            Value::Array(items) => items.first().and_then(|f| f.as_str()),
            _ => None,
        }
    }
    fn walk(node: &Value, out: &mut Vec<String>) {
        match node {
            Value::Object(map) => {
                for (op, arg) in map {
                    if op == "var"
                        && let Some(path) = literal_path(arg)
                        && let Some(name) = path.strip_prefix("headers.")
                        && !name.is_empty()
                    {
                        let name = name.to_ascii_lowercase();
                        if !out.contains(&name) {
                            out.push(name);
                        }
                    }
                    walk(arg, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(logic, &mut out);
    out
}

/// Check the channel's **principal** rate limit — the quota half, applied after
/// authentication.
///
/// [`check_rate_limit`] runs before `check_auth`, deliberately: a refusal
/// should cost the least work, and credential-stuffing should be metered like
/// any other traffic. The consequence is that it cannot know who the caller is.
/// The only identities available to it are the address and a request header,
/// and a header is caller-supplied — a key derived from one bounds an honest
/// client, which is a burst control and not a quota. Both limiters apply; this
/// one is the quota, and the address limit stays the cheap outer guard.
///
/// `claims` are the verified claims `check_auth` returned. They reach the key
/// expression at `auth`, alongside everything the outer limiter sees. There is
/// no fallback when the key cannot be computed — the same argument as N5, and
/// sharper here: falling back to the address would silently turn a per-user
/// quota into a per-address one.
pub(super) async fn check_principal_rate_limit(
    channel: &str,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
    exprs: &ExprEngine,
    caller_identity: &str,
    header: HeaderLookup<'_>,
    claims: Option<&Value>,
) -> Result<(), OrionError> {
    let Some(cfg) = channel_config else {
        return Ok(());
    };
    let (Some(limiter), Some(compiled)) = (
        cfg.principal_rate_limiter.as_ref(),
        cfg.principal_rate_limit_key_logic.as_ref(),
    ) else {
        return Ok(());
    };

    let mut context = rate_limit_context(
        caller_identity,
        channel,
        header,
        cfg.rate_limit_key_headers.as_deref(),
    );
    if let Some(obj) = context.as_object_mut() {
        obj.insert("auth".to_string(), claims.cloned().unwrap_or(Value::Null));
    }

    let unavailable = |reason: &str| {
        tracing::warn!(
            channel = %channel,
            reason = %reason,
            "principal_rate_limit.key_logic produced no usable key; rejecting request"
        );
        metrics::record_rate_limit_rejected(channel);
        metrics::record_rate_limit_key_unavailable(channel);
        OrionError::RateLimitKeyUnavailable("Too many requests".to_string())
    };
    let key = match exprs.eval(compiled, &context) {
        Ok(evaluated) => key_text(evaluated.value.into_owned()).map_err(unavailable)?,
        Err(e) => return Err(unavailable(&format!("evaluation failed: {e}"))),
    };

    let policy = cfg
        .parsed_config
        .principal_rate_limit
        .as_ref()
        .map(|rl| rl.on_backend_error)
        .unwrap_or_default();
    match check_limiter(limiter.as_ref(), key).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            // The same counter as the address limit: 429 accounting stays
            // whole, which is what an operator alerts on. Which of the two
            // refused is in the log line.
            metrics::record_rate_limit_rejected(channel);
            tracing::debug!(channel = %channel, "principal rate limit exceeded");
            Err(OrionError::RateLimited("Too many requests".to_string()))
        }
        Err(e) => {
            metrics::record_error("rate_limit_backend");
            match policy {
                crate::channel::BackendErrorPolicy::Allow => {
                    tracing::warn!(
                        channel = %channel,
                        error = %e,
                        "Principal rate-limit backend error; failing open (request allowed)"
                    );
                    Ok(())
                }
                crate::channel::BackendErrorPolicy::Deny => {
                    tracing::warn!(
                        channel = %channel,
                        error = %e,
                        "Principal rate-limit backend error; failing closed (request refused)"
                    );
                    metrics::record_rate_limit_rejected(channel);
                    Err(OrionError::unavailable(
                        crate::errors::Unavailable::GuardBackend,
                        format!(
                            "Channel '{channel}' cannot check its principal rate limit: the \
                             backend is unavailable and the channel is configured to fail closed"
                        ),
                    ))
                }
            }
        }
    }
}

/// One limiter verdict: in place when the backend can give one
/// ([`crate::channel::RateLimitBackend::check_now`] — the local governor),
/// else through its boxed future.
async fn check_limiter(
    limiter: &dyn crate::channel::RateLimitBackend,
    key: String,
) -> Result<bool, OrionError> {
    match limiter.check_now(&key) {
        Some(allowed) => Ok(allowed),
        None => limiter.check(key).await,
    }
}

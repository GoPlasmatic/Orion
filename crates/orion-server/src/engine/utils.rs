use serde_json::Value;

/// FNV-1a 64-bit hash mixin. Unkeyed and deterministic, so a given identity
/// lands in the same rollout bucket on every replica and across restarts.
///
/// Rollout bucketing is the only remaining caller, and it is a fit: the input
/// is an identity the caller already owns, and the worst a chosen collision
/// buys is the version the caller could have reached by retrying. The response
/// cache used this too until keys became attacker-reachable — see
/// `channel::guards::compute_cache_key` for why that one needs SHA-256.
fn fnv1a_feed(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

/// FNV-1a 64-bit offset basis (seed for [`fnv1a_feed`]).
const FNV1A_SEED: u64 = 0xcbf29ce484222325;

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = FNV1A_SEED;
    fnv1a_feed(&mut h, bytes);
    h
}

/// First forwarded client IP: the first `x-forwarded-for` hop, else
/// `x-real-ip`.
///
/// Rollout-bucketing policy ONLY — never a security identity. The leftmost
/// hop is client-supplied, which is tolerable here for the same reason the
/// unkeyed hash above is: a caller choosing its identity chooses only its
/// own canary bucket, which the sticky header already lets any caller do.
/// It is also the hop that stays per-client behind chained proxies, where
/// the rightmost hop would collapse every caller into one bucket. The
/// rate-limit / audit identity must NOT use this — see the trusted-proxy
/// aware, rightmost-hop resolution in `server::rate_limit`.
fn first_forwarded_value<'a>(mut get: impl FnMut(&str) -> Option<&'a str>) -> Option<&'a str> {
    if let Some(xff) = get("x-forwarded-for")
        && let Some(first) = xff.split(',').next().map(str::trim)
        && !first.is_empty()
    {
        return Some(first);
    }
    get("x-real-ip").map(str::trim).filter(|v| !v.is_empty())
}

/// Stable caller identity for sticky rollout bucketing: the configured
/// sticky header's value, else the forwarded client IP — read from the
/// request metadata (`metadata.headers`, built once per request and shared
/// by the sync and async paths). `None` (direct connection, no forwarding
/// headers) falls back to a random bucket.
pub fn rollout_identity<'a>(metadata: &'a Value, sticky_header: &str) -> Option<&'a str> {
    let headers = metadata.get("headers")?.as_object()?;
    if !sticky_header.is_empty()
        && let Some((_, v)) = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(sticky_header))
        && let Some(v) = v.as_str()
        && !v.is_empty()
    {
        return Some(v);
    }
    first_forwarded_value(|name| headers.get(name).and_then(|v| v.as_str()))
}

/// Serialize a captured `ExecutionTrace`, dropping it (with a warn and an
/// error metric) when it exceeds `max_bytes` (N15). `result_json` is capped
/// by `queue.max_result_size_bytes` on both the sync and async paths, but the
/// per-task trace rode along uncapped — a workflow with large intermediate
/// data could persist an unbounded blob per request. Task detail is a debug
/// aid, so an oversized one is dropped rather than failing the request.
/// `max_bytes = 0` disables the cap, mirroring the result cap's semantics.
pub fn serialize_task_trace_capped(
    trace: Option<&dataflow_rs::ExecutionTrace>,
    max_bytes: usize,
    context: &str,
) -> Option<String> {
    let json = serde_json::to_string(trace?).ok()?;
    if max_bytes > 0 && json.len() > max_bytes {
        crate::metrics::record_error("task_trace_size_exceeded");
        tracing::warn!(
            context = %context,
            task_trace_bytes = json.len(),
            limit_bytes = max_bytes,
            "task_trace_json exceeds queue.max_result_size_bytes; dropping task detail"
        );
        return None;
    }
    Some(json)
}

/// Compute the rollout bucket (0–99) for a caller.
///
/// With a stable identity (configured sticky header, else forwarded client
/// IP) the bucket is a hash — the same caller lands on the same canary
/// version on every request and every replica. Without one (direct
/// connection, no forwarding headers) it falls back to per-request random,
/// which still honors the rollout percentages in aggregate.
///
/// The bucket goes on the message as `MessageBuilder::routing_bucket`, which
/// the engine matches against each workflow's own `rollout` range. It used to
/// be written into `data._rollout_bucket` for a synthetic condition to read,
/// which meant it was a caller-visible field that every response and trace
/// boundary then had to strip back out — and, because dataflow-rs v3 had no
/// `unset`, could only be nulled rather than removed. Four helpers and a const
/// existed to hide that; none of them are needed now.
pub fn rollout_bucket_for_identity(identity: Option<&str>) -> u8 {
    match identity {
        Some(id) if !id.is_empty() => (fnv1a64(id.as_bytes()) % 100) as u8,
        _ => (rand::random::<u32>() % 100) as u8,
    }
}

/// Request headers whose value is a credential, and whose value therefore
/// enters message metadata masked rather than verbatim (S10).
///
/// The metadata map is persisted into `traces.result_json` on the async path
/// and `trace_dlq.metadata_json` on the failure path, so a plaintext value
/// here is a plaintext credential at rest. The key survives so logic can still
/// test presence; the value is never recoverable downstream.
///
/// Shared by the HTTP ingress (`server::routes::data::build_request_metadata`)
/// and the offline metadata builder below, so the two cannot disagree about
/// which headers are credentials — a disagreement that would let an offline
/// case pass while reading a value production masks.
pub const CREDENTIAL_HEADERS: [&str; 4] = [
    "authorization",
    "cookie",
    "proxy-authorization",
    "x-api-key",
];

/// Whether this header name's value is masked on its way into metadata.
pub fn is_credential_header(name: &str) -> bool {
    CREDENTIAL_HEADERS.contains(&name)
}

/// The reserved metadata keys whose shape the offline builder checks, and the
/// shape each one must have.
///
/// Only these are constrained. Everything else is passed through: the HTTP
/// envelope merges arbitrary caller-supplied `metadata`, so a closed key set
/// would make an offline case unable to test a workflow that reads a custom
/// key — the same gap this whole surface exists to close.
const STRING_MAP_KEYS: [&str; 4] = ["headers", "params", "query", "cookies"];

/// Build the message metadata for an offline run (`orion-server test` cases and
/// `dry-run --metadata`) from a case-supplied object.
///
/// The point of the normalization is that an offline pass must mean the same
/// thing as a production pass. Three things the HTTP ingress does are therefore
/// done here too:
///
/// * **Header keys are lowercased.** `axum` yields lowercase header names, so a
///   case writing `"DeviceId"` would match offline and miss in production.
/// * **Credential headers are masked**, per [`CREDENTIAL_HEADERS`] — a workflow
///   reading a raw bearer token out of `metadata.headers` is already broken in
///   production and must fail offline too.
/// * **`_orion_errors` is cleared**, because it is engine-owned and the ingress
///   clears it unconditionally.
///
/// Returns a human-readable error for a shape the ingress could never produce:
/// a non-object root, a `headers`/`params`/`query`/`cookies` that is not an
/// object of strings, a non-string `channel`/`http_method`, or an `auth`
/// carrying anything but `claims` (the ingress replaces `auth` wholesale with
/// `{"claims": …}`, so nothing else is reachable at runtime).
pub fn prepare_offline_metadata(metadata: Value) -> Result<Value, String> {
    let mut metadata = match metadata {
        Value::Null => return Ok(serde_json::json!({})),
        Value::Object(_) => metadata,
        other => {
            return Err(format!(
                "'metadata' must be a JSON object, got {}",
                json_kind(&other)
            ));
        }
    };

    for key in STRING_MAP_KEYS {
        let Some(value) = metadata.get(key) else {
            continue;
        };
        let Some(map) = value.as_object() else {
            return Err(format!(
                "'metadata.{key}' must be an object of strings, got {}",
                json_kind(value)
            ));
        };
        if let Some((name, bad)) = map.iter().find(|(_, v)| !v.is_string()) {
            return Err(format!(
                "'metadata.{key}.{name}' must be a string, got {}",
                json_kind(bad)
            ));
        }
    }

    for key in ["channel", "http_method"] {
        if let Some(value) = metadata.get(key)
            && !value.is_string()
        {
            return Err(format!(
                "'metadata.{key}' must be a string, got {}",
                json_kind(value)
            ));
        }
    }

    if let Some(auth) = metadata.get("auth") {
        let Some(map) = auth.as_object() else {
            return Err(format!(
                "'metadata.auth' must be an object, got {}",
                json_kind(auth)
            ));
        };
        // The ingress builds `auth` with `guards::merge_auth_claims`, which
        // *replaces* the key with `{"claims": …}`. A case setting anything else
        // would be asserting on state no request can produce.
        if let Some(unknown) = map.keys().find(|k| k.as_str() != "claims") {
            return Err(format!(
                "'metadata.auth.{unknown}' is not settable — the request path builds \
                 'auth' as {{\"claims\": …}} and nothing else reaches a workflow"
            ));
        }
    }

    crate::engine::clear_error_context(&mut metadata);

    if let Some(headers) = metadata.get("headers").and_then(Value::as_object) {
        let normalized: serde_json::Map<String, Value> = headers
            .iter()
            .map(|(name, value)| {
                let name = name.to_ascii_lowercase();
                let value = if is_credential_header(&name) {
                    Value::String(crate::connector::MASK.to_string())
                } else {
                    value.clone()
                };
                (name, value)
            })
            .collect();
        metadata["headers"] = Value::Object(normalized);
    }

    Ok(metadata)
}

/// JSON type name for a validation message.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dataflow_rs::Message;
    use serde_json::json;

    /// A message shaped the way every ingress builds one.
    fn ingress_message(payload: Value, metadata: Value, identity: Option<&str>) -> Message {
        Message::builder()
            .payload_json(&payload)
            .metadata_json(&metadata)
            .routing_bucket(rollout_bucket_for_identity(identity))
            .build()
    }

    #[test]
    fn test_task_trace_cap_drops_oversized_detail() {
        let trace = dataflow_rs::ExecutionTrace::new();
        // Empty trace serializes to a small JSON — passes any nonzero cap…
        assert!(serialize_task_trace_capped(Some(&trace), 1024, "t").is_some());
        // …fails a cap smaller than its serialization…
        assert!(serialize_task_trace_capped(Some(&trace), 1, "t").is_none());
        // …and 0 disables the cap, mirroring queue.max_result_size_bytes.
        assert!(serialize_task_trace_capped(Some(&trace), 0, "t").is_some());
        assert!(serialize_task_trace_capped(None, 1024, "t").is_none());
    }

    /// The ingress seeds `context.metadata` through the builder rather than
    /// one `set_nested_value("metadata.{k}")` per key. Note the keys are now
    /// literal: a caller-supplied `"a.b"` stays one key instead of becoming
    /// nested `metadata.a.b`.
    #[test]
    fn ingress_seeds_metadata_with_literal_keys() {
        let msg = ingress_message(json!({}), json!({"source": "test", "a.b": 2}), None);

        assert_eq!(
            msg.metadata().get("source").and_then(|v| v.as_str()),
            Some("test")
        );
        assert_eq!(msg.metadata().get("a.b").and_then(|v| v.as_i64()), Some(2));
        assert!(
            msg.metadata().get("a").is_none(),
            "a dotted metadata key must not be re-read as a path"
        );
    }

    #[test]
    fn test_rollout_bucket_is_sticky_per_identity() {
        let a1 = rollout_bucket_for_identity(Some("10.0.0.7"));
        let a2 = rollout_bucket_for_identity(Some("10.0.0.7"));
        assert_eq!(a1, a2, "same identity must map to the same bucket");
        assert!(a1 < 100);

        // Distinct identities distribute (spot-check that not everything
        // collapses onto one bucket).
        let buckets: std::collections::HashSet<u8> = (0..50)
            .map(|i| rollout_bucket_for_identity(Some(&format!("user-{i}"))))
            .collect();
        assert!(buckets.len() > 10, "expected spread, got {buckets:?}");
    }

    #[test]
    fn test_rollout_identity_prefers_sticky_header() {
        let metadata = json!({
            "method": "POST",
            "headers": {
                "x-user-id": "user-7",
                "x-forwarded-for": "10.1.1.1, 10.1.1.2"
            }
        });
        // Configured sticky header wins; lookup is case-insensitive.
        assert_eq!(rollout_identity(&metadata, "X-User-Id"), Some("user-7"));
        // No sticky header → first forwarded IP.
        assert_eq!(rollout_identity(&metadata, ""), Some("10.1.1.1"));
        // x-real-ip fallback.
        let metadata = json!({"headers": {"x-real-ip": "10.2.2.2"}});
        assert_eq!(rollout_identity(&metadata, "x-user-id"), Some("10.2.2.2"));
        // No headers at all → None (random bucket fallback).
        assert_eq!(rollout_identity(&json!({}), "x-user-id"), None);
    }

    #[test]
    fn test_rollout_bucket_empty_identity_falls_back_to_random() {
        // Empty identity must not pin every caller to one bucket.
        let buckets: std::collections::HashSet<u8> = (0..100)
            .map(|_| rollout_bucket_for_identity(Some("")))
            .collect();
        assert!(buckets.len() > 1, "empty identity should randomize");
    }

    /// F31, restated for the routing-bucket shape. The bucket used to live at
    /// `data._rollout_bucket`, which meant it serialized into every success
    /// body and into `traces.result_json` as a field the caller never sent —
    /// and, with no `unset` in dataflow-rs v3, could only be nulled rather than
    /// removed. It is now a message field the wire format does not carry, so
    /// neither the response view nor the persisted message can leak it.
    #[test]
    fn the_routing_bucket_is_not_part_of_the_message_body() {
        let msg = ingress_message(json!({"order_id": 7}), json!({}), Some("caller-1"));
        assert!(
            msg.routing_bucket().is_some(),
            "precondition: the ingress set a bucket"
        );

        let body: Value = msg.data().into();
        assert_eq!(body, json!({}), "routing must not write into `data`");

        let serialized = serde_json::to_string(&msg).expect("message serializes");
        assert!(
            !serialized.contains("_rollout_bucket") && !serialized.contains("routing_bucket"),
            "the bucket must not reach the persisted message: {serialized}"
        );
    }
}

#[cfg(test)]
mod offline_metadata_tests {
    use super::*;
    use serde_json::json;

    /// The three normalizations exist so an offline pass means what a
    /// production pass means. Asserted together because they are one contract.
    #[test]
    fn case_metadata_is_normalized_the_way_the_ingress_builds_it() {
        let out = prepare_offline_metadata(json!({
            "headers": { "DeviceId": "device-abc", "Authorization": "Bearer secret" },
            "auth": { "claims": { "sub": "asha@example.com" } },
            "custom": { "anything": true },
        }))
        .expect("a well-formed metadata object is accepted");

        assert_eq!(
            out["headers"]["deviceid"], "device-abc",
            "header keys lowercase, because axum yields lowercase names"
        );
        assert!(
            out["headers"].get("DeviceId").is_none(),
            "the original casing must not survive alongside it"
        );
        assert_eq!(
            out["headers"]["authorization"],
            crate::connector::MASK,
            "a credential header is masked here exactly as at ingress"
        );
        assert_eq!(out["auth"]["claims"]["sub"], "asha@example.com");
        assert_eq!(
            out["custom"]["anything"], true,
            "keys outside the reserved set pass through: the envelope merges \
             arbitrary caller metadata, so a closed set would be wrong"
        );
    }

    /// Engine-owned, and cleared unconditionally at ingress — so a case cannot
    /// pre-seed failures a workflow then branches on.
    #[test]
    fn the_engine_owned_error_context_is_cleared() {
        let out = prepare_offline_metadata(json!({"_orion_errors": [{"code": "FAKE"}]}))
            .expect("accepted");
        assert!(out.get("_orion_errors").is_none());
    }

    /// The shapes the ingress could never produce, each named so the fix is
    /// one edit.
    #[test]
    fn shapes_the_ingress_cannot_produce_are_refused() {
        let err = prepare_offline_metadata(json!({"headers": ["a", "b"]}))
            .expect_err("headers must be an object");
        assert!(err.contains("metadata.headers"), "{err}");

        let err = prepare_offline_metadata(json!({"query": {"page": 2}}))
            .expect_err("query values must be strings");
        assert!(err.contains("metadata.query.page"), "{err}");

        let err =
            prepare_offline_metadata(json!({"channel": 7})).expect_err("channel must be a string");
        assert!(err.contains("metadata.channel"), "{err}");

        // `merge_auth_claims` replaces `auth` wholesale with `{"claims": …}`,
        // so anything else is state no request can produce.
        let err = prepare_offline_metadata(json!({"auth": {"token": "t"}}))
            .expect_err("only auth.claims is reachable");
        assert!(err.contains("metadata.auth.token"), "{err}");

        let err = prepare_offline_metadata(json!("nope")).expect_err("root must be an object");
        assert!(err.contains("must be a JSON object"), "{err}");
    }

    /// The ingress and the offline builder must agree on which headers are
    /// credentials. They read one list; this is the guard that keeps it one.
    #[test]
    fn the_credential_header_set_is_the_ingress_set() {
        for name in CREDENTIAL_HEADERS {
            assert!(is_credential_header(name), "{name} must be masked");
        }
        assert!(!is_credential_header("deviceid"));
        assert!(!is_credential_header("content-type"));
    }
}

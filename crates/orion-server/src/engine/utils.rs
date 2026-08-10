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

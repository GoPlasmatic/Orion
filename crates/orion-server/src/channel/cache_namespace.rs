//! Response-cache invalidation by namespace.
//!
//! A channel's `cache.namespaces` names the families of data its response
//! depends on. Each namespace has a **version counter** in the response-cache
//! store, and an entry is stored beside the versions that were current when
//! its request looked the cache up. Invalidating a namespace is one `INCR` of
//! its counter: every entry stored under an older version stops matching, with
//! no scan and no delete, and is overwritten by the next miss or expires at its
//! TTL.
//!
//! Two properties carry the correctness:
//!
//! - **The versions are read with the entry**, in one `MGET`, so a hit is
//!   judged against the counters as they stand at that instant and a lookup
//!   still costs one round trip.
//! - **An entry is stored with the versions read at lookup**, not at store.
//!   An invalidation that lands while the workflow is running then makes the
//!   entry the run produces stale the moment it is written. Tagging it with the
//!   versions read after the run would pin a pre-invalidation response under
//!   the post-invalidation version.
//!
//! Tags (a set of entry keys per tag, deleted on invalidation) were the other
//! design. They cost a set per tag growing with every stored entry and a
//! delete per member on invalidation; a counter costs neither and behaves the
//! same on the in-memory backend and on Redis.

use std::sync::Arc;

use crate::connector::cache_backend::CacheBackend;
use crate::errors::OrionError;

/// Most namespaces one channel may declare. Each is one more key in the
/// lookup's `MGET`.
pub const MAX_NAMESPACES: usize = 8;

/// Longest namespace name.
pub const MAX_NAMESPACE_LEN: usize = 64;

/// The key a namespace's version counter lives under, in every response-cache
/// store. Documented: an operator's own tooling may read it, and `redis-cli
/// INCR orion:rc:ns:<name>` is an invalidation.
pub fn version_key(namespace: &str) -> String {
    format!("orion:rc:ns:{namespace}")
}

/// The key a namespaced channel's entry is stored under.
///
/// A prefix of its own rather than `cache:`: the stored value carries a
/// version header ([`encode_entry`]), and nothing that reads `cache:` keys as
/// bodies may ever be handed one.
pub fn entry_key(plain_key: &str) -> String {
    match plain_key.strip_prefix("cache:") {
        Some(rest) => format!("cache-ns:{rest}"),
        None => format!("cache-ns:{plain_key}"),
    }
}

/// Whether `name` may be a namespace: 1–64 of `a-z 0-9 _ - . :`.
///
/// Lowercase only, so `Ladder` and `ladder` cannot be two namespaces an
/// author meant to be one.
pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAMESPACE_LEN {
        return Err(format!(
            "namespace '{name}' must be 1 to {MAX_NAMESPACE_LEN} characters"
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.:".contains(&b))
    {
        return Err(format!(
            "namespace '{name}' may contain only lowercase letters, digits, '_', '-', '.' and ':'"
        ));
    }
    Ok(())
}

/// Encode a namespaced entry: the versions it was looked up under, a newline,
/// then the body exactly as an un-namespaced entry stores it.
///
/// A header rather than a JSON envelope: a body is JSON, and embedding it in a
/// JSON string would escape every quote on the way in and unescape the whole
/// body into a fresh allocation on every hit.
pub fn encode_entry(versions: &[i64], body: &str) -> String {
    let mut out = String::with_capacity(body.len() + versions.len() * 4 + 1);
    for (i, v) in versions.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&v.to_string());
    }
    out.push('\n');
    out.push_str(body);
    out
}

/// The body of a stored entry, if it was stored under exactly `versions`.
/// The header is drained off in place, so a hit costs no second allocation.
pub fn decode_entry(mut stored: String, versions: &[i64]) -> Option<String> {
    let newline = stored.find('\n')?;
    let header = &stored[..newline];
    let mut stored_versions = header.split(',').map(|v| v.parse::<i64>());
    let current = versions
        .iter()
        .all(|v| stored_versions.next().and_then(Result::ok) == Some(*v))
        && stored_versions.next().is_none();
    if !current {
        return None;
    }
    stored.drain(..=newline);
    Some(stored)
}

/// Parse a counter as stored. A missing key is version `0` — nothing has
/// invalidated the namespace yet. A value that is not an integer is `None`:
/// the counter is not Orion's, and no entry can safely be judged against it.
pub fn parse_version(raw: Option<&str>) -> Option<i64> {
    match raw {
        None => Some(0),
        Some(s) => s.trim().parse().ok(),
    }
}

/// Bump every namespace in every store. `source` labels the metric:
/// `workflow` or `admin`.
///
/// Every store rather than only those a channel declaring the namespace uses
/// today: a channel archived while its entries were live, and reactivated
/// later, reads its old entries against the counter in its own store, which
/// must have moved too. The stores are bumped concurrently. One that fails is
/// logged, and its error returned after every other store has been bumped, so
/// one unreachable connector does not leave the rest stale.
pub async fn invalidate(
    targets: &[Arc<dyn CacheBackend>],
    namespaces: &[String],
    source: &'static str,
) -> Result<(), OrionError> {
    let keys: Vec<String> = namespaces.iter().map(|ns| version_key(ns)).collect();
    let bumps = targets.iter().map(|backend| {
        let keys = &keys;
        async move {
            for key in keys {
                backend.incr_by(key, 1, None).await?;
            }
            Ok::<(), OrionError>(())
        }
    });
    let mut first_error = None;
    for result in futures::future::join_all(bumps).await {
        if let Err(e) = result {
            tracing::warn!(
                namespaces = ?namespaces,
                error = %e,
                "Failed to bump a response-cache namespace version"
            );
            first_error.get_or_insert(e);
        }
    }
    crate::metrics::record_cache_invalidations(source, namespaces.len() as u64);
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked() {
        assert!(check_name("ladder").is_ok());
        assert!(check_name("game:chess.v2_x-y").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("Ladder").is_err());
        assert!(check_name("a b").is_err());
        assert!(check_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn a_namespaced_entry_never_shares_a_key_with_a_plain_one() {
        assert_eq!(entry_key("cache:orders:abc"), "cache-ns:orders:abc");
        assert_ne!(entry_key("cache:orders:abc"), "cache:orders:abc");
    }

    #[test]
    fn entries_round_trip_only_under_their_own_versions() {
        let body = r#"{"data":{"q":"a\nb"}}"#;
        let stored = encode_entry(&[3, 0], body);
        assert_eq!(decode_entry(stored.clone(), &[3, 0]).as_deref(), Some(body));
        assert_eq!(decode_entry(stored.clone(), &[4, 0]), None);
        assert_eq!(decode_entry(stored.clone(), &[3]), None);
        assert_eq!(decode_entry(stored, &[3, 0, 1]), None);
        assert_eq!(decode_entry("no header".to_string(), &[0]), None);
    }

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version(None), Some(0));
        assert_eq!(parse_version(Some("7")), Some(7));
        assert_eq!(parse_version(Some("\"x\"")), None);
    }

    #[tokio::test]
    async fn invalidate_bumps_each_namespace_in_each_store() {
        use crate::connector::cache_backend::MemoryCacheBackend;
        let a: Arc<dyn CacheBackend> = MemoryCacheBackend::new(60, 0);
        let b: Arc<dyn CacheBackend> = MemoryCacheBackend::new(60, 0);
        let ns = vec!["ladder".to_string(), "season".to_string()];
        invalidate(&[a.clone(), b.clone()], &ns, "workflow")
            .await
            .expect("test");
        assert_eq!(
            a.get(&version_key("ladder")).await.expect("test"),
            Some("1".to_string())
        );
        assert_eq!(
            b.get(&version_key("season")).await.expect("test"),
            Some("1".to_string())
        );
    }
}

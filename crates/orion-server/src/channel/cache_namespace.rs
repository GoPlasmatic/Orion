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

use serde::{Deserialize, Serialize};

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
/// A prefix of its own rather than `cache:`: the stored value is an
/// [`Envelope`], not a body, and nothing that reads `cache:` keys as bodies
/// may ever be handed one.
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

/// A namespaced entry as stored: the versions it was looked up under, and
/// the pre-serialized response body.
#[derive(Debug, Serialize, Deserialize)]
pub struct Envelope<'a> {
    /// One version per declared namespace, in declaration order.
    pub v: Vec<i64>,
    /// The body, exactly as an un-namespaced entry stores it.
    #[serde(borrow)]
    pub b: std::borrow::Cow<'a, str>,
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

/// Bump every namespace in every store, answering how many counters moved.
/// `source` labels the metric: `workflow` or `admin`.
///
/// Every store rather than only those a channel declaring the namespace uses
/// today: a channel archived while its entries were live, and reactivated
/// later, reads its old entries against the counter in its own store, which
/// must have moved too. A store that fails is logged and skipped, and the
/// error is returned once the rest have been bumped, so one unreachable
/// connector does not leave every other store stale.
pub async fn invalidate(
    targets: &[Arc<dyn CacheBackend>],
    namespaces: &[String],
    source: &'static str,
) -> Result<u64, OrionError> {
    let mut bumped = 0;
    let mut first_error = None;
    for backend in targets {
        for namespace in namespaces {
            match backend.incr_by(&version_key(namespace), 1, None).await {
                Ok(_) => bumped += 1,
                Err(e) => {
                    tracing::warn!(
                        namespace = %namespace,
                        error = %e,
                        "Failed to bump a response-cache namespace version"
                    );
                    first_error.get_or_insert(e);
                }
            }
        }
    }
    crate::metrics::record_cache_invalidations(source, namespaces.len() as u64);
    match first_error {
        Some(e) => Err(e),
        None => Ok(bumped),
    }
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
        assert_eq!(
            invalidate(&[a.clone(), b.clone()], &ns, "workflow")
                .await
                .expect("test"),
            4
        );
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

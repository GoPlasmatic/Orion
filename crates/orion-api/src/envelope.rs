//! The 2xx envelopes: `{"data": …}` (R17) and its paginated sibling.
//!
//! Admin endpoints wrap every success body this way. These types are the
//! client-side reading of that shape — the server currently builds the same
//! JSON through its response helpers and publishes matching schema-only
//! envelopes in its OpenAPI document; the wire-shape tests on both sides keep
//! the three in agreement.

use serde::{Deserialize, Serialize};

/// `{"data": T}` — the envelope every admin 2xx carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Data<T> {
    pub data: T,
}

/// The paginated envelope: `data` plus the three counters, and nothing else.
///
/// This is the shape every admin list endpoint returns **except the trace
/// list** — read that one with [`TracePage`], not this type. `total` here is a
/// plain `i64` and defaults to `0`, so a body that legitimately omits the field
/// would read as "zero rows matched" rather than "not counted".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paginated<T> {
    #[serde(default = "Vec::new")]
    pub data: Vec<T>,
    /// Total rows matching the filter, ignoring `limit`/`offset`.
    #[serde(default)]
    pub total: i64,
    #[serde(default)]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

/// The trace list's envelope, which [`Paginated`] cannot represent.
///
/// `GET /api/v1/admin/traces` differs from every other list in two ways, both
/// deliberate and both invisible to `Paginated`:
///
/// - **`total` is opt-in.** Counting rows across a large trace table is the
///   expensive half of the query, so it is computed only for
///   `?include_total=true` and the key is otherwise absent. Read through
///   `Paginated` that absence becomes `0` — indistinguishable from an empty
///   result, and the reason this type exists.
/// - **`next_cursor` may be present.** The trace list pages by keyset, not by
///   offset; when more rows follow, this carries the cursor to pass as
///   `?cursor=`. `Paginated` drops the field silently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracePage<T> {
    #[serde(default = "Vec::new")]
    pub data: Vec<T>,
    /// Present only when the request asked for it with `?include_total=true`.
    /// `None` means "not counted", never "no rows".
    #[serde(default)]
    pub total: Option<i64>,
    #[serde(default)]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    /// Keyset cursor for the next page; `None` on the last page.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trace page's two differences from every other list survive a
    /// round trip, and an absent `total` stays absent rather than becoming 0.
    #[test]
    fn trace_page_distinguishes_uncounted_from_empty() {
        let uncounted: TracePage<serde_json::Value> =
            serde_json::from_str(r#"{"data": [{"id": "t-1"}], "limit": 50, "offset": 0}"#)
                .expect("test");
        assert_eq!(uncounted.total, None, "an absent total is not zero");
        assert_eq!(uncounted.next_cursor, None);
        assert_eq!(uncounted.data.len(), 1);

        let counted: TracePage<serde_json::Value> = serde_json::from_str(
            r#"{"data": [], "total": 0, "limit": 50, "offset": 0, "next_cursor": "abc"}"#,
        )
        .expect("test");
        assert_eq!(counted.total, Some(0), "a counted zero is not absent");
        assert_eq!(counted.next_cursor.as_deref(), Some("abc"));
    }

    #[test]
    fn data_envelope_roundtrip() {
        let env: Data<serde_json::Value> =
            serde_json::from_str(r#"{"data": {"id": "wf-1"}}"#).expect("test");
        assert_eq!(env.data["id"], "wf-1");
    }

    #[test]
    fn paginated_envelope_parses_the_v1_shape() {
        let env: Paginated<serde_json::Value> =
            serde_json::from_str(r#"{"data": [{"id": 1}], "total": 5, "limit": 20, "offset": 0}"#)
                .expect("test");
        assert_eq!(env.data.len(), 1);
        assert_eq!(env.total, 5);
        assert_eq!(env.limit, 20);
        assert_eq!(env.offset, 0);
    }
}

//! The 2xx envelopes: `{"data": …}` and its paginated sibling.
//!
//! The operational admin endpoints wrap their success bodies this way. The
//! strict v2 admin contract answers its DTOs bare. These types are the one
//! definition of the envelope shape: the server returns them, publishes them
//! in its OpenAPI document, and clients read them back.

use serde::{Deserialize, Serialize};

/// `{"data": T}` — the envelope an operational admin 2xx carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct Data<T> {
    pub data: T,
}

/// The envelope of every list: `data` plus how to get the next page.
///
/// One shape for every collection. A list pages by an opaque **cursor**, not an
/// offset: the lists are append-heavy ledgers read newest-first, so an offset
/// skips or repeats rows as new ones arrive and costs a scan of everything it
/// skips, while a cursor resumes at an exact position in constant time.
///
/// - `next_cursor` is present when a further page exists; pass it back as
///   `?cursor=`, unmodified. Absent on the last page. Do not parse it.
/// - `total` is **opt-in** (`?include_total=true`): counting the filtered set is
///   a full scan on Postgres and InnoDB, so it is computed only when asked. An
///   absent `total` means "not counted", never "no rows".
/// - `limit` is the page size applied. Absent from a complete catalogue that is
///   not paged at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct Page<T> {
    #[serde(default = "Vec::new")]
    pub data: Vec<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
}

impl<T> Page<T> {
    /// A complete catalogue: every row, no paging.
    pub fn complete(data: Vec<T>) -> Self {
        Self {
            data,
            limit: None,
            next_cursor: None,
            total: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absent `total` stays absent rather than becoming 0, and a counted
    /// zero is not absent.
    #[test]
    fn page_distinguishes_uncounted_from_empty() {
        let uncounted: Page<serde_json::Value> =
            serde_json::from_str(r#"{"data": [{"id": "t-1"}], "limit": 50}"#).expect("test");
        assert_eq!(uncounted.total, None, "an absent total is not zero");
        assert_eq!(uncounted.next_cursor, None);
        assert_eq!(uncounted.data.len(), 1);

        let counted: Page<serde_json::Value> =
            serde_json::from_str(r#"{"data": [], "total": 0, "limit": 50, "next_cursor": "abc"}"#)
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

    /// A complete catalogue serializes as just `data`, and a plain `Data<Vec<_>>`
    /// body reads back as a page with no cursor.
    #[test]
    fn a_complete_catalogue_is_a_page_with_no_cursor() {
        let json = serde_json::to_value(Page::complete(vec![1, 2])).expect("test");
        assert_eq!(json, serde_json::json!({"data": [1, 2]}));
        let page: Page<i32> = serde_json::from_value(json).expect("test");
        assert_eq!(page.data, [1, 2]);
        assert!(page.next_cursor.is_none());
    }
}

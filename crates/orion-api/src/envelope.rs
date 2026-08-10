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

#[cfg(test)]
mod tests {
    use super::*;

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

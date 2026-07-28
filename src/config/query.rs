use serde::{Deserialize, Serialize};

use crate::config::validation::require_nonzero;
use crate::errors::OrionError;

/// Page-size bounds applied to every `data_query` (the portable query handler).
///
/// A query that omits `limit` gets `default_limit`; a query asking for more than
/// `max_limit` is rejected (never silently clamped), so no query is ever
/// unbounded by accident. `max_skip` bounds the pagination offset the same way,
/// on every backend — Elasticsearch capped `skip` at its result window while
/// SQL and MongoDB scanned arbitrarily deep (W12). See
/// `proposals/query-dialect.md` §5.12.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueryConfig {
    /// Page size applied when a `data_query` omits `limit`.
    pub default_limit: u64,
    /// Hard maximum page size. A query requesting more is rejected.
    pub max_limit: u64,
    /// Hard maximum `skip` offset. A query skipping more is rejected.
    pub max_skip: u64,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            default_limit: 100,
            max_limit: 1000,
            max_skip: 10_000,
        }
    }
}

impl QueryConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        require_nonzero(self.default_limit, "query.default_limit")?;
        require_nonzero(self.max_limit, "query.max_limit")?;
        if self.default_limit > self.max_limit {
            return Err(OrionError::Config {
                message: format!(
                    "query.default_limit ({}) must not exceed query.max_limit ({})",
                    self.default_limit, self.max_limit
                ),
            });
        }
        Ok(())
    }
}

use serde::{Deserialize, Serialize};

use crate::config::validation::require_nonzero;
use crate::errors::OrionError;

/// Safety bounds applied to every `data_write` (the portable mutation handler).
///
/// A bulk insert asking for more than `max_rows` is rejected (never silently
/// truncated), and an unfiltered `update`/`delete` is rejected unless the caller
/// both sets `"all": true` on the envelope and `allow_unfiltered` is enabled here.
/// See `proposals/data-write-dialect.md` §6.1–6.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WriteConfig {
    /// Hard maximum number of rows per bulk `insert`/`upsert`. Over this is rejected.
    pub max_rows: u64,
    /// Permit an intentionally unfiltered `update`/`delete` (still requires the
    /// per-call `"all": true` acknowledgement). Off by default.
    pub allow_unfiltered: bool,
}

impl Default for WriteConfig {
    fn default() -> Self {
        Self {
            max_rows: 1000,
            allow_unfiltered: false,
        }
    }
}

impl WriteConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        require_nonzero(self.max_rows, "write.max_rows")?;
        Ok(())
    }
}

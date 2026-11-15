//! A workflow as the engine and the generation builder read it.
//!
//! This was `orion_storage::models::rows::Workflow`, a row struct for a table
//! that no longer exists: v2 keeps workflow definitions in the deployment
//! store, and the builder turns a definition into this value in memory. A
//! `sqlx::FromRow` derive and the vestigial storage columns were the only
//! trace of the old shape (Q-05).
//!
//! It lives in `orion-engine` because the loader compiles it and the runtime
//! above the engine consumes the same value; `orion-storage` is below both and
//! no longer needs to know what a workflow is.

use chrono::NaiveDateTime;

#[derive(Debug, Clone)]
pub struct Workflow {
    pub workflow_id: String,
    pub version: i64,
    pub name: String,
    pub description: Option<String>,
    pub priority: i64,
    pub status: String,
    pub rollout_percentage: i64,
    pub condition_json: String,
    pub tasks_json: String,
    /// JSON array of tag strings, still encoded.
    pub tags_json: String,
    /// The engine-managed loop over this workflow's task list, stored as the
    /// `LoopConfig` object verbatim — `{counter, init, increment, max}` — or
    /// `None` for a workflow that runs its tasks exactly once. Optional
    /// because absent and empty are different statements.
    pub loop_json: Option<String>,
    pub continue_on_error: bool,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

impl Workflow {
    /// The stored steps, parsed: `tasks`, and the `loop` whose `setup` runs
    /// before them. Every walk over what a workflow runs needs both.
    ///
    /// `None` when `tasks_json` does not parse, which the load screen reports
    /// by quarantining the workflow's channels. An unparseable `loop_json` is
    /// likewise left to the screen and reads as no loop here.
    pub fn parsed_steps(&self) -> Option<(serde_json::Value, Option<serde_json::Value>)> {
        let tasks = serde_json::from_str(&self.tasks_json).ok()?;
        let loop_config = self
            .loop_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok());
        Some((tasks, loop_config))
    }
}

/// A channel definition as the engine's loader and the runtime read it.
///
/// The same move as [`Workflow`]: the loader resolves each channel's workflow
/// and the runtime builds the channel estate, so the type belongs below both.
#[derive(Debug, Clone)]
pub struct Channel {
    pub channel_id: String,
    pub version: i64,
    pub name: String,
    pub description: Option<String>,
    pub channel_type: String,
    pub protocol: String,
    /// JSON array of HTTP method names, still encoded; `None` for non-REST
    /// channels.
    pub methods_json: Option<String>,
    pub route_pattern: Option<String>,
    pub transport_config_json: String,
    pub workflow_id: Option<String>,
    pub config_json: String,
    pub status: String,
    pub priority: i64,
    /// JSON array of tag strings, still encoded.
    pub tags_json: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

impl Channel {
    /// Tolerant decode of the [`Self::methods_json`] field: a corrupt value
    /// contributes no methods.
    pub fn methods(&self) -> Option<Vec<String>> {
        self.methods_json
            .as_deref()
            .and_then(|m| serde_json::from_str(m).ok())
    }
}

//! What `GET /engine/status` and `POST /engine/reload` answer: the runtime
//! generation a node serves, and what that generation could not load.
//!
//! A reload does not fail when one entity does not load — the entity is
//! quarantined and everything else serves. These types are how a caller that
//! just caused a reload (`package apply`) learns whether what it activated is
//! actually serving, without scraping `/health`, whose detail depends on
//! auth configuration. The issue field names are the ones `/health` already
//! uses, so the two surfaces are one vocabulary.
//!
//! As everywhere in this crate, every field defaults: a response from a
//! server one release away still parses. `load_issues: None` is the skew
//! signal — an empty [`EngineLoadIssues`] means "nothing quarantined", `None`
//! means "this server cannot tell you".

use serde::{Deserialize, Serialize};

/// A channel a generation refused to serve.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ChannelLoadIssueResponse {
    /// The channel's name — what the quarantine is keyed by.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel: String,
    /// The channel's id. Empty from a server that predates it.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel_id: String,
    /// The workflow the channel is bound to, when it names one.
    #[serde(default)]
    pub workflow_id: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub reason: String,
}

/// A plugin version a generation could not load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct PluginLoadIssueResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub plugin: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub version: i64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub digest: String,
    /// `disabled`, `manifest`, `signature`, `artifact`, `compile`, `link`,
    /// `size` or `self_test`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub stage: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub reason: String,
}

/// A model version a generation could not carry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ModelLoadIssueResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub model: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub version: i64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub digest: String,
    /// `disabled`, `admission`, `manifest`, `artifact` or `adapter`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub stage: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub reason: String,
}

/// An enabled connector the registry could not load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectorLoadIssueResponse {
    /// The connector's name — what workflows reference it by.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub connector: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub connector_id: String,
    /// `env_substitution`, `json_parse`, `var_reference`,
    /// `secret_resolution`, `deserialize` or `endpoint`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub stage: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub reason: String,
}

/// What a runtime generation could not load, on the node that answered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct EngineLoadIssues {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channels: Vec<ChannelLoadIssueResponse>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub plugins: Vec<PluginLoadIssueResponse>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub models: Vec<ModelLoadIssueResponse>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub connectors: Vec<ConnectorLoadIssueResponse>,
}

impl EngineLoadIssues {
    /// Nothing is quarantined.
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
            && self.plugins.is_empty()
            && self.models.is_empty()
            && self.connectors.is_empty()
    }
}

/// Node capabilities a plan can predict quarantines from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct EngineCapabilities {
    /// `cron.enabled`: whether this node schedules cron channels.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub cron: bool,
    /// `plugins.enabled`: whether this node runs the plugin sandbox.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub plugins: bool,
    /// `models.enabled`: whether this node carries the model runtime.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub models: bool,
}

/// `POST /api/v1/admin/engine/reload`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct EngineReloadedResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub reloaded: bool,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub workflows_count: u64,
    /// The id of the generation this reload published. `0` from a server
    /// that predates it.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub generation: u64,
    /// What that generation could not load. `None` from a server that
    /// predates the field.
    #[serde(default)]
    pub load_issues: Option<EngineLoadIssues>,
}

/// `GET /api/v1/admin/engine/status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct EngineStatusResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub version: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub uptime_seconds: i64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub workflows_count: u64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub active_workflows: u64,
    /// Distinct channel names across the loaded workflows.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channels: Vec<String>,
    /// The id of the generation this node serves.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub generation: u64,
    /// What that generation could not load. `None` from a server that
    /// predates the field.
    #[serde(default)]
    pub load_issues: Option<EngineLoadIssues>,
    /// What this node is configured to run. `None` from a server that
    /// predates the field.
    #[serde(default)]
    pub capabilities: Option<EngineCapabilities>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An older server's reload answer parses, and says it cannot tell.
    #[test]
    fn a_reload_answer_without_load_issues_is_the_skew_signal() {
        let old: EngineReloadedResponse =
            serde_json::from_str(r#"{"reloaded": true, "workflows_count": 3}"#).expect("parses");
        assert!(old.reloaded);
        assert_eq!(old.generation, 0);
        assert!(old.load_issues.is_none());

        let new: EngineReloadedResponse = serde_json::from_str(
            r#"{"reloaded": true, "workflows_count": 3, "generation": 7,
                "load_issues": {"channels": [{"channel": "c", "reason": "r",
                                              "some_future_key": 1}]}}"#,
        )
        .expect("parses");
        let issues = new.load_issues.expect("present");
        assert_eq!(issues.channels[0].channel, "c");
        assert_eq!(issues.channels[0].channel_id, "");
        assert!(!issues.is_empty());
        assert!(EngineLoadIssues::default().is_empty());
    }
}

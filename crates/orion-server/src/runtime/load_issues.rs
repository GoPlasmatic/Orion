//! What a runtime generation could not load, in the wire shape
//! [`orion_api::EngineLoadIssues`] — the one collector behind `/health`, the
//! `POST /engine/reload` answer and `GET /engine/status`, so the three
//! cannot disagree about what is quarantined.
//!
//! Three of the four lists live on the generation; connectors do not — the
//! registry loads them on its own schedule — so they are read from it.

use orion_api::{
    ChannelLoadIssueResponse, ConnectorLoadIssueResponse, EngineLoadIssues, ModelLoadIssueResponse,
    PluginLoadIssueResponse,
};

use super::RuntimeGeneration;

/// Every load issue of `generation`, plus the connector registry's.
pub async fn collect(
    generation: &RuntimeGeneration,
    connectors: &crate::connector::ConnectorRegistry,
) -> EngineLoadIssues {
    EngineLoadIssues {
        channels: generation
            .channels
            .quarantined()
            .into_iter()
            .map(|c| ChannelLoadIssueResponse {
                channel: c.channel,
                channel_id: c.channel_id,
                workflow_id: c.workflow_id,
                reason: c.reason,
            })
            .collect(),
        plugins: generation
            .plugins
            .issues
            .iter()
            .map(|p| PluginLoadIssueResponse {
                plugin: p.plugin.clone(),
                version: p.version,
                digest: p.digest.clone(),
                stage: p.stage.to_string(),
                reason: p.reason.clone(),
            })
            .collect(),
        models: generation
            .models
            .issues
            .iter()
            .map(|m| ModelLoadIssueResponse {
                model: m.model.clone(),
                version: m.version,
                digest: m.digest.clone(),
                stage: m.stage.to_string(),
                reason: m.reason.clone(),
            })
            .collect(),
        connectors: connectors
            .load_issues()
            .await
            .into_iter()
            .map(|c| ConnectorLoadIssueResponse {
                connector: c.connector,
                connector_id: c.connector_id,
                stage: c.stage.to_string(),
                reason: c.reason,
            })
            .collect(),
    }
}

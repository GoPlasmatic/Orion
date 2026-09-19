//! Whether an applied package is serving: which of its members the target's
//! runtime generation quarantined, and why. Pure — the caller fetches the
//! load issues ([`orion_api::EngineLoadIssues`]) from `POST /engine/reload`
//! or `GET /engine/status`.
//!
//! Matching is on structured identities only, never on a reason's prose.

use orion_api::EngineLoadIssues;
use serde_json::Value;

/// A channel an artifact carries: the quarantine is keyed by name, a
/// package names it by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMember {
    pub channel_id: String,
    pub name: String,
}

/// The entities one artifact carries, by the keys the target knows them by.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageMembers {
    /// `plugin_id`.
    pub plugins: Vec<String>,
    /// `model_id`.
    pub models: Vec<String>,
    /// The connector's name — its conflict key.
    pub connectors: Vec<String>,
    /// `workflow_id`.
    pub workflows: Vec<String>,
    pub channels: Vec<ChannelMember>,
}

impl PackageMembers {
    /// From an artifact's member arrays, in the `/import` item shapes. An
    /// item without the key is skipped: it cannot be told apart on the
    /// target either.
    pub fn from_entries(
        plugins: &[Value],
        models: &[Value],
        connectors: &[Value],
        workflows: &[Value],
        channels: &[Value],
    ) -> Self {
        let ids = |items: &[Value], key: &str| -> Vec<String> {
            let mut out: Vec<String> = items
                .iter()
                .filter_map(|item| item[key].as_str().map(str::to_string))
                .collect();
            out.sort();
            out.dedup();
            out
        };
        // A plugin entry's id may live only in its manifest (`name`).
        let mut plugin_ids: Vec<String> = plugins
            .iter()
            .filter_map(|item| {
                item["plugin_id"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| manifest_name(&item["manifest"]))
            })
            .collect();
        plugin_ids.sort();
        plugin_ids.dedup();
        let mut channel_members: Vec<ChannelMember> = channels
            .iter()
            .filter_map(|item| {
                Some(ChannelMember {
                    channel_id: item["channel_id"].as_str()?.to_string(),
                    name: item["name"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect();
        channel_members.sort_by(|a, b| a.channel_id.cmp(&b.channel_id));
        channel_members.dedup();
        Self {
            plugins: plugin_ids,
            models: ids(models, "model_id"),
            connectors: ids(connectors, "name"),
            workflows: ids(workflows, "workflow_id"),
            channels: channel_members,
        }
    }

    /// What a receipt records: every kind's keys, sorted, a channel by id.
    pub fn inventory(&self) -> orion_api::PackageInventory {
        orion_api::PackageInventory {
            plugins: self.plugins.clone(),
            models: self.models.clone(),
            connectors: self.connectors.clone(),
            workflows: self.workflows.clone(),
            channels: self.channels.iter().map(|c| c.channel_id.clone()).collect(),
        }
        .normalized()
    }
}

/// A plugin manifest's `name`, whether the entry carries it as TOML text or
/// as the object an export writes.
fn manifest_name(manifest: &Value) -> Option<String> {
    match manifest {
        Value::String(text) => toml::from_str::<toml::Value>(text)
            .ok()?
            .get("name")?
            .as_str()
            .map(str::to_string),
        other => other["name"].as_str().map(str::to_string),
    }
}

/// A member the target is not serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedEntity {
    /// `plugins`, `models`, `connectors`, `workflows` or `channels` — the
    /// `kind/id` vocabulary `package` prints.
    pub kind: &'static str,
    pub id: String,
    pub reason: String,
}

impl std::fmt::Display for QuarantinedEntity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}: {}", self.kind, self.id, self.reason)
    }
}

/// Which of `members` the issues say are not serving, and why. Ordered by
/// kind — plugins, models, connectors, workflows, channels — then by id.
///
/// - A member channel matches by `channel_id`, or by name from a server
///   that does not report ids.
/// - A member workflow is reported when a channel of *another* package,
///   bound to it, is quarantined — a member channel's issue is reported
///   once, as the channel.
/// - Plugins, models and connectors match by id (a connector by name), with
///   the stage in front of the reason.
pub fn quarantined_members(
    members: &PackageMembers,
    issues: &EngineLoadIssues,
) -> Vec<QuarantinedEntity> {
    let mut out = Vec::new();
    for issue in &issues.plugins {
        if members.plugins.contains(&issue.plugin) {
            out.push(QuarantinedEntity {
                kind: "plugins",
                id: issue.plugin.clone(),
                reason: format!("{}: {}", issue.stage, issue.reason),
            });
        }
    }
    for issue in &issues.models {
        if members.models.contains(&issue.model) {
            out.push(QuarantinedEntity {
                kind: "models",
                id: issue.model.clone(),
                reason: format!("{}: {}", issue.stage, issue.reason),
            });
        }
    }
    for issue in &issues.connectors {
        if members.connectors.contains(&issue.connector) {
            out.push(QuarantinedEntity {
                kind: "connectors",
                id: issue.connector.clone(),
                reason: format!("{}: {}", issue.stage, issue.reason),
            });
        }
    }
    let member_channel = |issue: &orion_api::ChannelLoadIssueResponse| {
        members.channels.iter().find(|m| {
            if issue.channel_id.is_empty() {
                m.name == issue.channel
            } else {
                m.channel_id == issue.channel_id
            }
        })
    };
    let mut workflows = Vec::new();
    let mut channels = Vec::new();
    for issue in &issues.channels {
        match member_channel(issue) {
            Some(member) => channels.push(QuarantinedEntity {
                kind: "channels",
                id: member.channel_id.clone(),
                reason: issue.reason.clone(),
            }),
            None => {
                if let Some(workflow) = issue
                    .workflow_id
                    .as_ref()
                    .filter(|w| members.workflows.contains(w))
                {
                    workflows.push(QuarantinedEntity {
                        kind: "workflows",
                        id: workflow.clone(),
                        reason: format!(
                            "channel '{}' is quarantined: {}",
                            issue.channel, issue.reason
                        ),
                    });
                }
            }
        }
    }
    let by_id = |a: &QuarantinedEntity, b: &QuarantinedEntity| a.id.cmp(&b.id);
    out.sort_by(|a, b| kind_rank(a.kind).cmp(&kind_rank(b.kind)).then(by_id(a, b)));
    workflows.sort_by(by_id);
    channels.sort_by(by_id);
    out.extend(workflows);
    out.extend(channels);
    out
}

fn kind_rank(kind: &str) -> usize {
    ["plugins", "models", "connectors", "workflows", "channels"]
        .iter()
        .position(|k| *k == kind)
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orion_api::{
        ChannelLoadIssueResponse, ConnectorLoadIssueResponse, ModelLoadIssueResponse,
        PluginLoadIssueResponse,
    };
    use serde_json::json;

    fn members() -> PackageMembers {
        PackageMembers::from_entries(
            &[json!({"manifest": "abi = \"orion:plugin@1.0.0\"\nname = \"acme.scoring\"\n"})],
            &[json!({"model_id": "acme.fraud"})],
            &[json!({"name": "orders-db"})],
            &[
                json!({"workflow_id": "orders"}),
                json!({"workflow_id": "sweep"}),
            ],
            &[
                json!({"channel_id": "orders-api", "name": "orders"}),
                json!({"channel_id": "orders-sweep", "name": "sweep"}),
            ],
        )
    }

    fn channel(
        name: &str,
        id: &str,
        workflow: Option<&str>,
        reason: &str,
    ) -> ChannelLoadIssueResponse {
        ChannelLoadIssueResponse {
            channel: name.to_string(),
            channel_id: id.to_string(),
            workflow_id: workflow.map(str::to_string),
            reason: reason.to_string(),
        }
    }

    #[test]
    fn members_come_from_the_import_shapes() {
        let m = members();
        assert_eq!(m.plugins, ["acme.scoring"]);
        assert_eq!(m.models, ["acme.fraud"]);
        assert_eq!(m.connectors, ["orders-db"]);
        assert_eq!(m.workflows, ["orders", "sweep"]);
        assert_eq!(m.channels[1].name, "sweep");
    }

    #[test]
    fn every_kind_matches_on_its_identity_and_nothing_else_does() {
        let issues = EngineLoadIssues {
            channels: vec![
                // A member channel, by id.
                channel("sweep", "orders-sweep", Some("sweep"), "cron is disabled"),
                // Another package's channel bound to a member workflow.
                channel(
                    "billing-feed",
                    "billing-feed",
                    Some("orders"),
                    "workflow broke",
                ),
                // Another package's channel on its own workflow: not ours.
                channel("other", "other", Some("other"), "nope"),
            ],
            plugins: vec![PluginLoadIssueResponse {
                plugin: "acme.scoring".to_string(),
                stage: "signature".to_string(),
                reason: "does not verify".to_string(),
                ..Default::default()
            }],
            models: vec![ModelLoadIssueResponse {
                model: "someone.else".to_string(),
                ..Default::default()
            }],
            connectors: vec![ConnectorLoadIssueResponse {
                connector: "orders-db".to_string(),
                stage: "secret_resolution".to_string(),
                reason: "ORDERS_DB_URL is not set".to_string(),
                ..Default::default()
            }],
        };
        let found: Vec<String> = quarantined_members(&members(), &issues)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            found,
            [
                "plugins/acme.scoring: signature: does not verify",
                "connectors/orders-db: secret_resolution: ORDERS_DB_URL is not set",
                "workflows/orders: channel 'billing-feed' is quarantined: workflow broke",
                "channels/orders-sweep: cron is disabled",
            ]
        );
    }

    /// A server that does not report channel ids is matched by name.
    #[test]
    fn a_channel_without_an_id_matches_by_name() {
        let issues = EngineLoadIssues {
            channels: vec![channel("orders", "", None, "broken")],
            ..Default::default()
        };
        let found = quarantined_members(&members(), &issues);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "orders-api");
        assert!(quarantined_members(&members(), &EngineLoadIssues::default()).is_empty());
    }
}

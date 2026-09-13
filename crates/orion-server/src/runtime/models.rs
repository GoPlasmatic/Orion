//! The generation-side of models: which channels a model that did not load
//! takes down with it, and the warm-up a published generation runs.
//!
//! Both read the rows a generation was built from — the channels, the
//! workflows they name, the [`ModelSet`] the models became — and one needs
//! the node's live components to load through, which is why they sit here
//! above `model/` and `channel/` rather than in either.
//!
//! **Quarantine.** A workflow calling `model_infer` with a literal
//! `input.model` the set does not hold cannot serve: every request would
//! fail the task with `unavailable`. That is the plugin case exactly — a
//! workflow naming a function whose plugin did not load — and it gets the
//! same treatment: a [`ChannelLoadIssue`] for each channel routing to the
//! workflow, so the channel is refused at ingress with a `503` naming the
//! model and why, and `/health` says so. A computed `model` is not seen
//! here; the model it resolves to is decided per message and refused per
//! message.
//!
//! **Preload.** A load is the expensive step, and `models.preload` decides
//! which loads a generation pays before its first request rather than at it:
//! `referenced` (the default) warms every model an active workflow names,
//! `all` every entry, `none` nothing. The warm-up is spawned after the
//! publish and never blocks it — a node serves the moment the generation is
//! published, and the first inference of a model still warming shares the
//! load in flight through the cache's single-flight path.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use crate::channel::ChannelLoadIssue;
use crate::config::{ModelPreload, ModelsConfig};
use crate::connector::ConnectorRegistry;
use crate::model::{ModelSet, ModelsRuntime, literal_references};
use crate::storage::models::{Channel, Workflow};

use super::generation::RuntimeGeneration;

/// One issue per channel whose workflow names, by literal id, a model `set`
/// does not serve. The reason names the model and carries the set's own
/// reason for it — the load issue's stage and text when the row exists, or
/// that no active version does.
pub fn load_issues(
    channels: &[Channel],
    workflows: &[Workflow],
    set: &ModelSet,
) -> Vec<ChannelLoadIssue> {
    // Every active version of a workflow id: a rollout serves more than
    // one, and a channel routes to all of them.
    let mut by_id: HashMap<&str, Vec<&Workflow>> = HashMap::new();
    for workflow in workflows {
        by_id
            .entry(workflow.workflow_id.as_str())
            .or_default()
            .push(workflow);
    }
    let mut issues = Vec::new();
    for channel in channels {
        let Some(workflow_id) = channel.workflow_id.as_deref() else {
            continue;
        };
        let Some(versions) = by_id.get(workflow_id) else {
            continue;
        };
        let mut reasons: Vec<String> = Vec::new();
        for workflow in versions {
            let Ok(tasks) = serde_json::from_str::<serde_json::Value>(&workflow.tasks_json) else {
                continue;
            };
            for (task_id, model) in literal_references(&tasks) {
                if set.get(&model).is_some() {
                    continue;
                }
                let why = set
                    .issue_for(&model)
                    .map(|issue| format!("{}: {}", issue.stage, issue.reason))
                    .unwrap_or_else(|| "no active version is admitted".to_string());
                let reason = format!(
                    "task '{task_id}' calls model_infer on model '{model}', which is not \
                     available on this node: {why}"
                );
                if !reasons.contains(&reason) {
                    reasons.push(reason);
                }
            }
        }
        if !reasons.is_empty() {
            issues.push(ChannelLoadIssue {
                channel: channel.name.clone(),
                reason: format!("workflow '{workflow_id}': {}", reasons.join("; ")),
            });
        }
    }
    issues
}

/// What a warm-up loads through: the node's model runtime and the
/// components a cold load resolves the artifact with.
#[derive(Clone)]
pub struct PreloadDeps {
    pub models: Option<Arc<ModelsRuntime>>,
    pub config: Arc<ModelsConfig>,
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
}

impl PreloadDeps {
    /// Borrowed off a live `AppState`, for the reload path.
    pub fn from_state(state: &crate::server::state::AppState) -> Self {
        Self {
            models: state.models.clone(),
            config: Arc::new(state.config.models.clone()),
            registry: state.connector_registry.clone(),
            client: state.http_client.clone(),
        }
    }
}

/// The ids `config.preload` selects out of `generation.models`, sorted:
/// every entry for `all`, the literal references of `workflows` for
/// `referenced`, nothing for `none`. An id the set does not serve is not
/// selected — its workflows are quarantined, and there is nothing to load.
pub fn preload_targets(
    config: &ModelsConfig,
    generation: &RuntimeGeneration,
    workflows: &[Workflow],
) -> Vec<String> {
    let selected: BTreeSet<String> = match config.preload {
        ModelPreload::None => BTreeSet::new(),
        ModelPreload::All => generation.models.ids().map(str::to_string).collect(),
        ModelPreload::Referenced => workflows
            .iter()
            .filter_map(|w| serde_json::from_str::<serde_json::Value>(&w.tasks_json).ok())
            .flat_map(|tasks| literal_references(&tasks))
            .map(|(_, model)| model)
            .filter(|model| generation.models.get(model).is_some())
            .collect(),
    };
    selected.into_iter().collect()
}

/// Warm the models `config.preload` selects, in the background. Best
/// effort: each load is logged at info, a failure at warn with its stage,
/// and nothing here can fail the publish that preceded it. Each model loads
/// through the same cache path an inference uses, on the default runtime
/// for its format, so a request arriving mid-warm-up shares the load.
pub fn spawn_preload(
    deps: PreloadDeps,
    generation: Arc<RuntimeGeneration>,
    workflows: &[Workflow],
) {
    let Some(models) = deps.models.clone() else {
        return;
    };
    let targets = preload_targets(&deps.config, &generation, workflows);
    if targets.is_empty() {
        return;
    }
    tracing::info!(
        generation = generation.id,
        preload = deps.config.preload.as_str(),
        models = ?targets,
        "Preloading models"
    );
    let host = crate::model::InferenceHost::node(
        &models,
        deps.config.clone(),
        deps.registry.clone(),
        deps.client.clone(),
    );
    tokio::spawn(async move {
        for id in targets {
            let Some(entry) = generation.models.get(&id).cloned() else {
                continue;
            };
            let (runtime, device) = match models
                .runtimes
                .default_for(&deps.config, &entry.manifest.format)
            {
                Ok(selected) => selected,
                Err(selection) => {
                    tracing::warn!(
                        model = %id,
                        reason = %selection,
                        "Model not preloaded: no runtime for its format on this node"
                    );
                    continue;
                }
            };
            // The outcome is logged and counted by the load path itself;
            // a warm-up has no caller to answer.
            let _ = crate::model::load_model(&host, entry, runtime, device, "preload").await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use dataflow_rs::datalogic_rs as datalogic;
    use serde_json::json;

    fn channel(name: &str, workflow_id: &str) -> Channel {
        let now = chrono::Utc::now().naive_utc();
        Channel {
            channel_id: format!("ch-{name}"),
            version: 1,
            name: name.to_string(),
            description: None,
            channel_type: "sync".to_string(),
            protocol: "http".to_string(),
            methods_json: None,
            route_pattern: None,
            topic: None,
            consumer_group: None,
            transport_config_json: "{}".to_string(),
            workflow_id: Some(workflow_id.to_string()),
            config_json: "{}".to_string(),
            status: "active".to_string(),
            priority: 0,
            tags_json: "[]".to_string(),
            created_at: now,
            updated_at: now,
        }
    }

    fn workflow(id: &str, tasks: serde_json::Value) -> Workflow {
        let now = chrono::Utc::now().naive_utc();
        Workflow {
            workflow_id: id.to_string(),
            version: 1,
            name: id.to_string(),
            description: None,
            priority: 0,
            status: "active".to_string(),
            rollout_percentage: 100,
            condition_json: "true".to_string(),
            tasks_json: tasks.to_string(),
            tags_json: "[]".to_string(),
            loop_json: None,
            continue_on_error: false,
            created_at: now,
            updated_at: now,
        }
    }

    fn infer(id: &str, model: serde_json::Value) -> serde_json::Value {
        json!({"id": id, "name": id, "function": {"name": "model_infer",
            "input": {"model": model, "input": {"var": ""}}}})
    }

    fn model_row(id: &str, admission: serde_json::Value) -> crate::storage::models::Model {
        let now = chrono::Utc::now().naive_utc();
        crate::storage::models::Model {
            model_id: id.to_string(),
            version: 1,
            status: "active".to_string(),
            digest: "sha256:abc".to_string(),
            manifest_json: crate::model::fixture::MANIFEST.replace("ada.c4-tiny", id),
            artifact_json: json!({"connector": "bucket", "key": "k", "digest": "sha256:abc"})
                .to_string(),
            admission_json: admission.to_string(),
            stats_json: None,
            tags_json: "[]".to_string(),
            signature: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn set(rows: &[crate::storage::models::Model]) -> ModelSet {
        let engine = datalogic::Engine::builder().with_templating(true).build();
        ModelSet::load_active(rows, &ModelsConfig::default(), true, &engine)
    }

    /// A channel whose workflow names a served model is untouched; one
    /// naming a model with a load issue carries that issue's reason; one
    /// naming a model with no active row says so. A computed reference is
    /// never an issue.
    #[test]
    fn a_literal_reference_to_an_unavailable_model_quarantines_the_channel() {
        let rows = [
            model_row("ada.ok", json!({"state": "passed"})),
            model_row(
                "ada.bad",
                json!({"state": "failed", "stage": "fetch", "reason": "GET answered HTTP 503"}),
            ),
        ];
        let set = set(&rows);
        let workflows = [
            workflow("w-ok", json!([infer("t", json!("ada.ok"))])),
            workflow(
                "w-bad",
                json!([{"id": "g", "tasks": [infer("t", json!("ada.bad"))]}]),
            ),
            workflow("w-none", json!([infer("t", json!("ada.none"))])),
            workflow("w-dyn", json!([infer("t", json!({"var": "data.model"}))])),
        ];
        let channels = [
            channel("ok", "w-ok"),
            channel("bad", "w-bad"),
            channel("none", "w-none"),
            channel("dyn", "w-dyn"),
            channel("orphan", "w-missing"),
        ];
        let issues = load_issues(&channels, &workflows, &set);
        let names: Vec<&str> = issues.iter().map(|i| i.channel.as_str()).collect();
        assert_eq!(names, ["bad", "none"]);
        assert!(
            issues[0].reason.contains("task 't'")
                && issues[0].reason.contains("model 'ada.bad'")
                && issues[0]
                    .reason
                    .contains("admission: admission is 'failed'")
                && issues[0].reason.contains("GET answered HTTP 503"),
            "{}",
            issues[0].reason
        );
        assert!(
            issues[1].reason.contains("model 'ada.none'")
                && issues[1].reason.contains("no active version is admitted"),
            "{}",
            issues[1].reason
        );
    }

    /// `referenced` selects what active workflows name (and the set serves),
    /// `all` every served entry, `none` nothing.
    #[test]
    fn preload_targets_follow_the_config() {
        let rows = [
            model_row("ada.a", json!({"state": "passed"})),
            model_row("ada.b", json!({"state": "passed"})),
            model_row("ada.c", json!({"state": "pending"})),
        ];
        let generation = RuntimeGeneration {
            id: 1,
            engine: Arc::new(dataflow_rs::Engine::builder().build().expect("builds")),
            channels: Arc::new(crate::channel::ChannelSnapshot::empty()),
            functions: crate::engine::FunctionRegistry::builtin().clone(),
            plugins: Arc::new(crate::plugin::PluginSet::empty()),
            models: Arc::new(set(&rows)),
        };
        let workflows = [
            workflow(
                "w",
                json!([infer("t", json!("ada.a")), infer("u", json!("ada.c"))]),
            ),
            workflow("v", json!([infer("t", json!("ada.a"))])),
        ];
        let targets = |preload| {
            let config = ModelsConfig {
                preload,
                ..ModelsConfig::default()
            };
            preload_targets(&config, &generation, &workflows)
        };
        assert_eq!(targets(ModelPreload::Referenced), ["ada.a"]);
        assert_eq!(targets(ModelPreload::All), ["ada.a", "ada.b"]);
        assert!(targets(ModelPreload::None).is_empty());
    }
}

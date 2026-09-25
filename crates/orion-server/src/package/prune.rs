//! What `package apply --prune` removes: the entities the package's current
//! applied version carried and this artifact does not. Pure — the caller
//! fetches the receipts and the target's active rows.
//!
//! The baseline is always a receipt's inventory, never tags or a listing of
//! the estate, so nothing the package did not record is ever a candidate.

use orion_api::PackageInventory;
use serde_json::Value;

use crate::engine::FunctionRegistry;

/// What a removal does to the entity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PruneMode {
    /// Archive a channel, workflow, plugin or model, and disable a
    /// connector: reversible, and it frees the route or schedule.
    #[default]
    Archive,
    /// Delete the entity, every version of it.
    Delete,
}

/// One entity to remove, in the `kind/id` vocabulary `package` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    /// `plugins`, `connectors`, `models`, `workflows` or `channels`.
    pub kind: &'static str,
    /// The kind's conflict key: a connector's name, every other kind's id.
    pub id: String,
}

impl std::fmt::Display for Removal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.kind, self.id)
    }
}

/// The receipt removals are measured from: the package's `current` one.
#[derive(Debug, Clone, Copy)]
pub struct Baseline<'a> {
    /// `name@version`, for messages.
    pub package: &'a str,
    /// `None` when the receipt predates inventories.
    pub inventory: Option<&'a PackageInventory>,
}

/// Everything `--prune` would do, in the order it does it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrunePlan {
    /// `name@version` of the baseline receipt; `None` on a first apply.
    pub baseline: Option<String>,
    /// The baseline receipt recorded no inventory, so nothing is pruned
    /// this time — the apply records one for the next.
    pub baseline_without_inventory: bool,
    /// Channels, removed *before* activation: the route and name gates read
    /// active rows, so a route moving to a new channel id could not
    /// activate while the old channel is still active.
    pub early: Vec<Removal>,
    /// Workflows, then plugins, models and connectors, removed after
    /// activation: their dependants' gates only pass once the new versions
    /// are active.
    pub late: Vec<Removal>,
    /// Candidates another package's current version now carries, with that
    /// package's `name@version` — left alone.
    pub kept: Vec<(Removal, String)>,
    /// Kinds the baseline carried and this artifact carries none of, with
    /// how many of them are pruned — the "missing directory" accident.
    pub emptied: Vec<(&'static str, usize)>,
}

impl PrunePlan {
    /// Every removal, in execution order.
    pub fn removals(&self) -> impl Iterator<Item = &Removal> {
        self.early.iter().chain(self.late.iter())
    }

    pub fn is_empty(&self) -> bool {
        self.early.is_empty() && self.late.is_empty()
    }
}

/// The order `late` removals run in: a workflow before what it calls, so
/// the plugin and model gates (no active workflow may name them) pass.
const LATE_ORDER: [&str; 4] = ["workflows", "plugins", "models", "connectors"];

/// What `--prune` would remove, given the package's current receipt, what
/// this artifact carries, and every *other* package's current inventory.
pub fn prune_plan(
    baseline: Option<Baseline<'_>>,
    next: &PackageInventory,
    others_current: &[(String, PackageInventory)],
) -> PrunePlan {
    let Some(baseline) = baseline else {
        return PrunePlan::default();
    };
    let mut plan = PrunePlan {
        baseline: Some(baseline.package.to_string()),
        ..PrunePlan::default()
    };
    let Some(previous) = baseline.inventory else {
        plan.baseline_without_inventory = true;
        return plan;
    };
    let previous_kinds = previous.kinds();
    let next_kinds = next.kinds();
    for ((kind, before), (_, after)) in previous_kinds.iter().zip(next_kinds.iter()) {
        let mut pruned = 0usize;
        for id in before.iter().filter(|id| !after.contains(id)) {
            let removal = Removal {
                kind,
                id: id.clone(),
            };
            let owner = others_current.iter().find(|(_, inventory)| {
                inventory
                    .kinds()
                    .iter()
                    .any(|(k, ids)| k == kind && ids.contains(id))
            });
            if let Some((owner, _)) = owner {
                plan.kept.push((removal, owner.clone()));
                continue;
            }
            pruned += 1;
            if *kind == "channels" {
                plan.early.push(removal);
            } else {
                plan.late.push(removal);
            }
        }
        if after.is_empty() && pruned > 0 {
            plan.emptied.push((kind, pruned));
        }
    }
    plan.late.sort_by_key(|r| {
        LATE_ORDER
            .iter()
            .position(|k| *k == r.kind)
            .unwrap_or(usize::MAX)
    });
    plan
}

/// A removal the CLI refuses before removing anything: something outside
/// the prune still depends on the entity. The server refuses a plugin or a
/// model with a live dependant itself; nothing refuses a workflow or a
/// connector, so these are checked here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub removal: Removal,
    /// Who still depends on it, e.g. `active channel 'billing-in' (not in
    /// this package) still routes to it`.
    pub reason: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot prune {}: {}", self.removal, self.reason)
    }
}

/// The rows a reference check reads: the artifact's own member entries and
/// the target's active rows (`GET /{kind}/export?status=active`).
#[derive(Debug, Clone, Copy, Default)]
pub struct References<'a> {
    pub carried_channels: &'a [Value],
    pub carried_workflows: &'a [Value],
    pub carried_models: &'a [Value],
    pub active_channels: &'a [Value],
    pub active_workflows: &'a [Value],
    pub active_models: &'a [Value],
}

/// Every workflow or connector in `plan` that something the prune leaves
/// in place still names. An entry the artifact carries is judged by its new
/// content, not the target's row; a row the prune removes is not a holder.
pub fn refusals(
    plan: &PrunePlan,
    refs: &References<'_>,
    functions: &FunctionRegistry,
) -> Vec<Refusal> {
    let pruned = |kind: &str, id: &str| plan.removals().any(|r| r.kind == kind && r.id == id);
    let carried = |items: &[Value], key: &str, id: &str| items.iter().any(|i| i[key] == id);
    let mut out = Vec::new();
    for removal in plan.removals() {
        match removal.kind {
            "workflows" => {
                let routes_here = |channel: &&Value| channel["workflow_id"] == removal.id.as_str();
                for channel in refs.carried_channels.iter().filter(routes_here) {
                    out.push(Refusal {
                        removal: removal.clone(),
                        reason: format!(
                            "channel '{}' in this artifact still routes to it",
                            label(channel, "channel_id")
                        ),
                    });
                }
                for channel in refs.active_channels.iter().filter(routes_here) {
                    let id = channel["channel_id"].as_str().unwrap_or_default();
                    if pruned("channels", id) || carried(refs.carried_channels, "channel_id", id) {
                        continue;
                    }
                    out.push(Refusal {
                        removal: removal.clone(),
                        reason: format!(
                            "active channel '{}' (not in this package) still routes to it",
                            label(channel, "channel_id")
                        ),
                    });
                }
            }
            "connectors" => {
                let names = |workflow: &&Value| {
                    crate::engine::connector_refs(
                        &workflow["tasks"],
                        workflow.get("loop"),
                        functions,
                    )
                    .iter()
                    .any(|r| r.connector == removal.id)
                };
                for workflow in refs.carried_workflows.iter().filter(names) {
                    out.push(Refusal {
                        removal: removal.clone(),
                        reason: format!(
                            "workflow '{}' in this artifact still uses it",
                            label(workflow, "workflow_id")
                        ),
                    });
                }
                for workflow in refs.active_workflows.iter().filter(names) {
                    let id = workflow["workflow_id"].as_str().unwrap_or_default();
                    if pruned("workflows", id) || carried(refs.carried_workflows, "workflow_id", id)
                    {
                        continue;
                    }
                    out.push(Refusal {
                        removal: removal.clone(),
                        reason: format!(
                            "active workflow '{}' (not in this package) still uses it",
                            label(workflow, "workflow_id")
                        ),
                    });
                }
                let fetches =
                    |model: &&Value| model["artifact"]["connector"] == removal.id.as_str();
                for model in refs.carried_models.iter().filter(fetches) {
                    out.push(Refusal {
                        removal: removal.clone(),
                        reason: format!(
                            "model '{}' in this artifact is fetched through it",
                            label(model, "model_id")
                        ),
                    });
                }
                for model in refs.active_models.iter().filter(fetches) {
                    let id = model["model_id"].as_str().unwrap_or_default();
                    if pruned("models", id) || carried(refs.carried_models, "model_id", id) {
                        continue;
                    }
                    out.push(Refusal {
                        removal: removal.clone(),
                        reason: format!(
                            "active model '{}' (not in this package) is fetched through it",
                            label(model, "model_id")
                        ),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// A row's display name, falling back to its id.
fn label<'a>(row: &'a Value, id_key: &str) -> &'a str {
    row["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .or_else(|| row[id_key].as_str())
        .unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inventory(channels: &[&str], workflows: &[&str], plugins: &[&str]) -> PackageInventory {
        let ids = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect();
        PackageInventory {
            channels: ids(channels),
            workflows: ids(workflows),
            plugins: ids(plugins),
            ..PackageInventory::default()
        }
    }

    fn ids(removals: &[Removal]) -> Vec<String> {
        removals.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_difference_is_split_channels_first_then_workflows_before_what_they_call() {
        let before = inventory(&["a", "b"], &["w1", "w2"], &["p"]);
        let after = inventory(&["a"], &["w1"], &["q"]);
        let plan = prune_plan(
            Some(Baseline {
                package: "orders@1.0.0",
                inventory: Some(&before),
            }),
            &after,
            &[],
        );
        assert_eq!(plan.baseline.as_deref(), Some("orders@1.0.0"));
        assert_eq!(ids(&plan.early), ["channels/b"]);
        assert_eq!(ids(&plan.late), ["workflows/w2", "plugins/p"]);
        assert!(plan.kept.is_empty());
        assert!(
            plan.emptied.is_empty(),
            "the artifact still carries plugins"
        );
    }

    #[test]
    fn a_first_apply_prunes_nothing_and_an_old_receipt_says_why() {
        let next = inventory(&["a"], &[], &[]);
        assert_eq!(prune_plan(None, &next, &[]), PrunePlan::default());
        let plan = prune_plan(
            Some(Baseline {
                package: "orders@1.0.0",
                inventory: None,
            }),
            &next,
            &[],
        );
        assert!(plan.baseline_without_inventory);
        assert!(plan.is_empty());
    }

    #[test]
    fn identical_inventories_prune_nothing() {
        let same = inventory(&["a"], &["w"], &["p"]);
        let plan = prune_plan(
            Some(Baseline {
                package: "orders@1.0.0",
                inventory: Some(&same),
            }),
            &same,
            &[],
        );
        assert!(plan.is_empty());
        assert!(plan.emptied.is_empty());
    }

    #[test]
    fn what_another_package_now_carries_is_kept() {
        let before = inventory(&["a", "moved"], &[], &[]);
        let after = inventory(&["a"], &[], &[]);
        let others = vec![("billing@2.1.0".to_string(), inventory(&["moved"], &[], &[]))];
        let plan = prune_plan(
            Some(Baseline {
                package: "orders@1.0.0",
                inventory: Some(&before),
            }),
            &after,
            &others,
        );
        assert!(plan.is_empty());
        assert_eq!(plan.kept.len(), 1);
        assert_eq!(plan.kept[0].0.to_string(), "channels/moved");
        assert_eq!(plan.kept[0].1, "billing@2.1.0");
    }

    #[test]
    fn a_kind_the_artifact_no_longer_carries_is_reported() {
        let before = inventory(&[], &[], &["p1", "p2", "p3"]);
        let after = inventory(&[], &[], &[]);
        let plan = prune_plan(
            Some(Baseline {
                package: "orders@1.0.0",
                inventory: Some(&before),
            }),
            &after,
            &[],
        );
        assert_eq!(plan.emptied, [("plugins", 3)]);
        assert_eq!(plan.late.len(), 3);
    }

    fn plan_removing(kind: &'static str, id: &str) -> PrunePlan {
        PrunePlan {
            late: vec![Removal {
                kind,
                id: id.to_string(),
            }],
            ..PrunePlan::default()
        }
    }

    #[test]
    fn a_workflow_an_outside_channel_routes_to_is_refused() {
        let plan = plan_removing("workflows", "w");
        let active = [
            json!({"channel_id": "billing-in", "name": "billing-in", "workflow_id": "w"}),
            json!({"channel_id": "ours", "name": "ours", "workflow_id": "w"}),
        ];
        // The package's own channel is re-carried pointing elsewhere.
        let carried = [json!({"channel_id": "ours", "name": "ours", "workflow_id": "w2"})];
        let found = refusals(
            &plan,
            &References {
                carried_channels: &carried,
                active_channels: &active,
                ..References::default()
            },
            FunctionRegistry::builtin(),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].to_string(),
            "cannot prune workflows/w: active channel 'billing-in' (not in this package) still \
             routes to it"
        );
    }

    #[test]
    fn a_connector_a_remaining_workflow_or_model_uses_is_refused() {
        let plan = plan_removing("connectors", "crm");
        let reads = |id: &str| {
            json!({"workflow_id": id, "tasks": [
                {"id": "t", "function": {"name": "http_call", "input": {"connector": "crm"}}}
            ]})
        };
        let active_workflows = [reads("outside")];
        let carried_workflows = [reads("inside")];
        let active_models = [json!({"model_id": "m", "artifact": {"connector": "crm"}})];
        let found: Vec<String> = refusals(
            &plan,
            &References {
                carried_workflows: &carried_workflows,
                active_workflows: &active_workflows,
                active_models: &active_models,
                ..References::default()
            },
            FunctionRegistry::builtin(),
        )
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(
            found,
            [
                "cannot prune connectors/crm: workflow 'inside' in this artifact still uses it",
                "cannot prune connectors/crm: active workflow 'outside' (not in this package) \
                 still uses it",
                "cannot prune connectors/crm: active model 'm' (not in this package) is fetched \
                 through it",
            ]
        );
    }

    #[test]
    fn a_holder_the_prune_also_removes_does_not_refuse() {
        let mut plan = plan_removing("workflows", "w");
        plan.early.push(Removal {
            kind: "channels",
            id: "old".to_string(),
        });
        let active = [json!({"channel_id": "old", "name": "old", "workflow_id": "w"})];
        let found = refusals(
            &plan,
            &References {
                active_channels: &active,
                ..References::default()
            },
            FunctionRegistry::builtin(),
        );
        assert!(found.is_empty(), "{found:?}");
    }
}

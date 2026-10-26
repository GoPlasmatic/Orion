//! Which connections each workflow of a generation may call through
//! `connector_call`.
//!
//! The deployment manifest is the reviewed statement of it: a workflow
//! member's `requires` names the connections it depends on. A workflow may
//! call a connection its member requires, and only the operations its own
//! `connector_call` tasks name for it. A generation built from anything
//! other than a manifest grants nothing.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::extensions::ConnectionReference;
use crate::extensions::context::AuthorityGrant;
use crate::extensions::deployment_store::{CanonicalManifest, EntityKind};
use crate::storage::models::Workflow;

/// Per engine workflow id, the `(connection, operation)` pairs its
/// `connector_call` tasks may use.
#[derive(Debug, Clone, Default)]
pub struct ConnectorAuthority {
    by_workflow: BTreeMap<String, AuthorityGrant>,
    /// The revision of each connection the manifest deploys: the one a call
    /// resolves, never a later revision stored since.
    connections: BTreeMap<String, u64>,
    /// What each connection over a workflow provider package may call as
    /// its dependencies, keyed by
    /// [`dependency_key`](crate::extensions::workflow_provider::dependency_key):
    /// the bound connection at the revision the member requires, and the
    /// operations the package declares for the alias. A package invocation
    /// derives its dependency calls from these; no workflow is granted them.
    dependencies: BTreeMap<String, (ConnectionReference, BTreeSet<String>)>,
}

impl ConnectorAuthority {
    /// The authority `workflows` have under `manifest`. A workflow is keyed
    /// by both ids the engine may give it: `workflow_id`, and
    /// `workflow_id:v{version}` when versions share a channel's traffic.
    pub fn from_manifest(manifest: &CanonicalManifest, workflows: &[Workflow]) -> Self {
        let mut required: BTreeMap<(&str, u64), BTreeSet<&str>> = BTreeMap::new();
        for member in &manifest.members {
            if member.entity.kind == EntityKind::Workflow {
                required
                    .entry((member.entity.id.as_str(), member.entity.revision))
                    .or_default()
                    .extend(
                        member
                            .requires
                            .iter()
                            .filter(|r| r.kind == EntityKind::Connection)
                            .map(|r| r.id.as_str()),
                    );
            }
        }

        let connections = manifest
            .members
            .iter()
            .filter(|m| m.entity.kind == EntityKind::Connection)
            .map(|m| (m.entity.id.clone(), m.entity.revision))
            .collect();

        let mut by_workflow: BTreeMap<String, AuthorityGrant> = BTreeMap::new();
        for wf in workflows {
            let version = u64::try_from(wf.version).unwrap_or_default();
            let Some(connections) = required.get(&(wf.workflow_id.as_str(), version)) else {
                continue;
            };
            let grant = workflow_grant(wf, connections);
            by_workflow
                .entry(wf.workflow_id.clone())
                .or_default()
                .extend(&grant);
            by_workflow.insert(format!("{}:v{}", wf.workflow_id, wf.version), grant);
        }
        Self {
            by_workflow,
            connections,
            dependencies: BTreeMap::new(),
        }
    }

    /// This authority, with the dependencies the manifest's connections over
    /// workflow packages bind
    /// ([`manifest_dependencies`](crate::extensions::workflow_provider::manifest_dependencies)).
    pub fn with_dependencies(
        mut self,
        dependencies: BTreeMap<String, (ConnectionReference, BTreeSet<String>)>,
    ) -> Self {
        self.dependencies = dependencies;
        self
    }

    /// One workflow's grant, for tests that need a generation with one; each
    /// granted connection deployed at revision 1.
    #[cfg(test)]
    pub(crate) fn for_test(engine_id: &str, grant: AuthorityGrant) -> Self {
        Self {
            connections: grant
                .allowed_operations
                .keys()
                .map(|c| (c.clone(), 1))
                .collect(),
            by_workflow: BTreeMap::from([(engine_id.to_string(), grant)]),
            dependencies: BTreeMap::new(),
        }
    }

    /// The revision of `connection` the manifest deploys, if it deploys one.
    pub fn connection_revision(&self, connection: &str) -> Option<u64> {
        self.connections.get(connection).copied()
    }

    /// What the workflow the engine runs as `engine_id` may call.
    pub fn for_workflow(&self, engine_id: &str) -> Option<&AuthorityGrant> {
        self.by_workflow.get(engine_id)
    }

    /// Everything any workflow may call, and every package dependency: a
    /// run's ceiling.
    pub fn union(&self) -> AuthorityGrant {
        let mut all = AuthorityGrant::default();
        for grant in self.by_workflow.values() {
            all.extend(grant);
        }
        all.declared_dependencies.extend(
            self.dependencies
                .iter()
                .map(|(key, dep)| (key.clone(), dep.clone())),
        );
        all
    }
}

/// The `connector_call` tasks of `wf` whose connection is in `connections`.
///
/// Only `connector_call` tasks grant. A member requires every connection its
/// workflow uses — the `connector` of an `http_call`, a `db_read`, a
/// `cache_write` too, because the deployment must carry each — but a
/// requirement is the deployment's bookkeeping, not authority: a connection
/// required only by a built-in task grants no operation here.
fn workflow_grant(wf: &Workflow, connections: &BTreeSet<&str>) -> AuthorityGrant {
    let mut grant = AuthorityGrant::default();
    let Ok(tasks) = serde_json::from_str::<Value>(&wf.tasks_json) else {
        return grant;
    };
    let loop_config = wf
        .loop_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<Value>(json).ok());
    for task in crate::engine::leaf_tasks(&tasks, loop_config.as_ref()) {
        let function = &task["function"];
        if function["name"] != crate::engine::functions::connector_call::NAME {
            continue;
        }
        let input = &function["input"];
        if let (Some(connection), Some(operation)) =
            (input["connection"].as_str(), input["operation"].as_str())
            && connections.contains(connection)
        {
            grant
                .allowed_operations
                .entry(connection.to_string())
                .or_default()
                .insert(operation.to_string());
        }
    }
    grant
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::deployment_store::{EntityRef, ManifestMember};
    use serde_json::json;

    fn workflow(id: &str, version: i64, tasks: Value) -> Workflow {
        Workflow {
            workflow_id: id.to_string(),
            version,
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
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        }
    }

    fn call(connection: &str, operation: &str) -> Value {
        json!({"id": format!("{connection}-{operation}"), "function": {
            "name": "connector_call",
            "input": {"connection": connection, "operation": operation}
        }})
    }

    fn member(kind: EntityKind, id: &str, requires: &[&str]) -> ManifestMember {
        ManifestMember {
            entity: EntityRef {
                kind,
                id: id.to_string(),
                revision: 1,
            },
            definition_digest: format!("sha256:{}", "0".repeat(64)),
            owning_package: None,
            requires: requires
                .iter()
                .map(|c| EntityRef {
                    kind: EntityKind::Connection,
                    id: (*c).to_string(),
                    revision: 1,
                })
                .collect(),
            rollouts: Vec::new(),
        }
    }

    /// A workflow may call the connections its member requires, with the
    /// operations its tasks name, including those inside a task group;
    /// a connection it does not require is not granted.
    #[test]
    fn a_workflow_is_granted_what_its_member_requires_and_its_tasks_name() {
        let manifest = CanonicalManifest {
            schema_version: 2,
            providers: Vec::new(),
            provider_lock_digest: None,
            members: vec![member(EntityKind::Workflow, "orders", &["crm"])],
        };
        let tasks = json!([
            call("crm", "http.get"),
            {"id": "group", "tasks": [call("crm", "http.post")]},
            call("billing", "sql.query"),
        ]);
        let authority =
            ConnectorAuthority::from_manifest(&manifest, &[workflow("orders", 1, tasks)]);
        for id in ["orders", "orders:v1"] {
            let grant = authority.for_workflow(id).expect("granted");
            assert!(grant.allows("crm", "http.get"));
            assert!(grant.allows("crm", "http.post"));
            assert!(!grant.allows("crm", "http.delete"));
            assert!(!grant.allows("billing", "sql.query"));
        }
        assert!(authority.for_workflow("other").is_none());
        assert!(authority.union().allows("crm", "http.post"));
    }

    /// Requiring a connection grants nothing by itself: one the workflow
    /// uses only through a built-in function's `connector` field is
    /// required, and no operation on it is granted.
    #[test]
    fn a_requirement_from_a_built_in_task_grants_nothing() {
        let manifest = CanonicalManifest {
            schema_version: 2,
            providers: Vec::new(),
            provider_lock_digest: None,
            members: vec![member(
                EntityKind::Workflow,
                "orders",
                &["crm", "orders-db"],
            )],
        };
        let tasks = json!([
            call("crm", "http.get"),
            {"id": "read", "name": "Read", "function": {"name": "db_read",
                "input": {"connector": "orders-db", "query": "SELECT 1"}}},
        ]);
        let authority =
            ConnectorAuthority::from_manifest(&manifest, &[workflow("orders", 1, tasks)]);
        let grant = authority.for_workflow("orders").expect("granted");
        assert!(grant.allows("crm", "http.get"));
        assert!(!grant.allowed_operations.contains_key("orders-db"));
        assert!(
            !authority
                .union()
                .allowed_operations
                .contains_key("orders-db")
        );
    }
}

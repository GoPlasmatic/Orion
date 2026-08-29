//! Workflow activation gates.

use crate::errors::OrionError;
use serde_json::Value;

/// R5 / F52: every connector a workflow's tasks reference must exist, be of a
/// type the referencing function can actually use, and carry the extra keys
/// that type needs — all before the workflow may activate.
///
/// Missing connectors were previously a warning at create and unchecked at
/// activate, so the workflow failed at its first request instead (R5). The
/// *type* stayed unchecked even then: pointing `cache_read` at a `db`
/// connector activated cleanly and 500'd on first traffic, though
/// `CONNECTOR_FUNCTIONS` already implied the required kind (F52). Both are
/// fully determined at authoring time, so both are answered here.
pub(crate) async fn ensure_connectors_exist(
    connectors: &crate::connector::ConnectorRegistry,
    workflow: &crate::storage::models::Workflow,
) -> Result<(), OrionError> {
    let Ok(tasks) = serde_json::from_str::<Value>(&workflow.tasks_json) else {
        return Ok(()); // unparseable tasks are caught elsewhere
    };

    // The lookup is this caller's — an async registry — so it is done first,
    // for every name the tasks mention; the rules over the result are shared
    // with the offline set check (`definitions::check`).
    let mut facts: std::collections::HashMap<String, crate::engine::ConnectorFacts> =
        std::collections::HashMap::new();
    for r in crate::engine::connector_refs(&tasks) {
        if facts.contains_key(r.connector) {
            continue;
        }
        if let Some(config) = connectors.get(r.connector).await {
            facts.insert(
                r.connector.to_string(),
                crate::engine::ConnectorFacts {
                    connector_type: config.connector_type(),
                    is_mongo: config.is_mongo(),
                },
            );
        }
    }

    let mut missing: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    for problem in crate::engine::check_connector_refs(&tasks, |name| facts.get(name).copied()) {
        match problem {
            crate::engine::RefProblem::Missing { connector } => {
                if !missing.iter().any(|m| m == connector) {
                    missing.push(connector.to_string());
                }
            }
            crate::engine::RefProblem::WrongType {
                function,
                connector,
                actual,
                wanted,
            } => problems.push(format!(
                "task calling '{function}' points at connector '{connector}', which is a \
                 '{actual}' connector — '{function}' requires {}",
                wanted
                    .iter()
                    .map(|t| format!("'{t}'"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            )),
            crate::engine::RefProblem::MissingMongoDatabase {
                function,
                connector,
            } => problems.push(format!(
                "task calling '{function}' points at MongoDB connector '{connector}' but \
                 sets no 'database' — MongoDB connection strings carry no default database"
            )),
        }
    }

    if !missing.is_empty() {
        return Err(OrionError::validation(format!(
            "Cannot activate workflow '{}': connector(s) {} not found — create \
             them first, or fix the reference",
            workflow.workflow_id,
            missing
                .iter()
                .map(|m| format!("'{m}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if !problems.is_empty() {
        return Err(OrionError::validation(format!(
            "Cannot activate workflow '{}': {}",
            workflow.workflow_id,
            problems.join("; ")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflow_with(tasks: serde_json::Value) -> crate::storage::models::Workflow {
        let now = chrono::Utc::now().naive_utc();
        crate::storage::models::Workflow {
            workflow_id: "w".to_string(),
            version: 1,
            name: "w".to_string(),
            description: None,
            priority: 0,
            status: "draft".to_string(),
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

    fn empty_registry() -> crate::connector::ConnectorRegistry {
        crate::connector::ConnectorRegistry::new(Default::default())
    }

    /// The point of the extraction: the activation gate answers without an
    /// `AppState`, a router, or a request.
    ///
    /// It used to take `&AppState`, so "would this workflow activate?" could
    /// only be asked by issuing an HTTP call — which is why the offline set
    /// check grew its own copy of the walk rather than reusing this.
    #[tokio::test]
    async fn a_missing_connector_is_refused_without_going_through_http() {
        let registry = empty_registry();
        let wf = workflow_with(serde_json::json!([{
            "id": "t1",
            "name": "read",
            "function": { "name": "db_read", "input": { "connector": "nope", "query": "SELECT 1" } }
        }]));

        let err = ensure_connectors_exist(&registry, &wf)
            .await
            .expect_err("a workflow naming a connector that does not exist must not activate");
        assert!(
            err.to_string().contains("nope"),
            "the refusal must name the connector: {err}"
        );
    }

    /// A workflow that names no connector has nothing to check.
    #[tokio::test]
    async fn a_workflow_with_no_connector_refs_passes() {
        let registry = empty_registry();
        let wf = workflow_with(serde_json::json!([{
            "id": "t1",
            "name": "log",
            "function": { "name": "log", "input": { "message": "hi" } }
        }]));
        assert!(ensure_connectors_exist(&registry, &wf).await.is_ok());
    }
}

//! Every SQL statement a definition set ships: each `db_read` and `db_write`
//! task's literal `query`, where it sits and the connector it runs on.
//!
//! A statement and its connector are both literal inputs — neither is
//! templated — so the compiled set says, offline, exactly what will be sent
//! where. Only the bind *values* are run-time. `sql check` prepares each of
//! these against a real database; this module is the walk, with no I/O.

use serde_json::Value;

use super::{DefinitionSet, Entity};

/// One statement, as a finding names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlStatement<'a> {
    /// The workflow's name, falling back to its file.
    pub workflow: &'a str,
    /// The file the workflow was read from.
    pub origin: &'a str,
    pub task_id: &'a str,
    /// The `query` coordinate: `tasks[1].tasks[0].function.input.query`.
    pub path: String,
    /// `db_read` or `db_write`.
    pub function: &'static str,
    pub connector: &'a str,
    pub query: &'a str,
    /// How many values `params` binds: `Some(0)` when it is absent or null,
    /// `Some(n)` for a literal array, `None` when it is computed.
    pub bound: Option<usize>,
}

/// Every `db_read`/`db_write` statement of the set's workflows, in file
/// order, a loop's `setup` and task groups included — through
/// `engine::walk_steps`, never a flat loop over `tasks`, which would skip
/// both.
pub fn sql_statements(set: &DefinitionSet) -> Vec<SqlStatement<'_>> {
    let mut out = Vec::new();
    for def in set.iter(Entity::Workflow) {
        let Some(tasks) = def.doc.get("tasks") else {
            continue;
        };
        let workflow = def
            .doc
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&def.origin);
        for (path, task) in crate::engine::walk_steps(tasks, def.doc.get("loop")).tasks {
            let function = task.get("function");
            let function_name = match function.and_then(|f| f.get("name")).and_then(Value::as_str) {
                Some("db_read") => "db_read",
                Some("db_write") => "db_write",
                _ => continue,
            };
            let input = function.and_then(|f| f.get("input"));
            let (Some(query), Some(connector)) = (
                input.and_then(|i| i.get("query")).and_then(Value::as_str),
                input
                    .and_then(|i| i.get("connector"))
                    .and_then(Value::as_str),
            ) else {
                continue;
            };
            let bound = match input.and_then(|i| i.get("params")) {
                None | Some(Value::Null) => Some(0),
                Some(Value::Array(items)) => Some(items.len()),
                Some(_) => None,
            };
            out.push(SqlStatement {
                workflow,
                origin: &def.origin,
                task_id: task.get("id").and_then(Value::as_str).unwrap_or("?"),
                path: format!("{path}.function.input.query"),
                function: function_name,
                connector,
                query,
                bound,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(id: &str, query: &str, params: Value) -> Value {
        let mut input = json!({"connector": "db", "query": query, "output": "data.r"});
        if !params.is_null() {
            input["params"] = params;
        }
        json!({"id": id, "name": id, "function": {"name": "db_read", "input": input}})
    }

    #[test]
    fn statements_are_found_at_every_depth_with_their_bindings() {
        let set = DefinitionSet::from_entries([(
            Entity::Workflow,
            "wf.json".to_string(),
            json!({"workflow_id": "w", "name": "Orders", "tasks": [
                read("a", "SELECT 1", Value::Null),
                {"id": "g", "condition": true, "tasks": [
                    read("b", "SELECT $1", json!([{"var": "data.x"}])),
                    {"id": "w", "name": "w", "function": {"name": "db_write", "input": {
                        "connector": "other", "query": "UPDATE t SET x = 1",
                        "params": {"var": "data.params"}}}}
                ]},
                {"id": "m", "name": "m", "function": {"name": "map", "input": {"mappings": []}}}
            ]}),
        )]);
        let found = sql_statements(&set);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].workflow, "Orders");
        assert_eq!(found[0].bound, Some(0));
        assert_eq!(found[1].path, "tasks[1].tasks[0].function.input.query");
        assert_eq!(found[1].bound, Some(1));
        assert_eq!(found[2].function, "db_write");
        assert_eq!(found[2].connector, "other");
        assert_eq!(found[2].bound, None, "computed params");
    }
}

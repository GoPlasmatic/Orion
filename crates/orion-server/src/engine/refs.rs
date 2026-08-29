//! The task-reference walk: what a workflow's tasks point at.
//!
//! One implementation for every consumer that answers "what does this
//! workflow depend on" — the activation gates and connector-rename guard in
//! the admin routes, the K9 `/dependencies` endpoint, and the package CLI's
//! offline `lint` (which, living in the binary crate, is the reason this is
//! `pub` in the library rather than a route-module private). A second copy of
//! this walk is exactly how a new connector-bearing function or input
//! spelling silently escapes closure checking.

use serde_json::Value;

/// One task's connector reference, as authored.
pub struct ConnectorRef<'a> {
    /// The task's `function.name`, always one of
    /// [`crate::engine::CONNECTOR_FUNCTIONS`].
    pub function: &'a str,
    pub connector: &'a str,
    /// The task's whole `function.input` object, for the cross-field rules.
    pub input: &'a Value,
}

/// Every connector a workflow's tasks reference, in task order.
pub fn connector_refs(tasks: &Value) -> Vec<ConnectorRef<'_>> {
    // Flattened: since 3.6 a `tasks` element may be a group, and a connector
    // referenced only from inside one would otherwise pass closure checking.
    super::steps::leaf_tasks(tasks)
        .into_iter()
        .filter_map(|task| {
            let function = task.get("function")?;
            let name = function.get("name")?.as_str()?;
            if !crate::engine::CONNECTOR_FUNCTIONS.contains(&name) {
                return None;
            }
            let input = function.get("input")?;
            Some(ConnectorRef {
                function: name,
                connector: input.get("connector")?.as_str()?,
                input,
            })
        })
        .collect()
}

/// What a connector must be, for the reference rules to be decidable.
///
/// Two facts, because the type alone does not settle the questions: `db` covers
/// SQL and MongoDB, and only the connection string says which.
#[derive(Clone, Copy)]
pub struct ConnectorFacts {
    pub connector_type: crate::connector::ConnectorType,
    pub is_mongo: bool,
}

/// A problem with one connector reference.
///
/// Rendered by the caller, because the two callers render differently: the
/// activation gate produces one `OrionError` per class with the names joined,
/// and the offline set check produces one `Diagnostic` per problem with a
/// stable check id.
pub enum RefProblem<'a> {
    /// No connector of that name.
    Missing { connector: &'a str },
    /// A connector of the wrong type for the function referencing it.
    WrongType {
        function: &'a str,
        connector: &'a str,
        actual: crate::connector::ConnectorType,
        wanted: &'static [crate::connector::ConnectorType],
    },
    /// A MongoDB connector reached by a function that needs a database named,
    /// with no `database` on the task.
    MissingMongoDatabase {
        function: &'a str,
        connector: &'a str,
    },
}

/// Check every connector reference in `tasks` against what the connectors are.
///
/// The **predicate**, over an index the caller supplies. The lookup itself
/// cannot be shared — the activation gate reads an async registry, the offline
/// set check reads parsed files — but the rules can, and had drifted: the
/// offline check knew the first two problems and not the third, so `lint` and
/// `package lint` passed a workflow whose `mongo_read` names no database and
/// which the handler refuses at its first request.
pub fn check_connector_refs<'a, F>(tasks: &'a Value, facts: F) -> Vec<RefProblem<'a>>
where
    F: Fn(&str) -> Option<ConnectorFacts>,
{
    let mut problems = Vec::new();
    for r in connector_refs(tasks) {
        let Some(facts) = facts(r.connector) else {
            problems.push(RefProblem::Missing {
                connector: r.connector,
            });
            continue;
        };

        // Type. The handler would refuse this at request time via the
        // connector target; saying so now costs one lookup.
        if let Some(wanted) = super::required_connector_types(r.function)
            && !wanted.contains(&facts.connector_type)
        {
            problems.push(RefProblem::WrongType {
                function: r.function,
                connector: r.connector,
                actual: facts.connector_type,
                wanted,
            });
            continue;
        }

        // Cross-field: MongoDB has no default database in its connection
        // string, so the task must name one. `mongo_read` declares `database`
        // required outright; `data_query`/`data_write` cannot, because the same
        // shape is valid against SQL and Elasticsearch — which is why the schema
        // marks it optional and the handler enforces it at request time.
        // Whether it applies is knowable here: the connector is resolved.
        if facts.is_mongo
            && super::requires_mongo_database(r.function)
            && !r
                .input
                .get("database")
                .is_some_and(|d| d.as_str().is_some_and(|s| !s.trim().is_empty()))
        {
            problems.push(RefProblem::MissingMongoDatabase {
                function: r.function,
                connector: r.connector,
            });
        }
    }
    problems
}

/// Channel names a workflow's `channel_call` tasks target statically, plus
/// whether any call resolves its target dynamically (`channel_logic`), in
/// which case the static list is incomplete by construction.
pub fn channel_call_targets(tasks: &Value) -> (Vec<&str>, bool) {
    let mut targets = Vec::new();
    let mut dynamic = false;
    for task in super::steps::leaf_tasks(tasks) {
        let Some(input) = task
            .get("function")
            .filter(|f| f.get("name").and_then(|n| n.as_str()) == Some("channel_call"))
            .and_then(|f| f.get("input"))
        else {
            continue;
        };
        if let Some(target) = input.get("channel").and_then(|c| c.as_str())
            && !target.is_empty()
            && !targets.contains(&target)
        {
            targets.push(target);
        }
        if input.get("channel_logic").is_some_and(|l| !l.is_null()) {
            dynamic = true;
        }
    }
    (targets, dynamic)
}

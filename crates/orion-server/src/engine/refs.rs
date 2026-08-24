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

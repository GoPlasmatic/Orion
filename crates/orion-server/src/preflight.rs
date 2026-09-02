//! Pre-upgrade scan of the stored estate.
//!
//! Backs `orion-server preflight`. The 0.3.0 → 1.0.0 upgrade guide carries an
//! 18-row checklist, and several of its rows were only answerable by running
//! SQL against the channels and workflows tables by hand. This module runs
//! those checks in the binary that knows the rules, so the answer is a command
//! rather than an exercise.
//!
//! **What this covers, and what covers the rest.** Config-file and `ORION_*`
//! problems never reach here: `config::load_config` rejects unknown keys
//! (`deny_unknown_fields` throughout) and retired variable names (the table in
//! `src/config/retired_env.rs`) before any subcommand dispatches, so a
//! `preflight` that gets as far as opening the database has already passed
//! both. `validate-config` is the command for that surface. This one reads the
//! *database*, which nothing else does.
//!
//! **Why the database needs its own pass.** The two surfaces fail at different
//! times. An operator edits the config file during the upgrade, so a startup
//! error lands while they are looking. Nobody edits stored channel and workflow
//! rows during an upgrade — those were written months ago by someone else, and
//! their failure surfaces at load (a quarantined channel) or, worse, at the
//! first request that reaches a task. `data_query`/`data_write` tasks with no
//! `schema` are exactly that case: they keep loading and activating, and fail
//! only once production traffic arrives. Finding them beforehand is the whole
//! point.
//!
//! Checks are read-only. Nothing here mutates a row.

use serde_json::Value;

use crate::errors::OrionError;
use crate::storage::repositories::channels::{ChannelFilter, ChannelRepository};
use crate::storage::repositories::workflows::{WorkflowFilter, WorkflowRepository};

/// Rows per repository call. The repositories clamp a limit to `1..=1000`
/// (`clamp_pagination`), and the loop below treats a short page as "exhausted",
/// so a larger value would come back clamped and silently stop the scan early —
/// the same bound the export snapshot (`snapshot_pages`) documents for the
/// same reason.
const PAGE_SIZE: i64 = 500;

/// One thing that will break on upgrade, and what to do about it.
///
/// The shared [`Diagnostic`]: `check` is the `upgrading.md` checklist row this
/// belongs to, so the report and the guide can be read side by side; `entity`
/// is named the way the operator stored it; `message` is what is wrong and
/// `remedy` what to change. A scan over stored rows has no file to point at, so
/// `file`/`path`/`line` stay `None` — which is why the location is optional on
/// the shared type rather than a second type carrying it.
///
/// Every finding here is a break, so all of them are `Severity::Error` and
/// [`Diagnostic::render_preflight`] does not print a level.
pub use crate::definitions::Diagnostic;

/// Scan every stored channel and workflow. Returns findings in a stable order:
/// channels first, then workflows, each in repository order.
///
/// Reads the latest version of each entity — the row that serves now, or the
/// one the next activation would promote. Superseded versions are not scanned:
/// they cannot be activated without first becoming the latest, at which point
/// the create/update validators apply.
pub async fn scan(
    channels: &dyn ChannelRepository,
    workflows: &dyn WorkflowRepository,
) -> Result<Vec<Diagnostic>, OrionError> {
    let mut findings = scan_channels(channels).await?;
    findings.extend(scan_workflows(workflows).await?);
    Ok(findings)
}

async fn scan_channels(repo: &dyn ChannelRepository) -> Result<Vec<Diagnostic>, OrionError> {
    let mut findings = Vec::new();
    // K7: channel names must be unique across channel_ids — the create path
    // refuses new collisions, but rows written before 1.0 can still carry
    // one, and activation will refuse the loser. Cross-row, so accumulated
    // over the same paging pass the per-row checks ride.
    let mut names: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut offset = 0i64;
    loop {
        let page = repo
            .list_paginated(&ChannelFilter {
                limit: Some(PAGE_SIZE),
                offset: Some(offset),
                ..Default::default()
            })
            .await?;
        let page_len = page.data.len() as i64;
        for channel in &page.data {
            findings.extend(check_channel_config(&channel.name, &channel.config_json));
            names
                .entry(channel.name.clone())
                .or_default()
                .push(channel.channel_id.clone());
        }
        if page_len < PAGE_SIZE {
            findings.extend(duplicate_name_findings(&names));
            return Ok(findings);
        }
        offset += page_len;
    }
}

/// K7: one finding per channel name that more than one `channel_id` holds.
fn duplicate_name_findings(
    names: &std::collections::BTreeMap<String, Vec<String>>,
) -> Vec<Diagnostic> {
    names
        .iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(name, ids)| Diagnostic::error(
            "channel-names",
            format!("channel '{name}'"),
            format!(
                "{} channels share this name (ids: {}) — the data plane and \
                 channel_call address channels by name, so only one of them can \
                 serve, and 1.0 refuses to create or activate the collision",
                ids.len(),
                ids.join(", ")
            ),
        ).with_remedy("rename all but one (create a new version with a distinct name, activate it), or delete the redundant channels" .to_string()))
        .collect()
}

async fn scan_workflows(repo: &dyn WorkflowRepository) -> Result<Vec<Diagnostic>, OrionError> {
    let mut findings = Vec::new();
    let mut offset = 0i64;
    loop {
        let page = repo
            .list(&WorkflowFilter {
                limit: Some(PAGE_SIZE),
                offset: Some(offset),
                ..Default::default()
            })
            .await?;
        let page_len = page.len() as i64;
        for workflow in &page {
            findings.extend(check_workflow_tasks(&workflow.name, &workflow.tasks_json));
        }
        if page_len < PAGE_SIZE {
            return Ok(findings);
        }
        offset += page_len;
    }
}

/// Checklist row 3 (and the `cors` / `max_concurrent` rows): a stored channel
/// config that no longer parses.
///
/// `ChannelConfig` is `deny_unknown_fields`, so this catches the pre-1.0 `cors`
/// spelling, `backpressure.max_concurrent`, and any key that was always a typo.
/// All three have the same consequence — the channel is quarantined at load,
/// refused at every ingress — so they share one check rather than being
/// enumerated. The serde message names the offending key, which is the part the
/// operator needs.
pub fn check_channel_config(name: &str, config_json: &str) -> Vec<Diagnostic> {
    let parsed: Result<Value, _> = serde_json::from_str(config_json);
    let Ok(value) = parsed else {
        return vec![
            Diagnostic::error(
                "3",
                format!("channel '{name}'"),
                "its stored config is not valid JSON".to_string(),
            )
            .with_remedy("repair the config_json column, or re-create the channel".to_string()),
        ];
    };

    // An empty object is the documented "no config" default and is never
    // parsed by the runtime either.
    if value.as_object().is_some_and(|o| o.is_empty()) {
        return Vec::new();
    }

    match serde_json::from_value::<crate::channel::ChannelConfig>(value) {
        Ok(_) => Vec::new(),
        Err(e) => vec![
            Diagnostic::error(
                "3",
                format!("channel '{name}'"),
                format!("its stored config no longer parses: {e}"),
            )
            .with_remedy(pick_config_remedy(config_json)),
        ],
    }
}

/// Name the specific rename when the config carries one, since those have a
/// mechanical fix, and fall back to the generic advice otherwise.
fn pick_config_remedy(config_json: &str) -> String {
    if config_json.contains("\"cors\"") {
        "replace `\"cors\": {\"allowed_origins\": [...]}` with \
         `\"origin_allow_list\": [...]` (upgrading.md, \"A channel's `cors` is now \
         `origin_allow_list`\")"
            .to_string()
    } else if config_json.contains("\"max_concurrent\"") {
        "rename `backpressure.max_concurrent` to `max_concurrent_per_node` — and \
         check the value, which now means per replica rather than per cluster"
            .to_string()
    } else {
        "remove or correct the key the error names; unknown keys are refused \
         because a guard Orion does not recognise is a guard that never runs"
            .to_string()
    }
}

/// Checklist rows 14 and the `data_write` envelope row, plus everything the
/// shared task validator already knows.
pub fn check_workflow_tasks(name: &str, tasks_json: &str) -> Vec<Diagnostic> {
    let Ok(tasks) = serde_json::from_str::<Value>(tasks_json) else {
        return vec![
            Diagnostic::error(
                "14",
                format!("workflow '{name}'"),
                "its stored tasks are not valid JSON".to_string(),
            )
            .with_remedy("repair the tasks_json column, or re-create the workflow".to_string()),
        ];
    };

    // The chokepoint create, update, import, POST /validate and `lint` all
    // share — so preflight agrees with them by construction rather than by
    // re-implementing the rules. Covers missing/duplicate task ids, unknown
    // function names, and missing required inputs (including `write`, now that
    // the pre-1.0 flat envelope is gone).
    let mut findings: Vec<Diagnostic> = crate::validation::validate_workflow_tasks_schema(&tasks)
        .into_iter()
        .map(|e| {
            Diagnostic::error("14", format!("workflow '{name}' {}", e.path), e.message).with_remedy(
                "fix the task and PUT the workflow; it is refused at create and update until then"
                    .to_string(),
            )
        })
        .collect();

    // The other refusal a *stored* workflow can carry unknowingly: a secret
    // reference in a field that resolves none. It is a create/update gate, not
    // a load-time screen, so a workflow holding one keeps serving until the
    // next edit is refused — which is precisely the shape of break this
    // command exists to find before an operator hits it from a pipeline.
    findings.extend(
        crate::validation::secret_reference_errors(&tasks)
            .into_iter()
            .map(|(path, message)| Diagnostic::error(
            "14",
            format!("workflow '{name}' {path}"),
            message,
            ).with_remedy("move the value to a connector, or declare it in the config                          file — under [vars] if it belongs in a trace, [secrets] if                          it does not; the next update of this workflow is refused                          until then" .to_string())),
    );

    // Not a refusal at any gate — the engine reports it and builds anyway —
    // but the sharpest kind of upgrade break this command exists to find: the
    // workflow keeps loading and keeps serving, and the document it composes
    // silently changes shape. A `{"$set": …}` update built in a `map` task
    // emits `{"set": …}` from dataflow-rs 3.9 on, so the write goes to Mongo
    // with no `$set` operator in it and replaces the document instead of
    // updating it. Nothing logs, nothing 500s.
    findings.extend(
        crate::validation::escaped_template_key_warnings(&tasks)
            .into_iter()
            .map(|(path, message)| {
                Diagnostic::error("14", format!("workflow '{name}' {path}"), message).with_remedy(
                    "double the prefix — '$set' becomes '$$set' — and PUT the workflow; \
                     the doubled spelling is what emits the '$' key"
                        .to_string(),
                )
            }),
    );

    // dataflow-rs 3.10's two informational findings, over the *stored* estate.
    // Neither stops a workflow loading, which is what makes them preflight's
    // business rather than a load-time refusal: a `validation` that asserts
    // nothing and a `continue_on_error` the engine drops both keep serving,
    // and an operator upgrading is the person who can still fix them.
    findings.extend(
        crate::validation::engine_advisories(&tasks)
            .into_iter()
            .map(|advisory| {
                Diagnostic::warning(
                    "14",
                    format!("workflow '{name}' {}", advisory.path),
                    advisory.message,
                )
            }),
    );

    findings.extend(check_dialect_schemas(name, &tasks));
    findings
}

/// Checklist row 14: a `data_query`/`data_write` that declares no `schema`.
///
/// This is the one high-impact 1.0 break that nothing else catches in advance.
/// `unmapped` defaulted to `identity` before 1.0 — every logical name passing
/// through to the physical one, so a dialect task reached every table the
/// connector's database user could see, to read *and* write. The default is now
/// `reject`. A stored workflow in this shape still loads and still activates;
/// it fails at the *first request*, which means production traffic.
///
/// Not part of `validate_workflow_tasks_schema` because it is not a schema
/// violation — `schema` is a genuinely optional input, and a task can legally
/// omit it by opting into `identity` explicitly. It is a *migration* question,
/// which is what this module is for.
fn check_dialect_schemas(workflow: &str, tasks: &Value) -> Vec<Diagnostic> {
    let mut findings = Vec::new();
    // Flattened, like every other walk over authored tasks: a dialect task
    // inside a guard clause is one the engine will run, and an upgrade scan
    // that skipped it would report the estate clean and let the request fail
    // in production — the failure this whole module exists to pre-empt.
    for (path, task) in crate::engine::walk_steps(tasks).tasks {
        let Some(function) = task.get("function") else {
            continue;
        };
        let Some(fname) = function.get("name").and_then(Value::as_str) else {
            continue;
        };
        if fname != "data_query" && fname != "data_write" {
            continue;
        }
        let input = function.get("input");
        if input.and_then(|i| i.get("schema")).is_some() {
            continue;
        }
        let task_id = task
            .get("id")
            .and_then(Value::as_str)
            .map_or(path, str::to_string);
        findings.push(Diagnostic::error(
            "14",
            format!("workflow '{workflow}' task '{task_id}' ({fname})"),
            "declares no `schema`, so it will fail at its first request. \
                      Before 1.0 an absent schema meant `unmapped: identity` — every \
                      name passed through to the physical one, reaching every table \
                      the connector could see. The default is now `reject`"
                .to_string(),
        ).with_remedy("add a `schema` declaring the entities and columns this task uses, or `\"schema\": {\"unmapped\": \"identity\"}` to restore the 0.x behaviour exactly" .to_string()));
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The break an operator cannot otherwise see coming: the workflow keeps
    /// serving, and the refusal arrives at whatever edits it next.
    #[test]
    fn a_stored_stray_secret_reference_is_reported() {
        let tasks = json!([{
            "id": "call", "name": "Call",
            "function": {"name": "http_call", "input": {
                "connector": "crm", "path": "env://API_BASE"
            }}
        }]);
        let found = check_workflow_tasks("orders-wf", &tasks.to_string());
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(
            found[0].entity.contains("input.path"),
            "{}",
            found[0].entity
        );
        assert!(
            found[0]
                .remedy
                .as_deref()
                .unwrap_or_default()
                .contains("[secrets]"),
            "the remedy must name where the value goes: {}",
            found[0].remedy.as_deref().unwrap_or_default()
        );
    }

    /// And the five paths that do resolve one stay quiet, or every signing
    /// workflow in the estate reports a break that is not one.
    #[test]
    fn a_stored_reference_in_a_key_field_is_not_reported() {
        let tasks = json!([{
            "id": "mac", "name": "MAC",
            "function": {"name": "crypto", "input": {
                "op": "hmac", "key": "env://PARTNER_KEY", "data": {"var": "data.body"}
            }}
        }]);
        assert!(check_workflow_tasks("signer", &tasks.to_string()).is_empty());
    }

    #[test]
    fn a_clean_channel_config_reports_nothing() {
        assert!(
            check_channel_config(
                "ok",
                r#"{"origin_allow_list": ["https://app.example"],
                    "backpressure": {"max_concurrent_per_node": 10}}"#
            )
            .is_empty()
        );
        assert!(check_channel_config("empty", "{}").is_empty());
    }

    #[test]
    fn the_pre_1_0_cors_spelling_is_reported_with_its_rename() {
        let found =
            check_channel_config("orders", r#"{"cors": {"allowed_origins": ["https://a"]}}"#);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].check, "3");
        assert!(found[0].entity.contains("orders"));
        assert!(
            found[0]
                .remedy
                .as_deref()
                .unwrap_or_default()
                .contains("origin_allow_list"),
            "the remedy must name the new key: {}",
            found[0].remedy.as_deref().unwrap_or_default()
        );
    }

    #[test]
    fn the_pre_1_0_backpressure_spelling_is_reported_with_its_rename() {
        let found = check_channel_config("bulk", r#"{"backpressure": {"max_concurrent": 50}}"#);
        assert_eq!(found.len(), 1);
        assert!(
            found[0]
                .remedy
                .as_deref()
                .unwrap_or_default()
                .contains("max_concurrent_per_node"),
            "the remedy must name the new key: {}",
            found[0].remedy.as_deref().unwrap_or_default()
        );
        assert!(
            found[0]
                .remedy
                .as_deref()
                .unwrap_or_default()
                .contains("per replica"),
            "renaming alone is not the whole fix — the value changes meaning: {}",
            found[0].remedy.as_deref().unwrap_or_default()
        );
    }

    #[test]
    fn a_misspelled_guard_key_is_reported() {
        let found = check_channel_config("typo", r#"{"deduplicaton": {"header": "Idem"}}"#);
        assert_eq!(found.len(), 1);
        assert!(
            found[0].message.contains("deduplicaton"),
            "the serde message names the key: {}",
            found[0].message
        );
    }

    #[test]
    fn unparseable_stored_json_is_reported_rather_than_panicking() {
        let found = check_channel_config("broken", "{not json");
        assert_eq!(found.len(), 1);
        assert!(found[0].message.contains("not valid JSON"));
    }

    #[test]
    fn a_dialect_task_without_a_schema_is_reported() {
        let tasks = json!([{
            "id": "read", "name": "Read",
            "function": { "name": "data_query", "input": {
                "connector": "db", "query": { "source": "orders" }, "output": "data.o"
            }}
        }]);
        let found = check_workflow_tasks("orders-wf", &tasks.to_string());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].check, "14");
        assert!(found[0].entity.contains("read"));
        assert!(
            found[0]
                .remedy
                .as_deref()
                .unwrap_or_default()
                .contains("unmapped"),
            "the remedy must offer the one-line escape hatch: {}",
            found[0].remedy.as_deref().unwrap_or_default()
        );
    }

    #[test]
    fn a_dialect_task_declaring_a_schema_is_clean() {
        let tasks = json!([{
            "id": "read", "name": "Read",
            "function": { "name": "data_query", "input": {
                "connector": "db",
                "query": { "source": "orders" },
                "schema": { "entities": { "orders": { "columns": { "id": {} } } } },
                "output": "data.o"
            }}
        }]);
        assert!(check_workflow_tasks("orders-wf", &tasks.to_string()).is_empty());
    }

    /// The explicit opt-in is a declaration, not an omission — it restores the
    /// 0.x behaviour deliberately and must not be reported as unmigrated.
    #[test]
    fn the_identity_escape_hatch_counts_as_declared() {
        let tasks = json!([{
            "id": "read", "name": "Read",
            "function": { "name": "data_query", "input": {
                "connector": "db",
                "query": { "source": "orders" },
                "schema": { "unmapped": "identity" },
                "output": "data.o"
            }}
        }]);
        assert!(check_workflow_tasks("orders-wf", &tasks.to_string()).is_empty());
    }

    /// Non-dialect tasks have no `schema` input at all and must not be swept
    /// up — including the other connector-backed ones, which are the plausible
    /// false positives.
    #[test]
    fn a_non_dialect_task_is_not_asked_for_a_schema() {
        let tasks = json!([
            {
                "id": "call", "name": "Call",
                "function": { "name": "http_call", "input": {
                    "connector": "api", "method": "GET", "path": "/orders"
                }}
            },
            {
                "id": "note", "name": "Note",
                // `message` is required by the engine's own `log` config
                // parse — the shared validator's engine-parse catch-all
                // refuses it absent, exactly as `Engine::new` would.
                "function": { "name": "log", "input": { "message": "noted" } }
            },
        ]);
        let found = check_workflow_tasks("wf", &tasks.to_string());
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn the_shared_task_validator_findings_are_reported() {
        // Duplicate ids — the case that fails the whole engine reload, not
        // just its own channel.
        let tasks = json!([
            { "id": "a", "name": "A", "function": { "name": "log", "input": {} } },
            { "id": "a", "name": "B", "function": { "name": "log", "input": {} } },
        ]);
        let found = check_workflow_tasks("dupes", &tasks.to_string());
        assert!(
            found
                .iter()
                .any(|f| f.message.contains("Duplicate step id")),
            "{found:?}"
        );
    }

    /// W7: a stored `data_write` still carrying the pre-1.0 flat envelope is
    /// reported, because `write` is a required input now.
    #[test]
    fn a_flat_data_write_envelope_is_reported() {
        let tasks = json!([{
            "id": "w", "name": "W",
            "function": { "name": "data_write", "input": {
                "connector": "db",
                "schema": { "unmapped": "identity" },
                "op": "insert", "target": "orders", "values": { "id": 1 },
                "output": "data.w"
            }}
        }]);
        let found = check_workflow_tasks("writer", &tasks.to_string());
        assert!(
            found.iter().any(|f| f.message.contains("write")),
            "the missing envelope must be reported: {found:?}"
        );
    }

    #[test]
    fn findings_render_with_their_checklist_row() {
        let f = Diagnostic::error(
            "14",
            "workflow 'w' task 't'".to_string(),
            "declares no schema".to_string(),
        )
        .with_remedy("add one".to_string());
        let rendered = f.render_preflight();
        assert!(
            rendered.starts_with("[14] workflow 'w' task 't'"),
            "{rendered}"
        );
        assert!(rendered.contains("fix: add one"), "{rendered}");
    }
}

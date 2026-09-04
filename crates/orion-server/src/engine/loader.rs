//! Turning stored channel + workflow rows into the dataflow-rs workflow set the
//! engine is built from — including the per-channel quarantine that keeps one
//! bad row from taking the instance down.

use std::collections::HashMap;

use crate::storage::models::{Channel, Workflow};
use crate::storage::repositories::workflows::{
    workflow_to_dataflow, workflow_to_dataflow_with_rollout,
};

/// Filter channels based on include/exclude glob patterns from [`ChannelFilterConfig`].
///
/// - If `include` is non-empty, only channels matching at least one include pattern are kept.
/// - Channels matching any `exclude` pattern are removed (applied after include).
/// - Supports simple `*` wildcards (e.g., `internal-*`, `*-debug`).
///
/// [`ChannelFilterConfig`]: crate::config::ChannelFilterConfig
pub fn filter_channels(
    channels: Vec<Channel>,
    config: &crate::config::ChannelFilterConfig,
) -> Vec<Channel> {
    if config.include.is_empty() && config.exclude.is_empty() {
        return channels;
    }

    channels
        .into_iter()
        .filter(|ch| {
            // Include filter: if non-empty, channel must match at least one pattern
            if !config.include.is_empty() && !config.include.iter().any(|p| glob_match(p, &ch.name))
            {
                return false;
            }
            // Exclude filter: channel must not match any exclude pattern
            !config.exclude.iter().any(|p| glob_match(p, &ch.name))
        })
        .collect()
}

/// Glob matching supporting `*` wildcards, with real backtracking.
///
/// F32: the previous successive-`find` implementation diverged from glob
/// semantics on patterns needing backtracking (`a*bc` vs `abxbc` matched the
/// first `b` and failed) — and a wrong `channels.include`/`exclude` pattern
/// silently drops a channel. This is the standard two-pointer matcher:
/// linear scan with a single saved star position to retry from.
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            // Backtrack: let the last `*` swallow one more character.
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Something that can answer "will the handlers that run this workflow
/// actually dispatch every task in it, and do their inputs parse?".
///
/// Two implementations, because the two callers hold different things. At boot
/// the handler map is still on an [`EngineBuilder`] that has not been built
/// yet; on reload the handlers live on the running [`Engine`], which
/// `with_new_workflows` carries across. Both expose the same
/// `check_workflow`, so this trait exists only to name that shared shape.
///
/// [`Engine`]: dataflow_rs::Engine
/// [`EngineBuilder`]: dataflow_rs::engine::EngineBuilder
pub trait HandlerScreen {
    /// Every reason this workflow would not run, without building anything.
    fn check_workflow(&self, workflow: &dataflow_rs::Workflow) -> Vec<dataflow_rs::WorkflowIssue>;
}

impl HandlerScreen for dataflow_rs::Engine {
    fn check_workflow(&self, workflow: &dataflow_rs::Workflow) -> Vec<dataflow_rs::WorkflowIssue> {
        dataflow_rs::Engine::check_workflow(self, workflow)
    }
}

impl HandlerScreen for dataflow_rs::engine::EngineBuilder {
    fn check_workflow(&self, workflow: &dataflow_rs::Workflow) -> Vec<dataflow_rs::WorkflowIssue> {
        dataflow_rs::engine::EngineBuilder::check_workflow(self, workflow)
    }
}

/// Screen one converted workflow against the handlers that will run it.
///
/// Without this, a task naming an unregistered function or carrying an input
/// its handler cannot parse is invisible until engine construction — which is
/// a whole-instance failure, not a per-channel one: at boot the process
/// aborts, and on reload every channel on every node goes down because one
/// stored row is unusable. That defeats the F33/F35 quarantine, whose premise
/// is that one broken row must never stop the instance. Checking here turns it
/// back into a `ChannelLoadIssue` (F41).
///
/// This used to be a hand-written mirror: a membership test against a
/// hand-kept list of registered names, a `match` naming the one handler with a typed `Input`, and
/// a locally-built datalogic engine standing in for the crate-private
/// `TemplateCompiler`. dataflow-rs 3.7's `check_workflow` does all three
/// against the real registry and the real compiler, so the mirror is gone and
/// with it three ways for it to drift:
///
/// - A new handler no longer has to be added to a list to be screened.
/// - A handler that grows a typed `Input` is screened for it immediately,
///   rather than silently going unchecked until someone remembers the `match`.
/// - `http_call`, `publish_kafka` and `enrich` are covered too. They
///   deserialize into typed built-in variants, so they never reached the
///   `Custom` arm the mirror inspected — an `enrich` task in a stored row
///   built cleanly and then failed every request with `FunctionNotFound`.
///
/// Issues are joined into one message: the caller reports per workflow, and a
/// workflow that cannot run is quarantined whole regardless of how many of its
/// tasks are the reason.
///
/// **Not every issue `check_workflow` reports makes a workflow unusable**, and
/// the engine is what says which. Until dataflow-rs 3.9 every code it returned
/// was one `Engine::build` also refused, so "any issue" and "cannot run" were
/// the same set and this screen could take the whole list. `ESCAPED_TEMPLATE_KEY`
/// broke that and 3.10 added two more, which Orion tracked in a hand-kept list
/// that could only ever be wrong in one direction — silently, on upgrade,
/// quarantining channels that were fine. 3.11's `Severity` replaced it:
/// `severity()` is a match with no wildcard arm, so a code added in a later
/// minor is classified upstream before it can reach here.
///
/// `Advisory` is the only class this may pass. **`Defect` must still
/// quarantine**: `MISSING_HANDLER` is a config-only integration whose typed
/// variant parses, so `build` accepts it and every message then fails — F54,
/// the `enrich` case that made this function exist. Screening on "would this
/// build" would call that healthy.
///
/// Filtered here rather than at each caller because all four surfaces — boot,
/// reload, `dry-run` and the test endpoint — must agree on what "unusable"
/// means.
fn screen_workflow(
    workflow: &dataflow_rs::Workflow,
    screen: &dyn HandlerScreen,
) -> Result<(), String> {
    let issues = screen.check_workflow(workflow);
    let (advisories, issues): (Vec<_>, Vec<_>) = issues
        .into_iter()
        .partition(|i| i.severity() == dataflow_rs::Severity::Advisory);
    // Dropped from the fatal set but not dropped. Each of the three says
    // something a load is the last chance to notice: a stripped `$` changes the
    // shape of a composed document without failing anything, a `validation`
    // asserts nothing, a group's `continue_on_error` is discarded. None of them
    // stops the workflow running, which is why they are logged here rather than
    // returned. `lint` and `preflight` report the same findings against a
    // document the author can still edit.
    for advisory in &advisories {
        tracing::warn!(
            workflow = %workflow.id,
            task = advisory.task_id.as_deref().unwrap_or("-"),
            path = advisory.path.as_deref().unwrap_or("-"),
            "{}",
            advisory.message
        );
    }
    if issues.is_empty() {
        return Ok(());
    }
    Err(issues
        .iter()
        .map(|issue| match &issue.task_id {
            Some(task_id) => format!("task '{task_id}': {}", issue.message),
            None => issue.message.clone(),
        })
        .collect::<Vec<_>>()
        .join("; "))
}

/// Build an engine over exactly one workflow, screened the way the serving
/// engine screens.
///
/// Four places in the tree build a workflow engine, and until this existed only
/// two of them went through the [`HandlerScreen`]: boot and reload. The other
/// two — `orion-server dry-run` and `POST /workflows/{id}/test` — assembled the
/// builder by hand and called `.build()`, so they answered a *different
/// question* from the one they are asked.
///
/// `.build()` alone is not the same check. It refuses an unregistered *custom*
/// function, but `http_call`, `publish_kafka` and `enrich` deserialize into
/// typed built-in variants, so a workflow naming one builds cleanly with no
/// handler behind it and fails every request with `FunctionNotFound`. Orion
/// registers the first two and never registers `enrich` — which is exactly the
/// case that made this screen exist (F54). The author saw a green dry run and a
/// channel that would not serve; worse in reverse, since "test this workflow"
/// is the endpoint people trust before activating.
///
/// So the screen is not optional and not the caller's to remember. `handlers`
/// go on the builder first, because that is what the screen consults — pass
/// the same map the surface would run with (the real handlers for the test
/// endpoint, the stub table for a dry run), and the answer is about *that*
/// engine.
///
/// `Err` is the joined screen message, in the same wording
/// [`build_engine_workflows`] puts in a `ChannelLoadIssue`, so an author reads
/// one explanation of an unusable task wherever they meet it.
pub fn build_single(
    workflow: dataflow_rs::Workflow,
    handlers: HashMap<String, dataflow_rs::BoxedFunctionHandler>,
    secrets: &crate::engine::ResolvedSecrets,
) -> Result<dataflow_rs::Engine, crate::errors::OrionError> {
    let builder = crate::engine::operators::with_orion_engine_defaults(
        dataflow_rs::Engine::builder(),
        secrets,
    )
    .with_handlers(handlers);

    screen_workflow(&workflow, &builder).map_err(|e| {
        crate::errors::OrionError::validation(format!(
            "workflow '{}' has an unusable task: {e}",
            workflow.id
        ))
    })?;

    builder
        .with_workflow(workflow)
        .build()
        .map_err(crate::errors::OrionError::Engine)
}

/// Convert active channels and their workflows to dataflow-rs workflows for the engine.
///
/// For each active channel, finds the associated workflow(s) and builds
/// dataflow-rs Workflow objects with the channel name injected as the channel field.
///
/// F33: a channel whose workflows cannot be built — missing `workflow_id`,
/// workflow not found among the active set, or a version that fails
/// conversion — is reported as a [`ChannelLoadIssue`] instead of being
/// silently skipped. Callers feed these into `ChannelLoader::build`, which
/// quarantines the channel: previously it stayed registered in the route
/// table with no workflow behind it, so requests got an opaque engine error.
///
/// [`ChannelLoadIssue`]: crate::channel::ChannelLoadIssue
pub fn build_engine_workflows(
    channels: &[Channel],
    workflows: &[Workflow],
    screen: &dyn HandlerScreen,
) -> (
    Vec<dataflow_rs::Workflow>,
    Vec<crate::channel::ChannelLoadIssue>,
) {
    // Index workflows by workflow_id for fast lookup
    let mut workflow_map: HashMap<String, Vec<&Workflow>> = HashMap::new();
    for workflow in workflows {
        workflow_map
            .entry(workflow.workflow_id.clone())
            .or_default()
            .push(workflow);
    }

    let mut result = Vec::new();
    let mut issues: Vec<crate::channel::ChannelLoadIssue> = Vec::new();

    for channel in channels {
        let Some(ref wf_id) = channel.workflow_id else {
            issues.push(crate::channel::ChannelLoadIssue {
                channel: channel.name.clone(),
                reason: "channel has no workflow_id".to_string(),
            });
            continue;
        };

        let Some(wf_versions) = workflow_map.get(wf_id) else {
            issues.push(crate::channel::ChannelLoadIssue {
                channel: channel.name.clone(),
                reason: format!("workflow '{wf_id}' not found among active workflows"),
            });
            continue;
        };

        if wf_versions.len() == 1 && wf_versions[0].rollout_percentage == 100 {
            // Single version at 100% — convert normally
            match workflow_to_dataflow(wf_versions[0], &channel.name) {
                Ok(w) => match screen_workflow(&w, screen) {
                    Ok(()) => result.push(w),
                    Err(e) => {
                        issues.push(crate::channel::ChannelLoadIssue {
                            channel: channel.name.clone(),
                            reason: format!("workflow '{wf_id}' has an unusable task: {e}"),
                        });
                    }
                },
                Err(e) => {
                    issues.push(crate::channel::ChannelLoadIssue {
                        channel: channel.name.clone(),
                        reason: format!("workflow '{wf_id}' failed to convert: {e}"),
                    });
                }
            }
        } else {
            // Multiple versions or partial rollout — wrap with bucket ranges.
            let mut sorted: Vec<&&Workflow> = wf_versions.iter().collect();
            sorted.sort_by_key(|b| std::cmp::Reverse(b.version));

            // Newest first, because input order is traffic order: the version
            // being rolled out takes the low buckets and the one it replaces
            // keeps the rest.
            //
            // A percentage outside `0..=100` cannot come through the API — the
            // repository bounds it on write — but a hand-edited row can carry
            // one, and it must not wrap into something plausible. Saturating
            // at `u8::MAX` guarantees the sum overshoots and the channel is
            // quarantined with the over-100 message.
            let percentages: Vec<u8> = sorted
                .iter()
                .map(|wf| u8::try_from(wf.rollout_percentage).unwrap_or(u8::MAX))
                .collect();

            // F30: buckets are 0–99, and percentages that do not sum to 100
            // misroute silently — under 100, the remainder of the traffic
            // matches no workflow version at all; over 100, later versions'
            // ranges start past bucket 99 and are unreachable. Both used to be
            // an accumulator and an `!= 100` check here. `Rollout::partition`
            // is the engine's own arithmetic for exactly this, error direction
            // included, so the invariant is now stated once — in the crate
            // that routes on it.
            match dataflow_rs::Rollout::partition(&percentages) {
                Ok(ranges) => {
                    let mut converted = Vec::new();
                    let mut failed = false;
                    for (wf, rollout) in sorted.iter().zip(ranges) {
                        match workflow_to_dataflow_with_rollout(wf, &channel.name, rollout)
                            .map_err(|e| e.to_string())
                            .and_then(|w| screen_workflow(&w, screen).map(|()| w))
                        {
                            Ok(w) => converted.push(w),
                            Err(e) => {
                                issues.push(crate::channel::ChannelLoadIssue {
                                    channel: channel.name.clone(),
                                    reason: format!(
                                        "workflow '{}' v{} failed to convert: {e}",
                                        wf.workflow_id, wf.version
                                    ),
                                });
                                failed = true;
                                break;
                            }
                        }
                    }
                    // All-or-nothing per channel: a partially-converted
                    // rollout would silently blackhole the failed version's
                    // bucket range.
                    if !failed {
                        result.append(&mut converted);
                    }
                }
                Err(e) => issues.push(crate::channel::ChannelLoadIssue {
                    channel: channel.name.clone(),
                    reason: format!(
                        "rollout percentages for workflow '{wf_id}' do not partition the \
                         bucket space: {e}"
                    ),
                }),
            }
        }
    }

    (result, issues)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The screen these tests convert against.
    ///
    /// A bare builder registers no custom handlers, which is all the fixtures
    /// below need — they are about rollout arithmetic and quarantine, and
    /// their tasks are `log`, a self-contained built-in every engine
    /// dispatches.
    fn screen() -> dataflow_rs::engine::EngineBuilder {
        dataflow_rs::Engine::builder()
    }

    /// `ESCAPED_TEMPLATE_KEY` must not quarantine a workflow.
    ///
    /// It is the one code `check_workflow` reports that `Engine::build` does
    /// not refuse, and this screen used to take the whole list — correct while
    /// every code was also a build refusal, wrong from dataflow-rs 3.9 on. It
    /// fires on *correct* code too: `$$set` is the documented fix, so treating
    /// it as fatal took down the channel of an author who had done the right
    /// thing.
    #[test]
    fn an_escaped_template_key_is_an_advisory_not_a_quarantine() {
        let workflow = dataflow_rs::Workflow::from_json(
            r#"{"id":"w","name":"w","condition":true,"tasks":[
                {"id":"t","name":"t","function":{"name":"map","input":{"mappings":[
                  {"path":"data.a","logic":{"$set":{"x":1}}},
                  {"path":"data.b","logic":{"$$oid":"abc"}}]}}}]}"#,
        )
        .expect("the fixture is a valid workflow");

        // The engine does report both keys — this is not a claim that they are
        // invisible, only that they are not a reason to refuse to serve.
        assert_eq!(
            screen()
                .check_workflow(&workflow)
                .iter()
                .filter(|i| i.code == dataflow_rs::IssueCode::EscapedTemplateKey)
                .count(),
            2
        );
        assert!(screen_workflow(&workflow, &screen()).is_ok());
    }

    /// The 3.10 pair, and the regression that upgrade would otherwise have been.
    ///
    /// Both are `Severity::Advisory` — reported by `check_workflow`, never
    /// refused by `build` — and both fire on shapes that are common in a real
    /// estate: a `validation` that collects errors and carries on is what the
    /// built-in is documented to do, and `continue_on_error` on a group is old
    /// enough that stored definitions carry it. Treating either as fatal
    /// quarantines working channels, which is what a hand-kept list did until
    /// it was edited: `dry-run` answered "workflow has an unusable task" for a
    /// workflow that runs perfectly.
    ///
    /// The severity table itself is upstream's to pin. What this asserts is
    /// Orion's half — that `screen_workflow` acts on it.
    #[test]
    fn the_advisory_issues_do_not_quarantine() {
        let cases = [
            (
                dataflow_rs::IssueCode::UnguardedValidation,
                r#"{"id":"w","name":"w","condition":true,"tasks":[
                    {"id":"check","name":"check","function":{"name":"validation","input":{
                      "rules":[{"logic":{"==":[1,2]},"message":"no"}]}}},
                    {"id":"after","name":"after","function":{"name":"map","input":{
                      "mappings":[{"path":"data.x","logic":true}]}}}]}"#,
            ),
            (
                dataflow_rs::IssueCode::GroupContinueOnError,
                r#"{"id":"w","name":"w","condition":true,"tasks":[
                    {"id":"g","name":"g","continue_on_error":true,"tasks":[
                      {"id":"t","name":"t","function":{"name":"log","input":{
                        "message":"x"}}}]}]}"#,
            ),
        ];

        for (code, json) in cases {
            let workflow =
                dataflow_rs::Workflow::from_json(json).expect("the fixture is a valid workflow");
            // The engine reports it — so this is a claim about severity, not
            // about the finding being absent.
            assert!(
                screen()
                    .check_workflow(&workflow)
                    .iter()
                    .any(|i| i.code == code),
                "{code:?} must be reported by check_workflow"
            );
            assert_eq!(
                code.severity(),
                dataflow_rs::Severity::Advisory,
                "{code:?} must be advisory"
            );
            assert!(
                screen_workflow(&workflow, &screen()).is_ok(),
                "{code:?} must not quarantine"
            );
        }
    }

    #[test]
    fn test_glob_match_exact() {
        assert!(glob_match("orders", "orders"));
        assert!(!glob_match("orders", "events"));
    }

    #[test]
    fn test_glob_match_prefix_wildcard() {
        assert!(glob_match("internal-*", "internal-debug"));
        assert!(glob_match("internal-*", "internal-"));
        assert!(!glob_match("internal-*", "external-debug"));
    }

    #[test]
    fn test_glob_match_suffix_wildcard() {
        assert!(glob_match("*-debug", "internal-debug"));
        assert!(!glob_match("*-debug", "internal-prod"));
    }

    #[test]
    fn test_glob_match_star_only() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn test_glob_match_middle_wildcard() {
        assert!(glob_match("pre*suf", "presuf"));
        assert!(glob_match("pre*suf", "pre-middle-suf"));
        assert!(!glob_match("pre*suf", "pre-middle"));
    }

    /// F32: cases the old successive-`find` matcher got wrong.
    #[test]
    fn test_glob_match_backtracking() {
        // The old matcher bound `b` to the first occurrence and failed.
        assert!(glob_match("a*bc", "abxbc"));
        assert!(glob_match("a*bc", "abcbc"));
        assert!(!glob_match("a*bc", "abxbd"));
    }

    #[test]
    fn test_glob_match_multi_star() {
        assert!(glob_match("a*b*c", "a-x-b-y-c"));
        assert!(glob_match("*orders*", "internal-orders-debug"));
        assert!(!glob_match("a*b*c", "a-x-c-y-b"));
        assert!(glob_match("**", "anything"));
    }

    fn make_channel(name: &str) -> Channel {
        Channel {
            tags_json: "[]".to_string(),
            channel_id: name.to_string(),
            name: name.to_string(),
            version: 1,
            status: crate::storage::models::EntityStatus::Active
                .as_str()
                .to_string(),
            channel_type: "sync".to_string(),
            protocol: crate::storage::models::ChannelProtocol::Http
                .as_str()
                .to_string(),
            methods_json: Some("POST".to_string()),
            workflow_id: None,
            topic: None,
            consumer_group: None,
            route_pattern: None,
            description: None,
            transport_config_json: "{}".to_string(),
            config_json: "{}".to_string(),
            priority: 0,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        }
    }

    fn make_workflow(wf_id: &str, version: i64, rollout: i64) -> Workflow {
        Workflow {
            workflow_id: wf_id.to_string(),
            version,
            name: format!("{wf_id}-v{version}"),
            description: None,
            priority: 0,
            status: "active".to_string(),
            rollout_percentage: rollout,
            condition_json: "true".to_string(),
            tasks_json:
                r#"[{"id":"t1","name":"log","function":{"name":"log","input":{"message":"x"}}}]"#
                    .to_string(),
            tags_json: "[]".to_string(),
            loop_json: None,
            continue_on_error: false,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        }
    }

    /// F30: rollout percentages that don't sum to 100 quarantine the channel
    /// instead of silently blackholing (or shadowing) part of the traffic.
    #[test]
    fn test_rollout_sum_must_be_100() {
        let mut channel = make_channel("rollout-ch");
        channel.workflow_id = Some("wf".to_string());

        // 50 + 30 = 80 — buckets 80–99 would match no version.
        let wfs = vec![make_workflow("wf", 1, 30), make_workflow("wf", 2, 50)];
        let (converted, issues) = build_engine_workflows(&[channel.clone()], &wfs, &screen());
        assert!(converted.is_empty(), "under-100 rollout must not half-load");
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].reason.contains("sum to 80"),
            "{}",
            issues[0].reason
        );

        // 60 + 60 = 120 — later versions unreachable.
        let wfs = vec![make_workflow("wf", 1, 60), make_workflow("wf", 2, 60)];
        let (converted, issues) = build_engine_workflows(&[channel.clone()], &wfs, &screen());
        assert!(converted.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].reason.contains("sum to 120"),
            "{}",
            issues[0].reason
        );

        // 50 + 50 = 100 — loads both versions cleanly.
        let wfs = vec![make_workflow("wf", 1, 50), make_workflow("wf", 2, 50)];
        let (converted, issues) = build_engine_workflows(&[channel], &wfs, &screen());
        assert_eq!(converted.len(), 2);
        assert!(issues.is_empty(), "{issues:?}");
    }

    /// The `[min, max)` rollout bucket range a converted workflow serves.
    ///
    /// This used to parse the range back out of a synthetic condition
    /// (`and[1]` is `>= min`, `and[2]` is `< max`) — which is why the split
    /// could be inert for as long as it was: the assertion only ever checked
    /// the *shape* of the generated JSON, never that a bucket routed anywhere.
    fn bucket_range(wf: &dataflow_rs::Workflow) -> (u8, u8) {
        let rollout = wf
            .rollout
            .expect("converted rollout workflow carries a range");
        (rollout.bucket_start, rollout.bucket_end)
    }

    /// The bucket ranges must partition 0–99 contiguously, newest version
    /// first: 20/50/30 across v3/v2/v1 → v3 [0,20), v2 [20,70), v1 [70,100).
    /// A gap or overlap in the offsets silently misroutes traffic, which is
    /// exactly what this accumulation arithmetic exists to prevent.
    #[test]
    fn test_rollout_bucket_offsets_partition_newest_first() {
        let mut channel = make_channel("rollout-ch");
        channel.workflow_id = Some("wf".to_string());
        let wfs = vec![
            make_workflow("wf", 1, 30),
            make_workflow("wf", 2, 50),
            make_workflow("wf", 3, 20),
        ];
        let (converted, issues) = build_engine_workflows(&[channel], &wfs, &screen());
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(converted.len(), 3);

        let by_id: std::collections::HashMap<String, (u8, u8)> = converted
            .iter()
            .map(|w| (w.id.clone(), bucket_range(w)))
            .collect();
        assert_eq!(
            by_id["wf:v3"],
            (0, 20),
            "newest version gets the first bucket"
        );
        assert_eq!(by_id["wf:v2"], (20, 70));
        assert_eq!(by_id["wf:v1"], (70, 100));

        // …and the ranges actually route. Every bucket 0–99 must be served by
        // exactly one version. The previous wiring passed the shape assertion
        // above while sending 100% of traffic to whichever version started at
        // 0, because the condition's `var` addressed the context root and the
        // bucket was injected under `data`.
        for bucket in 0u8..100 {
            let serving: Vec<&str> = converted
                .iter()
                .filter(|w| w.rollout.is_some_and(|r| r.accepts(bucket)))
                .map(|w| w.id.as_str())
                .collect();
            assert_eq!(
                serving.len(),
                1,
                "bucket {bucket} is served by {serving:?}, not exactly one version"
            );
        }
    }

    /// F33: a channel with no workflow_id, and one pointing at a workflow
    /// that is not in the active set, must each surface a load issue naming
    /// the problem rather than being silently skipped.
    #[test]
    fn test_missing_and_unknown_workflow_are_reported_as_issues() {
        let no_wf = make_channel("no-wf");
        let mut unknown = make_channel("unknown-wf");
        unknown.workflow_id = Some("ghost".to_string());

        let (converted, issues) = build_engine_workflows(&[no_wf, unknown], &[], &screen());
        assert!(converted.is_empty());
        assert_eq!(issues.len(), 2);
        assert!(
            issues[0].reason.contains("no workflow_id"),
            "{}",
            issues[0].reason
        );
        assert!(
            issues[1].reason.contains("'ghost' not found"),
            "{}",
            issues[1].reason
        );
    }

    /// All-or-nothing per channel: when one version of a rollout fails to
    /// convert, the versions that DID convert must not load — a partial
    /// rollout would silently blackhole the failed version's bucket range.
    #[test]
    fn test_partial_rollout_conversion_failure_loads_nothing() {
        let mut channel = make_channel("rollout-ch");
        channel.workflow_id = Some("wf".to_string());

        // v2 (processed first, newest) converts fine; v1 has broken tasks.
        let mut bad_v1 = make_workflow("wf", 1, 50);
        bad_v1.tasks_json = "not json".to_string();
        let wfs = vec![bad_v1, make_workflow("wf", 2, 50)];

        let (converted, issues) = build_engine_workflows(&[channel], &wfs, &screen());
        assert!(
            converted.is_empty(),
            "the successfully-converted v2 must be discarded with v1"
        );
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].reason.contains("v1 failed to convert"),
            "{}",
            issues[0].reason
        );
    }

    #[test]
    fn test_filter_channels_no_config() {
        let channels = vec![make_channel("orders"), make_channel("events")];
        let config = crate::config::ChannelFilterConfig::default();
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_filter_channels_include_only() {
        let channels = vec![
            make_channel("orders"),
            make_channel("events"),
            make_channel("internal-debug"),
        ];
        let config = crate::config::ChannelFilterConfig {
            include: vec!["orders".to_string(), "events".to_string()],
            exclude: vec![],
        };
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|c| c.name != "internal-debug"));
    }

    #[test]
    fn test_filter_channels_exclude_only() {
        let channels = vec![
            make_channel("orders"),
            make_channel("events"),
            make_channel("internal-debug"),
        ];
        let config = crate::config::ChannelFilterConfig {
            include: vec![],
            exclude: vec!["internal-*".to_string()],
        };
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|c| c.name != "internal-debug"));
    }

    #[test]
    fn test_filter_channels_include_and_exclude() {
        let channels = vec![
            make_channel("orders"),
            make_channel("orders-debug"),
            make_channel("events"),
        ];
        let config = crate::config::ChannelFilterConfig {
            include: vec!["orders*".to_string()],
            exclude: vec!["*-debug".to_string()],
        };
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "orders");
    }

    // -- F41: a malformed task input quarantines its channel, not the engine --

    fn workflow_with_raw_tasks(wf_id: &str, tasks_json: &str) -> Workflow {
        let mut wf = make_workflow(wf_id, 1, 100);
        wf.tasks_json = tasks_json.to_string();
        wf
    }

    #[test]
    fn unregistered_custom_function_quarantines_only_its_channel() {
        // A task naming a function that is neither a dataflow-rs builtin nor a
        // registered Orion handler lands in FunctionConfig::Custom, where
        // precompile_custom_inputs fails with FunctionNotFound — aborting the
        // *entire* engine build. Before F41 that meant boot aborted, or a reload
        // took down every channel on every node, because of one stored row.
        let mut good = make_channel("good");
        good.workflow_id = Some("wf-good".to_string());
        let mut bad = make_channel("bad");
        bad.workflow_id = Some("wf-bad".to_string());

        let wfs = vec![
            make_workflow("wf-good", 1, 100),
            workflow_with_raw_tasks(
                "wf-bad",
                r#"[{"id":"t1","name":"oops","function":{"name":"totally_not_a_function",
                   "input":{}}}]"#,
            ),
        ];

        let (converted, issues) = build_engine_workflows(&[good, bad], &wfs, &screen());

        assert_eq!(
            converted.len(),
            1,
            "the healthy channel must still be built"
        );
        assert_eq!(converted[0].channel, "good");
        assert_eq!(issues.len(), 1, "issues = {issues:?}");
        assert_eq!(issues[0].channel, "bad");
        assert!(
            issues[0].reason.contains("'totally_not_a_function'")
                && issues[0].reason.contains("task 't1'"),
            "reason should name the offending task and function: {}",
            issues[0].reason
        );
    }

    // ---- build_single: the screen is not the caller's to remember --------

    fn one_task_workflow(function: &str, input: serde_json::Value) -> dataflow_rs::Workflow {
        serde_json::from_value(serde_json::json!({
            "id": "wf-1",
            "name": "wf-1",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "t1",
                "function": { "name": function, "input": input }
            }],
        }))
        .expect("a well-formed workflow")
    }

    /// The divergence this closes, in its sharpest form.
    ///
    /// `enrich` is a dataflow-rs built-in that *requires a handler*, and Orion
    /// never registers one (F54). So it deserializes, `.build()` accepts it,
    /// and every request fails with `FunctionNotFound` — which is why
    /// `dry-run` and `POST /workflows/{id}/test` calling `.build()` by hand
    /// gave a green answer for a workflow boot would quarantine.
    ///
    /// It is also the trap in screening by severity: this is
    /// `Severity::Defect`, the class between "refused" and "advisory", and the
    /// only one where `build` and the first message disagree. A screen written
    /// as "pass anything `build` would accept" lets it through. Both halves of
    /// that are asserted below, so the prose above is checked rather than
    /// trusted.
    #[test]
    fn build_single_refuses_a_built_in_with_no_handler_behind_it() {
        let task = || {
            one_task_workflow(
                "enrich",
                serde_json::json!({ "connector": "c", "merge_path": "data" }),
            )
        };
        assert!(
            screen()
                .check_workflow(&task())
                .iter()
                .any(|i| i.severity() == dataflow_rs::Severity::Defect),
            "`enrich` with no handler is the Defect class"
        );
        assert!(
            dataflow_rs::Engine::builder()
                .with_workflow(task())
                .build()
                .is_ok(),
            "`build` accepts it — the premise that makes this screen necessary"
        );

        // `dataflow_rs::Engine` is not `Debug`, so unwrap the arms by hand.
        let Err(err) = build_single(
            task(),
            std::collections::HashMap::new(),
            &crate::engine::ResolvedSecrets::empty(),
        ) else {
            unreachable!(
                "`enrich` has no handler in any Orion engine — building it is \
                 the bug this screen exists to catch"
            );
        };

        let msg = err.to_string();
        assert!(
            msg.contains("wf-1") && msg.contains("t1"),
            "the refusal must name the workflow and the task: {msg}"
        );
        assert!(
            matches!(err, crate::errors::OrionError::Validation { .. }),
            "an unusable task is the author's document to fix, not a server \
             fault: {err:?}"
        );
    }

    /// The control: a self-contained built-in needs no handler, so the same
    /// call builds. Without this the test above would pass for a `build_single`
    /// that refuses everything.
    #[test]
    fn build_single_builds_a_workflow_every_engine_can_run() {
        assert!(
            build_single(
                one_task_workflow("log", serde_json::json!({ "message": "x" })),
                std::collections::HashMap::new(),
                &crate::engine::ResolvedSecrets::empty(),
            )
            .is_ok(),
            "`log` is self-contained — every engine dispatches it"
        );
    }
}

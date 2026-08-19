//! Turning stored channel + workflow rows into the dataflow-rs workflow set the
//! engine is built from — including the per-channel quarantine that keeps one
//! bad row from taking the instance down.

use dataflow_rs::datalogic_rs;
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

/// Every function name registered by [`build_custom_functions`].
///
/// A task naming anything else lands in `FunctionConfig::Custom` and makes
/// `precompile_custom_inputs` fail with `FunctionNotFound` — which aborts the
/// *whole* engine build. Checking membership here downgrades that to a
/// per-channel quarantine (proposal F41).
/// `registered_handler_names_match_the_constant` pins the two together.
///
/// [`build_custom_functions`]: super::handlers::build_custom_functions
pub const CUSTOM_HANDLER_FUNCTIONS: &[&str] = &[
    "cache_read",
    "cache_write",
    "channel_call",
    "crypto",
    "data_query",
    "data_write",
    "db_read",
    "db_write",
    "http_call",
    "mongo_read",
    "publish_kafka",
];

/// Run the same `serde` deserialization that `dataflow_rs`'s
/// `precompile_custom_inputs` will run at engine construction.
///
/// `AsyncFunctionHandler::parse_input_box` delegates to
/// `serde_json::from_value::<Self::Input>`, which is purely type-driven — no
/// handler state is consulted — so checking the type here is equivalent to
/// checking it there. The engine's own handler map is not reachable once the
/// engine exists (`Engine::new` consumes it and the field is private), which is
/// why this is a table rather than a lookup.
///
/// Only `channel_call` has a typed `Input`; every other Orion handler takes
/// `serde_json::Value` and accepts any JSON. (`http_call` and `publish_kafka`
/// are dataflow-rs *builtins* — their typed configs are already parsed during
/// `workflow_to_dataflow`, so they never reach `Custom`.)
///
/// For `channel_call` this also **compiles** its two JSONLogic fields, which
/// `AsyncFunctionHandler::compile_input` will compile again inside
/// `Engine::new`. That duplication is deliberate: since those fields became
/// `Template`s a malformed expression fails the *build* rather than one
/// message, and an engine-build failure is a whole-instance failure. Compiling
/// here first keeps it a per-channel `ChannelLoadIssue` (F33/F41).
fn custom_input_parse_check(name: &str, input: &serde_json::Value) -> Result<(), String> {
    match name {
        "channel_call" => {
            let parsed: super::functions::channel_call::ChannelCallInput =
                serde_json::from_value(input.clone()).map_err(|e| e.to_string())?;
            // `TemplateCompiler::new` is crate-private, so this cannot call
            // `Template::compile` — it compiles the same raw JSON against an
            // engine configured the way `LogicCompiler` configures its own
            // (templating on, Orion's operators registered), which is the
            // property that matters.
            let engine = crate::engine::operators::add_to_datalogic(
                datalogic_rs::Engine::builder().with_templating(true),
            )
            .build();
            for (label, template) in [
                ("channel_logic", parsed.channel_logic.as_ref()),
                ("data_logic", parsed.data_logic.as_ref()),
            ] {
                if let Some(t) = template {
                    engine
                        .compile(t.as_json())
                        .map_err(|e| format!("{label} does not compile: {e}"))?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validate every custom task before the engine is built.
///
/// Without this, an unregistered function name or a typed-input mismatch is
/// invisible until engine construction — which is a whole-instance failure, not
/// a per-channel one: at boot the process aborts, and on reload every channel on
/// every node goes down because one stored row is unusable. That defeats the
/// F33/F35 quarantine, whose premise is that one broken row must never stop the
/// instance. Checking here turns it back into a `ChannelLoadIssue` (F41).
fn check_custom_inputs(wf: &dataflow_rs::Workflow) -> Result<(), String> {
    use dataflow_rs::engine::functions::config::FunctionConfig;
    for task in &wf.tasks {
        let FunctionConfig::Custom { name, input, .. } = &task.function else {
            continue;
        };
        if !CUSTOM_HANDLER_FUNCTIONS.contains(&name.as_str()) {
            return Err(format!(
                "task '{}' calls unregistered function '{name}'",
                task.id
            ));
        }
        custom_input_parse_check(name, input)
            .map_err(|e| format!("task '{}' has invalid input for '{name}': {e}", task.id))?;
    }
    Ok(())
}

/// Convert active channels and their workflows to dataflow-rs workflows for the engine.
///
/// For each active channel, finds the associated workflow(s) and builds
/// dataflow-rs Workflow objects with the channel name injected as the channel field.
///
/// F33: a channel whose workflows cannot be built — missing `workflow_id`,
/// workflow not found among the active set, or a version that fails
/// conversion — is reported as a [`ChannelLoadIssue`] instead of being
/// silently skipped. Callers feed these into `ChannelRegistry::reload`, which
/// quarantines the channel: previously it stayed registered in the route
/// table with no workflow behind it, so requests got an opaque engine error.
///
/// [`ChannelLoadIssue`]: crate::channel::ChannelLoadIssue
pub fn build_engine_workflows(
    channels: &[Channel],
    workflows: &[Workflow],
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
                Ok(w) => match check_custom_inputs(&w) {
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
            // Multiple versions or partial rollout — wrap with bucket ranges
            let mut sorted: Vec<&&Workflow> = wf_versions.iter().collect();
            sorted.sort_by_key(|b| std::cmp::Reverse(b.version));

            let mut bucket_offset = 0i64;
            let mut converted = Vec::new();
            let mut failed = false;
            for wf in &sorted {
                let bucket_min = bucket_offset;
                let bucket_max = bucket_offset + wf.rollout_percentage;
                match workflow_to_dataflow_with_rollout(wf, &channel.name, bucket_min, bucket_max)
                    .map_err(|e| e.to_string())
                    .and_then(|w| check_custom_inputs(&w).map(|()| w))
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
                bucket_offset = bucket_max;
            }
            // F30: buckets are 0–99; percentages that don't sum to 100
            // silently misroute — under 100, the remainder of the traffic
            // matches no workflow version at all; over 100, later versions'
            // ranges start past bucket 99 and are unreachable.
            if !failed && bucket_offset != 100 {
                issues.push(crate::channel::ChannelLoadIssue {
                    channel: channel.name.clone(),
                    reason: format!(
                        "rollout percentages for workflow '{wf_id}' sum to {bucket_offset}, \
                         not 100 — {}",
                        if bucket_offset < 100 {
                            format!(
                                "{}% of traffic would match no workflow version",
                                100 - bucket_offset
                            )
                        } else {
                            "later versions would be unreachable".to_string()
                        }
                    ),
                });
                failed = true;
            }
            // All-or-nothing per channel: a partially-converted rollout would
            // silently blackhole the failed version's bucket range.
            if !failed {
                result.append(&mut converted);
            }
        }
    }

    (result, issues)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (converted, issues) = build_engine_workflows(&[channel.clone()], &wfs);
        assert!(converted.is_empty(), "under-100 rollout must not half-load");
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].reason.contains("sum to 80"),
            "{}",
            issues[0].reason
        );

        // 60 + 60 = 120 — later versions unreachable.
        let wfs = vec![make_workflow("wf", 1, 60), make_workflow("wf", 2, 60)];
        let (converted, issues) = build_engine_workflows(&[channel.clone()], &wfs);
        assert!(converted.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].reason.contains("sum to 120"),
            "{}",
            issues[0].reason
        );

        // 50 + 50 = 100 — loads both versions cleanly.
        let wfs = vec![make_workflow("wf", 1, 50), make_workflow("wf", 2, 50)];
        let (converted, issues) = build_engine_workflows(&[channel], &wfs);
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
        let (converted, issues) = build_engine_workflows(&[channel], &wfs);
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

        let (converted, issues) = build_engine_workflows(&[no_wf, unknown], &[]);
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

        let (converted, issues) = build_engine_workflows(&[channel], &wfs);
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

        let (converted, issues) = build_engine_workflows(&[good, bad], &wfs);

        assert_eq!(
            converted.len(),
            1,
            "the healthy channel must still be built"
        );
        assert_eq!(converted[0].channel, "good");
        assert_eq!(issues.len(), 1, "issues = {issues:?}");
        assert_eq!(issues[0].channel, "bad");
        assert!(
            issues[0]
                .reason
                .contains("unregistered function 'totally_not_a_function'"),
            "reason should name the offending function: {}",
            issues[0].reason
        );
    }

    #[test]
    fn channel_call_with_only_channel_logic_builds() {
        // F23: the schema (and the docs) declare `channel` optional when
        // `channel_logic` is given, but the struct required it — so this exact
        // workflow passed admin validation and then failed the engine build.
        let mut channel = make_channel("dyn");
        channel.workflow_id = Some("wf-dyn".to_string());
        let wfs = vec![workflow_with_raw_tasks(
            "wf-dyn",
            r#"[{"id":"t1","name":"fan","function":{"name":"channel_call",
               "input":{"channel_logic":{"var":"target"}}}}]"#,
        )];

        let (converted, issues) = build_engine_workflows(&[channel], &wfs);
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
        assert_eq!(converted.len(), 1);
    }

    #[test]
    fn channel_call_input_rejects_a_wrongly_typed_field() {
        // `channel` is defaulted (F23) but still typed: a non-string must not
        // slip through to the engine build.
        assert!(
            custom_input_parse_check("channel_call", &serde_json::json!({ "channel": 7 })).is_err()
        );
        assert!(
            custom_input_parse_check("channel_call", &serde_json::json!({ "channel": "a" }))
                .is_ok()
        );
        // A `Value`-input handler accepts anything.
        assert!(custom_input_parse_check("db_read", &serde_json::json!({ "x": 1 })).is_ok());
    }
}

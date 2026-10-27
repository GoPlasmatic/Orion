use super::testing::*;
use super::*;

/// [`Engines::bare`]'s load, as `(the workflows, the issues)`.
fn load(
    channels: &[Channel],
    workflows: &[Workflow],
) -> (Vec<CompiledWorkflow>, Vec<ChannelLoadIssue>) {
    let loaded = Engines::bare().load(channels, workflows);
    (loaded.workflows, loaded.issues)
}

/// An advisory never quarantines: a stripped `$` key and a `validation`
/// that gates nothing both load, because the workflow runs. `$$set` is the documented fix for the first, so treating
/// it as fatal would take down the channel of an author who did the right
/// thing.
#[test]
fn an_advisory_never_quarantines() {
    let mut channel = make_channel("advised");
    channel.workflow_id = Some("wf-advised".to_string());
    let wfs = vec![workflow_with_raw_tasks(
        "wf-advised",
        r#"[
                {"id":"check","name":"check","function":{"name":"validation","input":{
                    "rules":[{"logic":{"==":[1,2]},"message":"no"}]}}},
                {"id":"g","name":"g","tasks":[
                    {"id":"m","name":"m","function":{"name":"map","input":{"mappings":[
                        {"path":"data.doc","logic":{"$set":{"a":1}}}]}}}]}
            ]"#,
    )];
    let (loaded, issues) = load(&[channel], &wfs);
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(loaded.len(), 1);
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

/// F32: cases a matcher that binds each literal to its first occurrence
/// gets wrong.
#[test]
fn test_glob_match_backtracking() {
    // Binding `b` to its first occurrence would fail here.
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

/// F30: rollout percentages that don't sum to 100 quarantine the channel
/// instead of silently blackholing (or shadowing) part of the traffic.
#[test]
fn test_rollout_sum_must_be_100() {
    let mut channel = make_channel("rollout-ch");
    channel.workflow_id = Some("wf".to_string());

    // 50 + 30 = 80 — buckets 80–99 would match no version.
    let wfs = vec![make_workflow("wf", 1, 30), make_workflow("wf", 2, 50)];
    let (converted, issues) = load(&[channel.clone()], &wfs);
    assert!(converted.is_empty(), "under-100 rollout must not half-load");
    assert_eq!(issues.len(), 1);
    assert!(
        issues[0].reason.contains("sum to 80"),
        "{}",
        issues[0].reason
    );

    // 60 + 60 = 120 — later versions unreachable.
    let wfs = vec![make_workflow("wf", 1, 60), make_workflow("wf", 2, 60)];
    let (converted, issues) = load(&[channel.clone()], &wfs);
    assert!(converted.is_empty());
    assert_eq!(issues.len(), 1);
    assert!(
        issues[0].reason.contains("sum to 120"),
        "{}",
        issues[0].reason
    );

    // 50 + 50 = 100 — loads both versions cleanly.
    let wfs = vec![make_workflow("wf", 1, 50), make_workflow("wf", 2, 50)];
    let (converted, issues) = load(&[channel], &wfs);
    assert_eq!(converted.len(), 2);
    assert!(issues.is_empty(), "{issues:?}");
}

/// The bucket ranges must partition 0–99 contiguously, newest version
/// first: 20/50/30 across v3/v2/v1 → v3 [0,20), v2 [20,70), v1 [70,100).
/// A gap or overlap in the offsets silently misroutes traffic, which is
/// exactly what this accumulation arithmetic exists to prevent — so every
/// bucket is run, and must be served by exactly the version that owns it.
#[tokio::test]
async fn test_rollout_bucket_offsets_partition_newest_first() {
    let mut channel = make_channel("rollout-ch");
    channel.workflow_id = Some("wf".to_string());
    let version = |v: i64, pct: i64| {
        let mut wf = make_workflow("wf", v, pct);
        wf.tasks_json = serde_json::json!([{"id": "mark", "name": "mark", "function": {
                "name": "map", "input": {"mappings": [{"path": "data.version", "logic": v}]}}}])
        .to_string();
        wf
    };
    let wfs = vec![version(1, 30), version(2, 50), version(3, 20)];
    let engines = Engines::bare();
    let loaded = engines.load(&[channel], &wfs);
    assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
    assert_eq!(
        loaded.workflows.iter().map(|w| w.id()).collect::<Vec<_>>(),
        ["wf:v3", "wf:v2", "wf:v1"]
    );

    let engine = serving_engine(engines.compiler.link(loaded.workflows));
    for bucket in 0u8..100 {
        let mut message =
            crate::execute::serving_message(&serde_json::json!({}), &serde_json::json!({}));
        let options = dataflow_rs::run::RunOptions::default()
            .channel("rollout-ch")
            .routing_bucket(bucket);
        let run = engine.run_with(&mut message, &options).await;
        assert_eq!(run.matched(), 1, "bucket {bucket} is served by one version");
        let expected = match bucket {
            0..20 => 3,
            20..70 => 2,
            _ => 1,
        };
        assert_eq!(
            Value::from(message.context().data())["version"],
            expected,
            "bucket {bucket}"
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

    let (converted, issues) = load(&[no_wf, unknown], &[]);
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

    let (converted, issues) = load(&[channel], &wfs);
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

#[test]
fn unregistered_custom_function_quarantines_only_its_channel() {
    // F41: a task naming a function that is neither a dataflow-rs built-in
    // nor a registered Orion handler is refused by the engine for its row.
    // Failing the *entire* engine build on it would abort boot, or take down
    // every channel on every node on a reload, because of one stored row.
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

    let (converted, issues) = load(&[good, bad], &wfs);

    assert_eq!(
        converted.len(),
        1,
        "the healthy channel must still be built"
    );
    assert_eq!(converted[0].id(), "wf-good");
    assert_eq!(issues.len(), 1, "issues = {issues:?}");
    assert_eq!(issues[0].channel, "bad");
    // The engine's verdict decides, so the reason is its issue's code
    // and path (I182).
    assert!(
        issues[0].reason.contains("`totally_not_a_function`")
            && issues[0]
                .reason
                .contains("task 't1': UNKNOWN_FUNCTION at tasks[0].function.name"),
        "reason should name the offending task and function: {}",
        issues[0].reason
    );
}

// ---- dataflow-rs 4 decides which rows load (I182) ---------------------

/// A v3 spelling is not read: a task's `continue_on_error` and
/// `halt_on` are refused at the key, `validate` as an unknown function,
/// and each refusal names the v4 spelling to write. A function input's
/// 1.12 alias is an unknown key of the function's config.
#[test]
fn a_v3_spelling_is_refused_naming_its_v4_spelling() {
    let engines = Engines::orion();
    let task = |extra: Value, name: &str, input: Value| {
        let mut task = serde_json::json!({"id": "t", "function": {"name": name, "input": input}});
        if let (Some(task), Some(extra)) = (task.as_object_mut(), extra.as_object()) {
            task.extend(extra.clone());
        }
        serde_json::json!({"id": "w", "tasks": [task]})
    };
    let log = serde_json::json!({"message": "x"});
    let cases = [
        (
            task(
                serde_json::json!({"continue_on_error": true}),
                "log",
                log.clone(),
            ),
            "tasks[0].continue_on_error",
            "write `on_failure: continue, on_rejection: continue`",
        ),
        (
            task(
                serde_json::json!({"halt_on": "failure"}),
                "log",
                log.clone(),
            ),
            "tasks[0].halt_on",
            "write `on_failure: fail, on_rejection: halt`",
        ),
        (
            task(
                serde_json::json!({}),
                "validate",
                serde_json::json!({"rules": [{"logic": true, "message": "m"}]}),
            ),
            "tasks[0].function.name",
            "write `validation`",
        ),
    ];
    for (json, path, instead) in cases {
        let issues = compile_row(&engines.compiler, &json).expect_err("refused");
        let issue = issues
            .iter()
            .find(|i| i.path.to_string() == path)
            .unwrap_or_else(|| unreachable!("no issue at {path}: {issues:?}"));
        assert!(issue.message.contains(instead), "{}", issue.message);
    }

    for (function, alias) in [
        ("http_call", "response_path"),
        ("http_call", "path_logic"),
        ("channel_call", "channel_logic"),
    ] {
        let json = task(
            serde_json::json!({}),
            function,
            serde_json::json!({"connector": "c", alias: "data.x"}),
        );
        let issues = compile_row(&engines.compiler, &json).expect_err("refused");
        let at = format!("tasks[0].function.input.{alias}");
        assert!(
            issues
                .iter()
                .any(|i| i.path.to_string() == at && i.severity == Severity::Rejected),
            "{function}.{alias}: {issues:?}"
        );
    }
}

/// A rollout set is all or nothing: a version the compiler refuses drops
/// the whole set. The refusal is break 2a: `cache_write`'s `output`,
/// which 1.12 resolved and ignored, is an unknown key of its config.
#[tokio::test]
async fn a_rollout_set_one_of_whose_versions_is_rejected_is_dropped_whole() {
    let mut channel = make_channel("rollout-ch");
    channel.workflow_id = Some("wf".to_string());
    let mut v1 = make_workflow("wf", 1, 50);
    v1.tasks_json = r#"[{"id":"t1","name":"put","function":{"name":"cache_write","input":{
            "connector":"c","key":"k","value":{"a":1},"output":"data.x"}}}]"#
        .to_string();
    let wfs = vec![v1, make_workflow("wf", 2, 50)];

    let engines = Engines::orion();
    let loaded = engines.load(std::slice::from_ref(&channel), &wfs);
    assert!(loaded.workflows.is_empty());
    assert_eq!(loaded.issues.len(), 1, "{:?}", loaded.issues);
    let reason = &loaded.issues[0].reason;
    assert!(
        reason.starts_with(
            "workflow 'wf' v1 failed to compile: task 't1': UNKNOWN_KEY at \
                 tasks[0].function.input.output"
        ),
        "{reason}"
    );

    // The same set without the stray key loads whole.
    let mut fixed = wfs.clone();
    fixed[0].tasks_json = fixed[0].tasks_json.replace(r#","output":"data.x""#, "");
    let loaded = engines.load(&[channel], &fixed);
    assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
    assert_eq!(
        loaded.workflows.iter().map(|w| w.id()).collect::<Vec<_>>(),
        ["wf:v2", "wf:v1"]
    );
}

/// I185 and I182: break 21's root-output Defect quarantines, its code and
/// path in the reason (1.12 loaded the row, and replaced `data` with the
/// rows on every call). A `PAYLOAD_VAR` Defect does not: that row loads,
/// and the compile keeps the Defect for the log.
#[tokio::test]
async fn a_root_output_defect_quarantines_its_channel_and_a_payload_var_does_not() {
    let engines = Engines::orion();
    let mut rows = make_channel("rows");
    rows.workflow_id = Some("wf-rows".to_string());
    let mut payload = make_channel("payload");
    payload.workflow_id = Some("wf-payload".to_string());
    let wfs = vec![
        workflow_with_raw_tasks(
            "wf-rows",
            r#"[{"id":"t1","name":"read","function":{"name":"db_read","input":{
                    "connector":"db","query":"SELECT 1","output":"data"}}}]"#,
        ),
        workflow_with_raw_tasks(
            "wf-payload",
            r#"[{"id":"t1","name":"m","function":{"name":"map","input":{"mappings":[
                    {"path":"data.x","logic":{"var":"payload.x"}}]}}}]"#,
        ),
    ];
    let loaded = engines.load(&[rows, payload], &wfs);
    assert_eq!(loaded.issues.len(), 1, "{:?}", loaded.issues);
    assert_eq!(loaded.issues[0].channel, "rows");
    assert!(
        loaded.issues[0].reason.starts_with(
            "workflow 'wf-rows' failed to compile: task 't1': INPUT_PARSE at \
                 tasks[0].function.input.output: `db_read` writes"
        ),
        "{}",
        loaded.issues[0].reason
    );
    assert_eq!(
        loaded.workflows.iter().map(|w| w.id()).collect::<Vec<_>>(),
        ["wf-payload"]
    );

    let json = workflow_json(&wfs[1], "payload", "wf-payload", "active", None).expect("json");
    let compiled = compile_row(&engines.compiler, &json).expect("loads");
    assert!(
        compiled
            .issues
            .iter()
            .any(|i| i.code == IssueCode::PayloadVar && i.severity == Severity::Defect),
        "{:?}",
        compiled.issues
    );
}

/// Break 2: a near miss of a task key is a `LIKELY_TYPO`, and it
/// quarantines, where 1.12 ignored the key and ran the task
/// unconditionally. With the v3 compat reader off, a stray key that
/// resembles none is refused too: a definition carries only its own keys,
/// and an extension goes in `ext`.
#[test]
fn a_likely_typo_and_an_unknown_key_both_quarantine() {
    let mut typo = make_channel("typo");
    typo.workflow_id = Some("wf-typo".to_string());
    let mut stray = make_channel("stray");
    stray.workflow_id = Some("wf-stray".to_string());
    let wfs = vec![
        workflow_with_raw_tasks(
            "wf-typo",
            r#"[{"id":"t1","name":"log","conditon":false,
                    "function":{"name":"log","input":{"message":"x"}}}]"#,
        ),
        workflow_with_raw_tasks(
            "wf-stray",
            r#"[{"id":"t1","name":"log","owner":"ops",
                    "function":{"name":"log","input":{"message":"x"}}}]"#,
        ),
    ];

    let (converted, issues) = load(&[typo, stray], &wfs);
    assert_eq!(issues.len(), 2, "{issues:?}");
    assert_eq!(issues[0].channel, "typo");
    assert!(
        issues[0]
            .reason
            .contains("task 't1': LIKELY_TYPO at tasks[0].conditon"),
        "{}",
        issues[0].reason
    );
    assert_eq!(issues[1].channel, "stray");
    assert!(
        issues[1]
            .reason
            .contains("task 't1': UNKNOWN_KEY at tasks[0].owner"),
        "{}",
        issues[1].reason
    );
    assert!(converted.is_empty());
}

/// The rule itself, one severity and code at a time.
#[test]
fn quarantines_takes_rejected_issues_and_two_defects_only() {
    let at = dataflow_rs::program::AuthoredPath::root();
    let issue = |code, severity| Issue::new(code, at.clone(), "x").with_severity(severity);
    assert!(quarantines(&issue(
        IssueCode::UnknownKey,
        Severity::Rejected
    )));
    assert!(quarantines(&issue(IssueCode::InputParse, Severity::Defect)));
    assert!(quarantines(&issue(IssueCode::LikelyTypo, Severity::Defect)));
    assert!(!quarantines(&issue(
        IssueCode::PayloadVar,
        Severity::Defect
    )));
    assert!(!quarantines(&issue(
        IssueCode::UnresolvedReference,
        Severity::Defect
    )));
    assert!(!quarantines(&issue(
        IssueCode::InputParse,
        Severity::Advisory
    )));
    assert!(!quarantines(&issue(
        IssueCode::V3Spelling,
        Severity::Advisory
    )));
}

// ---- The named load breaks, each a quarantine (CHANGELOG, Unreleased) --

/// `base` with each key of `extra` set over it.
fn over(mut base: Value, extra: Value) -> Value {
    let Value::Object(extra) = extra else {
        unreachable!("the overrides are an object")
    };
    base.as_object_mut()
        .expect("an input is an object")
        .extend(extra);
    base
}

/// Every load refusal the cutover brings for Orion's own functions, each
/// quarantining its channel with its code and path in the reason: 1.12
/// failed every call on each, or refused it when the workflow was
/// written, and loaded it (or for break 2a and break 21, ran it). Each
/// row cites the break and the task that ported the field.
#[tokio::test]
async fn every_named_load_break_quarantines_its_channel() {
    use serde_json::json;
    let engines = Engines::orion_with::<NoFunction>(exprs_with_a_secret(), Vec::new());
    let hmac = |extra| {
        over(
            json!({"op": "hmac", "algorithm": "sha256", "data": "x", "key": "k",
                    "output": "data.d"}),
            extra,
        )
    };
    let verify = |extra| {
        over(
            json!({"token": "t", "algorithms": ["HS256"],
                    "keys": [{"algorithm": "HS256", "key": "k"}]}),
            extra,
        )
    };
    let sign = |extra| {
        over(
            json!({"algorithm": "HS256", "key": "k", "expires_in": "5m",
                    "output": "data.token"}),
            extra,
        )
    };
    #[rustfmt::skip]
        let rows = [
            // I183: a field schema.rs marks required, missing.
            row("I183 (O3)", "http_call", json!({}), "INPUT_PARSE", ".connector"),
            row("I183 (O4)", "db_read", json!({"connector": "db"}), "INPUT_PARSE", ".query"),
            row("I183 (O4)", "db_write", json!({"connector": "db"}), "INPUT_PARSE", ".query"),
            row("I183 (O4)", "data_query", json!({"connector": "db"}), "INPUT_PARSE", ".query"),
            row("I183 (O4)", "data_write", json!({"connector": "db"}), "INPUT_PARSE", ".write"),
            row("I183 (O6)", "cache_read", json!({"key": "k"}), "INPUT_PARSE", ".connector"),
            row("I183 (O6)", "cache_write", json!({"connector": "c", "value": 1}), "INPUT_PARSE", ".key"),
            row("I183 (O6)", "cache_write", json!({"connector": "c", "key": "k"}), "INPUT_PARSE", ".value"),
            row("I183 (O6)", "cache_delete", json!({"connector": "c"}), "INPUT_PARSE", ".keys"),
            row("I183 (O6)", "cache_incr", json!({"connector": "c"}), "INPUT_PARSE", ".key"),
            row("I183 (O6)", "cache_invalidate", json!({}), "INPUT_PARSE", ".namespaces"),
            row("I183 (O7)", "crypto", json!({"algorithm": "sha256", "data": "x"}), "INPUT_PARSE", ".op"),
            row("I183 (O7)", "jwt_sign", json!({"key": "k", "expires_in": "5m"}), "INPUT_PARSE", ".algorithm"),
            row("I183 (O7)", "jwt_sign", json!({"algorithm": "HS256", "expires_in": "5m"}), "INPUT_PARSE", ".key"),
            row("I183 (O7)", "jwt_verify", json!({"algorithms": ["HS256"], "jwks_url": "https://x"}), "INPUT_PARSE", ".token"),
            row("I183 (O7)", "jwt_verify", json!({"token": "t", "jwks_url": "https://x"}), "INPUT_PARSE", ".algorithms"),
            row("I183 (O10)", "channel_call", json!({}), "INPUT_PARSE", ".channel"),
            // O2: key material read by the engine's secret checks.
            row("O2, KeyMaterial", "crypto", hmac(json!({"key": {"secret": "nope"}})), "UNKNOWN_SECRET", ".key"),
            row("O2, KeyMaterialList", "jwt_verify", verify(json!({"issuer": {"secret": "nope"}})), "UNKNOWN_SECRET", ".issuer"),
            row("O2, a nested secret in a key-list object", "jwt_verify", verify(json!({"audience": [{"a": {"secret": "s"}}]})), "SECRET_IN_DOCUMENT", ".audience"),
            // O3.
            row("O3, an output that is not a path", "http_call", json!({"connector": "api", "output": "response"}), "INVALID_PATH", ".output"),
            // O4.
            row("O4, a non-string connector", "db_read", json!({"connector": 5, "query": "SELECT 1", "output": "data.r"}), "INPUT_PARSE", ".connector"),
            row("O4, a non-string query", "db_read", json!({"connector": "db", "query": 5, "output": "data.r"}), "INPUT_PARSE", ".query"),
            row("O4, a non-string query", "db_write", json!({"connector": "db", "query": 5}), "INPUT_PARSE", ".query"),
            row("O4, a secret in params", "db_read", json!({"connector": "db", "query": "q", "output": "data.r", "params": [{"secret": "s"}]}), "SECRET_IN_DOCUMENT", ".params"),
            row("O4, a secret in numeric_as", "db_read", json!({"connector": "db", "query": "q", "output": "data.r", "numeric_as": {"secret": "s"}}), "SECRET_IN_DOCUMENT", ".numeric_as"),
            row("O4, a secret in binary_as", "db_read", json!({"connector": "db", "query": "q", "output": "data.r", "binary_as": {"secret": "s"}}), "SECRET_IN_DOCUMENT", ".binary_as"),
            row("O4, a malformed var hole", "db_read", json!({"connector": "db", "query": "q", "output": "data.r", "params": [{"var": 5}]}), "INVALID_PATH", ".params"),
            row("O4, a malformed var hole", "db_write", json!({"connector": "db", "query": "q", "params": [{"var": []}]}), "INVALID_PATH", ".params"),
            row("O4, a malformed var hole", "db_write", json!({"connector": "db", "query": "q", "params": [{"var": ["a", 1, 2]}]}), "INVALID_PATH", ".params"),
            // A literal hole rooted at no context root; one rooted at `payload` is
            // a `PAYLOAD_VAR` Defect, which loads.
            row("O4, a malformed var hole", "db_write", json!({"connector": "db", "query": "q", "params": [{"var": "order.id"}]}), "INVALID_PATH", ".params"),
            // O6.
            row("O6, both key and keys", "cache_read", json!({"connector": "c", "key": "k", "keys": ["k"], "output": "data.r"}), "INPUT_PARSE", ".keys"),
            row("O6, neither key nor keys", "cache_read", json!({"connector": "c", "output": "data.r"}), "INPUT_PARSE", ".key"),
            row("O6, a non-string connector", "cache_write", json!({"connector": 5, "key": "k", "value": 1}), "INPUT_PARSE", ".connector"),
            row("O6, break 2a: cache_write's output", "cache_write", json!({"connector": "c", "key": "k", "value": 1, "output": "data.x"}), "UNKNOWN_KEY", ".output"),
            row("O6, break 2a", "cache_read", json!({"connector": "c", "key": "k", "output": "data.r", "tll": 5}), "UNKNOWN_KEY", ".tll"),
            row("O6, break 2a", "cache_delete", json!({"connector": "c", "keys": ["k"], "prefix": "x"}), "UNKNOWN_KEY", ".prefix"),
            row("O6, break 2a", "cache_incr", json!({"connector": "c", "key": "k", "step": 2}), "UNKNOWN_KEY", ".step"),
            row("O6, a secret in value", "cache_write", json!({"connector": "c", "key": "k", "value": {"a": {"secret": "s"}}}), "SECRET_IN_DOCUMENT", ".value"),
            row("O6, a malformed var hole in value", "cache_write", json!({"connector": "c", "key": "k", "value": {"a": {"var": 5}}}), "INVALID_PATH", ".value"),
            row("O6, a secret in ttl_secs", "cache_write", json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": {"secret": "s"}}), "SECRET_IN_DOCUMENT", ".ttl_secs"),
            row("O6, a malformed var hole in ttl_secs", "cache_write", json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": {"var": 5}}), "INVALID_PATH", ".ttl_secs"),
            // O7.
            row("O7, crypto: an unknown op", "crypto", json!({"op": "frobnicate"}), "INPUT_PARSE", ""),
            row("O7, crypto: the op x algorithm table", "crypto", hmac(json!({"algorithm": "md5"})), "INPUT_PARSE", ".algorithm"),
            row("O7, crypto: a field the op does not take", "crypto", json!({"op": "hash", "algorithm": "sha256", "data": "x", "key": "k"}), "INPUT_PARSE", ".key"),
            row("O7, crypto: an unknown encoding", "crypto", hmac(json!({"encoding": "base99"})), "INPUT_PARSE", ".encoding"),
            row("O7, crypto: a non-string op", "crypto", json!({"op": 5}), "INPUT_PARSE", ".op"),
            row("O7, jwt_sign: an unsupported algorithm", "jwt_sign", sign(json!({"algorithm": "none"})), "INPUT_PARSE", ".algorithm"),
            row("O7, jwt_sign: no expires_in and no exp", "jwt_sign", json!({"algorithm": "HS256", "key": "k", "output": "data.t"}), "INPUT_PARSE", ".expires_in"),
            row("O7, jwt_sign: an unparseable expires_in", "jwt_sign", sign(json!({"expires_in": "soon"})), "INPUT_PARSE", ".expires_in"),
            row("O7, jwt_verify: empty algorithms", "jwt_verify", verify(json!({"algorithms": []})), "INPUT_PARSE", ".algorithms"),
            row("O7, jwt_verify: neither keys nor jwks_url", "jwt_verify", json!({"token": "t", "algorithms": ["HS256"]}), "INPUT_PARSE", ": jwt_verify requires 'keys' and/or 'jwks_url'"),
            row("O7, jwt_verify: a jwks_url that is not HTTPS", "jwt_verify", json!({"token": "t", "algorithms": ["RS256"], "jwks_url": "http://x"}), "INPUT_PARSE", ".jwks_url"),
            row("O7, jwt_verify: a non-array algorithms", "jwt_verify", verify(json!({"algorithms": "HS256"})), "INPUT_PARSE", ".algorithms"),
            row("O7, jwt_verify: a non-boolean require_exp", "jwt_verify", verify(json!({"require_exp": "yes"})), "INPUT_PARSE", ".require_exp"),
            row("O7, jwt_verify: a non-string algorithms entry", "jwt_verify", verify(json!({"algorithms": [5]})), "INPUT_PARSE", ".algorithms"),
            row("O7, jwt_verify: a keys element that is not an object", "jwt_verify", verify(json!({"keys": ["k"]})), "INPUT_PARSE", ".keys[0]"),
            row("O7, jwt_verify: an undeclared secret in keys[].key", "jwt_verify", verify(json!({"keys": [{"algorithm": "HS256", "key": {"secret": "nope"}}]})), "UNKNOWN_SECRET", ".keys[0].key"),
            row("O7, jwt_verify: break 2a in a keys element", "jwt_verify", verify(json!({"keys": [{"algorithm": "HS256", "key": "k", "use": "sig"}]})), "UNKNOWN_KEY", ".keys[0].use"),
            row("O7, jwt_sign: a secret in claims", "jwt_sign", sign(json!({"claims": {"sub": {"secret": "s"}}})), "SECRET_IN_DOCUMENT", ".claims"),
            row("O7, jwt_sign: a malformed var hole in claims", "jwt_sign", sign(json!({"claims": {"sub": {"var": 5}}})), "INVALID_PATH", ".claims"),
            row("O7, jwt_sign: a secret in audience", "jwt_sign", sign(json!({"audience": {"secret": "s"}})), "SECRET_IN_DOCUMENT", ".audience"),
            row("O7, a secret in an output", "jwt_verify", verify(json!({"output": {"secret": "s"}})), "SECRET_IN_MESSAGE_WRITE", ".output"),
            // O10.
            row("O10, \"channel\": null", "channel_call", json!({"channel": null}), "INPUT_PARSE", ".channel"),
            row("O10, break 2a", "channel_call", json!({"channel": "x", "retries": 2}), "UNKNOWN_KEY", ".retries"),
            row("O10, an output of 7", "channel_call", json!({"channel": "x", "output": 7}), "INPUT_PARSE", ".output"),
            row("O10, an output of \"\"", "channel_call", json!({"channel": "x", "output": ""}), "INVALID_PATH", ".output"),
            row("O10, an output of \"result\"", "channel_call", json!({"channel": "x", "output": "result"}), "INVALID_PATH", ".output"),
            // I186: an alias beside its canonical key.
            row("I186, both spellings", "http_call", json!({"connector": "api", "output": "data.a", "response_path": "data.b"}), "UNKNOWN_KEY", ".response_path"),
            row("I186, both spellings", "channel_call", json!({"channel": "x", "channel_logic": "y"}), "UNKNOWN_KEY", ".channel_logic"),
            // Break 21 (I185): a result that is never an object, at a
            // literal root (the default `data` included).
            row("I185, db_read", "db_read", json!({"connector": "db", "query": "q"}), "INPUT_PARSE", ".output"),
            row("I185, data_query reading rows", "data_query", json!({"connector": "db", "query": {"source": "t"}}), "INPUT_PARSE", ".output"),
            row("I185, crypto", "crypto", hmac(json!({"output": "data"})), "INPUT_PARSE", ".output"),
            row("I185, jwt_sign", "jwt_sign", sign(json!({"output": "data"})), "INPUT_PARSE", ".output"),
            row("I185, cache_read with keys", "cache_read", json!({"connector": "c", "keys": ["k"]}), "INPUT_PARSE", ".output"),
            row("I185, cache_incr (O6)", "cache_incr", json!({"connector": "c", "key": "k", "output": "data"}), "INPUT_PARSE", ".output"),
            // Break 2: a near miss of a known key in a built-in's input.
            row("break 2", "map", json!({"mappings": [{"path": "data.x", "logic": 1}], "mapings": []}), "LIKELY_TYPO", ".mapings"),
        ];
    assert_quarantined(&engines, &rows);
}

// ---- one workflow, compiled as a generation compiles it ---------------

fn one_task_workflow(function: &str, input: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": "wf-1",
        "name": "wf-1",
        "channel": "c",
        "condition": true,
        "tasks": [{
            "id": "t1",
            "name": "t1",
            "function": { "name": function, "input": input }
        }],
    })
}

/// A task naming a function no registry holds is refused when the
/// workflow is compiled, naming the workflow and the task — an offline
/// run or `POST /definitions/test` cannot give a green answer for a
/// workflow boot would quarantine.
#[test]
fn single_engine_refuses_a_function_nothing_dispatches() {
    let engines = Engines::bare();
    let Err(err) = crate::offline::single_engine(
        &engines.compiler,
        &one_task_workflow("enrich", serde_json::json!({ "connector": "c" })),
    ) else {
        unreachable!("`enrich` has no function behind it");
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

/// The control: a self-contained built-in needs no host function, so the
/// same call builds.
#[test]
fn single_engine_builds_a_workflow_every_engine_can_run() {
    assert!(
        crate::offline::single_engine(
            &Engines::bare().compiler,
            &one_task_workflow("log", serde_json::json!({ "message": "x" })),
        )
        .is_ok(),
        "`log` is self-contained"
    );
}

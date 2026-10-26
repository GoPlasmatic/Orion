//! dataflow-rs 3.6: task groups and `terminal`

use super::*;

/// The guard clause the upgrade exists for: a terminal group ends the
/// workflow, so nothing after it needs a hand-written negation.
#[test]
fn a_terminal_task_group_ends_the_workflow() {
    let wf = write_temp(
        r#"{"name":"guard","condition":true,"tasks":[
            {"id":"seed","name":"Seed","function":{"name":"map","input":{"mappings":[
               {"path":"data.user","logic":null}]}}},
            {"id":"not_found","condition":{"==":[{"var":"data.user"},null]},
             "terminal":true,"tasks":[
               {"id":"body","name":"404","function":{"name":"map","input":{"mappings":[
                  {"path":"data.status","logic":404}]}}}]},
            {"id":"never","name":"Never","function":{"name":"map","input":{"mappings":[
               {"path":"data.reached","logic":true}]}}}]}"#,
        "group-terminal",
    );
    let input = write_temp("{}", "group-terminal-in");
    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input])
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");

    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    assert_eq!(parsed["data"]["status"], 404, "the group's task ran");
    assert!(
        parsed["data"].get("reached").is_none(),
        "a terminal group must end the workflow: {}",
        parsed["data"]
    );
    for p in [&wf, &input] {
        let _ = std::fs::remove_file(p);
    }
}

/// A task inside a group is a task. Every check that walks the array has to
/// see it, or a guarded half of a workflow ships unvalidated.
#[test]
fn tasks_inside_a_group_are_validated_with_nested_paths() {
    let wf = write_temp(
        r#"{"name":"w","tasks":[
            {"id":"g","condition":true,"tasks":[
               {"id":"bad","name":"bad","function":{"name":"data_writes","input":{}}}]}]}"#,
        "group-validate",
    );
    let out = Command::new(orion_bin())
        .args(["lint", &wf])
        .output()
        .expect("run lint");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("tasks[0].tasks[0].function.name"),
        "the path must address the nested task: {stderr}"
    );
    assert!(
        stderr.contains("did you mean 'data_write'?"),
        "and the suggestion must still fire inside a group: {stderr}"
    );
    let _ = std::fs::remove_file(&wf);
}

/// Closure checking reaches inside groups, or a connector referenced only
/// from a guard clause passes a set lint that exists to catch exactly that.
#[test]
fn closure_checking_reaches_inside_a_group() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"g","condition":true,"tasks":[
               {"id":"r","name":"r","function":{"name":"data_query","input":{
                  "connector":"nowhere","query":{"source":"c"},
                  "output":"temp_data.z"}}}]}]}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "{report}");
    assert!(report.contains("closure.connector"), "{report}");
    assert!(report.contains("nowhere"), "{report}");
}

/// Groups and tasks share one id namespace — the engine refuses a collision at
/// build, which for Orion is a failed reload rather than one bad workflow.
#[test]
fn a_group_id_colliding_with_a_task_id_is_refused() {
    let wf = write_temp(
        r#"{"name":"w","tasks":[
            {"id":"dup","name":"t","function":{"name":"map","input":{"mappings":[]}}},
            {"id":"dup","condition":true,"tasks":[
               {"id":"inner","name":"i","function":{"name":"map","input":{"mappings":[]}}}]}]}"#,
        "group-dup-id",
    );
    let out = Command::new(orion_bin())
        .args(["lint", &wf])
        .output()
        .expect("run lint");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("DUPLICATE_TASK_ID"), "{stderr}");
    let _ = std::fs::remove_file(&wf);
}

/// Every workflow written before 3.6 is a flat array, and none of them may
/// change behaviour.
#[test]
fn a_flat_workflow_is_unaffected_by_the_step_walk() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/packages");
    let (ok, report) = lint_dir(&dir, &[]);
    assert!(ok, "the shipped packages must still lint clean: {report}");
}

/// `env://` is the documented way to author a connector secret, so the
/// inventory line the set lint prints for one must not gate a pipeline.
/// Counting it as a warning left `--deny-warnings` failing on every real set,
/// which is the same as not shipping the flag.
#[test]
fn an_env_reference_is_reported_without_failing_deny_warnings() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn.json"),
        r#"{"name":"crm","connector_type":"http","config":{
             "url":"https://example.com",
             "headers":{"Authorization":"env://CRM_TOKEN"}}}"#,
    )
    .unwrap();

    let (ok, report) = lint_dir(dir, &[]);
    assert!(ok, "a clean set must lint clean: {report}");
    assert!(
        report.contains("CRM_TOKEN"),
        "the environment inventory is the point of the line: {report}"
    );
    assert!(
        report.contains("0 warning(s)"),
        "an inventory note is not a warning: {report}"
    );

    let (ok, report) = lint_dir(dir, &["--deny-warnings"]);
    assert!(
        ok,
        "--deny-warnings must not gate on an env:// inventory note: {report}"
    );
}

/// #338: a connector whose URL and private-address opt-out are references
/// lints clean, each reference inventoried; a reference inside a longer
/// string is a warning — it would be sent literally — and gates
/// `--deny-warnings`.
#[test]
fn a_referenced_url_lints_clean_and_an_embedded_reference_warns() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("peer.json"),
        r#"{"name":"peer-api","connector_type":"http","config":{
             "url":"env://PEER_API_URL",
             "allow_private_urls":"env://PEER_API_PRIVATE","timeout_ms":10000}}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &["--deny-warnings"]);
    assert!(ok, "{report}");
    assert!(
        report.contains("PEER_API_URL") && report.contains("PEER_API_PRIVATE"),
        "{report}"
    );

    std::fs::write(
        dir.join("crm.json"),
        r#"{"name":"crm","connector_type":"http","config":{
             "url":"https://example.com",
             "headers":{"Authorization":"Bearer env://CRM_TOKEN"}}}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(ok, "a warning alone does not fail: {report}");
    assert!(
        report.contains("[env.embedded_reference]")
            && report.contains("config.headers.Authorization")
            && report.contains("\"type\": \"bearer\""),
        "{report}"
    );
    let (ok, report) = lint_dir(dir, &["--deny-warnings"]);
    assert!(!ok, "{report}");
}

/// The other half of the deployment checklist: a `{"secret": …}` node names an
/// entry the serving instance must declare in `[secrets]`, and an instance
/// that lacks one quarantines the channel. A separate check id, because the
/// action it asks for is a config edit rather than an environment variable.
#[test]
fn a_declared_secret_is_inventoried_under_its_own_check_id() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"sign","name":"sign","condition":true,"tasks":[
             {"id":"s","name":"s","function":{"name":"crypto","input":{
               "op":"hmac","algorithm":"sha256","data":"x",
               "key":{"secret":"partner_hmac"},"output":"data.mac"}}}]}"#,
    )
    .unwrap();

    let (ok, report) = lint_dir(dir, &["--deny-warnings"]);
    assert!(ok, "an inventory note must not gate a pipeline: {report}");
    assert!(
        report.contains("[secrets.reference]") && report.contains("partner_hmac"),
        "the set's [secrets] requirement is the point of the line: {report}"
    );
    assert!(
        report.contains("0 warning(s)"),
        "an inventory note is not a warning: {report}"
    );
}

/// The inventory covers every scheme this build can resolve, not just
/// `env://` — a set authored against Vault would otherwise report clean and
/// then be missing a secret at deploy. And it uses the masking policy's strict
/// predicate, so a `postgres://user:password@host` connection string is a
/// connection string, not a secret reference.
#[test]
fn the_secret_inventory_covers_every_resolvable_scheme() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn.json"),
        r#"{"name":"db","connector_type":"db","config":{"driver":"postgresql",
             "connection_string":"postgres://user:password@host/db",
             "options":{"ca":"vault://secret/db#ca"}}}"#,
    )
    .unwrap();

    let (ok, report) = lint_dir(dir, &[]);
    assert!(ok, "{report}");
    assert!(
        report.contains("vault://secret/db#ca"),
        "a vault reference belongs in the inventory: {report}"
    );
    assert!(
        !report.contains("postgres://user:password@host"),
        "a connection string is not a secret reference: {report}"
    );
}

/// The lint projects a route the way the route table does, so the gate and the
/// thing being gated agree. Each of these disagreed while the lint compared
/// pattern strings and folded methods into an "ANY" sentinel of its own.
#[test]
fn route_collisions_are_judged_the_way_the_route_table_judges_them() {
    // Parameter *names* do not distinguish routes: `/o/{id}` and
    // `/o/{orderId}` are one entry in the table, so they collide.
    let scratch = defs_with_channels(&[
        r#"{"channel_id":"c0","name":"a","channel_type":"sync","protocol":"rest",
            "route_pattern":"/o/{id}","methods":["GET"],"workflow_id":"w"}"#,
        r#"{"channel_id":"c1","name":"b","channel_type":"sync","protocol":"rest",
            "route_pattern":"/o/{orderId}","methods":["GET"],"workflow_id":"w"}"#,
    ]);
    let (ok, report) = lint_dir(scratch.path(), &[]);
    assert!(
        !ok && report.contains("duplicate.route_pattern"),
        "differing param names are the same route: {report}"
    );

    // No `methods` means every method, so it overlaps one that names GET.
    let scratch = defs_with_channels(&[
        r#"{"channel_id":"c0","name":"a","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","workflow_id":"w"}"#,
        r#"{"channel_id":"c1","name":"b","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","methods":["GET"],"workflow_id":"w"}"#,
    ]);
    let (ok, report) = lint_dir(scratch.path(), &[]);
    assert!(
        !ok && report.contains("duplicate.route_pattern"),
        "an unrestricted channel claims every method: {report}"
    );

    // Different priorities are a deliberate override, which activation allows.
    let scratch = defs_with_channels(&[
        r#"{"channel_id":"c0","name":"a","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","methods":["GET"],"workflow_id":"w","priority":0}"#,
        r#"{"channel_id":"c1","name":"b","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","methods":["GET"],"workflow_id":"w","priority":10}"#,
    ]);
    let (ok, report) = lint_dir(scratch.path(), &[]);
    assert!(
        ok,
        "a higher-priority override activates fine and must lint clean: {report}"
    );

    // Disjoint methods on one pattern never match the same request.
    let scratch = defs_with_channels(&[
        r#"{"channel_id":"c0","name":"a","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","methods":["GET"],"workflow_id":"w"}"#,
        r#"{"channel_id":"c1","name":"b","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","methods":["POST"],"workflow_id":"w"}"#,
    ]);
    let (ok, report) = lint_dir(scratch.path(), &[]);
    assert!(ok, "disjoint methods do not collide: {report}");
}

/// #294: a fragment's ids are namespaced by the call site so it "cannot
/// collide with the including workflow, or with a second instance of itself".
/// That held only while every step was a plain task — a fragment holding a
/// **task group** had the group's own id rewritten and the ids inside it
/// emitted verbatim, into the host workflow's namespace.
///
/// Both of the issue's reproductions, run end to end. They failed with
/// `DUPLICATE_TASK_ID`, whose own message notes it "fails the entire engine
/// reload rather than just this workflow" — and the author could not see it
/// coming, because the colliding name is private to the fragment.
#[test]
fn a_fragments_nested_ids_are_namespaced_by_the_call_site() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("shared.json"),
        r#"{ "fragments": { "guard": { "tasks": [
             { "id": "probe", "name": "Probe", "function": { "name": "map",
                 "input": { "mappings": [ { "path": "temp_data.p", "logic": 1 } ] } } },
             { "id": "span", "condition": true, "tasks": [
                 { "id": "inner", "name": "Inner", "function": { "name": "map",
                     "input": { "mappings": [ { "path": "temp_data.i", "logic": 1 } ] } } } ] } ] } } }"#,
    )
    .unwrap();

    // (1) two instances of one fragment.
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "repro", "name": "Repro",
             "tasks": [ { "id": "a", "use": "guard" }, { "id": "b", "use": "guard" } ] }"#,
    )
    .unwrap();
    let (ok, report) = lint_with_definitions(dir);
    assert!(
        ok,
        "two instances of one fragment must not collide:\n{report}"
    );

    // (2) one instance, against a host task sharing a nested task's name.
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "repro", "name": "Repro",
             "tasks": [ { "id": "a", "use": "guard" },
                        { "id": "inner", "name": "The workflow's own task",
                          "function": { "name": "map", "input": { "mappings": [
                            { "path": "temp_data.own", "logic": 1 } ] } } } ] }"#,
    )
    .unwrap();
    let (ok, report) = lint_with_definitions(dir);
    assert!(
        ok,
        "a fragment's nested id must not collide with the host workflow's own:\n{report}"
    );

    // And the ids the engine actually runs are the namespaced ones — the
    // check the issue made against a live 1.2.0 with `expect_tasks`, which
    // reported `_session.call` namespaced beside a bare `_session_deny`.
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "repro", "name": "Repro",
             "tasks": [ { "id": "a", "use": "guard" }, { "id": "b", "use": "guard" } ] }"#,
    )
    .unwrap();
    let input = write_temp("{}", "i294");
    let out = Command::new(orion_bin())
        .args([
            "dry-run",
            "-w",
            dir.join("wf.json").to_str().unwrap(),
            "-i",
            &input,
            "--definitions",
            dir.to_str().unwrap(),
        ])
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    let ran: Vec<&str> = parsed["trace"]["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter_map(|s| s["task_id"].as_str())
        .collect();
    assert_eq!(ran, ["a.probe", "a.inner", "b.probe", "b.inner"]);
    let _ = std::fs::remove_file(&input);
}

/// #333: a fragment may use a fragment, inside a group as at its top level
/// (#294 was the same shape surviving expansion unrefused). The nested step
/// carries both call sites, and a single-file `lint` sees the whole thing
/// compiled. A cycle is named rather than expanded for ever.
#[test]
fn a_fragment_may_use_a_fragment_and_a_cycle_is_named() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("shared.json"),
        r#"{ "fragments": {
             "leaf": { "tasks": [ { "id": "l", "name": "Leaf", "function": { "name": "map",
                 "input": { "mappings": [ { "path": "temp_data.l", "logic": 1 } ] } } } ] },
             "outer": { "tasks": [
                 { "id": "span", "condition": true, "tasks": [
                     { "id": "nested", "use": "leaf" } ] } ] },
             "ping": { "tasks": [ { "id": "p", "use": "pong" } ] },
             "pong": { "tasks": [ { "id": "q", "use": "ping" } ] } } }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "nest", "name": "Nest", "tasks": [ { "id": "a", "use": "outer" } ] }"#,
    )
    .unwrap();
    let (ok, report) = lint_with_definitions(dir);
    assert!(ok, "{report}");

    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "nest", "name": "Nest", "tasks": [ { "id": "a", "use": "ping" } ] }"#,
    )
    .unwrap();
    let (ok, report) = lint_with_definitions(dir);
    assert!(!ok, "{report}");
    assert!(
        report.contains("shared.cycle") && report.contains("'ping' → 'pong' → 'ping'"),
        "{report}"
    );
}

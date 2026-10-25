use super::*;

// The tests below predate the registry parameter; these shadow the glob
// import with the built-in registry so each case reads as it did.
fn registry() -> &'static FunctionRegistry {
    FunctionRegistry::builtin()
}
fn validate_create_workflow(req: &CreateWorkflowRequest, cap: i64) -> Result<(), OrionError> {
    super::validate_create_workflow(req, cap, registry())
}
fn validate_update_workflow(req: &UpdateWorkflowRequest, cap: i64) -> Result<(), OrionError> {
    super::validate_update_workflow(req, None, cap, registry())
}
fn unresolvable_logic_warnings(tasks: &Value) -> Vec<(String, String)> {
    super::unresolvable_logic_warnings(tasks, None, registry())
}
fn secret_reference_errors(tasks: &Value) -> Vec<(String, String)> {
    super::secret_reference_errors(tasks, None, registry())
}

/// A plugin never sees key material, so a `{"secret": ..}` node is
/// refused in every one of its fields — and in the template field it is
/// the refusal that matters, because the engine would otherwise evaluate
/// the node straight into the guest's input. A built-in's literal field
/// stays silent for the same node: nothing there would resolve it.
#[test]
fn a_plugin_function_refuses_a_secret_node_in_every_field() {
    use crate::engine::functions::schema::{FieldKind, RetrySafety, Source, WriteShape};
    use crate::engine::{FieldSpec, FunctionEntry, PluginBinding};

    let field = |name: &str, template_at: &'static [&'static str], resolvable: bool| FieldSpec {
        name: name.to_string(),
        description: String::new(),
        kind: FieldKind::Any,
        required: false,
        resolvable,
        secret_at: &[],
        template_at,
    };
    let registry = FunctionRegistry::builtin()
        .with_entries(vec![FunctionEntry {
            name: "acme.codec.parse".to_string(),
            description: String::new(),
            category: "transform".to_string(),
            source: Source::Plugin,
            input_fields: Some(vec![
                field("template", &[""], false),
                field("folded", &[], true),
                field("literal", &[], false),
                field("output", &[], false),
            ]),
            writes: WriteShape::OutputPath { default_root: None },
            retry_safety: RetrySafety::Pure,
            deny_unknown: true,
            validate_static: None,
            connector: None,
            plugin: Some(PluginBinding {
                id: "acme.codec".to_string(),
                version: 1,
                digest: "sha256:00".to_string(),
                abi: "orion:plugin@1.0.0".to_string(),
            }),
        }])
        .expect("extends");

    let found = super::secret_reference_errors(
        &json!([{
            "id": "t", "name": "t",
            "function": {"name": "acme.codec.parse", "input": {
                "template": {"cat": ["k=", {"secret": "api_key"}]},
                "folded": {"secret": "api_key"},
                "literal": {"nested": [{"secret": "api_key"}]},
                "output": "data.out"
            }}
        }]),
        None,
        &registry,
    );
    let mut paths: Vec<&str> = found.iter().map(|(p, _)| p.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        [
            "tasks[0].function.input.folded",
            "tasks[0].function.input.literal.nested[0]",
            "tasks[0].function.input.template.cat[1]",
        ],
        "{found:?}"
    );
    assert!(
        found
            .iter()
            .all(|(_, m)| m.contains("never sees key material")),
        "{found:?}"
    );
}
use crate::validation::common::MAX_DESCRIPTION_LEN;
use serde_json::json;

/// A minimal valid pipeline, for the tests that are about *other* fields.
///
/// These used to pass `json!([])`, which the engine refuses — a workflow
/// with no tasks fails `Workflow::validate()` and so fails the whole
/// engine build. Create rejects it now, so a fixture standing in for
/// "valid tasks" has to actually be some.
fn one_task() -> serde_json::Value {
    json!([{
        "id": "t1", "name": "t1",
        "function": {"name": "map", "input": {"mappings": []}}
    }])
}

#[test]
fn test_validate_create_workflow_full() {
    let req = CreateWorkflowRequest {
        workflow_id: Some("my-workflow-1".to_string()),
        name: "Test Workflow".to_string(),
        description: Some("A test workflow".to_string()),
        priority: 10,
        condition: json!(true),
        tasks: one_task(),
        tags: vec!["tag1".to_string()],
        loop_config: None,
        continue_on_error: false,
    };
    assert!(validate_create_workflow(&req, 10_000).is_ok());
}

#[test]
fn test_validate_create_workflow_invalid_id() {
    let req = CreateWorkflowRequest {
        workflow_id: Some("bad id with spaces".to_string()),
        name: "Test Workflow".to_string(),
        description: None,
        priority: 0,
        condition: json!(true),
        tasks: one_task(),
        tags: vec![],
        loop_config: None,
        continue_on_error: false,
    };
    assert!(validate_create_workflow(&req, 10_000).is_err());
}

#[test]
fn test_validate_create_workflow_long_description() {
    let req = CreateWorkflowRequest {
        workflow_id: None,
        name: "Test Workflow".to_string(),
        description: Some("d".repeat(MAX_DESCRIPTION_LEN + 1)),
        priority: 0,
        condition: json!(true),
        tasks: one_task(),
        tags: vec![],
        loop_config: None,
        continue_on_error: false,
    };
    assert!(validate_create_workflow(&req, 10_000).is_err());
}

#[test]
fn test_validate_update_workflow_all_fields() {
    let req = UpdateWorkflowRequest {
        name: Some("Updated Name".to_string()),
        description: Some("Updated desc".to_string()),
        priority: Some(5),
        condition: None,
        tasks: None,
        tags: None,
        loop_config: None,
        continue_on_error: None,
    };
    assert!(validate_update_workflow(&req, 10_000).is_ok());
}

#[test]
fn test_validate_update_workflow_invalid_name() {
    let req = UpdateWorkflowRequest {
        name: Some("".to_string()),
        description: None,
        priority: None,
        condition: None,
        tasks: None,
        tags: None,
        loop_config: None,
        continue_on_error: None,
    };
    assert!(validate_update_workflow(&req, 10_000).is_err());
}

#[test]
fn test_validate_update_workflow_invalid_description() {
    let req = UpdateWorkflowRequest {
        name: None,
        description: Some("x".repeat(MAX_DESCRIPTION_LEN + 1)),
        priority: None,
        condition: None,
        tasks: None,
        tags: None,
        loop_config: None,
        continue_on_error: None,
    };
    assert!(validate_update_workflow(&req, 10_000).is_err());
}

/// One `log` step with this id.
fn log_step(id: &str) -> serde_json::Value {
    serde_json::json!({"id": id, "name": id, "function": {"name": "log", "input": {"message": "x"}}})
}

fn create_with_loop(tasks: Value, loop_config: Value) -> CreateWorkflowRequest {
    CreateWorkflowRequest {
        workflow_id: None,
        name: "Looping".to_string(),
        description: None,
        priority: 0,
        condition: serde_json::json!(true),
        tasks,
        tags: vec![],
        loop_config: Some(loop_config),
        continue_on_error: false,
    }
}

fn update_of(tasks: Option<Value>, loop_config: Option<Value>) -> UpdateWorkflowRequest {
    UpdateWorkflowRequest {
        name: None,
        description: None,
        priority: None,
        condition: None,
        tasks,
        tags: None,
        loop_config,
        continue_on_error: None,
    }
}

/// A stored draft carrying these halves.
fn stored_draft(tasks: &Value, loop_config: Option<&Value>) -> crate::storage::models::Workflow {
    crate::storage::models::Workflow {
        workflow_id: "wf".to_string(),
        version: 1,
        name: "Looping".to_string(),
        description: None,
        priority: 0,
        status: "draft".to_string(),
        rollout_percentage: 100,
        condition_json: "true".to_string(),
        tasks_json: tasks.to_string(),
        tags_json: "[]".to_string(),
        loop_json: loop_config.map(Value::to_string),
        continue_on_error: false,
        created_at: chrono::NaiveDateTime::default(),
        updated_at: chrono::NaiveDateTime::default(),
    }
}

fn details(err: OrionError) -> (String, Vec<String>) {
    match err {
        OrionError::Validation {
            message, details, ..
        } => (message, details.into_iter().map(|d| d.path).collect()),
        other => (other.to_string(), Vec::new()),
    }
}

/// `Engine::build` validates the loop with the body: a setup step shares
/// the body's step-id namespace. The create check handed the engine the
/// body alone, so this was a `201` that then failed every reload.
#[test]
fn a_setup_step_colliding_with_a_body_step_is_refused_at_create() {
    let req = create_with_loop(
        serde_json::json!([log_step("t1")]),
        serde_json::json!({"max": 2, "setup": [log_step("t1")]}),
    );
    let (_, paths) = details(validate_create_workflow(&req, 10_000).expect_err("must be refused"));
    assert!(!paths.is_empty(), "the collision must be reported");
}

/// A loop the engine refuses on its own account names the loop, not the
/// task list, in the summary.
#[test]
fn a_loop_as_that_is_not_a_plain_key_is_refused_as_a_loop_error() {
    let req = create_with_loop(
        serde_json::json!([log_step("t1")]),
        serde_json::json!({"max": 2, "over": {"var": "data.items"}, "as": "a..b"}),
    );
    let (message, paths) =
        details(validate_create_workflow(&req, 10_000).expect_err("must be refused"));
    assert_eq!(message, "Workflow loop is invalid", "paths: {paths:?}");
    assert!(paths.iter().all(|p| p.starts_with("loop")), "{paths:?}");
}

#[test]
fn a_loop_with_distinct_setup_ids_is_accepted() {
    let req = create_with_loop(
        serde_json::json!([log_step("t1")]),
        serde_json::json!({"max": 2, "setup": [log_step("s1")]}),
    );
    assert!(validate_create_workflow(&req, 10_000).is_ok());
}

/// An update carrying only `tasks` keeps the stored loop, so the new body
/// is checked against the stored setup steps.
#[test]
fn an_update_of_tasks_is_checked_against_the_stored_loop() {
    let loop_config = serde_json::json!({"max": 2, "setup": [log_step("s1")]});
    let stored = stored_draft(&serde_json::json!([log_step("t1")]), Some(&loop_config));
    let req = update_of(Some(serde_json::json!([log_step("s1")])), None);
    assert!(super::validate_update_workflow(&req, Some(&stored), 10_000, registry()).is_err());
    // Without the stored draft the request alone is consistent.
    assert!(validate_update_workflow(&req, 10_000).is_ok());
}

/// And the reverse: a loop-only update is checked against the stored body.
#[test]
fn an_update_of_the_loop_is_checked_against_the_stored_tasks() {
    let stored = stored_draft(&serde_json::json!([log_step("t1")]), None);
    let colliding = update_of(
        None,
        Some(serde_json::json!({"max": 2, "setup": [log_step("t1")]})),
    );
    assert!(
        super::validate_update_workflow(&colliding, Some(&stored), 10_000, registry()).is_err()
    );
    let distinct = update_of(
        None,
        Some(serde_json::json!({"max": 2, "setup": [log_step("s1")]})),
    );
    assert!(super::validate_update_workflow(&distinct, Some(&stored), 10_000, registry()).is_ok());
}

/// `loop: null` removes the loop, so a stored setup step can no longer
/// collide with anything.
#[test]
fn clearing_the_loop_drops_its_setup_steps_from_the_check() {
    let loop_config = serde_json::json!({"max": 2, "setup": [log_step("t1")]});
    let stored = stored_draft(&serde_json::json!([log_step("t0")]), Some(&loop_config));
    let req = update_of(Some(serde_json::json!([log_step("t1")])), Some(Value::Null));
    assert!(super::validate_update_workflow(&req, Some(&stored), 10_000, registry()).is_ok());
}

/// #351: neither `loop` nor `for_each` is parsed strictly, so a typo in
/// either was dropped and the workflow ran without it.
#[test]
fn unknown_loop_and_for_each_keys_are_refused_with_a_suggestion() {
    let req = create_with_loop(
        serde_json::json!([{
            "id": "fan", "name": "Fan",
            "for_each": {"over": [1, 2], "as": "it", "max_concurency": 8},
            "function": {"name": "crypto", "input": {
                "op": "hash", "data": {"var": "temp_data.it"}, "output": "temp_data.h"}}
        }]),
        serde_json::json!({"max": 2, "steup": []}),
    );
    let err = validate_create_workflow(&req, 10_000).expect_err("must be refused");
    let (_, paths) = details(err);
    assert_eq!(paths, vec!["loop.steup"], "the loop is checked first");

    let req = create_with_loop(req.tasks.clone(), serde_json::json!({"max": 2}));
    let err = validate_create_workflow(&req, 10_000).expect_err("must be refused");
    let details = match err {
        OrionError::Validation { details, .. } => details,
        _ => Vec::new(),
    };
    assert_eq!(details.len(), 1, "{details:?}");
    assert_eq!(details[0].path, "tasks[0].for_each.max_concurency");
    assert_eq!(details[0].code, "UNKNOWN_FIELD");
    assert!(
        details[0]
            .message
            .contains("Did you mean 'max_concurrency'?"),
        "{}",
        details[0].message
    );
}

/// Build a one-task workflow's `tasks` array around a `cache_write` whose
/// resolvable `value` is `value`.
fn cache_write_tasks(value: serde_json::Value) -> serde_json::Value {
    serde_json::json!([{
        "id": "w", "name": "Write",
        "function": {"name": "cache_write", "input": {
            "connector": "cache", "key": "k", "value": value
        }}
    }])
}

/// The bug the check exists for: an expression in a resolvable field is
/// stored as a literal object, with no error at write time.
#[test]
fn an_expression_in_a_resolvable_field_is_reported() {
    let warnings = unresolvable_logic_warnings(&cache_write_tasks(serde_json::json!({
        "id": {"var": "data.id"},
        "session": {"expiresAt": {"cat": ["2026-", {"var": "data.month"}]}}
    })));
    assert_eq!(warnings.len(), 1, "one finding: {warnings:?}");
    assert_eq!(
        warnings[0].0, "tasks[0].function.input.value.session.expiresAt",
        "the path must point at the node, not the field"
    );
    assert!(warnings[0].1.contains("'cat'"), "{}", warnings[0].1);
}

/// `val` is a documented operator one letter from the one that works, and
/// folds to nothing here — so it gets its own message naming the fix.
#[test]
fn a_val_node_is_reported_with_the_var_spelling() {
    let warnings = unresolvable_logic_warnings(&cache_write_tasks(serde_json::json!({
        "id": {"val": "data.id"}
    })));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].1.contains("{\"var\": ..}"),
        "the message must name the spelling that works: {}",
        warnings[0].1
    );
}

/// Nested inside a `$`-keyed wrapper, which is where this bug is most often
/// written: `{"$date": {"now": []}}` looks like it computes a time.
#[test]
fn an_expression_nested_under_a_dollar_wrapper_is_reported() {
    let warnings = unresolvable_logic_warnings(&cache_write_tasks(serde_json::json!({
        "createdAt": {"$date": {"now": []}}
    })));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].0.ends_with("value.createdAt.$date"),
        "path: {}",
        warnings[0].0
    );
}

/// The false positives that make this a warning rather than an error must
/// at least not be *reported*. Operator names overlap real field names, and
/// `$`-keyed operators are not JSONLogic at all.
#[test]
fn data_that_merely_looks_like_an_operator_is_not_reported() {
    let clean = unresolvable_logic_warnings(&cache_write_tasks(serde_json::json!({
        // `$set`/`$and`/`$lt` are not JSONLogic, `{"var": ..}` is folded, and
        // `{"length": 120}` is a plain field whose argument is not an array.
        "filter": {"$and": [{"_id": {"var": "data.id"}}, {"n": {"$lt": 5}}]},
        "update": {"$set": {"video": {"length": 120}, "type": "clip"}}
    })));
    assert!(clean.is_empty(), "no findings expected, got {clean:?}");
}

/// An operator inside an *array* element of a resolvable field is reported
/// like one nested in an object.
///
/// `db_read`'s `params` is the shape this matters for and the one an author
/// actually writes: the handler folds `{"var": ..}` and nothing else, so an
/// `or` computing a default reaches the driver as a literal object and
/// binds as one — a `jsonb` document on a JSON column, a type error on any
/// other, and never the value that was meant. The walk descends into arrays
/// for exactly this; without that it would report the same mistake in a
/// `cache_write` `value` and miss it here.
#[test]
fn an_operator_in_an_array_element_of_a_resolvable_field_is_reported() {
    let warnings = unresolvable_logic_warnings(&serde_json::json!([{
        "id": "q", "name": "Query",
        "function": {"name": "db_read", "input": {
            "connector": "orders",
            "query": "SELECT id FROM orders LIMIT $1",
            "params": [{"or": [{"var": "data.req.limit"}, 50]}]
        }}
    }]));
    assert_eq!(warnings.len(), 1, "got {warnings:?}");
    assert_eq!(warnings[0].0, "tasks[0].function.input.params[0]");
    assert!(warnings[0].1.contains("'or'"), "{}", warnings[0].1);
    assert!(warnings[0].1.contains("'map' task"), "{}", warnings[0].1);
}

/// The literal a JSON column legitimately binds is not an expression, and
/// must not be reported as one: post-1.7 a `json`/`jsonb` parameter takes
/// the document itself, so an object in `params` is a real value. Only a
/// single-key object whose key is a known operator *and* whose argument is
/// an array is reported, which is what keeps an ordinary document out.
#[test]
fn a_json_document_bound_as_a_parameter_is_not_reported() {
    let clean = unresolvable_logic_warnings(&serde_json::json!([{
        "id": "q", "name": "Query",
        "function": {"name": "db_read", "input": {
            "connector": "orders",
            "query": "SELECT id FROM orders WHERE meta @> $1",
            "params": [{"tier": "gold", "length": 120}]
        }}
    }]));
    assert!(clean.is_empty(), "got {clean:?}");
}

/// A non-resolvable field is not scanned: the handler never folds it, so
/// whatever it holds is a literal by design.
#[test]
fn a_non_resolvable_field_is_not_scanned() {
    let clean = unresolvable_logic_warnings(&serde_json::json!([{
        "id": "q", "name": "Query",
        "function": {"name": "db_read", "input": {
            "connector": "orders",
            // `sql` is literal text, not a resolvable field.
            "sql": "SELECT 1 WHERE cat = 'if'"
        }}
    }]));
    assert!(clean.is_empty(), "got {clean:?}");
}

#[test]
fn test_validate_workflow_id() {
    assert!(validate_workflow_id("my-workflow-1").is_ok());
    assert!(validate_workflow_id("bad id!").is_err());
}

/// The motivating case: a reference in a field nothing resolves. Left
/// alone it is requested as a URL spelled `env://API_BASE`.
#[test]
fn a_reference_outside_a_secret_field_is_reported() {
    let found = secret_reference_errors(&serde_json::json!([{
        "id": "call", "name": "Call",
        "function": {"name": "http_call", "input": {
            "connector": "crm",
            "path": "env://API_BASE"
        }}
    }]));
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(found[0].0, "tasks[0].function.input.path");
    assert!(found[0].1.contains("env://API_BASE"), "{}", found[0].1);
}

/// §3.3: a `{"secret": …}` node in a field that does not read key material
/// is refused at authoring time, exactly as `env://` is.
///
/// Left alone it is not a no-op: the handler stores the node and sends it
/// on as an object, so a query gets `{"secret":"api_key"}` as a bind
/// parameter. The author asked for a credential and the database received
/// a JSON object naming one.
#[test]
fn a_secret_node_outside_a_key_field_is_reported() {
    let found = secret_reference_errors(&serde_json::json!([{
        "id": "t1",
        "function": {"name": "db_read", "input": {
            "connector": "orders",
            "query": "SELECT 1 WHERE token = $1",
            "params": [{"secret": "api_key"}],
        }},
    }]));
    assert_eq!(
        found.len(),
        1,
        "expected exactly one finding, got {found:?}"
    );
    assert!(found[0].0.contains("params"), "{:?}", found[0]);
    assert!(found[0].1.contains("api_key"), "{:?}", found[0]);
}

/// The array spelling resolves identically at runtime, so it must be
/// refused identically — recognising only the string form would leave one
/// of the two reaching a backend as an object.
#[test]
fn the_array_spelling_of_a_secret_node_is_reported_too() {
    let found = secret_reference_errors(&serde_json::json!([{
        "id": "t1",
        "function": {"name": "db_read", "input": {
            "connector": "pg",
            "query": "SELECT 1",
            "params": [{"secret": ["api_key"]}],
        }},
    }]));
    assert_eq!(
        found.len(),
        1,
        "expected exactly one finding, got {found:?}"
    );
}

/// And silent in a field the engine *evaluates*, where the node resolves.
///
/// `secret` is a reserved JSONLogic operator registered on every engine, so
/// in a `template_at` field — an `http_call` header —
/// `{"secret": …}` produces the value rather than surviving as an object.
/// This rule exists for the fields that fold `{"var": …}` and nothing else,
/// where it would not; firing here would refuse a task that works.
#[test]
fn a_secret_node_in_an_evaluated_field_is_not_a_finding() {
    let found = secret_reference_errors(&serde_json::json!([{
        "id": "t1",
        "function": {"name": "http_call", "input": {
            "connector": "api",
            "headers": {"X-Token": {"cat": ["token=", {"secret": "api_key"}]}},
        }},
    }]));
    assert!(found.is_empty(), "{found:?}");
}

/// And silent where the handler does read key material — the whole point
/// of the per-path exemption. `crypto.key` takes a secret node; a check
/// that fired here would refuse the recommended way to write the task.
#[test]
fn a_secret_node_in_a_key_field_is_left_alone() {
    let clean = secret_reference_errors(&serde_json::json!([{
        "id": "t1",
        "function": {"name": "crypto", "input": {
            "operation": "hmac_sha256",
            "data": "payload",
            "key": {"secret": "signing_key"},
        }},
    }]));
    assert!(clean.is_empty(), "{clean:?}");
}

/// The exemption is per path, not per field: `jwt_verify.keys` reads key
/// material at `keys[].key` and nowhere else, so a secret node in a
/// sibling `kid` — matched verbatim against the token's — is still
/// reported, and the `key` beside it still is not.
#[test]
fn the_secret_node_exemption_follows_the_path_not_the_field() {
    let found = secret_reference_errors(&serde_json::json!([{
        "id": "t1",
        "function": {"name": "jwt_verify", "input": {
            "token": "t",
            "keys": [{"kid": {"secret": "which_key"}, "key": {"secret": "signing_key"}}],
        }},
    }]));
    assert_eq!(
        found.len(),
        1,
        "expected only the `kid` finding, got {found:?}"
    );
    assert!(found[0].0.contains("kid"), "{:?}", found[0]);
}

/// A column named `secret` is not a secret node.
///
/// `data_query`'s `sort` is a list of single-key objects keyed by column
/// name, so sorting on a column called `secret` authors as
/// `{"secret": "asc"}` — structurally identical to the reference, and
/// nothing of the kind. `query` is neither `resolvable` nor
/// `secret_at`-bearing, so the handler never looks inside it for nodes and
/// neither does this check.
///
/// The shape is what makes the `inspects_nodes` qualifier necessary rather
/// than tidy: without it this is a hard 400 on a legitimate query, and the
/// same false positive reaches any keyed-object field whose keys are the
/// author's own names.
#[test]
fn a_column_named_secret_is_not_a_secret_node() {
    let clean = secret_reference_errors(&serde_json::json!([{
        "id": "t1",
        "function": {"name": "data_query", "input": {
            "connector": "orders",
            "schema": {"entities": {"items": {
                "physical": "items",
                "columns": {"id": {"queryable": true}, "secret": {"queryable": false}},
            }}},
            "query": {"source": "items", "sort": [{"secret": "asc"}]},
            "output": "data.result",
        }},
    }]));
    assert!(clean.is_empty(), "{clean:?}");
}

/// The same node in the *resolvable* field of the same function is still
/// reported — so the rule above narrows where the check speaks, and does
/// not soften what it says where it does.
#[test]
fn a_secret_node_in_data_querys_resolvable_params_is_still_reported() {
    let found = secret_reference_errors(&serde_json::json!([{
        "id": "t1",
        "function": {"name": "data_query", "input": {
            "connector": "orders",
            "query": {"source": "items"},
            "params": {"token": {"secret": "api_key"}},
            "output": "data.result",
        }},
    }]));
    assert_eq!(
        found.len(),
        1,
        "expected exactly one finding, got {found:?}"
    );
    assert!(found[0].0.contains("params"), "{:?}", found[0]);
}

/// An object that merely has a `secret` member among others is data, not a
/// secret node — the engine would not resolve it, so neither does this.
#[test]
fn a_multi_key_object_holding_a_secret_member_is_data() {
    let clean = secret_reference_errors(&serde_json::json!([{
        "id": "t1",
        "function": {"name": "db_read", "input": {
            "connector": "orders",
            "query": "SELECT 1",
            "params": [{"secret": "a", "public": "b"}],
        }},
    }]));
    assert!(clean.is_empty(), "{clean:?}");
}

/// The five fields that do resolve one must stay clean, including the
/// reference two levels down inside `jwt_verify.keys`.
#[test]
fn a_reference_in_a_secret_field_is_left_alone() {
    let clean = secret_reference_errors(&serde_json::json!([
        {
            "id": "mac", "name": "MAC",
            "function": {"name": "crypto", "input": {
                "op": "hmac", "key": "env://PARTNER_KEY", "data": {"var": "data.body"}
            }}
        },
        {
            "id": "jwt", "name": "Verify",
            "function": {"name": "jwt_verify", "input": {
                "token": {"var": "data.token"},
                "keys": [{"algorithm": "HS256", "key": "env://JWT_SECRET"}],
                "audience": "env://OAUTH_CLIENT_ID"
            }}
        }
    ]));
    assert!(clean.is_empty(), "got {clean:?}");
}

/// The exemption is per path, not per field. `jwt_verify.keys` resolves a
/// reference at each entry's `key` and nowhere else — `kid` is matched
/// verbatim against the token's, so a reference there never matches any
/// token and nothing says why.
#[test]
fn a_reference_in_a_sibling_of_a_key_is_still_reported() {
    let found = secret_reference_errors(&serde_json::json!([{
        "id": "jwt", "name": "Verify",
        "function": {"name": "jwt_verify", "input": {
            "token": {"var": "data.token"},
            "keys": [{
                "algorithm": "HS256",
                "key": {"secret": "partner_hmac"},
                "kid": "env://PARTNER_KID",
                "key_encoding": "utf8"
            }]
        }}
    }]));
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(found[0].0, "tasks[0].function.input.keys[0].kid");
}

/// A task group is where a stray reference is least likely to be caught by
/// eye, so the walk has to descend into one.
#[test]
fn a_reference_inside_a_task_group_is_reported() {
    let found = secret_reference_errors(&serde_json::json!([{
        "id": "guarded", "name": "Guarded",
        "condition": {"var": "data.ok"},
        "tasks": [{
            "id": "send", "name": "Send",
            "function": {"name": "http_call", "input": {
                "connector": "api", "method": "POST",
                "body": "vault://secret/data/x#y"
            }}
        }]
    }]));
    assert_eq!(found.len(), 1, "got {found:?}");
    assert!(
        found[0].0.ends_with("function.input.body"),
        "{}",
        found[0].0
    );
}

/// An ordinary connection string is not a secret reference — the check
/// asks the resolver registry, not "does it contain `://`".
#[test]
fn an_ordinary_url_is_not_a_reference() {
    let clean = secret_reference_errors(&serde_json::json!([{
        "id": "call", "name": "Call",
        "function": {"name": "http_call", "input": {
            "connector": "crm", "path": "https://example.com/v1/orders"
        }}
    }]));
    assert!(clean.is_empty(), "got {clean:?}");
}

#[cfg(test)]
mod engine_advisory_tests {
    fn engine_advisories(tasks: &serde_json::Value) -> Vec<super::super::EngineAdvisory> {
        super::super::engine_advisories(tasks, None, crate::engine::FunctionRegistry::builtin())
    }
    use serde_json::json;

    /// #308's shape. A failing rule records 400, the executor's 4xx branch
    /// carries on, and `continue_on_error` governs 5xx and Err only — so the
    /// check reads correct and changes nothing. The engine says so from 3.10;
    /// this asserts the warning reaches an Orion surface rather than only a
    /// server log.
    #[test]
    fn an_unguarded_validation_is_reported() {
        let tasks = json!([
            { "id": "check", "name": "Check", "function": { "name": "validation", "input": {
                "rules": [{ "logic": { "==": [1, 2] }, "message": "no" }] } } },
            { "id": "respond", "name": "Respond", "function": { "name": "map", "input": {
                "mappings": [{ "path": "data.x", "logic": true }] } } }
        ]);
        let found = engine_advisories(&tasks);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].check, "engine.unguarded_validation");
        assert!(
            found[0].message.contains("on_rejection"),
            "{}",
            found[0].message
        );
        assert_eq!(found[0].path, "task 'check'.on_rejection");
    }

    /// The fix silences it, which is the half that keeps the advisory from
    /// being noise on every workflow that validates anything.
    #[test]
    fn on_rejection_halt_silences_it() {
        let tasks = json!([
            { "id": "check", "name": "Check", "on_rejection": "halt",
              "function": { "name": "validation", "input": {
                "rules": [{ "logic": { "==": [1, 2] }, "message": "no" }] } } },
            { "id": "respond", "name": "Respond", "function": { "name": "map", "input": {
                "mappings": [{ "path": "data.x", "logic": true }] } } }
        ]);
        assert!(engine_advisories(&tasks).is_empty());
    }

    /// So does gating what follows, which is the older spelling and still
    /// correct — the advisory is about the *shape*, not about the keyword.
    #[test]
    fn a_guarded_successor_silences_it() {
        let tasks = json!([
            { "id": "check", "name": "Check", "function": { "name": "validation", "input": {
                "rules": [{ "logic": { "==": [1, 2] }, "message": "no" }] } } },
            { "id": "respond", "name": "Respond",
              "condition": { "==": [{ "var": "metadata.progress.status_code" }, 200] },
              "function": { "name": "map", "input": {
                "mappings": [{ "path": "data.x", "logic": true }] } } }
        ]);
        assert!(engine_advisories(&tasks).is_empty());
    }

    /// A workflow with neither shape reports nothing, so the two above are not
    /// simply reporting everything.
    #[test]
    fn an_ordinary_workflow_is_quiet() {
        let tasks = json!([
            { "id": "t", "name": "T", "function": { "name": "log", "input": {
                "message": "x" } } }
        ]);
        assert!(engine_advisories(&tasks).is_empty());
    }
}

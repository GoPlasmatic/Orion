use super::*;

/// §3.3: `FieldSchema::resolvable` is now the *only* declaration of which
/// input fields fold `{"var": ..}` against the message.
///
/// Four surfaces read it, and before this they could each be right about a
/// different answer: the connector handlers decided per call site by
/// calling a resolve helper or not,
/// `validation::unresolvable_logic_warnings` warns about an expression in
/// a field it believes literal, `stub.rs` folds the declared set when
/// `dry-run` executes offline, and `analysis::operators` decides what a
/// clippy rule can see through. Every resolve helper in
/// `connector_helpers` now gates on this table, so the handler cannot be
/// the one that disagrees.
#[test]
fn the_table_is_what_decides_whether_a_field_folds() {
    // Two fields of the same function, differing only in this flag.
    assert!(
        is_resolvable_field("db_read", "params"),
        "bind parameters are the request-controlled half of a statement"
    );
    assert!(
        !is_resolvable_field("db_read", "query"),
        "the SQL text is literal by design — it is what makes `params` the \
         *only* request-controlled part of the statement"
    );
    assert!(
        !is_resolvable_field("db_read", "connector"),
        "a connector name must not be chosen by the message"
    );

    // An unknown function declares nothing, so nothing folds — treating it
    // as permissive would make the gate vacuous exactly where it cannot
    // see.
    assert!(!is_resolvable_field("no_such_function", "params"));
}

/// The same non-resolvable string field is refused at authoring time, so
/// the runtime gate is defence in depth rather than the only guard: a
/// `{"var": ..}` node is an object, and `query` is declared a `String`.
#[test]
fn an_expression_in_a_literal_field_is_refused_at_create_time() {
    let errors = validate_input(
        "db_read",
        &serde_json::json!({
            "connector": "orders",
            "query": {"var": "data.req.sql"},
        }),
        "tasks[0]",
    );
    assert!(
        errors.iter().any(|e| e.path.contains("query")),
        "a message-derived `query` must be refused at authoring time: {errors:?}"
    );
}

/// And the resolvable twin is accepted in the same position, so the test
/// above is about the flag and not about objects being refused generally.
#[test]
fn an_expression_in_a_resolvable_field_is_accepted_at_create_time() {
    let errors = validate_input(
        "db_read",
        &serde_json::json!({
            "connector": "orders",
            "query": "SELECT 1 WHERE id = $1",
            "params": [{"var": "data.req.id"}],
        }),
        "tasks[0]",
    );
    assert!(errors.is_empty(), "{errors:?}");
}

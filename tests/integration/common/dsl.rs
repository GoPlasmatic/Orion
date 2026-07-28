//! Shared mini-DSL for the portable data-plane tests (`data_query` /
//! `data_write` / connector operation gates). One canonical copy of the task
//! builders and rejection predicates that the data-plane test files drive
//! their workflows with.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{body_json, json_request};

/// A raw `db_write` task (used for DDL, which is outside the portable dialect).
pub fn ddl(conn: &str, id: &str, sql: &str) -> Value {
    json!({
        "id": id, "name": id,
        "function": { "name": "db_write", "input": { "connector": conn, "query": sql, "output": "data.ddl" } }
    })
}

/// A `data_write` task; `input` carries the envelope (op/target/values/…). The
/// connector is filled in, and the result path defaults to `data.w` when the
/// envelope names none.
pub fn dw(conn: &str, id: &str, mut input: Value) -> Value {
    input["connector"] = json!(conn);
    if input.get("output").is_none() {
        input["output"] = json!("data.w");
    }
    json!({ "id": id, "name": id, "function": { "name": "data_write", "input": input } })
}

/// A `data_query` read-back task writing rows to `data.result`.
pub fn dq(conn: &str, id: &str, query: Value) -> Value {
    json!({
        "id": id, "name": id,
        "function": { "name": "data_query", "input": { "connector": conn, "query": query, "output": "data.result" } }
    })
}

/// POST a JSON payload to a data channel and return `(status, body)`.
pub async fn post(app: &axum::Router, channel: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{channel}"),
            Some(body),
        ))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

/// Whether a response body signals a task rejection: a non-empty task
/// `errors` array or the `error` envelope. Deliberately NOT any bare 5xx —
/// an intended refusal always carries one of the explicit signals, and a
/// server error without either is a crash that must fail the test, not
/// satisfy it.
pub fn is_rejection(_status: StatusCode, body: &Value) -> bool {
    let errors = body
        .get("errors")
        .and_then(|e| e.as_array())
        .is_some_and(|a| !a.is_empty());
    let error = body.get("error").is_some_and(|e| e.get("code").is_some());
    errors || error
}

/// [`is_rejection`] restricted to a validation-class refusal.
///
/// This deliberately does **not** assert the connector's name appears in the
/// body: operation-gate messages name the connector, and the data plane is
/// anonymous, so that detail is redacted before it reaches the caller and kept
/// in the log and the trace instead (proposal G3). Asserting on the leaked
/// text is what pinned the leak as expected behaviour.
pub fn is_gate_rejection(status: StatusCode, body: &Value) -> bool {
    if !is_rejection(status, body) {
        return false;
    }
    let code_of = |v: &Value| {
        v.get("code")
            .and_then(|c| c.as_str())
            .map(str::to_string)
            .unwrap_or_default()
    };
    let envelope_is_validation = body
        .get("error")
        .map(|e| code_of(e) == "VALIDATION_ERROR")
        .unwrap_or(false);
    let task_error_is_validation = body
        .get("errors")
        .and_then(|e| e.as_array())
        .is_some_and(|a| a.iter().any(|e| code_of(e).contains("Validation")));
    envelope_is_validation || task_error_is_validation
}

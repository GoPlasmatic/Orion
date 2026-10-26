//! The error envelope's published names, and the one response shape still
//! built with `json!`.

use serde_json::Value;

/// Error response body matching Orion's
/// `{"error": {"code": "...", "message": "...", "request_id": "..."}}` format.
///
/// Every 4xx/5xx from `OrionError` — and the 500 emitted by the panic-recovery
/// layer — uses this envelope.
///
/// These are the shared contract types, not mirrors of them. Three hand-written
/// structs used to stand in here and had already drifted: they published
/// `details` as a nullable array, while `ErrorBody` omits the key when empty and
/// never sends null. `schema(as = …)` on the orion-api types keeps the component
/// names the document has always used, since renaming one breaks clients
/// generated against it.
pub(crate) use orion_api::{
    ErrorBody as ErrorDetail, ErrorEnvelope as ErrorResponse, FieldError as ErrorFieldDetail,
};

/// `POST /api/v2/admin/definitions/test` — one run of a workflow definition.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct WorkflowTestResult {
    /// Whether the workflow's condition matched the supplied input.
    matched: bool,
    /// The run's trace: the context it started from (`initial`), then each
    /// step with its `task_id`, `result` (`executed` | `skipped` | `failed`)
    /// and the changes it made.
    trace: Value,
    /// The message data after the pipeline ran.
    output: Value,
    errors: Vec<Value>,
    /// Why the run stopped short, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

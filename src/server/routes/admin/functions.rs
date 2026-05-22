//! `GET /api/v1/admin/functions` — surfaces the per-function input schemas
//! registered in `crate::engine::functions::schema`. Powers tooling (CLI,
//! IDEs, docs) that needs to know what each workflow function accepts.

use axum::Json;
use serde_json::{Value, json};

use crate::errors::OrionError;

#[utoipa::path(
    get,
    path = "/api/v1/admin/functions",
    tag = "Functions",
    responses(
        (status = 200, description = "Registered workflow function schemas")
    )
)]
#[tracing::instrument]
pub(crate) async fn list_functions() -> Result<Json<Value>, OrionError> {
    let schemas = crate::engine::functions::schema::registry();
    Ok(Json(json!({ "data": schemas })))
}

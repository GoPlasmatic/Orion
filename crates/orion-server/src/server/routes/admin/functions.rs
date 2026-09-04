//! `GET /api/v1/admin/functions` — the catalogue of every function a workflow
//! may name, with the input schema of each one that declares one. Powers
//! tooling (CLI, IDEs, docs) that needs to know what exists and what it
//! accepts.
//!
//! It used to serve the schema registry directly, which listed only the
//! functions Orion input-validates — 18 of the 27 valid names, omitting `map`,
//! `filter`, `parse_json` and the rest (#288). Those are the most-used ones, so
//! a completion source built on this offered the connector functions and none
//! of the ones people type. Engine built-ins now appear with `source: "engine"`
//! and no `input_fields`.

use axum::Json;
use axum::extract::State;
use serde_json::Value;

use crate::errors::OrionError;
use crate::server::routes::openapi::{DataEnvelope, FunctionSchemaItem};
use crate::server::routes::response_helpers::data_response;
use crate::server::state::AppState;

#[utoipa::path(
    get,
    path = "/api/v1/admin/functions",
    tag = "Functions",
    responses(
        (status = 200, description = "Registered workflow function schemas", body = DataEnvelope<Vec<FunctionSchemaItem>>)
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_functions(
    State(state): State<AppState>,
) -> Result<Json<Value>, OrionError> {
    // The serving generation's registry: what this node will actually
    // dispatch, plugin entries included once a generation carries them.
    Ok(data_response(state.runtime.load().functions.catalogue()))
}

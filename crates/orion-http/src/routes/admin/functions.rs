//! `GET /api/v2/admin/functions` — the catalogue of every function a workflow
//! may name, with the input schema of each one that declares one. Powers
//! tooling (CLI, IDEs, docs) that needs to know what exists and what it
//! accepts.
//!
//! It lists every valid name, not only the functions Orion input-validates
//! (#288): the engine built-ins — `map`, `filter`, `parse_json` and the rest —
//! are the most-used ones, and a completion source without them would offer
//! the connector functions and none of the ones people type. Engine built-ins
//! appear with `source: "engine"` and no `input_fields`.

use axum::Json;
use axum::extract::State;

use crate::engine::functions::registry::CatalogueEntry;
use crate::errors::OrionError;
use crate::routes::response_helpers::data_response;
use crate::runtime::state::AppState;
use orion_api::Data;

#[utoipa::path(
    get,
    path = "/api/v2/admin/functions",
    tag = "Functions",
    responses(
        (status = 200, description = "Registered workflow function schemas", body = Data<Vec<CatalogueEntry>>)
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_functions(
    State(state): State<AppState>,
) -> Result<Json<Data<Vec<CatalogueEntry>>>, OrionError> {
    // The serving generation's registry: what this node will actually
    // dispatch, plugin entries included once a generation carries them.
    Ok(data_response(state.runtime.load().functions.catalogue()))
}

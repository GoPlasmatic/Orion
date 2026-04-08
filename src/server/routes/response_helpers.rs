use axum::Json;
use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};

/// Wraps a serialisable item in `{"data": ...}` and returns a 200 JSON response.
pub fn data_response<T: Serialize>(item: T) -> Json<Value> {
    Json(json!({ "data": item }))
}

/// Wraps a serialisable item in `{"data": ...}` and returns a 201 JSON response.
pub fn created_response<T: Serialize>(item: T) -> (StatusCode, Json<Value>) {
    (StatusCode::CREATED, Json(json!({ "data": item })))
}

/// Returns a paginated JSON response with `data`, `total`, `limit`, and `offset`.
pub fn paginated_response<T: Serialize>(
    data: Vec<T>,
    total: i64,
    limit: i64,
    offset: i64,
) -> Json<Value> {
    Json(json!({
        "data": data,
        "total": total,
        "limit": limit,
        "offset": offset,
    }))
}

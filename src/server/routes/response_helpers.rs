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

/// Map a `PaginatedResult<T>` from the repository into a JSON response by
/// converting each row via `map_fn`. Replaces the recurring 5-line block
/// `let data: Vec<R> = result.data.iter().map(...).collect::<Result<_, _>>()?;
///  Ok(paginated_response(data, result.total, result.limit, result.offset))`
/// in admin list handlers.
pub fn paginated_into<T, R, E>(
    result: crate::storage::repositories::helpers::PaginatedResult<T>,
    map_fn: impl Fn(&T) -> Result<R, E>,
) -> Result<Json<Value>, E>
where
    R: Serialize,
{
    let data: Vec<R> = result.data.iter().map(map_fn).collect::<Result<_, _>>()?;
    Ok(paginated_response(
        data,
        result.total,
        result.limit,
        result.offset,
    ))
}

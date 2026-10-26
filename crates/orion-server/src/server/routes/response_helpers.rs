//! The 2xx envelopes, as the typed values `orion_api::envelope` defines: a
//! handler returns `Json<Data<T>>` or `Json<Paginated<T>>`, which is what the
//! OpenAPI document publishes and what a client reads back.

use axum::Json;
use orion_api::{Data, Paginated};

/// `{"data": item}`.
pub fn data_response<T>(item: T) -> Json<Data<T>> {
    Json(Data { data: item })
}

/// `{"data": [...], "total", "limit", "offset"}`.
pub fn paginated_response<T>(
    data: Vec<T>,
    total: i64,
    limit: i64,
    offset: i64,
) -> Json<Paginated<T>> {
    Json(Paginated {
        data,
        total,
        limit,
        offset,
    })
}

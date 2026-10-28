//! The 2xx envelopes, as the typed values `orion_api::envelope` defines: a
//! handler returns `Json<Data<T>>` or `Json<Page<T>>`, which is what the
//! OpenAPI document publishes and what a client reads back.

use axum::Json;
use orion_api::{Data, Page};

/// `{"data": item}`.
pub fn data_response<T>(item: T) -> Json<Data<T>> {
    Json(Data { data: item })
}

/// `{"data": [...], "limit", "next_cursor"?, "total"?}` — one page of a list.
pub fn page_response<T>(
    data: Vec<T>,
    limit: i64,
    next_cursor: Option<String>,
    total: Option<i64>,
) -> Json<Page<T>> {
    Json(Page {
        data,
        limit: Some(limit),
        next_cursor,
        total,
    })
}

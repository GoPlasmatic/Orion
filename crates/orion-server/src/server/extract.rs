//! Custom Axum extractor that maps JSON deserialization failures to
//! `OrionError` so admin handlers keep the v0.1 status-code contract
//! (400 BAD_REQUEST instead of axum-default 422 UNPROCESSABLE_ENTITY)
//! and gain field-pathed `details[]` entries (A3) for malformed bodies.
//!
//! Drop-in replacement for `axum::Json<T>` in handlers that accept
//! request bodies: change `Json(req): Json<T>` to
//! `OrionJson(req): OrionJson<T>`.

use axum::extract::FromRequest;
use axum::extract::FromRequestParts;
use axum::extract::Request;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::http::request::Parts;

use crate::errors::OrionError;

pub struct OrionJson<T>(pub T);

/// Query-string counterpart of [`OrionJson`]: axum's own `Query` rejection is
/// a plain-text body, which breaks the `{"error": {...}}` envelope every other
/// response uses.
pub struct OrionQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for OrionQuery<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = OrionError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(OrionQuery(value)),
            Err(rej) => Err(map_query_rejection(rej)),
        }
    }
}

fn map_query_rejection(rej: QueryRejection) -> OrionError {
    OrionError::validation(format!("Invalid query string: {}", rej.body_text()))
}

impl<T, S> FromRequest<S> for OrionJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = OrionError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(OrionJson(value)),
            Err(rej) => Err(map_rejection(rej)),
        }
    }
}

/// The data plane's raw body, with an envelope-carrying rejection.
///
/// The dynamic handler takes the body as bytes because a channel's payload is
/// not necessarily JSON. Taking it as a bare `Bytes` meant axum's own
/// rejection answered an oversize request — a `413` with the plain-text body
/// `Failed to buffer the request body: length limit exceeded`, the one
/// non-2xx in the whole surface that carried no `{"error": …}` envelope, and
/// so the one a client's error path could not parse.
pub struct OrionBody(pub axum::body::Bytes);

impl<S> FromRequest<S> for OrionBody
where
    S: Send + Sync,
{
    type Rejection = OrionError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::body::Bytes::from_request(req, state).await {
            Ok(bytes) => Ok(OrionBody(bytes)),
            Err(_) => Err(OrionError::PayloadTooLarge(
                "Request body exceeded `ingest.max_payload_size`".to_string(),
            )),
        }
    }
}

fn map_rejection(rej: JsonRejection) -> OrionError {
    match rej {
        // Type/shape mismatch (default axum: 422). v0.1 returned 400 for
        // these, and a field-pathed detail is far more useful for clients.
        JsonRejection::JsonDataError(e) => {
            let msg = e.body_text();
            // serde error messages typically look like
            //   "unknown variant `grpc`, expected one of `http`, `kafka`, ..."
            // or "missing field `name`". Extract a coarse path when we can.
            let path = extract_path_from_serde_message(&msg).unwrap_or_else(|| "body".to_string());
            OrionError::invalid_field(path, "INVALID", msg)
        }
        JsonRejection::JsonSyntaxError(e) => {
            OrionError::validation(format!("Invalid JSON: {}", e.body_text()))
        }
        JsonRejection::MissingJsonContentType(_) => OrionError::UnsupportedMediaType(
            "Expected `content-type: application/json`".to_string(),
        ),
        // The body exceeded the plane's limit. Matched explicitly because the
        // catch-all below turns it into a 400 `VALIDATION_ERROR`, which tells
        // the caller to fix their JSON when the JSON was never read.
        JsonRejection::BytesRejection(_) => OrionError::PayloadTooLarge(
            "Request body exceeded the configured limit — see \
             `ingest.max_payload_size` (data plane) or `server.max_admin_body_size` \
             (admin plane)"
                .to_string(),
        ),
        // Catch-all: surface body_text but use the rejection's status as a hint.
        other => OrionError::validation(other.body_text()),
    }
}

/// Best-effort: pull a JSON field name out of a serde error message so
/// the field-pathed `details[]` entry points somewhere useful. The serde
/// message format varies by error kind; when we can't parse it the
/// caller falls back to "body".
fn extract_path_from_serde_message(msg: &str) -> Option<String> {
    // Patterns we recognize, in order:
    //   "missing field `name` at line X column Y"        -> name
    //   "unknown field `foo`, expected one of ..."       -> foo
    //   "invalid type: string \"x\", expected ... at line ..."  -> no path
    //   "... at line N column M for key `k`"             -> k (rare)
    for marker in ["missing field `", "unknown field `", "for key `"] {
        if let Some(rest) = msg.split_once(marker)
            && let Some((field, _)) = rest.1.split_once('`')
        {
            return Some(format!("body.{field}"));
        }
    }
    None
}

/// The direct peer's socket address, when the serve layer supplied one.
///
/// `ConnectInfo<SocketAddr>` itself is a fallible extractor and axum 0.8 has
/// no `Option` impl for it, so a handler that wants "the peer if there is
/// one" — the data plane, which is also driven by `tower::oneshot` in tests
/// where no peer exists — needs this. Never a rejection: absence is a valid
/// answer, and the callers fall back to forwarded headers exactly as the
/// rate-limit middleware does.
pub struct PeerAddr(pub Option<std::net::SocketAddr>);

impl<S> FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(PeerAddr(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|ci| ci.0),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_path_missing_field() {
        let msg = "missing field `name` at line 1 column 42";
        assert_eq!(
            extract_path_from_serde_message(msg),
            Some("body.name".into())
        );
    }

    #[test]
    fn extract_path_unknown_field() {
        let msg = "unknown field `extra`, expected one of `name`, `description`";
        assert_eq!(
            extract_path_from_serde_message(msg),
            Some("body.extra".into())
        );
    }

    #[test]
    fn extract_path_unparseable_returns_none() {
        let msg = "invalid type: string \"x\", expected u64";
        assert_eq!(extract_path_from_serde_message(msg), None);
    }
}

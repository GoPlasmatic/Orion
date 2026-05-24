//! Custom Axum extractor that maps JSON deserialization failures to
//! `OrionError` so admin handlers keep the v0.1 status-code contract
//! (400 BAD_REQUEST instead of axum-default 422 UNPROCESSABLE_ENTITY)
//! and gain field-pathed `details[]` entries (A3) for malformed bodies.
//!
//! Drop-in replacement for `axum::Json<T>` in handlers that accept
//! request bodies: change `Json(req): Json<T>` to
//! `OrionJson(req): OrionJson<T>`.

use axum::extract::FromRequest;
use axum::extract::Request;
use axum::extract::rejection::JsonRejection;

use crate::errors::OrionError;

pub struct OrionJson<T>(pub T);

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
            OrionError::BadRequest(format!("Invalid JSON: {}", e.body_text()))
        }
        JsonRejection::MissingJsonContentType(_) => OrionError::UnsupportedMediaType(
            "Expected `content-type: application/json`".to_string(),
        ),
        // Catch-all: surface body_text but use the rejection's status as a hint.
        other => OrionError::BadRequest(other.body_text()),
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

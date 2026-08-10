//! The transport: one reqwest wrapper for every Orion API caller.

use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::time::Duration;

use crate::error::ClientError;

/// HTTP client for an Orion server's admin and data APIs.
///
/// Paths are passed relative to the server root (see [`crate::paths`]);
/// authentication, the optional `X-Orion-Change-Context` audit header, and
/// timeout are configured once at construction.
#[derive(Debug, Clone)]
pub struct OrionClient {
    base_url: String,
    http: reqwest::Client,
    api_key: Option<String>,
    api_key_header: Option<String>,
    change_context: Option<String>,
}

impl OrionClient {
    /// A client with the default 30-second request timeout.
    pub fn new(base_url: &str) -> Result<Self, ClientError> {
        Self::with_timeout(base_url, Some(Duration::from_secs(30)))
    }

    /// A client with an explicit request timeout — `None` means no timeout
    /// (the server's package CLI keeps its historical no-timeout behaviour
    /// for long-running bulk imports).
    pub fn with_timeout(base_url: &str, timeout: Option<Duration>) -> Result<Self, ClientError> {
        let mut builder = reqwest::Client::builder();
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        let http = builder.build().map_err(|source| ClientError::Connect {
            url: base_url.to_string(),
            source,
        })?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
            api_key: None,
            api_key_header: None,
            change_context: None,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Authenticate with an API key. With no `header` (or an
    /// `Authorization`-named one) the key is sent as `Authorization: Bearer`;
    /// any other header name carries the key verbatim.
    pub fn with_api_key(mut self, api_key: String, header: Option<String>) -> Self {
        self.api_key = Some(api_key);
        self.api_key_header = header;
        self
    }

    /// Send `X-Orion-Change-Context` on every request (K5), so the audit
    /// trail groups a multi-call operation.
    pub fn with_change_context(mut self, context: String) -> Self {
        self.change_context = Some(context);
        self
    }

    // -- Full-body verbs: parse the response body as `T`, envelope and all --

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let (_, bytes) = self.execute(Method::GET, path, None).await?;
        decode(&bytes)
    }

    pub async fn get_text(&self, path: &str) -> Result<String, ClientError> {
        let (_, bytes) = self.execute(Method::GET, path, None).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError> {
        let (_, bytes) = self.execute(Method::POST, path, Some(body)).await?;
        decode(&bytes)
    }

    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let (_, bytes) = self.execute(Method::POST, path, None).await?;
        decode(&bytes)
    }

    pub async fn put<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError> {
        let (_, bytes) = self.execute(Method::PUT, path, Some(body)).await?;
        decode(&bytes)
    }

    pub async fn patch<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError> {
        let (_, bytes) = self.execute(Method::PATCH, path, Some(body)).await?;
        decode(&bytes)
    }

    pub async fn delete(&self, path: &str) -> Result<(), ClientError> {
        self.execute(Method::DELETE, path, None).await?;
        Ok(())
    }

    // -- Envelope-unwrapping verbs: the `data` payload of an admin 2xx,
    //    tolerating the bare pre-1.0 shape --

    pub async fn get_data<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let (_, bytes) = self.execute(Method::GET, path, None).await?;
        decode_data(&bytes)
    }

    /// Like [`Self::get_data`], but a 404 answers `Ok(None)`.
    pub async fn get_data_opt<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<Option<T>, ClientError> {
        match self.execute(Method::GET, path, None).await {
            Ok((_, bytes)) => decode_data(&bytes).map(Some),
            Err(e) if e.status() == Some(StatusCode::NOT_FOUND) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn post_data<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError> {
        let (_, bytes) = self.execute(Method::POST, path, Some(body)).await?;
        decode_data(&bytes)
    }

    pub async fn post_data_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let (_, bytes) = self.execute(Method::POST, path, None).await?;
        decode_data(&bytes)
    }

    pub async fn put_data<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError> {
        let (_, bytes) = self.execute(Method::PUT, path, Some(body)).await?;
        decode_data(&bytes)
    }

    pub async fn patch_data<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError> {
        let (_, bytes) = self.execute(Method::PATCH, path, Some(body)).await?;
        decode_data(&bytes)
    }

    // -- The one request path --

    async fn execute(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Vec<u8>), ClientError> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self.http.request(method, &url);
        req = match (&self.api_key, &self.api_key_header) {
            (Some(key), Some(header)) if !header.eq_ignore_ascii_case("authorization") => {
                req.header(header, key)
            }
            (Some(key), _) => req.header("Authorization", format!("Bearer {key}")),
            _ => req,
        };
        if let Some(context) = &self.change_context {
            req = req.header("x-orion-change-context", context);
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req.send().await.map_err(|source| ClientError::Connect {
            url: url.clone(),
            source,
        })?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|source| ClientError::Transport { source })?;
        if !status.is_success() {
            return Err(ClientError::from_response(status, &bytes));
        }
        Ok((status, bytes.to_vec()))
    }
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ClientError> {
    serde_json::from_slice(bytes).map_err(|source| ClientError::Decode { source })
}

/// Unwrap the `{"data": …}` envelope, tolerating the bare pre-1.0 shape.
fn decode_data<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ClientError> {
    let value: Value = decode(bytes)?;
    let payload = match value {
        Value::Object(mut map) if map.contains_key("data") => {
            map.remove("data").unwrap_or(Value::Null)
        }
        other => other,
    };
    serde_json::from_value(payload).map_err(|source| ClientError::Decode { source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_data_unwraps_the_envelope_and_tolerates_bare() {
        let enveloped: Value = decode_data(br#"{"data": {"id": "wf-1"}}"#).expect("test");
        assert_eq!(enveloped["id"], "wf-1");
        let bare: Value = decode_data(br#"{"id": "wf-1"}"#).expect("test");
        assert_eq!(bare["id"], "wf-1");
    }

    #[test]
    fn base_url_trailing_slash_is_trimmed() {
        let client = OrionClient::new("http://localhost:8080/").expect("test");
        assert_eq!(client.base_url(), "http://localhost:8080");
    }
}

//! The transport: one reqwest wrapper for every Orion API caller.

use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::time::Duration;

use crate::error::ClientError;

/// Default upper bound on a single response body (64 MiB).
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// HTTP client for an Orion server's admin and data APIs.
///
/// Paths are passed relative to the server root (see [`crate::paths`]);
/// authentication, the optional `X-Orion-Change-Context` audit header,
/// timeout, and response size cap are configured once at construction.
#[derive(Clone)]
pub struct OrionClient {
    base_url: String,
    http: reqwest::Client,
    api_key: Option<String>,
    api_key_header: Option<String>,
    change_context: Option<String>,
    max_response_bytes: usize,
    get_retries: u32,
    get_retry_backoff: Duration,
}

/// Never prints the credential: a `{:?}` of a client lands in logs and panic
/// messages.
impl std::fmt::Debug for OrionClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrionClient")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("api_key_header", &self.api_key_header)
            .field("change_context", &self.change_context)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("get_retries", &self.get_retries)
            .finish_non_exhaustive()
    }
}

impl OrionClient {
    /// A client with the default 30-second request timeout and 64 MiB response cap.
    pub fn new(base_url: &str) -> Result<Self, ClientError> {
        Self::with_timeout(base_url, Some(Duration::from_secs(30)))
    }

    /// A client with an explicit request timeout — `None` means no timeout
    /// (the server's package CLI keeps its historical no-timeout behaviour
    /// for long-running bulk imports).
    pub fn with_timeout(base_url: &str, timeout: Option<Duration>) -> Result<Self, ClientError> {
        // Identify the caller: an API behind a proxy or WAF may refuse a
        // request that sends no `User-Agent`, and this transport is what both
        // `orion-cli` and the server's own `package plan/apply` speak (#356).
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("orion-client/", env!("CARGO_PKG_VERSION")));
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
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            get_retries: 0,
            get_retry_backoff: Duration::from_millis(200),
        })
    }

    /// Override the maximum response body size in bytes (`0` disables the cap).
    pub fn with_max_response_bytes(mut self, max_bytes: usize) -> Self {
        self.max_response_bytes = max_bytes;
        self
    }

    /// Opt in to bounded exponential-backoff retries for idempotent `GET`
    /// requests that fail on connection/transport errors or `502`/`503`/`504`
    /// gateway statuses. Mutating verbs (`POST`, `PUT`, `PATCH`, `DELETE`) are
    /// never retried automatically.
    pub fn with_get_retries(mut self, max_retries: u32, initial_backoff: Duration) -> Self {
        self.get_retries = max_retries;
        self.get_retry_backoff = initial_backoff;
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `true` when this client would send its API key over plain `http://`
    /// to a host other than the local machine — the one configuration in
    /// which the credential crosses a network unencrypted. Loopback
    /// (`localhost`, `*.localhost`, `127.0.0.0/8`, `::1`) is exempt: that is
    /// every development setup, and the bytes never leave the host. Callers
    /// print the warning; a transport library has no business writing to
    /// stderr.
    pub fn sends_credential_in_clear(&self) -> bool {
        if self.api_key.is_none() {
            return false;
        }
        let Ok(url) = reqwest::Url::parse(&self.base_url) else {
            return false;
        };
        if url.scheme() != "http" {
            return false;
        }
        let Some(host) = url.host_str() else {
            return false;
        };
        let is_loopback = host.eq_ignore_ascii_case("localhost")
            || host.to_ascii_lowercase().ends_with(".localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        !is_loopback
    }

    /// Authenticate with an API key. With no `header` (or an
    /// `Authorization`-named one) the key is sent as `Authorization: Bearer`;
    /// any other header name carries the key verbatim.
    pub fn with_api_key(mut self, api_key: String, header: Option<String>) -> Self {
        self.api_key = Some(api_key);
        self.api_key_header = header;
        self
    }

    /// Send `X-Orion-Change-Context` on every request, so the audit
    /// trail groups a multi-call operation.
    pub fn with_change_context(mut self, context: String) -> Self {
        self.change_context = Some(context);
        self
    }

    // -- Full-body verbs: parse the response body as `T`, envelope and all --

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let bytes = self.execute(Method::GET, path, None).await?;
        decode(&bytes)
    }

    pub async fn get_text(&self, path: &str) -> Result<String, ClientError> {
        let bytes = self.execute(Method::GET, path, None).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError> {
        let bytes = self.execute(Method::POST, path, Some(body)).await?;
        decode(&bytes)
    }

    pub async fn post_dto<Req: serde::Serialize + ?Sized, Resp: DeserializeOwned>(
        &self,
        path: &str,
        body: &Req,
    ) -> Result<Resp, ClientError> {
        let val = serde_json::to_value(body).map_err(|source| ClientError::Decode { source })?;
        let bytes = self.execute(Method::POST, path, Some(&val)).await?;
        decode(&bytes)
    }

    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let bytes = self.execute(Method::POST, path, None).await?;
        decode(&bytes)
    }

    pub async fn put<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError> {
        let bytes = self.execute(Method::PUT, path, Some(body)).await?;
        decode(&bytes)
    }

    pub async fn patch<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError> {
        let bytes = self.execute(Method::PATCH, path, Some(body)).await?;
        decode(&bytes)
    }

    pub async fn delete(&self, path: &str) -> Result<(), ClientError> {
        self.execute(Method::DELETE, path, None).await?;
        Ok(())
    }

    // -- Envelope-unwrapping verbs: the `data` payload of the operational
    //    admin responses. A v2 admin answer is a bare DTO and does not use
    //    these. --

    pub async fn get_data<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let bytes = self.execute(Method::GET, path, None).await?;
        decode_data(&bytes)
    }

    /// Like [`Self::get_data`], but a 404 answers `Ok(None)`.
    pub async fn get_data_opt<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<Option<T>, ClientError> {
        match self.execute(Method::GET, path, None).await {
            Ok(bytes) => decode_data(&bytes).map(Some),
            Err(e) if e.status() == Some(StatusCode::NOT_FOUND) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn post_data<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError> {
        let bytes = self.execute(Method::POST, path, Some(body)).await?;
        decode_data(&bytes)
    }

    pub async fn post_data_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let bytes = self.execute(Method::POST, path, None).await?;
        decode_data(&bytes)
    }

    pub async fn put_data<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError> {
        let bytes = self.execute(Method::PUT, path, Some(body)).await?;
        decode_data(&bytes)
    }

    pub async fn patch_data<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError> {
        let bytes = self.execute(Method::PATCH, path, Some(body)).await?;
        decode_data(&bytes)
    }

    // -- The one request path --

    pub(crate) async fn execute(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<bytes::Bytes, ClientError> {
        let max_attempts = if method == Method::GET {
            self.get_retries.saturating_add(1)
        } else {
            1
        };
        let mut attempt = 0u32;
        loop {
            attempt = attempt.saturating_add(1);
            match self.execute_once(method.clone(), path, body).await {
                Ok(bytes) => return Ok(bytes),
                Err(err) if attempt < max_attempts && is_retryable_get_error(&err) => {
                    let shift = (attempt - 1).min(6);
                    tokio::time::sleep(self.get_retry_backoff.saturating_mul(1u32 << shift)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn execute_once(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<bytes::Bytes, ClientError> {
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
        let mut resp = req.send().await.map_err(|source| ClientError::Connect {
            url: url.clone(),
            source,
        })?;
        let status = resp.status();
        let cap = self.max_response_bytes;
        if cap > 0
            && let Some(len) = resp.content_length()
            && len > cap as u64
        {
            return Err(ClientError::ResponseTooLarge {
                url,
                max_bytes: cap,
            });
        }
        let mut buf = bytes::BytesMut::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|source| ClientError::Transport { source })?
        {
            if cap > 0 && buf.len().saturating_add(chunk.len()) > cap {
                return Err(ClientError::ResponseTooLarge {
                    url,
                    max_bytes: cap,
                });
            }
            buf.extend_from_slice(&chunk);
        }
        let bytes = buf.freeze();
        if !status.is_success() {
            return Err(ClientError::from_response(status, &bytes));
        }
        Ok(bytes)
    }
}

fn is_retryable_get_error(err: &ClientError) -> bool {
    match err {
        ClientError::Connect { .. } | ClientError::Transport { .. } => true,
        other => matches!(
            other.status(),
            Some(
                StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            )
        ),
    }
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ClientError> {
    serde_json::from_slice(bytes).map_err(|source| ClientError::Decode { source })
}

/// Unwrap the `{"data": …}` envelope.
fn decode_data<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ClientError> {
    decode::<orion_api::Data<T>>(bytes).map(|env| env.data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_data_unwraps_the_envelope_and_refuses_bare() {
        let enveloped: Value = decode_data(br#"{"data": {"id": "wf-1"}}"#).expect("test");
        assert_eq!(enveloped["id"], "wf-1");
        assert!(decode_data::<Value>(br#"{"id": "wf-1"}"#).is_err());
    }

    #[test]
    fn base_url_trailing_slash_is_trimmed() {
        let client = OrionClient::new("http://localhost:8080/").expect("test");
        assert_eq!(client.base_url(), "http://localhost:8080");
    }

    #[test]
    fn credential_in_clear_is_only_plain_http_to_a_remote_host() {
        let keyed = |url: &str| {
            OrionClient::new(url)
                .expect("test")
                .with_api_key("k".into(), None)
        };
        // Plain http off the box: the key would cross a network in the clear.
        assert!(keyed("http://orion.internal:8080").sends_credential_in_clear());
        assert!(keyed("http://10.0.0.5").sends_credential_in_clear());
        // Encrypted, or never leaving the host: fine.
        assert!(!keyed("https://orion.internal").sends_credential_in_clear());
        assert!(!keyed("http://localhost:8080").sends_credential_in_clear());
        assert!(!keyed("http://LOCALHOST").sends_credential_in_clear());
        assert!(!keyed("http://dev.localhost").sends_credential_in_clear());
        assert!(!keyed("http://127.0.0.1:8080").sends_credential_in_clear());
        assert!(!keyed("http://127.1.2.3").sends_credential_in_clear());
        assert!(!keyed("http://[::1]:8080").sends_credential_in_clear());
        // No key: nothing to leak, whatever the URL.
        assert!(
            !OrionClient::new("http://orion.internal")
                .expect("test")
                .sends_credential_in_clear()
        );
    }

    #[test]
    fn get_retry_policy_only_retries_transient_server_statuses() {
        let make_api = |status: StatusCode| ClientError::Api {
            status,
            error: orion_api::ErrorBody {
                code: "ERR".into(),
                message: "msg".into(),
                details: Vec::new(),
                request_id: None,
            },
        };
        assert!(is_retryable_get_error(&make_api(StatusCode::BAD_GATEWAY)));
        assert!(is_retryable_get_error(&make_api(
            StatusCode::SERVICE_UNAVAILABLE
        )));
        assert!(is_retryable_get_error(&make_api(
            StatusCode::GATEWAY_TIMEOUT
        )));
        assert!(!is_retryable_get_error(&make_api(
            StatusCode::INTERNAL_SERVER_ERROR
        )));
        assert!(!is_retryable_get_error(&make_api(StatusCode::NOT_FOUND)));
    }
}

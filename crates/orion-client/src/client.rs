//! The transport: one reqwest wrapper for every Orion API caller.

use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;

use crate::error::ClientError;

/// Default upper bound on a single response body (64 MiB).
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Per-request knobs the buffered verbs do not need: a body that is not
/// JSON, extra headers, and the streaming response path.
///
/// Additive: every existing verb uses [`RequestOptions::default`], so its
/// wire behaviour is unchanged. Build one with [`RequestOptions::new`] and
/// the `with_*` methods; the type is non-exhaustive so a later option does
/// not break a caller.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct RequestOptions {
    /// `None` keeps today's behaviour: `application/json` when a body is
    /// sent. `Some` overrides both that default and a `content-type` in
    /// `headers`.
    pub content_type: Option<String>,
    /// Extra request headers, applied before the client's auth and
    /// change-context headers, so a request cannot replace its credential
    /// by accident.
    pub headers: Vec<(String, String)>,
    /// Selects the streaming response path. [`OrionClient::get_stream`] is
    /// the public entry point; the buffered verbs always read the body.
    pub stream: bool,
}

impl RequestOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Send the body as `content_type` instead of JSON.
    #[must_use]
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// Add a request header. Applied before auth, so `authorization` and
    /// `x-orion-change-context` cannot be overridden here.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The options a streaming request uses.
    pub fn streaming() -> Self {
        Self {
            stream: true,
            ..Self::default()
        }
    }
}

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
        // Straight to bytes: a DTO→`Value`→bytes path built a whole JSON tree
        // the request never reads, which matters for a plugin upload's base64
        // payload (P-28).
        let bytes = serde_json::to_vec(body).map_err(|source| ClientError::Decode { source })?;
        let bytes = self
            .execute_bytes(Method::POST, path, Some(bytes.into()))
            .await?;
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
        // Serialized once here, not by `RequestBuilder::json` on every
        // attempt: the bytes are the same each time.
        let body = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|source| ClientError::Decode { source })?
            .map(bytes::Bytes::from);
        self.execute_bytes(method, path, body).await
    }

    pub(crate) async fn execute_bytes(
        &self,
        method: Method,
        path: &str,
        body: Option<bytes::Bytes>,
    ) -> Result<bytes::Bytes, ClientError> {
        self.execute_bytes_with(method, path, body, &RequestOptions::default())
            .await
    }

    /// Send a raw body with per-request options and return the buffered
    /// response. The existing typed verbs are shortcuts over this path.
    pub async fn request_bytes_with(
        &self,
        method: Method,
        path: &str,
        body: Option<bytes::Bytes>,
        options: &RequestOptions,
    ) -> Result<bytes::Bytes, ClientError> {
        self.execute_bytes_with(method, path, body, options).await
    }

    pub(crate) async fn execute_bytes_with(
        &self,
        method: Method,
        path: &str,
        body: Option<bytes::Bytes>,
        options: &RequestOptions,
    ) -> Result<bytes::Bytes, ClientError> {
        let max_attempts = if method == Method::GET {
            self.get_retries.saturating_add(1)
        } else {
            1
        };
        let mut attempt = 0u32;
        loop {
            attempt = attempt.saturating_add(1);
            match self
                .execute_once(method.clone(), path, body.clone(), options)
                .await
            {
                Ok(bytes) => return Ok(bytes),
                Err(err) if attempt < max_attempts && is_retryable_get_error(&err) => {
                    self.backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn execute_once(
        &self,
        method: Method,
        path: &str,
        body: Option<bytes::Bytes>,
        options: &RequestOptions,
    ) -> Result<bytes::Bytes, ClientError> {
        let url = format!("{}{path}", self.base_url);
        let resp = self.send_once(method, &url, body, options).await?;
        let status = resp.status();
        let bytes = self.read_capped(resp, url).await?;
        if !status.is_success() {
            return Err(ClientError::from_response(status, &bytes));
        }
        Ok(bytes)
    }

    /// Build and send one request, with no status or body handling. The
    /// buffered path reads it through [`Self::read_capped`]; the streaming
    /// path wraps it in [`CappedStream`].
    async fn send_once(
        &self,
        method: Method,
        url: &str,
        body: Option<bytes::Bytes>,
        options: &RequestOptions,
    ) -> Result<reqwest::Response, ClientError> {
        let mut req = self.http.request(method, url);
        // Extras first: auth and the change context are applied last so a
        // request cannot replace its own credential.
        let has_content_type = options
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
        for (name, value) in &options.headers {
            if options.content_type.is_some() && name.eq_ignore_ascii_case("content-type") {
                continue;
            }
            let header =
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                    ClientError::InvalidRequestHeader {
                        name: name.clone(),
                        message: error.to_string(),
                    }
                })?;
            let value = reqwest::header::HeaderValue::from_str(value).map_err(|error| {
                ClientError::InvalidRequestHeader {
                    name: name.clone(),
                    message: error.to_string(),
                }
            })?;
            req = req.header(header, value);
        }
        if let Some(body) = body {
            // An explicit option wins; an explicit header is left alone;
            // otherwise the default stays JSON.
            let content_type = match &options.content_type {
                Some(content_type) => Some(content_type.as_str()),
                None if has_content_type => None,
                None => Some("application/json"),
            };
            if let Some(content_type) = content_type {
                req = req.header(reqwest::header::CONTENT_TYPE, content_type);
            }
            req = req.body(body);
        } else if let Some(content_type) = &options.content_type {
            req = req.header(reqwest::header::CONTENT_TYPE, content_type);
        }
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
        req.send().await.map_err(|source| ClientError::Connect {
            url: url.to_string(),
            source,
        })
    }

    /// Read a response body under the client's ceiling.
    async fn read_capped(
        &self,
        mut resp: reqwest::Response,
        url: String,
    ) -> Result<bytes::Bytes, ClientError> {
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
        Ok(buf.freeze())
    }

    /// A `GET` whose body is yielded as it arrives, for an SSE or other
    /// streaming channel. Auth, change context and the response ceiling are
    /// the same as every buffered verb.
    ///
    /// A non-2xx answer is read under the same ceiling and returned through
    /// [`ClientError::from_response`] before any item is yielded. Retries are
    /// the client's configured `GET` policy, applied only while establishing
    /// the response; once the stream is returned it is never replayed.
    pub async fn get_stream(
        &self,
        path: &str,
        options: &RequestOptions,
    ) -> Result<impl Stream<Item = Result<bytes::Bytes, ClientError>> + use<>, ClientError> {
        let max_attempts = self.get_retries.saturating_add(1);
        let mut attempt = 0u32;
        loop {
            attempt = attempt.saturating_add(1);
            let url = format!("{}{path}", self.base_url);
            match self.send_once(Method::GET, &url, None, options).await {
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        let bytes = self.read_capped(resp, url.clone()).await?;
                        let err = ClientError::from_response(status, &bytes);
                        if attempt < max_attempts && is_retryable_get_error(&err) {
                            self.backoff(attempt).await;
                            continue;
                        }
                        return Err(err);
                    }
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
                    return Ok(CappedStream {
                        inner: Box::pin(resp.bytes_stream()),
                        url,
                        remaining: (cap > 0).then_some(cap),
                        cap,
                        done: false,
                    });
                }
                Err(err) if attempt < max_attempts && is_retryable_get_error(&err) => {
                    self.backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn backoff(&self, attempt: u32) {
        let shift = (attempt - 1).min(6);
        tokio::time::sleep(self.get_retry_backoff.saturating_mul(1u32 << shift)).await;
    }
}

/// A response body streamed under the client's response ceiling. A chunk
/// that would cross it yields one [`ClientError::ResponseTooLarge`] and ends
/// the stream.
struct CappedStream {
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
    url: String,
    remaining: Option<usize>,
    cap: usize,
    done: bool,
}

impl Stream for CappedStream {
    type Item = Result<bytes::Bytes, ClientError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                this.done = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(remaining) = this.remaining {
                    if chunk.len() > remaining {
                        this.done = true;
                        return Poll::Ready(Some(Err(ClientError::ResponseTooLarge {
                            url: this.url.clone(),
                            max_bytes: this.cap,
                        })));
                    }
                    this.remaining = Some(remaining - chunk.len());
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(source))) => {
                this.done = true;
                Poll::Ready(Some(Err(ClientError::Transport { source })))
            }
        }
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
#[expect(clippy::panic)]
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
                connector: None,
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

    // -- Request options and streaming, over a raw HTTP server --

    use std::future::poll_fn;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;

    /// Read one HTTP/1.1 request, header block and declared body included.
    async fn read_request(socket: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        loop {
            let mut chunk = [0u8; 2048];
            let read = socket.read(&mut chunk).await.expect("test server read");
            if read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..read]);
            let Some(end) = buf
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|at| at + 4)
            else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if buf.len() >= end + length {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Answer one request with `response`, and hand the request text back.
    async fn serve_fixed(
        status: &'static str,
        body: &'static str,
    ) -> (SocketAddr, oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_request(&mut socket).await;
            let _ = tx.send(request);
            let response = format!(
                "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.expect("write");
            socket.shutdown().await.expect("close");
        });
        (addr, rx)
    }

    /// A chunked body: the first event, a pause, then the second. A client
    /// that buffered to EOF would not see the first within the pause.
    async fn serve_chunked() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let _ = read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
                      3\r\none\r\n",
                )
                .await
                .expect("headers");
            socket.flush().await.expect("flush");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            socket
                .write_all(b"3\r\ntwo\r\n0\r\n\r\n")
                .await
                .expect("second");
            socket.shutdown().await.expect("close");
        });
        addr
    }

    /// Poll one item from a boxed stream.
    async fn next(
        stream: &mut std::pin::Pin<Box<impl Stream<Item = Result<bytes::Bytes, ClientError>>>>,
    ) -> Option<Result<bytes::Bytes, ClientError>> {
        poll_fn(|cx| stream.as_mut().poll_next(cx)).await
    }

    #[tokio::test]
    async fn request_options_send_content_type_and_headers_without_losing_auth() {
        let (addr, captured) = serve_fixed("HTTP/1.1 200 OK", "{}").await;
        let client = OrionClient::new(&format!("http://{addr}"))
            .expect("client")
            .with_api_key("secret".into(), None);
        let options = RequestOptions::new()
            .with_content_type("application/graphql")
            .with_header("x-trace", "1")
            // The configured credential must win over an extra header.
            .with_header("authorization", "Bearer wrong");
        let answer = client
            .request_bytes_with(
                Method::POST,
                "/graphql",
                Some(bytes::Bytes::from_static(b"query { ping }")),
                &options,
            )
            .await
            .expect("request");
        assert_eq!(answer, "{}");

        let request = captured.await.expect("captured").to_ascii_lowercase();
        assert!(
            request.contains("content-type: application/graphql"),
            "{request}"
        );
        assert!(request.contains("x-trace: 1"), "{request}");
        assert!(
            request.contains("authorization: bearer secret"),
            "{request}"
        );
        assert!(request.contains("query { ping }"), "{request}");
    }

    #[tokio::test]
    async fn get_stream_yields_the_first_chunk_before_the_response_ends() {
        let addr = serve_chunked().await;
        let client = OrionClient::new(&format!("http://{addr}")).expect("client");
        let mut stream = Box::pin(
            client
                .get_stream("/events", &RequestOptions::streaming())
                .await
                .expect("stream"),
        );
        let first = tokio::time::timeout(std::time::Duration::from_millis(100), next(&mut stream))
            .await
            .expect("the first event must arrive before the server's pause")
            .expect("an item")
            .expect("a chunk");
        assert_eq!(&first[..], b"one");
        let second = next(&mut stream).await.expect("an item").expect("a chunk");
        assert_eq!(&second[..], b"two");
        assert!(next(&mut stream).await.is_none(), "the stream ends");
    }

    #[tokio::test]
    async fn a_stream_stops_at_the_response_ceiling() {
        let addr = serve_chunked().await;
        let client = OrionClient::new(&format!("http://{addr}"))
            .expect("client")
            .with_max_response_bytes(4);
        let mut stream = Box::pin(
            client
                .get_stream("/events", &RequestOptions::streaming())
                .await
                .expect("stream"),
        );
        let first = next(&mut stream).await.expect("an item").expect("a chunk");
        assert_eq!(&first[..], b"one");
        let second = next(&mut stream).await.expect("an item");
        assert!(matches!(
            second,
            Err(ClientError::ResponseTooLarge { max_bytes: 4, .. })
        ));
        assert!(next(&mut stream).await.is_none(), "the stream ends");
    }

    #[tokio::test]
    async fn a_stream_error_answer_keeps_the_envelope() {
        let (addr, _) = serve_fixed(
            "HTTP/1.1 502 Bad Gateway",
            r#"{"error":{"code":"BAD_GATEWAY","message":"upstream"}}"#,
        )
        .await;
        let client = OrionClient::new(&format!("http://{addr}")).expect("client");
        let err = match client
            .get_stream("/events", &RequestOptions::streaming())
            .await
        {
            Ok(_) => panic!("a non-2xx answer is not a stream"),
            Err(error) => error,
        };
        match err {
            ClientError::Api { status, error } => {
                assert_eq!(status, StatusCode::BAD_GATEWAY);
                assert_eq!(error.code, "BAD_GATEWAY");
                assert_eq!(error.message, "upstream");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_invalid_header_is_refused_before_sending() {
        let client = OrionClient::new("http://127.0.0.1:1").expect("client");
        let err = client
            .request_bytes_with(
                Method::GET,
                "/",
                None,
                &RequestOptions::new().with_header("bad name", "x"),
            )
            .await
            .expect_err("refused");
        assert!(matches!(err, ClientError::InvalidRequestHeader { .. }));
    }
}

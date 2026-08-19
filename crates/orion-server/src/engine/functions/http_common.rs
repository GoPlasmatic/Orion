use std::time::Duration;

use dataflow_rs::engine::error::DataflowError;
use serde_json::Value;

use crate::connector::{AuthConfig, HttpConnectorConfig};

/// Build a URL from a base URL and optional path segment.
pub fn build_url(base: &str, path: Option<&str>) -> String {
    match path {
        Some(p) if !p.is_empty() => {
            let base = base.trim_end_matches('/');
            let path = p.trim_start_matches('/');
            format!("{base}/{path}")
        }
        _ => base.to_string(),
    }
}

/// Apply authentication to a request builder.
pub fn apply_auth(req: reqwest::RequestBuilder, auth: &AuthConfig) -> reqwest::RequestBuilder {
    match auth {
        AuthConfig::Bearer { token } => req.header("authorization", format!("Bearer {token}")),
        AuthConfig::Basic { username, password } => req.basic_auth(username, Some(password)),
        AuthConfig::ApiKey { header, key } => req.header(header, key),
        // #268: never applied raw — `effective_auth` resolves oauth2 to a
        // Bearer token before any request is built, and authoring validation
        // refuses oauth2 on the surfaces that do not resolve (es). Reaching
        // this arm sends no credential, so the endpoint refuses loudly
        // instead of receiving a config blob as a header.
        AuthConfig::OAuth2(_) => req,
    }
}

/// Map a token-acquisition failure into the engine's error vocabulary along
/// the estate's F42 split: transport-class failures are retryable `Io` (they
/// trip the breaker like any dependency outage); rejections and config
/// problems are non-retryable connector detail — a credential failure is not
/// evidence about the API's health.
pub fn oauth_error_to_dataflow(e: crate::connector::oauth::OAuthError) -> DataflowError {
    if e.retryable() {
        DataflowError::Io(e.to_string())
    } else {
        crate::errors::connector_detail_error(e.to_string())
    }
}

/// Redirect-hop cap for the manual follower in [`execute_request`]. The shared
/// client is built with `redirect::Policy::none()` (see main.rs), so every hop
/// returns here and passes SSRF validation before being followed.
const MAX_REDIRECTS: usize = 5;

/// How the resolved `body` becomes request bytes — the server-side value table
/// behind `http_call`'s `body_format` field. dataflow-rs carries the field as
/// an uninterpreted string precisely so a new encoding lands here as a new
/// variant, with no upstream release.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BodyFormat {
    #[default]
    Json,
    Form,
    Text,
}

impl BodyFormat {
    /// Parse the wire value. `None` — the field absent — is today's JSON
    /// behavior, so every pre-existing workflow encodes as before.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("json") => Ok(Self::Json),
            Some("form") => Ok(Self::Form),
            Some("text") => Ok(Self::Text),
            Some(other) => Err(format!(
                "unknown body_format '{other}' — expected one of 'json', 'form', 'text'"
            )),
        }
    }
}

/// How response bytes become the captured value — the value table behind
/// `http_call`'s `response_format` field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResponseFormat {
    #[default]
    Json,
    Text,
}

impl ResponseFormat {
    /// Parse the wire value. `None` is today's JSON behavior.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("json") => Ok(Self::Json),
            Some("text") => Ok(Self::Text),
            Some(other) => Err(format!(
                "unknown response_format '{other}' — expected one of 'json', 'text'"
            )),
        }
    }
}

/// A request body encoded per the task's `body_format`: the bytes to send and
/// the `content-type` stamped when no explicit header names one. `Bytes`, not
/// `Vec<u8>`: the body is attached per attempt (redirect hops, retries), and
/// a `Bytes` clone is a refcount bump instead of a payload copy.
#[derive(Debug)]
pub struct EncodedBody {
    pub bytes: bytes::Bytes,
    pub content_type: &'static str,
}

/// Turn a resolved body into bytes per `format`.
///
/// Shape errors name the offending key: a `body_logic` body only exists per
/// message, so this error is the only diagnosis its author ever sees.
/// Workflow validation runs the same function over a *static* `body` at
/// authoring time (see `schema::validate_input`), keeping the two layers from
/// drifting.
pub fn encode_body(body: &Value, format: BodyFormat) -> dataflow_rs::Result<EncodedBody> {
    let encoded = match format {
        BodyFormat::Json => EncodedBody {
            bytes: serde_json::to_vec(body)
                .map_err(|e| {
                    DataflowError::Validation(format!(
                        "Failed to serialize request body as JSON: {e}"
                    ))
                })?
                .into(),
            content_type: "application/json",
        },
        BodyFormat::Form => EncodedBody {
            bytes: encode_form(body)?.into_bytes().into(),
            content_type: "application/x-www-form-urlencoded",
        },
        BodyFormat::Text => match body {
            Value::String(s) => EncodedBody {
                bytes: s.clone().into_bytes().into(),
                content_type: "text/plain; charset=utf-8",
            },
            other => {
                return Err(DataflowError::Validation(format!(
                    "body_format 'text' requires the body to be a string, got {}",
                    json_type_name(other)
                )));
            }
        },
    };
    Ok(encoded)
}

/// URL-encode a body object as `key=value` pairs.
///
/// Scalars encode directly; arrays of scalars become repeated keys (`to=a&to=b`
/// — the multi-value convention of the common form APIs); `null` entries are
/// skipped so one body shape with conditionally-null entries expresses optional
/// parameters. Anything nested is refused: form encoding has no canonical
/// nesting. Bracket-path conventions need no support here — `"metadata[id]"`
/// is an ordinary top-level key.
fn encode_form(body: &Value) -> dataflow_rs::Result<String> {
    let Some(obj) = body.as_object() else {
        return Err(DataflowError::Validation(format!(
            "body_format 'form' requires the body to be an object of key/value pairs, got {}",
            json_type_name(body)
        )));
    };
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in obj {
        match value {
            Value::Null => {}
            Value::Array(items) => {
                for item in items {
                    let scalar =
                        scalar_form_value(item).ok_or_else(|| form_value_error(key, item))?;
                    ser.append_pair(key, &scalar);
                }
            }
            other => {
                let scalar =
                    scalar_form_value(other).ok_or_else(|| form_value_error(key, other))?;
                ser.append_pair(key, &scalar);
            }
        }
    }
    Ok(ser.finish())
}

/// A scalar's form-encoding text: strings verbatim, numbers and booleans in
/// their canonical JSON spelling. `None` for anything that is not a scalar.
fn scalar_form_value(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn form_value_error(key: &str, value: &Value) -> DataflowError {
    DataflowError::Validation(format!(
        "body_format 'form' cannot encode '{key}' ({}): entries must be scalars \
         or arrays of scalars — form encoding has no canonical nesting",
        json_type_name(value)
    ))
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The per-call half of one HTTP request — what varies task to task, as
/// opposed to the connector-level `HttpConnectorConfig`.
#[derive(Debug)]
pub struct RequestSpec<'a> {
    pub method: &'a reqwest::Method,
    pub url: &'a str,
    pub task_headers: Option<&'a std::collections::HashMap<String, String>>,
    pub body: Option<&'a Value>,
    pub body_format: BodyFormat,
    pub response_format: ResponseFormat,
    pub timeout: Duration,
    /// The auth to apply — the *effective* auth, resolved by the caller
    /// (#268): static variants pass `http_config.auth.as_ref()` through;
    /// `oauth2` connectors resolve a Bearer token via
    /// [`crate::connector::oauth::effective_auth`] first. A parameter rather
    /// than a read of `http_config.auth`, because resolving a token is async
    /// state the connector config cannot carry.
    pub auth: Option<&'a AuthConfig>,
}

/// Execute an HTTP request with connector config applied.
///
/// Builds the request with connector headers, auth, optional task-level headers,
/// optional body (encoded per `body_format`), and timeout. Returns the response
/// captured per `response_format`. Redirects are followed manually (up to
/// `MAX_REDIRECTS`) with SSRF re-validation per hop.
#[tracing::instrument(
    skip(client, http_config, spec),
    fields(method = ?spec.method, url = spec.url, timeout = ?spec.timeout)
)]
pub async fn execute_request(
    client: &reqwest::Client,
    http_config: &HttpConnectorConfig,
    spec: RequestSpec<'_>,
) -> dataflow_rs::Result<Value> {
    let RequestSpec {
        method,
        url,
        task_headers,
        body,
        body_format,
        response_format,
        timeout,
        auth,
    } = spec;
    let original = url::Url::parse(url)
        .map_err(|e| DataflowError::Validation(format!("Invalid URL '{url}': {e}")))?;
    let mut current = original.clone();
    let mut method = method.clone();
    // Encoded once — the bytes are identical on every retry and redirect hop.
    let mut body = body.map(|b| encode_body(b, body_format)).transpose()?;

    for _ in 0..=MAX_REDIRECTS {
        // SSRF protection: block requests to private/internal IPs.
        // `allow_private_urls` exempts only the connector's own endpoint — a
        // redirect is server-controlled data, so any hop leaving the original
        // host:port is validated even for private-allowed connectors.
        let own_endpoint = same_endpoint(&current, &original);
        if !(http_config.allow_private_urls && own_endpoint)
            && let Err(msg) = crate::validation::validate_url_not_private(current.as_str()).await
        {
            return Err(DataflowError::function_execution(
                format!("SSRF protection: {msg}"),
                None,
            ));
        }

        let mut req = client
            .request(method.clone(), current.clone())
            .timeout(timeout);

        // Inject W3C trace context headers (traceparent/tracestate) for distributed tracing
        {
            let mut trace_headers = std::collections::HashMap::new();
            crate::server::trace_context::inject_trace_context(&mut trace_headers);
            for (k, v) in &trace_headers {
                req = req.header(k, v);
            }
        }

        // Connector headers, auth, and task headers can carry credentials —
        // they go only to the connector's own endpoint, never on a cross-host hop.
        if own_endpoint {
            // Apply connector default headers (lowest priority)
            for (k, v) in &http_config.headers {
                req = req.header(k, v);
            }

            // Apply auth headers (override connector defaults)
            if let Some(auth) = auth {
                req = apply_auth(req, auth);
            }
        }

        // Apply the body and its format's content-type. The format decides the
        // bytes; the stamp is only a default — an explicit content-type (task-
        // or connector-level) wins, and must not be *joined* by the stamp:
        // `RequestBuilder::header` appends, so stamping unconditionally would
        // put two content-type headers on the wire.
        if let Some(enc) = &body {
            let explicit_content_type = own_endpoint
                && (task_headers.is_some_and(has_content_type)
                    || has_content_type(&http_config.headers));
            if !explicit_content_type {
                req = req.header("content-type", enc.content_type);
            }
            req = req.body(enc.bytes.clone());
        }

        // Apply task-level headers LAST (highest priority — workflow developer's explicit choice wins)
        if own_endpoint && let Some(headers) = task_headers {
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }

        let response = req.send().await.map_err(|e| {
            if e.is_timeout() {
                DataflowError::Timeout(format!("HTTP request to {current} timed out"))
            } else {
                DataflowError::Io(format!("HTTP request to {current} failed: {e}"))
            }
        })?;

        if let Some(next) = redirect_target(&response, &current)? {
            // Mirror reqwest's default policy: 301/302/303 turn a non-GET/HEAD
            // request into a bodyless GET; 307/308 re-send method and body.
            if matches!(response.status().as_u16(), 301..=303)
                && method != reqwest::Method::GET
                && method != reqwest::Method::HEAD
            {
                method = reqwest::Method::GET;
                body = None;
            }
            current = next;
            continue;
        }

        return read_response(
            response,
            &current,
            http_config.max_response_size,
            response_format,
        )
        .await;
    }

    Err(DataflowError::function_execution(
        format!("Stopped after {MAX_REDIRECTS} redirects requesting {url}"),
        None,
    ))
}

/// Same scheme-default-aware host:port — the boundary within which connector
/// credentials and the `allow_private_urls` exemption apply.
fn same_endpoint(a: &url::Url, b: &url::Url) -> bool {
    a.host_str().is_some()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Whether a header map names `content-type` explicitly — the signal to
/// withhold the body format's default stamp.
fn has_content_type(headers: &std::collections::HashMap<String, String>) -> bool {
    headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("content-type"))
}

/// The target of a redirect response, resolved against the current URL.
/// `None` when the response is not a followable redirect.
fn redirect_target(
    response: &reqwest::Response,
    current: &url::Url,
) -> dataflow_rs::Result<Option<url::Url>> {
    if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }
    let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
        return Ok(None);
    };
    let location = location.to_str().map_err(|_| {
        DataflowError::function_execution(
            format!("Redirect from {current} has a non-ASCII Location header"),
            None,
        )
    })?;
    let next = current.join(location).map_err(|e| {
        DataflowError::function_execution(
            format!("Redirect from {current} has invalid Location '{location}': {e}"),
            None,
        )
    })?;
    if !matches!(next.scheme(), "http" | "https") {
        return Err(DataflowError::function_execution(
            format!(
                "Redirect from {current} targets unsupported scheme '{}'",
                next.scheme()
            ),
            None,
        ));
    }
    Ok(Some(next))
}

/// Read a (non-redirect) response body per `format`, enforcing `max_size`.
///
/// Only the final parse step is format-dispatched — the size cap, the non-2xx
/// error path, and the streaming reads are identical for every format.
///
/// The limit is enforced *while streaming*: a chunked response with no
/// `Content-Length` must not get to sit fully in memory before the size
/// check — that is the exact OOM the limit exists to prevent.
async fn read_response(
    mut response: reqwest::Response,
    url: &url::Url,
    max_size: usize,
    format: ResponseFormat,
) -> dataflow_rs::Result<Value> {
    let status = response.status();

    // Check Content-Length hint before reading body
    if let Some(content_length) = response.content_length()
        && content_length as usize > max_size
    {
        return Err(DataflowError::function_execution(
            format!(
                "Response from {url} declared Content-Length {content_length} exceeds limit of {max_size} bytes"
            ),
            None,
        ));
    }

    if !status.is_success() {
        // Read at most `max_size` bytes of the error body for the message;
        // anything beyond that is dropped, never buffered. Read errors end
        // the body early (the status alone still makes a useful error).
        let mut body_bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok().flatten() {
            let room = max_size.saturating_sub(body_bytes.len());
            let take = chunk.len().min(room);
            body_bytes.extend_from_slice(&chunk[..take]);
            if take < chunk.len() {
                break;
            }
        }
        let body_text = String::from_utf8_lossy(&body_bytes);
        return Err(DataflowError::http(
            status.as_u16(),
            format!("HTTP {status} from {url}: {body_text}"),
        ));
    }

    let mut body_bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        DataflowError::function_execution(
            format!("Failed to read response body from {url}: {e}"),
            None,
        )
    })? {
        if body_bytes.len() + chunk.len() > max_size {
            return Err(DataflowError::function_execution(
                format!("Response body from {url} exceeds limit of {max_size} bytes"),
                None,
            ));
        }
        body_bytes.extend_from_slice(&chunk);
    }

    match format {
        ResponseFormat::Json => serde_json::from_slice(&body_bytes).map_err(|e| {
            DataflowError::function_execution(
                format!("Failed to parse response from {url} as JSON: {e}"),
                None,
            )
        }),
        // Lossy on invalid UTF-8, matching the error-path body capture above.
        ResponseFormat::Text => Ok(Value::String(
            String::from_utf8_lossy(&body_bytes).into_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_url() {
        assert_eq!(
            build_url("https://api.example.com", Some("/users")),
            "https://api.example.com/users"
        );
        assert_eq!(
            build_url("https://api.example.com/", Some("/users")),
            "https://api.example.com/users"
        );
        assert_eq!(
            build_url("https://api.example.com", None),
            "https://api.example.com"
        );
    }

    #[test]
    fn test_build_url_no_path() {
        assert_eq!(
            build_url("https://api.example.com", None),
            "https://api.example.com"
        );
    }

    #[test]
    fn test_build_url_empty_path() {
        assert_eq!(
            build_url("https://api.example.com", Some("")),
            "https://api.example.com"
        );
    }

    #[test]
    fn test_build_url_trims_slashes() {
        assert_eq!(
            build_url("https://api.example.com///", Some("///path")),
            "https://api.example.com/path"
        );
    }

    #[test]
    fn test_apply_auth_bearer() {
        let client = reqwest::Client::new();
        let auth = AuthConfig::Bearer {
            token: "tok123".to_string(),
        };
        let req = apply_auth(client.get("http://localhost"), &auth);
        let built = req.build().expect("test");
        assert_eq!(
            built
                .headers()
                .get("authorization")
                .expect("test")
                .to_str()
                .expect("test"),
            "Bearer tok123"
        );
    }

    #[test]
    fn test_apply_auth_api_key() {
        let client = reqwest::Client::new();
        let auth = AuthConfig::ApiKey {
            header: "x-api-key".to_string(),
            key: "secret123".to_string(),
        };
        let req = apply_auth(client.get("http://localhost"), &auth);
        let built = req.build().expect("test");
        assert_eq!(
            built
                .headers()
                .get("x-api-key")
                .expect("test")
                .to_str()
                .expect("test"),
            "secret123"
        );
    }

    #[tokio::test]
    async fn test_execute_request_success() {
        // Start a mock server using axum
        let mock_app = axum::Router::new().route(
            "/test",
            axum::routing::get(|| async { axum::Json(serde_json::json!({"result": "success"})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test");
        let addr = listener.local_addr().expect("test");
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.expect("test");
        });

        let client = reqwest::Client::new();
        let http_config = HttpConnectorConfig {
            retry_non_idempotent: false,
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
            operations: Default::default(),
        };

        let result = execute_request(
            &client,
            &http_config,
            RequestSpec {
                auth: http_config.auth.as_ref(),
                method: &reqwest::Method::GET,
                url: &format!("http://{}/test", addr),
                task_headers: None,
                body: None,
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await;

        assert!(result.is_ok());
        let val = result.expect("test");
        assert_eq!(val["result"], "success");
    }

    #[tokio::test]
    async fn test_execute_request_with_headers_auth_and_body() {
        let mock_app = axum::Router::new().route(
            "/post-test",
            axum::routing::post(|| async { axum::Json(serde_json::json!({"received": true})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test");
        let addr = listener.local_addr().expect("test");
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.expect("test");
        });

        let client = reqwest::Client::new();
        let mut headers = std::collections::HashMap::new();
        headers.insert("x-custom".to_string(), "custom-value".to_string());

        let http_config = HttpConnectorConfig {
            retry_non_idempotent: false,
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::from([(
                "x-connector-header".to_string(),
                "conn-val".to_string(),
            )]),
            auth: Some(AuthConfig::Bearer {
                token: "test-token".to_string(),
            }),
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
            operations: Default::default(),
        };

        let body = serde_json::json!({"data": "payload"});

        let result = execute_request(
            &client,
            &http_config,
            RequestSpec {
                auth: http_config.auth.as_ref(),
                method: &reqwest::Method::POST,
                url: &format!("http://{}/post-test", addr),
                task_headers: Some(&headers),
                body: Some(&body),
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_request_non_success_status() {
        let mock_app = axum::Router::new().route(
            "/error",
            axum::routing::get(|| async { (axum::http::StatusCode::BAD_REQUEST, "Bad Request") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test");
        let addr = listener.local_addr().expect("test");
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.expect("test");
        });

        let client = reqwest::Client::new();
        let http_config = HttpConnectorConfig {
            retry_non_idempotent: false,
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
            operations: Default::default(),
        };

        let result = execute_request(
            &client,
            &http_config,
            RequestSpec {
                auth: http_config.auth.as_ref(),
                method: &reqwest::Method::GET,
                url: &format!("http://{}/error", addr),
                task_headers: None,
                body: None,
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await;

        assert!(result.is_err());
        let err = result.expect_err("test");
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn test_execute_request_non_json_response() {
        let mock_app = axum::Router::new().route(
            "/text",
            axum::routing::get(|| async { "plain text response" }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test");
        let addr = listener.local_addr().expect("test");
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.expect("test");
        });

        let client = reqwest::Client::new();
        let http_config = HttpConnectorConfig {
            retry_non_idempotent: false,
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
            operations: Default::default(),
        };

        let result = execute_request(
            &client,
            &http_config,
            RequestSpec {
                auth: http_config.auth.as_ref(),
                method: &reqwest::Method::GET,
                url: &format!("http://{}/text", addr),
                task_headers: None,
                body: None,
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await;

        // Should fail to parse as JSON
        assert!(result.is_err());
        assert!(result.expect_err("test").to_string().contains("parse"));
    }

    #[tokio::test]
    async fn test_execute_request_response_too_large() {
        let mock_app = axum::Router::new().route(
            "/large",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({"data": "x".repeat(200)}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test");
        let addr = listener.local_addr().expect("test");
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.expect("test");
        });

        let client = reqwest::Client::new();
        let http_config = HttpConnectorConfig {
            retry_non_idempotent: false,
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10,    // Very small limit
            allow_private_urls: true, // Tests use localhost
            operations: Default::default(),
        };

        let result = execute_request(
            &client,
            &http_config,
            RequestSpec {
                auth: http_config.auth.as_ref(),
                method: &reqwest::Method::GET,
                url: &format!("http://{}/large", addr),
                task_headers: None,
                body: None,
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await;

        assert!(result.is_err());
        assert!(result.expect_err("test").to_string().contains("exceed"));
    }

    #[tokio::test]
    async fn test_execute_request_timeout() {
        let mock_app = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                axum::Json(serde_json::json!({"slow": true}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test");
        let addr = listener.local_addr().expect("test");
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.expect("test");
        });

        let client = reqwest::Client::new();
        let http_config = HttpConnectorConfig {
            retry_non_idempotent: false,
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
            operations: Default::default(),
        };

        let result = execute_request(
            &client,
            &http_config,
            RequestSpec {
                auth: http_config.auth.as_ref(),
                method: &reqwest::Method::GET,
                url: &format!("http://{}/slow", addr),
                task_headers: None,
                body: None,
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_millis(100), // Very short timeout,
            },
        )
        .await;

        assert!(result.is_err());
        assert!(result.expect_err("test").to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn test_execute_request_connection_refused() {
        let client = reqwest::Client::new();
        let http_config = HttpConnectorConfig {
            retry_non_idempotent: false,
            url: "http://127.0.0.1:1".to_string(),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
            operations: Default::default(),
        };

        let result = execute_request(
            &client,
            &http_config,
            RequestSpec {
                auth: http_config.auth.as_ref(),
                method: &reqwest::Method::GET,
                url: "http://127.0.0.1:1/test",
                task_headers: None,
                body: None,
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_secs(1),
            },
        )
        .await;

        assert!(result.is_err());
        assert!(result.expect_err("test").to_string().contains("failed"));
    }

    /// Mirrors the production client (main.rs): the manual follower in
    /// `execute_request` only sees 3xx responses when reqwest doesn't follow.
    fn redirectless_client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("test")
    }

    fn localhost_config(addr: std::net::SocketAddr) -> HttpConnectorConfig {
        HttpConnectorConfig {
            retry_non_idempotent: false,
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
            operations: Default::default(),
        }
    }

    async fn spawn_mock(app: axum::Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test");
        let addr = listener.local_addr().expect("test");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test");
        });
        addr
    }

    #[tokio::test]
    async fn test_redirect_to_private_target_refused() {
        // allow_private_urls covers only the connector's own endpoint; a
        // redirect pointing anywhere else must still pass SSRF validation.
        let mock_app = axum::Router::new().route(
            "/redirect",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::FOUND,
                    [(
                        axum::http::header::LOCATION,
                        "http://169.254.169.254/latest/meta-data",
                    )],
                )
            }),
        );
        let addr = spawn_mock(mock_app).await;

        let result = execute_request(
            &redirectless_client(),
            &localhost_config(addr),
            RequestSpec {
                auth: None,
                method: &reqwest::Method::GET,
                url: &format!("http://{}/redirect", addr),
                task_headers: None,
                body: None,
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await;

        let err = result.expect_err("test").to_string();
        assert!(err.contains("SSRF protection"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_redirect_followed_within_own_endpoint() {
        let mock_app = axum::Router::new()
            .route(
                "/a",
                axum::routing::get(|| async {
                    (
                        axum::http::StatusCode::FOUND,
                        [(axum::http::header::LOCATION, "/b")],
                    )
                }),
            )
            .route(
                "/b",
                axum::routing::get(|| async { axum::Json(serde_json::json!({"hop": "b"})) }),
            );
        let addr = spawn_mock(mock_app).await;

        let result = execute_request(
            &redirectless_client(),
            &localhost_config(addr),
            RequestSpec {
                auth: None,
                method: &reqwest::Method::GET,
                url: &format!("http://{}/a", addr),
                task_headers: None,
                body: None,
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await;

        assert_eq!(result.expect("test")["hop"], "b");
    }

    #[tokio::test]
    async fn test_redirect_loop_is_capped() {
        let mock_app = axum::Router::new().route(
            "/loop",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::FOUND,
                    [(axum::http::header::LOCATION, "/loop")],
                )
            }),
        );
        let addr = spawn_mock(mock_app).await;

        let result = execute_request(
            &redirectless_client(),
            &localhost_config(addr),
            RequestSpec {
                auth: None,
                method: &reqwest::Method::GET,
                url: &format!("http://{}/loop", addr),
                task_headers: None,
                body: None,
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await;

        let err = result.expect_err("test").to_string();
        assert!(err.contains("redirects"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_redirect_303_downgrades_post_to_get() {
        let mock_app = axum::Router::new()
            .route(
                "/submit",
                axum::routing::post(|| async {
                    (
                        axum::http::StatusCode::SEE_OTHER,
                        [(axum::http::header::LOCATION, "/done")],
                    )
                }),
            )
            .route(
                "/done",
                // GET-only: the hop only succeeds if the method was downgraded
                axum::routing::get(|| async { axum::Json(serde_json::json!({"done": true})) }),
            );
        let addr = spawn_mock(mock_app).await;

        let body = serde_json::json!({"data": "payload"});
        let result = execute_request(
            &redirectless_client(),
            &localhost_config(addr),
            RequestSpec {
                auth: None,
                method: &reqwest::Method::POST,
                url: &format!("http://{}/submit", addr),
                task_headers: None,
                body: Some(&body),
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await;

        assert_eq!(result.expect("test")["done"], true);
    }

    // -- Format axes (#261) --

    #[test]
    fn test_format_value_tables() {
        // Absent and "json" are today's behavior; unknown values are refused
        // with the allowed set named — the same table validate_input enforces
        // at authoring time.
        assert_eq!(BodyFormat::parse(None).expect("test"), BodyFormat::Json);
        assert_eq!(
            BodyFormat::parse(Some("json")).expect("test"),
            BodyFormat::Json
        );
        assert_eq!(
            BodyFormat::parse(Some("form")).expect("test"),
            BodyFormat::Form
        );
        assert_eq!(
            BodyFormat::parse(Some("text")).expect("test"),
            BodyFormat::Text
        );
        let err = BodyFormat::parse(Some("multipart")).expect_err("test");
        assert!(err.contains("'json', 'form', 'text'"), "{err}");

        assert_eq!(
            ResponseFormat::parse(None).expect("test"),
            ResponseFormat::Json
        );
        assert_eq!(
            ResponseFormat::parse(Some("text")).expect("test"),
            ResponseFormat::Text
        );
        let err = ResponseFormat::parse(Some("base64")).expect_err("test");
        assert!(err.contains("'json', 'text'"), "{err}");
    }

    #[test]
    fn test_encode_form_scalars_arrays_and_null() {
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "retries": 3,
            "dry_run": false,
            "to": ["+15551111111", "+15552222222"],
            "optional": null,
        });
        let enc = encode_body(&body, BodyFormat::Form).expect("test");
        assert_eq!(enc.content_type, "application/x-www-form-urlencoded");
        let encoded = String::from_utf8(enc.bytes.to_vec()).expect("test");
        // Scalars canonical, arrays as repeated keys, nulls skipped, reserved
        // characters percent-encoded.
        assert_eq!(
            encoded,
            "grant_type=refresh_token&retries=3&dry_run=false\
             &to=%2B15551111111&to=%2B15552222222"
        );
    }

    #[test]
    fn test_encode_form_bracket_keys_need_no_special_support() {
        // Stripe-style nesting is an ordinary top-level key.
        let body = serde_json::json!({"metadata[order_id]": "6735"});
        let enc = encode_body(&body, BodyFormat::Form).expect("test");
        assert_eq!(
            String::from_utf8(enc.bytes.to_vec()).expect("test"),
            "metadata%5Border_id%5D=6735"
        );
    }

    #[test]
    fn test_encode_form_rejects_nesting_naming_the_key() {
        for body in [
            serde_json::json!({"ok": "v", "bad": {"nested": true}}),
            serde_json::json!({"ok": "v", "bad": [["nested"]]}),
            serde_json::json!({"ok": "v", "bad": [null]}),
        ] {
            let err = encode_body(&body, BodyFormat::Form)
                .expect_err("test")
                .to_string();
            assert!(err.contains("'bad'"), "should name the key: {err}");
        }
        // And a body that is not an object at all.
        let err = encode_body(&serde_json::json!(["a", "b"]), BodyFormat::Form)
            .expect_err("test")
            .to_string();
        assert!(err.contains("requires the body to be an object"), "{err}");
    }

    #[test]
    fn test_encode_text_requires_string() {
        let enc = encode_body(&serde_json::json!("<doc/>"), BodyFormat::Text).expect("test");
        assert_eq!(enc.content_type, "text/plain; charset=utf-8");
        assert_eq!(enc.bytes.as_ref(), b"<doc/>");

        let err = encode_body(&serde_json::json!({"a": 1}), BodyFormat::Text)
            .expect_err("test")
            .to_string();
        assert!(err.contains("requires the body to be a string"), "{err}");
    }

    /// Echoes back the request's body and every `content-type` header value,
    /// so wire tests can assert what actually left the client.
    fn echo_app() -> axum::Router {
        axum::Router::new().route(
            "/echo",
            axum::routing::post(|headers: axum::http::HeaderMap, body: String| async move {
                let content_types: Vec<String> = headers
                    .get_all("content-type")
                    .iter()
                    .map(|v| v.to_str().unwrap_or_default().to_string())
                    .collect();
                axum::Json(serde_json::json!({
                    "content_types": content_types,
                    "body": body,
                }))
            }),
        )
    }

    #[tokio::test]
    async fn test_form_body_on_the_wire() {
        let addr = spawn_mock(echo_app()).await;
        let body = serde_json::json!({"grant_type": "client_credentials", "scope": "a b"});

        let result = execute_request(
            &reqwest::Client::new(),
            &localhost_config(addr),
            RequestSpec {
                auth: None,
                method: &reqwest::Method::POST,
                url: &format!("http://{}/echo", addr),
                task_headers: None,
                body: Some(&body),
                body_format: BodyFormat::Form,
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await
        .expect("test");

        assert_eq!(result["body"], "grant_type=client_credentials&scope=a+b");
        assert_eq!(
            result["content_types"],
            serde_json::json!(["application/x-www-form-urlencoded"])
        );
    }

    #[tokio::test]
    async fn test_text_body_with_explicit_content_type_wins_alone() {
        // The task's explicit content-type replaces the stamp rather than
        // joining it — exactly one content-type header may reach the wire.
        let addr = spawn_mock(echo_app()).await;
        let body = serde_json::json!("<Envelope/>");
        let headers = std::collections::HashMap::from([(
            "Content-Type".to_string(),
            "application/xml".to_string(),
        )]);

        let result = execute_request(
            &reqwest::Client::new(),
            &localhost_config(addr),
            RequestSpec {
                auth: None,
                method: &reqwest::Method::POST,
                url: &format!("http://{}/echo", addr),
                task_headers: Some(&headers),
                body: Some(&body),
                body_format: BodyFormat::Text,
                response_format: ResponseFormat::default(),
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await
        .expect("test");

        assert_eq!(result["body"], "<Envelope/>");
        assert_eq!(
            result["content_types"],
            serde_json::json!(["application/xml"])
        );
    }

    #[tokio::test]
    async fn test_response_format_text_captures_plain_body() {
        let mock_app =
            axum::Router::new().route("/text", axum::routing::get(|| async { "OK id=12345" }));
        let addr = spawn_mock(mock_app).await;

        let result = execute_request(
            &reqwest::Client::new(),
            &localhost_config(addr),
            RequestSpec {
                auth: None,
                method: &reqwest::Method::GET,
                url: &format!("http://{}/text", addr),
                task_headers: None,
                body: None,
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::Text,
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await
        .expect("test");

        assert_eq!(result, serde_json::json!("OK id=12345"));
    }

    #[tokio::test]
    async fn test_response_format_text_keeps_error_and_size_paths() {
        // Only the final parse is format-dispatched: a non-2xx still errors,
        // and the streaming size cap still applies.
        let mock_app = axum::Router::new()
            .route(
                "/fail",
                axum::routing::get(|| async {
                    (axum::http::StatusCode::BAD_GATEWAY, "upstream said no")
                }),
            )
            .route("/large", axum::routing::get(|| async { "x".repeat(200) }));
        let addr = spawn_mock(mock_app).await;
        let mut config = localhost_config(addr);
        config.max_response_size = 50;

        let err = execute_request(
            &reqwest::Client::new(),
            &config,
            RequestSpec {
                auth: None,
                method: &reqwest::Method::GET,
                url: &format!("http://{}/fail", addr),
                task_headers: None,
                body: None,
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::Text,
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await
        .expect_err("test")
        .to_string();
        assert!(err.contains("502"), "{err}");

        let err = execute_request(
            &reqwest::Client::new(),
            &config,
            RequestSpec {
                auth: None,
                method: &reqwest::Method::GET,
                url: &format!("http://{}/large", addr),
                task_headers: None,
                body: None,
                body_format: BodyFormat::default(),
                response_format: ResponseFormat::Text,
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await
        .expect_err("test")
        .to_string();
        assert!(err.contains("exceed"), "{err}");
    }

    #[test]
    fn test_apply_auth_basic() {
        let client = reqwest::Client::new();
        let auth = AuthConfig::Basic {
            username: "user".to_string(),
            password: "pass".to_string(),
        };
        let req = apply_auth(client.get("http://localhost"), &auth);
        let built = req.build().expect("test");
        let auth_header = built
            .headers()
            .get("authorization")
            .expect("test")
            .to_str()
            .expect("test");
        assert!(auth_header.starts_with("Basic "));
    }
}

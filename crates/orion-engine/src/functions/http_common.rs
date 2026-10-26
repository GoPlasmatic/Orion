use std::time::Duration;

use serde_json::Value;

use crate::connector::{AuthConfig, HttpConnectorConfig, redact_url_secrets_or_raw};
use crate::{HandlerError, http_error};

// The failures this module reports are `HandlerError`'s classes — caller
// input, backend, connector, timeout — converted through the one table it
// keeps (I184). An upstream's non-2xx answer is the fifth, `http_error`.

/// How much of a non-2xx response body may reach an error message (#281).
///
/// The body of someone else's error is the single most useful thing for
/// diagnosing a failing integration and the single least predictable thing to
/// copy: a 4xx can echo back a token, an account record or a stack trace, and
/// no connector setting opts out. A preview keeps the diagnostic and bounds
/// the exposure — and bounds the row, since this string is persisted to
/// `traces` and `trace_dlq`, where the previous cap let a 10 MB error body
/// become a 10 MB row.
const ERROR_BODY_PREVIEW: usize = 512;

/// A URL as it may appear in an error message, a log line or a span field.
///
/// Every URL leaving this module as text goes through here. These strings are
/// persisted to `traces` and `trace_dlq`, logged, attached to an OTel span,
/// and — for the timeout arm — returned verbatim to an unauthenticated caller
/// in a `504` (`errors.rs`, which does not gate it on `verbose_errors`). A
/// credential authored into the connector URL would otherwise be readable in
/// all of them (#281).
///
/// Redaction is name-keyed: `redact_url_secrets_or_raw` masks the userinfo
/// password and every query value whose name satisfies `is_secret_key`, so
/// `?pwd=` masks and `?pass=` does not. This closes the common shapes, not a
/// determined one — the connector-level answer is `query_params` (#277),
/// which keeps the credential out of the URL string entirely.
fn safe_url(url: &str) -> String {
    redact_url_secrets_or_raw(url)
}

/// Build a URL from a base URL and optional path segment.
///
/// Both sides may carry a query string, and the join has to keep them apart:
/// the previous implementation concatenated unconditionally, so a connector
/// URL of `https://h/api?a=1` with a task path `/orders` produced
/// `https://h/api?a=1/orders` — the path spliced into the query value. Split
/// each side at its first `?`, join the paths, then reassemble with the base
/// query first and the task query appended.
///
/// Fragments are not a supported shape — they are never sent on the wire — so
/// this splits on `?` only and leaves a `#` wherever it lands.
pub fn build_url(base: &str, path: Option<&str>) -> String {
    let (base_path, base_query) = split_query(base);
    let (task_path, task_query) = match path {
        Some(p) if !p.is_empty() => {
            let (p, q) = split_query(p);
            (Some(p), q)
        }
        _ => (None, None),
    };

    let mut url = match task_path {
        Some(p) => format!(
            "{}/{}",
            base_path.trim_end_matches('/'),
            p.trim_start_matches('/')
        ),
        None => base_path.to_string(),
    };
    // Base query first, task query appended — the precedence the docs state.
    match (base_query, task_query) {
        (Some(b), Some(t)) => {
            url.push('?');
            url.push_str(b);
            url.push('&');
            url.push_str(t);
        }
        (Some(q), None) | (None, Some(q)) => {
            url.push('?');
            url.push_str(q);
        }
        (None, None) => {}
    }
    url
}

/// Split a URL or path at its first `?` into (everything before, the query).
/// An empty query (`"/x?"`) yields `None` rather than an empty parameter list.
fn split_query(s: &str) -> (&str, Option<&str>) {
    match s.split_once('?') {
        Some((head, query)) if !query.is_empty() => (head, Some(query)),
        Some((head, _)) => (head, None),
        None => (s, None),
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
pub fn oauth_error_to_dataflow(e: crate::connector::oauth::OAuthError) -> dataflow_rs::Error {
    if e.retryable() {
        HandlerError::connector(e).into()
    } else {
        crate::service::connector_detail(e)
    }
}

/// Redirect-hop cap for the manual follower in [`execute_request`]. The shared
/// client is built with `redirect::Policy::none()` (see `bootstrap`), so every hop
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
                    HandlerError::caller_input(format!(
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
                return Err(HandlerError::caller_input(format!(
                    "body_format 'text' requires the body to be a string, got {}",
                    crate::utils::json_kind(other)
                ))
                .into());
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
        return Err(HandlerError::caller_input(format!(
            "body_format 'form' requires the body to be an object of key/value pairs, got {}",
            crate::utils::json_kind(body)
        ))
        .into());
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

fn form_value_error(key: &str, value: &Value) -> dataflow_rs::Error {
    HandlerError::caller_input(format!(
        "body_format 'form' cannot encode '{key}' ({}): entries must be scalars \
         or arrays of scalars — form encoding has no canonical nesting",
        crate::utils::json_kind(value)
    ))
    .into()
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
    fields(method = ?spec.method, url = %safe_url(spec.url), timeout = ?spec.timeout)
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
    // `Validation` reaches the caller as a 400 with the message verbatim
    // (`errors.rs`), so this one is caller-visible even before the trace.
    let original = url::Url::parse(url)
        .map_err(|e| HandlerError::caller_input(format!("Invalid URL '{}': {e}", safe_url(url))))?;
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
            && let Err(msg) = orion_common::ssrf::validate_url_not_private(current.as_str()).await
        {
            return Err(HandlerError::backend(format!("SSRF protection: {msg}")).into());
        }

        let mut req = client
            .request(method.clone(), current.clone())
            .timeout(timeout);

        // Inject W3C trace context headers (traceparent/tracestate) for distributed tracing
        {
            let mut trace_headers = std::collections::HashMap::new();
            crate::trace_context::inject_trace_context(&mut trace_headers);
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

            // Connector query parameters. Applied here rather than merged into
            // the URL so `current` — the SSRF-validated string that every
            // error message and the `url` OTel field interpolate — never
            // carries a credential. `RequestBuilder::query` also
            // percent-encodes, so a secret containing `&`, `=` or a space
            // works, where URL interpolation would break it silently.
            if !http_config.query_params.is_empty() {
                // The map serializes directly, so this allocates nothing —
                // and it runs once per redirect hop, per call.
                req = req.query(&http_config.query_params);
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
            let url = safe_url(current.as_str());
            if e.is_timeout() {
                HandlerError::timeout(format!("HTTP request to {url} timed out"))
            } else {
                // `without_url` because reqwest's own `Display` appends the URL
                // it was handed — unredacted, and a second time alongside ours.
                HandlerError::connector(format!(
                    "HTTP request to {url} failed: {}",
                    e.without_url()
                ))
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

    Err(HandlerError::backend(format!(
        "Stopped after {MAX_REDIRECTS} redirects requesting {}",
        safe_url(url)
    ))
    .into())
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
        HandlerError::backend(format!(
            "Redirect from {} has a non-ASCII Location header",
            safe_url(current.as_str())
        ))
    })?;
    let next = current.join(location).map_err(|e| {
        HandlerError::backend(format!(
            "Redirect from {} has invalid Location '{}': {e}",
            safe_url(current.as_str()),
            // The Location is a URL the *upstream* chose; it can carry a
            // credential of its own, so it is redacted like any other.
            safe_url(location)
        ))
    })?;
    if !matches!(next.scheme(), "http" | "https") {
        return Err(HandlerError::backend(format!(
            "Redirect from {} targets unsupported scheme '{}'",
            safe_url(current.as_str()),
            next.scheme()
        ))
        .into());
    }
    Ok(Some(next))
}

/// Read a (non-redirect) response body per `format`, enforcing `max_size`.
///
/// Only the final parse step is format-dispatched — the size cap, the non-2xx
/// error path, and the streaming reads are identical for every format.
///
/// The cap is enforced *while streaming*, by [`crate::http_body`]: a chunked
/// response with no `Content-Length` must not get to sit fully in memory
/// before the size check — that is the exact OOM the limit exists to prevent.
/// This function had the only correct implementation of that in the tree, and
/// the three egress paths that copied the shape around it did not; it is a
/// shared primitive now, and what stays here is the interpretation — which
/// statuses are failures, and how the body is parsed.
async fn read_response(
    response: reqwest::Response,
    url: &url::Url,
    max_size: usize,
    format: ResponseFormat,
) -> dataflow_rs::Result<Value> {
    let status = response.status();
    let safe = safe_url(url.as_str());

    // The declared-length refusal is taken before the status branch, so a
    // non-2xx that announces an oversized body is refused as oversized rather
    // than quoted back — the preview below would otherwise be the only thing
    // standing between that body and the trace it lands in.
    crate::http_body::check_declared_length(&response, max_size)
        .map_err(|e| HandlerError::backend(format!("Response from {safe}: {e}")))?;

    if !status.is_success() {
        // Read at most `ERROR_BODY_PREVIEW` bytes of the error body for the
        // message; anything beyond that is dropped, never buffered. Capped
        // well under `max_size` deliberately — this body is someone else's
        // error text headed for a persisted trace, not data the workflow
        // asked for.
        let preview =
            crate::http_body::read_preview(response, max_size.min(ERROR_BODY_PREVIEW)).await;
        return Err(http_error(
            status.as_u16(),
            format!("HTTP {status} from {safe}: {}", preview.to_message()),
        ));
    }

    let body_bytes = crate::http_body::read_bounded(response, max_size)
        .await
        .map_err(|e| HandlerError::backend(format!("Response from {safe}: {e}")))?;

    match format {
        ResponseFormat::Json => serde_json::from_slice(&body_bytes).map_err(|e| {
            HandlerError::backend(format!("Failed to parse response from {safe} as JSON: {e}"))
                .into()
        }),
        // Lossy on invalid UTF-8, matching the error-path body capture above.
        ResponseFormat::Text => Ok(Value::String(
            String::from_utf8_lossy(&body_bytes).into_owned(),
        )),
    }
}

#[cfg(test)]
#[path = "http_common_tests.rs"]
mod tests;

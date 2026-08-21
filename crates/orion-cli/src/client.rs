//! The CLI's view of [`orion_client::OrionClient`]: same transport, plus the
//! presentation this binary owns — actionable hints, the `[CODE] message`
//! error format, and field-error rendering.
//!
//! The transport itself (auth, envelope parsing, typed errors) lives in the
//! shared `orion-client` crate — the same client the server's `package`
//! subcommand drives. This wrapper only translates its typed
//! [`ClientError`] into the terminal-ready `anyhow` messages the CLI has
//! always printed.

use anyhow::Result;
use orion_api::codes;
use orion_client::{ClientError, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct OrionClient {
    inner: orion_client::OrionClient,
}

impl OrionClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let inner = orion_client::OrionClient::new(base_url)
            .map_err(|e| anyhow::anyhow!(e).context("Failed to create HTTP client"))?;
        Ok(Self { inner })
    }

    pub fn base_url(&self) -> &str {
        self.inner.base_url()
    }

    pub fn with_api_key(self, api_key: String, header: Option<String>) -> Self {
        Self {
            inner: self.inner.with_api_key(api_key, header),
        }
    }

    /// Stamp every request with `X-Orion-Change-Context`, so the audit rows a
    /// multi-command operation writes carry the caller's own label.
    pub fn with_change_context(self, context: String) -> Self {
        Self {
            inner: self.inner.with_change_context(context),
        }
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.inner.get(path).await.map_err(render)
    }

    pub async fn get_text(&self, path: &str) -> Result<String> {
        self.inner.get_text(path).await.map_err(render)
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        self.inner.post(path, body).await.map_err(render)
    }

    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.inner.post_empty(path).await.map_err(render)
    }

    pub async fn put<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        self.inner.put(path, body).await.map_err(render)
    }

    pub async fn patch<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        self.inner.patch(path, body).await.map_err(render)
    }

    pub async fn delete_request(&self, path: &str) -> Result<()> {
        self.inner.delete(path).await.map_err(render)
    }
}

/// Translate a typed transport error into the message this CLI has always
/// printed: `[CODE] message`, indented field errors, `request_id`, then an
/// actionable hint keyed on the shared `orion_api::codes` registry.
fn render(err: ClientError) -> anyhow::Error {
    match err {
        ClientError::Connect { url, source } => {
            anyhow::anyhow!(source).context(connection_hint(&url))
        }
        ClientError::Api { error, .. } => {
            let code = if error.code.is_empty() {
                "UNKNOWN"
            } else {
                error.code.as_str()
            };
            let msg = if error.message.is_empty() {
                "Unknown error"
            } else {
                error.message.as_str()
            };
            let mut out = format!("[{code}] {msg}");
            out.push_str(&format_field_errors(&error.details));
            if let Some(request_id) = &error.request_id {
                out.push_str(&format!("\n  request_id: {request_id}"));
            }
            out.push_str(error_hint(code));
            anyhow::anyhow!(out)
        }
        ClientError::Http { status, .. } if status == StatusCode::UNAUTHORIZED => {
            anyhow::anyhow!(
                "Unauthorized ({status})\n  Hint: Check your --api-key or ORION_API_KEY environment variable"
            )
        }
        ClientError::Http { status, .. } => anyhow::anyhow!("Request failed ({status})"),
        ClientError::Decode { source } => {
            anyhow::anyhow!(source).context("Failed to parse response JSON")
        }
        ClientError::Transport { source } => {
            anyhow::anyhow!(source).context("Failed to read response body")
        }
    }
}

fn connection_hint(url: &str) -> String {
    format!(
        "Failed to connect to {url}\n  \
         Hint: Is the server running? Check with 'orion-cli config show' or use '--server <url>'"
    )
}

// Keyed on the shared `orion_api::codes` registry — the codes the server
// actually emits.
fn error_hint(code: &str) -> &'static str {
    match code {
        codes::UNAUTHORIZED => {
            "\n  Hint: Check your --api-key or ORION_API_KEY environment variable"
        }
        codes::NOT_FOUND => {
            "\n  Hint: Verify the resource ID exists with the corresponding 'list' command"
        }
        // Resource-neutral: the same code comes back from the channel and
        // connector endpoints, and naming one command sent readers of the
        // other two to the wrong place.
        codes::VALIDATION_ERROR => {
            "\n  Hint: Check the JSON structure. Use '<workflows|channels|connectors> validate -f <file>' to check a definition without writing it"
        }
        codes::CONFLICT => "\n  Hint: A resource with this ID or name may already exist",
        _ => "",
    }
}

/// Render the structured `error.details[]` array (field-pathed validation
/// errors) as indented lines. Returns an empty string when the server is
/// v0.1 or the array is empty.
fn format_field_errors(details: &[orion_api::FieldError]) -> String {
    let mut out = String::new();
    for item in details {
        out.push_str("\n  - ");
        if !item.path.is_empty() {
            out.push_str(&format!("{}: ", item.path));
        }
        out.push_str(&item.message);
        match (&item.expected, &item.got) {
            (Some(exp), Some(got)) => {
                out.push_str(&format!(
                    " (expected {}, got {})",
                    compact_value(exp),
                    compact_value(got)
                ));
            }
            (Some(exp), None) => out.push_str(&format!(" (expected {})", compact_value(exp))),
            (None, Some(got)) => out.push_str(&format!(" (got {})", compact_value(got))),
            (None, None) => {}
        }
    }
    out
}

/// Render a JSON value compactly for inline error messages: strings unquoted,
/// arrays joined with `|`, everything else via its compact JSON form.
fn compact_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(compact_value)
            .collect::<Vec<_>>()
            .join("|"),
        other => other.to_string(),
    }
}

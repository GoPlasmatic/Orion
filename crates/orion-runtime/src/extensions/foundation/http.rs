use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{
    AttemptContext, ConnectorError, ConnectorOutput, DRIVER_HTTP, EffectStatus, FoundationOptions,
    ImplementationKind, ProviderBackend, codes, governed_connection, governed_secret,
    refuse_input_target,
};
use crate::extensions::deployment_store::ImmutableDeploymentStore;

/// Community foundation HTTP/REST connector provider (`org.orion.http`).
#[derive(Default)]
pub struct HttpProviderBackend {
    deployments: Option<ImmutableDeploymentStore>,
    options: FoundationOptions,
}

impl std::fmt::Debug for HttpProviderBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpProviderBackend").finish()
    }
}

impl HttpProviderBackend {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_deployment_store(deployments: ImmutableDeploymentStore) -> Self {
        Self {
            deployments: Some(deployments),
            options: FoundationOptions::default(),
        }
    }

    /// The same provider, making its calls on `options.http_client`.
    pub fn with_options(mut self, options: &FoundationOptions) -> Self {
        self.options = options.clone();
        self
    }
}

/// How an HTTP connection authenticates, from `public_config.auth`, in the
/// HTTP connector's own shape. The secret fields (`token`, `password`, `key`)
/// are never in `public_config`: each comes from the secret locator at its
/// path (`auth.token`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HttpAuth {
    /// `{"type": "bearer"}` + `auth.token`: `Authorization: Bearer <token>`.
    Bearer,
    /// `{"type": "basic", "username": "svc"}` + `auth.password`.
    Basic { username: String },
    /// `{"type": "api_key", "header": "X-API-Key"}` + `auth.key`: the named
    /// header carries the key.
    ApiKey(reqwest::header::HeaderName),
}

impl HttpAuth {
    /// The secret locator path this scheme's credential comes from.
    fn secret_path(&self) -> &'static str {
        match self {
            Self::Bearer => "auth.token",
            Self::Basic { .. } => "auth.password",
            Self::ApiKey(_) => "auth.key",
        }
    }
}

/// Headers an `api_key` auth may not name: the request's framing and
/// routing are Orion's, not the credential's.
const RESERVED_AUTH_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
];

/// The `auth` object of an HTTP connection's `public_config`, read strictly:
/// a known `type`, the public fields that type needs, and nothing else.
pub(super) fn http_auth(config: &Value) -> Result<Option<HttpAuth>, ConnectorError> {
    let Some(auth) = config.get("auth") else {
        return Ok(None);
    };
    let invalid = |message: String, pointer: &str| {
        ConnectorError::invalid_input(message, Some(json!({"pointer": pointer})))
    };
    let Some(object) = auth.as_object() else {
        return Err(invalid(
            "HTTP 'auth' must be an object".to_string(),
            "/auth",
        ));
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let allowed: &[&str] = match kind {
        "bearer" => &["type"],
        "basic" => &["type", "username"],
        "api_key" => &["type", "header"],
        _ => {
            return Err(invalid(
                format!("HTTP 'auth.type' must be 'bearer', 'basic' or 'api_key' (got '{kind}')"),
                "/auth/type",
            ));
        }
    };
    if let Some(extra) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid(
            format!(
                "HTTP 'auth' of type '{kind}' does not take '{extra}' in public_config \
                 (a secret field comes from secret_locators)"
            ),
            &format!("/auth/{extra}"),
        ));
    }
    let field = |name: &str| {
        object
            .get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                invalid(
                    format!("HTTP 'auth' of type '{kind}' requires a string '{name}'"),
                    &format!("/auth/{name}"),
                )
            })
    };
    Ok(Some(match kind {
        "bearer" => HttpAuth::Bearer,
        "api_key" => {
            let name = field("header")?;
            let header = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| invalid(format!("'{name}' is not a header name"), "/auth/header"))?;
            if RESERVED_AUTH_HEADERS.contains(&header.as_str()) {
                return Err(invalid(
                    format!("HTTP 'auth' may not carry its key in '{name}'"),
                    "/auth/header",
                ));
            }
            HttpAuth::ApiKey(header)
        }
        _ => HttpAuth::Basic {
            username: field("username")?.to_string(),
        },
    }))
}

/// `request` carrying `secret` the way `auth` says, the header marked
/// sensitive so it never reaches a debug log.
pub(super) fn authorize(
    request: reqwest::RequestBuilder,
    auth: &HttpAuth,
    secret: &str,
) -> Result<reqwest::RequestBuilder, ConnectorError> {
    Ok(match auth {
        HttpAuth::Bearer => request.bearer_auth(secret),
        HttpAuth::Basic { username } => request.basic_auth(username, Some(secret)),
        HttpAuth::ApiKey(name) => {
            let mut value = reqwest::header::HeaderValue::from_str(secret).map_err(|_| {
                ConnectorError::invalid_input(
                    "HTTP connection secret is not a valid header value",
                    None,
                )
            })?;
            value.set_sensitive(true);
            request.header(name.clone(), value)
        }
    })
}

#[async_trait::async_trait]
impl ProviderBackend for HttpProviderBackend {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Rust
    }

    fn driver_id(&self) -> &str {
        DRIVER_HTTP
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        let Some(url) = config.get("url").and_then(Value::as_str) else {
            return Err(ConnectorError::invalid_input(
                "HTTP connector config requires a string 'url'",
                Some(json!({"pointer": "/url"})),
            ));
        };
        let trimmed = url.trim();
        if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
            return Err(ConnectorError::invalid_input(
                "HTTP connector 'url' must start with http:// or https://",
                Some(json!({"pointer": "/url"})),
            ));
        }
        http_auth(config)?;
        if let Some(methods) = config
            .get("operations")
            .and_then(|o| o.get("methods"))
            .and_then(Value::as_array)
        {
            for (idx, m) in methods.iter().enumerate() {
                let Some(verb) = m.as_str() else {
                    return Err(ConnectorError::invalid_input(
                        "HTTP operation method must be a string",
                        Some(json!({"pointer": format!("/operations/methods/{idx}")})),
                    ));
                };
                if !crate::connector::VALID_HTTP_METHODS
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(verb))
                {
                    return Err(ConnectorError::invalid_input(
                        format!("Unsupported HTTP method '{verb}' in operations.methods"),
                        Some(json!({"pointer": format!("/operations/methods/{idx}")})),
                    ));
                }
            }
        }
        Ok(())
    }

    async fn invoke(
        &self,
        ctx: &AttemptContext,
        operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError> {
        ctx.check_live()?;

        let method = match operation.to_ascii_lowercase().as_str() {
            "get" | "post" | "put" | "patch" | "delete" => operation.to_ascii_uppercase(),
            "request" => input
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_ascii_uppercase(),
            other => {
                return Err(ConnectorError::forbidden(
                    codes::OPERATION_FORBIDDEN,
                    "dispatch",
                    format!("Unsupported HTTP operation '{other}'"),
                    ctx.effect_status(),
                ));
            }
        };

        if !crate::connector::VALID_HTTP_METHODS.contains(&method.as_str()) {
            return Err(ConnectorError::invalid_input(
                format!("Unsupported HTTP method '{method}'"),
                Some(json!({"pointer": "/method"})),
            ));
        }

        refuse_input_target(&input, &["url", "base_url"])?;
        let path = input.get("path").and_then(Value::as_str).unwrap_or("/");
        if !path.starts_with('/') {
            return Err(ConnectorError::invalid_input(
                "HTTP request 'path' must start with '/'",
                Some(json!({"pointer": "/path"})),
            ));
        }

        let identity = ctx.effective_identity();
        let conn = governed_connection(self.deployments.as_ref(), identity);
        let url = conn.as_ref().and_then(|conn| {
            conn.public_config
                .get("url")
                .or_else(|| conn.public_config.get("base_url"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        let Some((url, conn)) = url.zip(conn) else {
            return Err(ConnectorError::invalid_input(
                format!(
                    "HTTP connector invocation on '{}' requires a connection revision with a 'url'",
                    identity.connection_id
                ),
                Some(json!({"pointer": "/url"})),
            ));
        };

        // The credential is settled before anything leaves the node: a
        // revision whose `auth` has no secret locator for its credential is refused
        // without a DNS lookup.
        let credential = match http_auth(&conn.public_config)? {
            None => None,
            Some(auth) => {
                let path = auth.secret_path();
                let Some(secret) = governed_secret(&conn, path, identity, DRIVER_HTTP).await?
                else {
                    return Err(ConnectorError::invalid_input(
                        format!(
                            "HTTP connection '{}' declares 'auth' but has no secret \
                             locator for '{path}'",
                            identity.connection_id
                        ),
                        Some(json!({"pointer": "/auth"})),
                    ));
                };
                Some((auth, secret))
            }
        };

        if let Err(ssrf_reason) = orion_common::ssrf::validate_url_not_private(&url).await {
            return Err(ConnectorError::forbidden(
                codes::CAPABILITY_INVALID,
                "ssrf_validation",
                format!("Outbound HTTP target blocked by SSRF guard: {ssrf_reason}"),
                ctx.effect_status(),
            ));
        }

        let base = url.trim_end_matches('/');
        let full_url = format!("{base}{path}");
        let req_method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| {
            ConnectorError::invalid_input(
                format!("Invalid HTTP method '{method}': {e}"),
                Some(json!({"pointer": "/method"})),
            )
        })?;
        let mut req_builder = self.options.http_client.request(req_method, &full_url);
        if let Some((auth, secret)) = &credential {
            req_builder = authorize(req_builder, auth, secret)?;
        }
        if let Some(body_val) = input.get("body")
            && !body_val.is_null()
        {
            req_builder = req_builder.json(body_val);
        }
        ctx.record_effect(EffectStatus::Unknown);
        let resp = req_builder.send().await.map_err(|e| {
            ConnectorError::unavailable(
                format!("HTTP request failed: {}", e.without_url()),
                EffectStatus::Unknown,
            )
        })?;
        let status_code = resp.status().as_u16();
        let bytes = crate::http_body::read_bounded(resp, self.options.http_max_response_bytes)
            .await
            .map_err(|e| {
                ConnectorError::unavailable(
                    format!("Failed to read HTTP response body: {e}"),
                    EffectStatus::Completed,
                )
            })?;
        ctx.record_effect(EffectStatus::Completed);
        let parsed_body: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        let mut metadata = BTreeMap::new();
        metadata.insert("driver".to_string(), json!(DRIVER_HTTP));
        metadata.insert("attempt".to_string(), json!(ctx.attempt_number()));
        Ok(ConnectorOutput::with_metadata(
            json!({
                "status_code": status_code,
                "method": method,
                "path": path,
                "body": parsed_body,
            }),
            metadata,
        ))
    }
}

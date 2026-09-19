//! How the apply engine reaches a target's admin API.
//!
//! [`AdminApi`] is one request method plus the envelope-unwrapping verbs
//! every caller uses. The CLI implements it with `orion_client::OrionClient`
//! over HTTP; the server applying a package at startup uses
//! [`InProcessAdmin`], which sends the same requests through its own admin
//! router with no socket — no admin key a `sha256:`-only configuration
//! could not provide, no TLS name to verify, no rate limiter in the way —
//! and the same handlers, gates and audit rows.
//!
//! Both answer with `orion_client::ClientError`, so a failure reads the
//! same (`HTTP 409 CONFLICT: …`) whichever transport carried it.

use async_trait::async_trait;
use orion_client::{ClientError, OrionClient, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// A request method, as the transport sends it.
pub use axum::http::Method;

/// One target's admin API.
#[async_trait]
pub trait AdminApi: Send + Sync {
    /// One request to `path` (an `orion_client::paths` string). `Ok` is the
    /// 2xx body as JSON — `Null` when it is empty — with no envelope
    /// removed; a non-2xx answer is the `ClientError` it classifies as.
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ClientError>;

    /// `GET`, the body as it came.
    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError>
    where
        Self: Sized,
    {
        decode(self.request(Method::GET, path, None).await?)
    }

    /// `GET`, the `{"data": …}` envelope unwrapped.
    async fn get_data<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError>
    where
        Self: Sized,
    {
        decode(unwrap_data(self.request(Method::GET, path, None).await?))
    }

    /// Like [`Self::get_data`], but a 404 answers `Ok(None)`.
    async fn get_data_opt<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>, ClientError>
    where
        Self: Sized,
    {
        match self.request(Method::GET, path, None).await {
            Ok(value) => decode(unwrap_data(value)).map(Some),
            Err(e) if e.status() == Some(StatusCode::NOT_FOUND) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn post_data<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError>
    where
        Self: Sized,
    {
        decode(unwrap_data(
            self.request(Method::POST, path, Some(body)).await?,
        ))
    }

    /// `POST` with no body.
    async fn post_data_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError>
    where
        Self: Sized,
    {
        decode(unwrap_data(self.request(Method::POST, path, None).await?))
    }

    async fn put_data<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError>
    where
        Self: Sized,
    {
        decode(unwrap_data(
            self.request(Method::PUT, path, Some(body)).await?,
        ))
    }

    async fn patch_data<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ClientError>
    where
        Self: Sized,
    {
        decode(unwrap_data(
            self.request(Method::PATCH, path, Some(body)).await?,
        ))
    }

    async fn delete(&self, path: &str) -> Result<(), ClientError>
    where
        Self: Sized,
    {
        self.request(Method::DELETE, path, None).await.map(|_| ())
    }
}

/// The `{"data": …}` envelope's payload, tolerating the bare pre-1.0 shape
/// — the rule `OrionClient` applies.
fn unwrap_data(value: Value) -> Value {
    match value {
        Value::Object(mut map) if map.contains_key("data") => {
            map.remove("data").unwrap_or(Value::Null)
        }
        other => other,
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ClientError> {
    serde_json::from_value(value).map_err(|source| ClientError::Decode { source })
}

#[async_trait]
impl AdminApi for OrionClient {
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ClientError> {
        // `OrionClient`'s own verbs, so its auth, change context and error
        // classification apply unchanged.
        match (method, body) {
            (Method::GET, _) => OrionClient::get(self, path).await,
            (Method::POST, Some(body)) => OrionClient::post(self, path, body).await,
            (Method::POST, None) => OrionClient::post_empty(self, path).await,
            (Method::PUT, Some(body)) => OrionClient::put(self, path, body).await,
            (Method::PATCH, Some(body)) => OrionClient::patch(self, path, body).await,
            (Method::DELETE, _) => OrionClient::delete(self, path).await.map(|()| Value::Null),
            (method, _) => unreachable!("the apply engine sends no {method} request"),
        }
    }
}

/// The admin API of *this* server, reached through its own router.
///
/// The router is the admin routes with the server's state and none of
/// `build_router`'s layers: no admin auth (the caller is the server), no
/// rate limiter. Each request carries `principal` as its admin principal
/// and `change_context` in the request context, so a receipt and every
/// audit row say who did it and as part of what.
pub struct InProcessAdmin {
    router: axum::Router,
    principal: String,
    change_context: String,
}

impl InProcessAdmin {
    pub fn new(
        state: crate::server::state::AppState,
        principal: &str,
        change_context: String,
    ) -> Self {
        let admin = crate::server::routes::admin::admin_routes(
            state.config.server.max_admin_body_size,
            crate::server::plugin_body_size(&state.config),
        );
        Self {
            router: axum::Router::new()
                .nest("/api/v1/admin", admin)
                .with_state(state),
            principal: principal.to_string(),
            change_context,
        }
    }
}

#[async_trait]
impl AdminApi for InProcessAdmin {
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ClientError> {
        use tower::ServiceExt as _;
        let mut builder = axum::http::Request::builder().method(method).uri(path);
        let body = match body {
            Some(body) => {
                builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
                axum::body::Body::from(
                    serde_json::to_vec(body).map_err(|source| ClientError::Decode { source })?,
                )
            }
            None => axum::body::Body::empty(),
        };
        let mut request = builder
            .body(body)
            .expect("a method, a path and a body build");
        request
            .extensions_mut()
            .insert(crate::server::admin_auth::AdminPrincipal {
                key_id: self.principal.clone(),
            });
        let context = crate::request_context::RequestContext {
            request_id: uuid::Uuid::new_v4().to_string(),
            client_ip: String::new(),
            user_agent: None,
            change_context: Some(self.change_context.clone()),
        };
        let response = crate::request_context::REQUEST_CONTEXT
            .scope(context, self.router.clone().oneshot(request))
            .await
            .unwrap_or_else(|never| match never {});
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .map_err(|e| ClientError::Http {
                status,
                body: e.to_string(),
            })?;
        if !status.is_success() {
            return Err(ClientError::from_response(status, &bytes));
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|source| ClientError::Decode { source })
    }
}

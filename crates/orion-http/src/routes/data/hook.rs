//! A request to a hooked channel: its driver answers it.
//!
//! `dynamic_handler` takes this branch for an active channel whose driver
//! declares `http_hook` (`HttpServing::Hook`), before it reads the body as
//! JSON, because a hooked body is the driver's to read. What happens here, in
//! order:
//!
//! 1. The request's guard row (`Transport::HttpHook`): rate limit, auth (with
//!    the principal limit), origin, and the deduplication and backpressure
//!    the driver declared. A refusal answers as it does on the sync path.
//! 2. The driver's view of the request ([`HttpRequest`]): the raw body, the
//!    headers less the hop-by-hop and credential ones, the verified claims.
//! 3. `serve_http`, with a [`RequestHost`] executing every run on the
//!    generation this request loaded, then released.
//! 4. The driver's answer, as an HTTP response.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::response::Response;
use orion_extension_api::{ChannelBinding, ChannelDriver, HttpBody, HttpHookGuards, HttpRequest};
use serde_json::json;

use crate::channel::ChannelRuntimeConfig;
use crate::channel::guards;
use crate::errors::OrionError;
use crate::http_hook::RequestHost;
use crate::runtime::RuntimeGeneration;
use crate::runtime::state::DataPlane;

/// Headers a driver is not handed: the hop-by-hop ones, which describe this
/// connection rather than the request, and the credential ones, which the
/// host has already verified (the driver reads the caller from the claims).
const WITHHELD_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "authorization",
    "proxy-authorization",
    "cookie",
];

/// One request to a hooked channel, as the handler resolved it.
pub(super) struct Hooked<'a> {
    pub generation: Arc<RuntimeGeneration>,
    pub runtime: Arc<ChannelRuntimeConfig>,
    pub driver: Arc<dyn ChannelDriver>,
    pub guards: HttpHookGuards,
    pub method: &'a axum::http::Method,
    pub path: &'a str,
    pub headers: &'a axum::http::HeaderMap,
    pub peer: Option<&'a std::net::SocketAddr>,
    pub query: &'a HashMap<String, String>,
    pub route_params: &'a HashMap<String, String>,
    pub body: axum::body::Bytes,
}

pub(super) async fn serve(state: &DataPlane, req: Hooked<'_>) -> Result<Response, OrionError> {
    let Hooked {
        generation,
        runtime,
        driver,
        guards: declared,
        method,
        path,
        headers,
        peer,
        query,
        route_params,
        body,
    } = req;
    let channel = runtime.channel.name.clone();

    // 1. The request's row. The context carries no payload: validation is
    //    off on this row (it judges each run), and nothing else reads `data`.
    let header_lookup = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let client_ip =
        crate::rate_limit::request_client_ip_from_parts(peer, headers, &state.trusted_proxies);
    let mut context = guards::GuardContext::new(json!({}), json!({ "channel": channel }));
    let channel_runtime = Some(runtime.clone());
    let admission = guards::admit(guards::GuardRequest {
        transport: guards::Transport::HttpHook(declared),
        channel: &channel,
        runtime: &channel_runtime,
        context: &mut context,
        exprs: &state.exprs,
        origin: headers.get("origin").and_then(|v| v.to_str().ok()),
        caller_identity: &client_ip,
        auth_backoff: Some(&state.channel_auth_failures),
        header: &header_lookup,
        // An HMAC signature is computed over the bytes as received.
        raw_body: Some(&body),
        dedup_key_fallback: None,
        // One request, one delivery: a replay of the key is a `409`.
        dedup_owner: None,
        default_timeout_ms: None,
        max_timeout_ms: None,
        oauth: None,
    })
    .await;
    let admission = match admission {
        Ok(admission) => admission,
        Err(refused) => return Ok(super::shape_guard_rejection(refused, &channel, &runtime)),
    };
    // Held until the answer is complete; the claim stands for the window.
    let _permit = admission.backpressure_permit;
    let _claim = admission.dedup_claim;

    // 2. The driver's view of the request.
    let request_id = orion_common::request_context::request_id().unwrap_or_default();
    let mut request =
        HttpRequest::new(method.as_str(), path, body.to_vec()).with_request_id(request_id.clone());
    request.query = query
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect::<BTreeMap<_, _>>();
    request.route_params = route_params
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect::<BTreeMap<_, _>>();
    request.headers = headers
        .iter()
        .filter(|(name, _)| !WITHHELD_HEADERS.contains(&name.as_str()))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), v.to_string()))
        })
        .collect();
    request.principal = admission.auth_claims.clone();

    let binding = generation
        .channels
        .hook_binding(&runtime.channel.channel_id)
        .cloned()
        .unwrap_or_else(|| {
            // Every hooked channel's binding is built with its generation;
            // reaching here is a generation published without them (the
            // empty first one cannot hold a channel). Serve it without
            // connection resolvers rather than not at all.
            ChannelBinding::new(
                runtime.channel.channel_id.clone(),
                runtime.channel.name.clone(),
                String::new(),
                serde_json::from_str(&runtime.channel.transport_config_json)
                    .unwrap_or(serde_json::Value::Null),
            )
            .with_timeout_ms(runtime.parsed_config.timeout_ms)
        });

    // 3. The driver, with every run on this request's generation.
    let host = RequestHost::new(
        state.hook.clone(),
        declared,
        runtime.clone(),
        generation,
        request_id,
        admission.auth_claims,
    );
    let answered = driver.serve_http(&binding, request, host.clone()).await;
    host.release_generation();

    // 4. The answer.
    let answer = answered.map_err(|e| {
        tracing::error!(
            channel = %channel,
            protocol = %runtime.channel.protocol,
            error = %e,
            "A hooked channel's driver failed to serve a request"
        );
        OrionError::internal(format!(
            "the '{}' driver failed to serve the request",
            runtime.channel.protocol
        ))
    })?;
    let status = axum::http::StatusCode::from_u16(answer.status)
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    let body = match answer.body {
        HttpBody::Full(bytes) => axum::body::Body::from(bytes),
        HttpBody::Stream(_) => {
            tracing::error!(
                channel = %channel,
                "A hooked channel's driver answered a streamed body, which this node does not serve"
            );
            return Err(OrionError::internal(
                "the driver answered a streamed body, which this node does not serve",
            ));
        }
        _ => {
            return Err(OrionError::internal(
                "the driver answered a body of a kind this node does not serve",
            ));
        }
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    super::apply_headers(&mut response, &answer.headers, &channel);
    Ok(response)
}

/// The runtime config `generation` serves `channel` with, when the data plane
/// serves it through its driver's hook.
pub(super) fn hooked(
    generation: &RuntimeGeneration,
    channel: &str,
) -> Option<(
    Arc<ChannelRuntimeConfig>,
    Arc<dyn ChannelDriver>,
    HttpHookGuards,
)> {
    let runtime = generation.channels.get_by_name(channel)?;
    let crate::channel::HttpServing::Hook { driver, guards } = &runtime.http else {
        return None;
    };
    let (driver, guards) = (driver.clone(), *guards);
    Some((runtime, driver, guards))
}

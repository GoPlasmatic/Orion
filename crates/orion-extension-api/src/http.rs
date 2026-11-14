//! The data-plane HTTP hook: how a driver serves its channels over the node's
//! own HTTP data plane with an answer of its own choosing.
//!
//! A driver that declares [`ChannelDriverCapabilities::http_hook`](crate::ChannelDriverCapabilities::http_hook)
//! is handed each request to one of its channels as an [`HttpRequest`],
//! through [`ChannelDriver::serve_http`](crate::ChannelDriver::serve_http).
//! It runs the channel's workflow zero or more times through the
//! [`HttpHost`] and answers with an [`HttpResponse`], whose body may be a
//! stream (server-sent events) that outlives the request handler.
//!
//! The split is the ingress one. The **host** owns every guard and every run:
//! the request's rate limit, authentication and origin check run before the
//! driver sees the request, the guards the driver declared
//! ([`HttpHookGuards`]) run per request or per run as documented there, and
//! each [`HttpHost::run`] is traced and counted as one message. The **driver**
//! owns the protocol: parsing the body, deciding how many runs it takes, and
//! shaping the answer.
//!
//! The request handler loads the runtime generation once and every run made
//! while `serve_http` is pending executes on it. A streamed body is different:
//! once it is returned, the generation is released, and each later run loads
//! the current one and executes only while the channel is unchanged. A run
//! after the channel was redeployed answers
//! [`DispatchOutcome::Unavailable`]; end
//! the stream then, so the client reconnects to the new channel.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ingress::{DispatchOutcome, NodeInfo};

/// Which guards a hooked channel runs beyond the ones that are always on for
/// it: the rate limit, authentication (with its principal limit) and the
/// origin allow-list, each once per request when the channel configures it.
///
/// Start from [`HttpHookGuards::none`] and enable what the protocol uses.
/// Authoring refuses a channel of the protocol that configures a guard left
/// off here, so none is accepted and then silently not applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HttpHookGuards {
    /// The channel's `validation_logic`, judged against each run's `data`
    /// (what the driver runs, not the raw request body).
    pub validation: bool,
    /// Idempotency-key deduplication, once per request: a replayed key is
    /// answered `409` before the driver sees it.
    pub deduplication: bool,
    /// The channel's concurrency permit, once per request. A streamed body
    /// holds it until the stream ends or the client disconnects, so
    /// `backpressure.max_concurrent_per_node` caps a node's open streams.
    pub backpressure: bool,
}

impl HttpHookGuards {
    /// No guard beyond the always-on ones.
    pub const fn none() -> Self {
        Self {
            validation: false,
            deduplication: false,
            backpressure: false,
        }
    }

    /// Judge each run's `data` with the channel's `validation_logic`.
    #[must_use]
    pub const fn validation(mut self) -> Self {
        self.validation = true;
        self
    }

    /// Deduplicate requests by idempotency key.
    #[must_use]
    pub const fn deduplication(mut self) -> Self {
        self.deduplication = true;
        self
    }

    /// Hold the channel's concurrency permit for the request (and its stream).
    #[must_use]
    pub const fn backpressure(mut self) -> Self {
        self.backpressure = true;
        self
    }
}

/// One request to a hooked channel, as its driver sees it. Built by the host.
///
/// The body is already capped by `ingest.max_payload_size`, and is the bytes
/// as received: a hooked body need not be JSON.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct HttpRequest {
    /// The HTTP method, uppercase.
    pub method: String,
    /// The path below the data plane's mount, without a leading `/`.
    pub path: String,
    /// The query string's parameters, decoded.
    pub query: BTreeMap<String, String>,
    /// The request headers, names lowercased. Hop-by-hop headers and the
    /// credential headers (`authorization`, `proxy-authorization`, `cookie`)
    /// are not passed on: the host authenticated the request, and the
    /// driver reads who the caller is from [`Self::principal`].
    pub headers: Vec<(String, String)>,
    /// The route's captured parameters, percent-decoded once.
    pub route_params: BTreeMap<String, String>,
    /// The body, as received.
    pub body: Vec<u8>,
    /// The verified claims, when the channel authenticates with a token.
    /// Every run also sees them at `metadata.auth.claims`.
    pub principal: Option<Value>,
    /// The request's id (`x-request-id`), stamped on every run's trigger as
    /// `request_id` so its runs can be grouped.
    pub request_id: String,
}

impl HttpRequest {
    /// A request of `method` to `path` carrying `body`; everything else
    /// empty.
    pub fn new(method: impl Into<String>, path: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            query: BTreeMap::new(),
            headers: Vec::new(),
            route_params: BTreeMap::new(),
            body,
            principal: None,
            request_id: String::new(),
        }
    }

    /// One query parameter.
    #[must_use]
    pub fn with_query(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.insert(name.into(), value.into());
        self
    }

    /// One header; the name is lowercased.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers
            .push((name.into().to_ascii_lowercase(), value.into()));
        self
    }

    /// One route parameter.
    #[must_use]
    pub fn with_route_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.route_params.insert(name.into(), value.into());
        self
    }

    /// The verified claims.
    #[must_use]
    pub fn with_principal(mut self, claims: Value) -> Self {
        self.principal = Some(claims);
        self
    }

    /// The request's id.
    #[must_use]
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = request_id.into();
        self
    }

    /// The first value of header `name`, compared case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A driver's answer to one request.
#[derive(Debug)]
#[non_exhaustive]
pub struct HttpResponse {
    /// The status code. One outside `100..=999` answers `500`.
    pub status: u16,
    /// Headers, in order; a name may repeat.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: HttpBody,
}

impl HttpResponse {
    /// A complete body.
    pub fn full(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: HttpBody::Full(body.into()),
        }
    }

    /// A streamed body (see [`BodyStream`]).
    pub fn stream(status: u16, body: Box<dyn BodyStream>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: HttpBody::Stream(body),
        }
    }

    /// One header.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// An [`HttpResponse`]'s body.
#[non_exhaustive]
pub enum HttpBody {
    /// Every byte, now.
    Full(Vec<u8>),
    /// Chunks, as the stream yields them, until it ends.
    Stream(Box<dyn BodyStream>),
}

impl std::fmt::Debug for HttpBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(bytes) => f.debug_tuple("Full").field(&bytes.len()).finish(),
            Self::Stream(_) => f.write_str("Stream(..)"),
        }
    }
}

/// A streamed body.
///
/// [`next`](Self::next) must be cancel-safe: a client disconnect, or the
/// node's force timeout, drops the stream at whichever await it is parked
/// on. End the stream (return `None`) when [`HttpHost::shutdown`] resolves,
/// so the node drains without cutting it off.
#[async_trait]
pub trait BodyStream: Send {
    /// The next chunk, or `None` when the body is complete.
    async fn next(&mut self) -> Option<Vec<u8>>;
}

/// Resolves once, when the node stops serving: after `/readyz` has gone
/// unready and the drain period has let the load balancer stop routing here.
pub type ShutdownSignal = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// What the node offers a hooked driver for one request. Implemented by the
/// host.
#[async_trait]
pub trait HttpHost: Send + Sync {
    /// The protocol this host serves.
    fn protocol(&self) -> &str;

    /// The node this driver runs on.
    fn node(&self) -> &NodeInfo;

    /// Run the channel's workflow once over `data`, with `trigger` facts at
    /// `metadata.trigger` (`type` is set to the protocol, `request_id` to the
    /// request's). The run passes the guards [`HttpHookGuards`] puts on a run,
    /// is traced with the protocol as its `mode`, and is counted on the
    /// channel's message counters.
    async fn run(&self, data: Value, trigger: Map<String, Value>) -> DispatchOutcome;

    /// Resolves when the node is draining. A long-lived stream ends then.
    fn shutdown(&self) -> ShutdownSignal;
}

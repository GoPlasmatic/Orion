use std::borrow::Cow;

pub use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

// The wire half of this module — the `{"error": {...}}` envelope, the
// per-field detail shape, and the stable `error.code` registry — lives in the
// shared `orion-api` crate so the CLI reads the same definitions the server
// writes. `FieldError` is re-exported under its pre-1.0 path; everything else
// in this file (the `OrionError` domain, status mapping, redaction policy)
// stays server-side on purpose.
use orion_api::{ErrorBody, ErrorEnvelope, codes};

pub use orion_api::FieldError;

/// The RFC 6750 §3 `WWW-Authenticate` challenge a refused bearer token
/// answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BearerChallenge {
    /// `Bearer`, with no error code: the request presented no bearer
    /// credential at all (§3.1).
    Bare,
    /// `Bearer error="invalid_token"`, with the one description a client acts
    /// on when there is one.
    InvalidToken(Option<&'static str>),
}

impl BearerChallenge {
    /// The header value.
    pub fn header_value(self) -> String {
        match self {
            Self::Bare => "Bearer".to_string(),
            Self::InvalidToken(None) => "Bearer error=\"invalid_token\"".to_string(),
            Self::InvalidToken(Some(desc)) => {
                format!("Bearer error=\"invalid_token\", error_description=\"{desc}\"")
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OrionError {
    /// A refusal that is nothing but its HTTP status, wire code and a message
    /// the caller may read: not found, unauthorized, forbidden, conflict,
    /// payload too large, unsupported media type, method not allowed, upgrade
    /// required. Built with the constructor of that name
    /// ([`OrionError::not_found`], …); `Display` keeps the label each had as
    /// a variant of its own ("Not found: …"), so logs and persisted error text
    /// read as they did.
    #[error("{}: {message}", http_label(*status))]
    Http {
        status: StatusCode,
        code: &'static str,
        message: String,
    },

    /// A request the server refuses to act on, with structured per-field
    /// details when the refusal names a field. Maps to HTTP 400 with `code`
    /// defaulting to `VALIDATION_ERROR`. `details` is omitted from the
    /// response body when empty so clients expecting only the v0.1
    /// `{code, message}` envelope still work.
    ///
    /// G11: `BadRequest(String)` used to sit beside this variant. Both were
    /// 400, validators mixed them freely, and two identical `remap_to_field`
    /// helpers existed purely to convert one into the other — so the pair
    /// carried a distinction no caller could rely on. One variant now; the
    /// former `BAD_REQUEST` code answers `VALIDATION_ERROR` like the rest.
    #[error("Validation failed: {message}")]
    Validation {
        code: &'static str,
        message: String,
        details: Vec<FieldError>,
    },

    /// A refused bearer token (#267): 401 carrying `WWW-Authenticate: Bearer`
    /// (RFC 6750). `challenge` is everything the wire says about the cause —
    /// see `jwt::RejectReason::challenge`; the body stays uniform.
    #[error("Unauthorized: {message}")]
    UnauthorizedToken {
        message: String,
        challenge: BearerChallenge,
    },

    /// An internal failure, with the cause attached when there is one.
    ///
    /// G11: this was two variants. `Internal(String)` was `InternalSource`
    /// minus a cause, and since G2 routed every 5xx through one redaction
    /// policy the pair became indistinguishable at the boundary — both answer
    /// 500 `INTERNAL_ERROR` with "An internal error occurred" and log the
    /// detail. Two variants that provably differ in nothing a caller or an
    /// operator can observe are one variant with an optional field.
    #[error("{context}")]
    Internal {
        context: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("Configuration error: {message}")]
    Config { message: String },

    #[error("Rate limited: {0}")]
    RateLimited(String),

    /// A channel's `rate_limit.key_logic` could not be evaluated, so the
    /// request cannot be metered into any bucket (N5 refuses rather than
    /// counting it in the wrong one).
    ///
    /// Answers `429` like [`OrionError::RateLimited`], because a caller that
    /// cannot be metered must be told to back off and there is nothing
    /// actionable to disclose about a channel's own logic. It is a separate
    /// variant because the *condition* is not the same: being over a limit is
    /// transient and clears with time, while a key expression that fails on
    /// this payload will fail on every redelivery of it. A transport that
    /// retries can read that difference — retry the first, give up on the
    /// second.
    #[error("Rate limit key unavailable: {0}")]
    RateLimitKeyUnavailable(String),

    #[error("Response too large: {0}")]
    ResponseTooLarge(String),

    #[error("Service unavailable: {message}")]
    ServiceUnavailable {
        reason: Unavailable,
        message: String,
    },

    #[error("Timeout: channel '{channel}' exceeded {timeout_ms}ms")]
    Timeout { channel: String, timeout_ms: u64 },

    #[error("Storage error: {0}")]
    Storage(#[from] sqlx::Error),

    #[error("Engine error: {0}")]
    Engine(#[from] dataflow_rs::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Why a request was refused with `503`.
///
/// `ServiceUnavailable` carried only a formatted string, and four unrelated
/// causes arrived through it: a channel quarantined at load, a guard whose
/// backend is down, a queue or channel at capacity, and a queue whose consumer
/// has died. They differ in the one thing a `503` is supposed to tell a
/// caller — *come back, and when* — and nothing could tell them apart without
/// reading English. `is_retryable` said `true` for all four, which is wrong
/// for two of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// The channel failed to load and is refused at every ingress (F35).
    /// **Not transient**: it clears when an operator fixes the definition and
    /// reloads, not on any timescale a client should wait out.
    ChannelQuarantined,
    /// A guard's backend — the rate-limit store, the dedup store — could not
    /// be reached and the channel is configured to fail closed. Transient:
    /// the backend comes back.
    GuardBackend,
    /// The channel, node or queue is at capacity. Transient and short — this
    /// is backpressure working, and the caller should retry soon.
    AtCapacity,
    /// The queue behind this request has no consumer: its worker died. **Not
    /// transient** without operator action, and the node reports it on
    /// `/health` (`components.background_tasks`).
    QueueClosed,
}

impl Unavailable {
    /// Whether a client retrying, on its own, could succeed.
    pub fn is_transient(self) -> bool {
        matches!(self, Self::GuardBackend | Self::AtCapacity)
    }

    /// Seconds to put in `Retry-After`, or `None` when retrying is not the
    /// answer. A `503` with no hint leaves every client to invent its own
    /// backoff; a `503` that will never clear should not invite one at all.
    pub fn retry_after_secs(self) -> Option<u32> {
        match self {
            // Backpressure clears as fast as the work in front of it.
            Self::AtCapacity => Some(1),
            // A backend outage is not measured in single seconds.
            Self::GuardBackend => Some(5),
            Self::ChannelQuarantined | Self::QueueClosed => None,
        }
    }
}

/// The label an [`OrionError::Http`] refusal's `Display` opens with.
fn http_label(status: StatusCode) -> &'static str {
    match status {
        StatusCode::NOT_FOUND => "Not found",
        StatusCode::UNAUTHORIZED => "Unauthorized",
        StatusCode::FORBIDDEN => "Forbidden",
        StatusCode::CONFLICT => "Conflict",
        StatusCode::PAYLOAD_TOO_LARGE => "Payload too large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "Unsupported media type",
        StatusCode::METHOD_NOT_ALLOWED => "Method not allowed",
        StatusCode::UPGRADE_REQUIRED => "Upgrade required",
        _ => status.canonical_reason().unwrap_or("Error"),
    }
}

impl OrionError {
    fn http(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        OrionError::Http {
            status,
            code,
            message: message.into(),
        }
    }

    /// 404 `NOT_FOUND`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::http(StatusCode::NOT_FOUND, codes::NOT_FOUND, message)
    }

    /// 401 `UNAUTHORIZED`, with no bearer challenge (see [`OrionError::UnauthorizedToken`]).
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::http(StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED, message)
    }

    /// 403 `FORBIDDEN`.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::http(StatusCode::FORBIDDEN, codes::FORBIDDEN, message)
    }

    /// 409 `CONFLICT`.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::http(StatusCode::CONFLICT, codes::CONFLICT, message)
    }

    /// 415 `UNSUPPORTED_MEDIA_TYPE`.
    pub fn unsupported_media_type(message: impl Into<String>) -> Self {
        Self::http(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            codes::UNSUPPORTED_MEDIA_TYPE,
            message,
        )
    }

    /// 405 `METHOD_NOT_ALLOWED`.
    pub fn method_not_allowed(message: impl Into<String>) -> Self {
        Self::http(
            StatusCode::METHOD_NOT_ALLOWED,
            codes::METHOD_NOT_ALLOWED,
            message,
        )
    }

    /// 426 `UPGRADE_REQUIRED`: a pre-v2 write refused once v2 deployments are active (V2-07.5).
    pub fn upgrade_required(message: impl Into<String>) -> Self {
        Self::http(
            StatusCode::UPGRADE_REQUIRED,
            codes::UPGRADE_REQUIRED,
            message,
        )
    }

    /// 413 `PAYLOAD_TOO_LARGE`: the client sent more than the plane's body limit allows — a 413 that carries the error envelope like every other refusal, unlike the bare `Bytes` extractor's own.
    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::http(
            StatusCode::PAYLOAD_TOO_LARGE,
            codes::PAYLOAD_TOO_LARGE,
            message,
        )
    }
}

impl OrionError {
    /// Whether this error is likely transient and the operation could succeed on retry.
    pub fn is_retryable(&self) -> bool {
        match self {
            OrionError::Storage(_) => true,
            OrionError::Engine(e) => e.is_retryable(),
            OrionError::RateLimited(_) => true,
            OrionError::ServiceUnavailable { reason, .. } => reason.is_transient(),
            OrionError::Timeout { .. } => true,
            _ => false,
        }
    }

    /// A `503` whose cause the caller can act on.
    pub fn unavailable(reason: Unavailable, message: impl Into<String>) -> Self {
        OrionError::ServiceUnavailable {
            reason,
            message: message.into(),
        }
    }

    /// An internal failure with no cause to attach.
    pub fn internal(context: impl Into<String>) -> Self {
        OrionError::Internal {
            context: context.into(),
            source: None,
        }
    }

    /// An internal failure carrying the error that caused it. The cause reaches
    /// the log, never the client.
    pub fn internal_from(
        context: impl Into<String>,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        OrionError::Internal {
            context: context.into(),
            source: Some(source.into()),
        }
    }

    /// Construct a `Validation` error with no field details yet. For the
    /// common single-field case use [`OrionError::invalid_field`].
    pub fn validation(message: impl Into<String>) -> Self {
        OrionError::Validation {
            code: codes::VALIDATION_ERROR,
            message: message.into(),
            details: Vec::new(),
        }
    }

    /// Build a single-field validation error in one call. Most validators
    /// only fail on one field at a time — this keeps the call site tight.
    pub fn invalid_field(
        path: impl Into<String>,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        let message = message.into();
        OrionError::Validation {
            code: codes::VALIDATION_ERROR,
            message: message.clone(),
            details: vec![FieldError::new(path, code, message)],
        }
    }
}

impl OrionError {
    /// The HTTP status, error code, and **client-safe** message for this error.
    ///
    /// This is the single place the redaction policy lives. It used to be a
    /// per-arm convention inside `IntoResponse` — three different 5xx shapes
    /// coexisted, and whether an internal string reached the client was decided
    /// one arm at a time. That is how G2, G3, G5 and G8 each became true
    /// independently, and why nothing outside `IntoResponse` could ask "is this
    /// message safe to show?" — which is what let the bulk-import handlers
    /// embed raw driver text in a 200 body.
    ///
    /// The message is borrowed when the error's own text is what the client
    /// sees, and owned when it is redacted or formatted, so asking costs no
    /// copy. Does not log: callers that surface the error (`IntoResponse`) log
    /// the internal detail separately via `log_internal_detail`.
    pub fn response_parts(&self) -> (StatusCode, &'static str, Cow<'_, str>) {
        match self {
            OrionError::Http {
                status,
                code,
                message,
            } => (*status, code, Cow::Borrowed(message.as_str())),
            OrionError::Validation { code, message, .. } => (
                StatusCode::BAD_REQUEST,
                code,
                Cow::Borrowed(message.as_str()),
            ),
            OrionError::UnauthorizedToken { message, .. } => (
                StatusCode::UNAUTHORIZED,
                codes::UNAUTHORIZED,
                Cow::Borrowed(message.as_str()),
            ),
            OrionError::ServiceUnavailable { message, .. } => (
                StatusCode::SERVICE_UNAVAILABLE,
                codes::SERVICE_UNAVAILABLE,
                Cow::Borrowed(message.as_str()),
            ),
            OrionError::RateLimited(msg) | OrionError::RateLimitKeyUnavailable(msg) => (
                StatusCode::TOO_MANY_REQUESTS,
                codes::RATE_LIMITED,
                Cow::Borrowed(msg.as_str()),
            ),
            OrionError::Timeout {
                channel,
                timeout_ms,
            } => (
                StatusCode::GATEWAY_TIMEOUT,
                codes::TIMEOUT,
                Cow::Owned(format!(
                    "Workflow execution on channel '{channel}' exceeded {timeout_ms}ms timeout"
                )),
            ),
            // G11: this answered 502 `BAD_GATEWAY`, but the condition is a
            // workflow *result* exceeding the operator's own
            // `trace_queue.max_result_size_bytes` cap — no upstream is
            // involved, so nothing here is a gateway. It is the server
            // refusing to send what it built: a 500, under a code distinct
            // enough to diagnose. The message stays verbatim; it names two
            // byte counts and nothing sensitive.
            // The caller's own doing and fixable by them: a 4xx, with the
            // message naming the limit rather than the raw buffering error.
            OrionError::ResponseTooLarge(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::RESPONSE_TOO_LARGE,
                Cow::Borrowed(msg.as_str()),
            ),
            // G2: `Internal` and `Config` used to return their message verbatim.
            // Reachable `Config` messages carry filesystem paths (TLS cert/key)
            // and whole database URLs — `detect_backend` is called on a
            // *connector's* connection string at request time, so a DSN with
            // credentials could round-trip into a 500 body.
            OrionError::Internal { .. } => (
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::INTERNAL_ERROR,
                Cow::Borrowed("An internal error occurred"),
            ),
            OrionError::Config { .. } => (
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::CONFIG_ERROR,
                Cow::Borrowed("A configuration error occurred"),
            ),
            OrionError::Storage(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::STORAGE_ERROR,
                Cow::Borrowed("An internal storage error occurred"),
            ),
            OrionError::Engine(e) => {
                let (status, code, message) = engine_error_response(e);
                (status, code, Cow::Owned(message))
            }
            // G14 (revising G8): every `?` reach of this variant is
            // server-side — decoding stored rows the repositories wrote, or
            // serializing a response — because client-input parses all handle
            // their errors explicitly (the import loop, `OrionQuery`, the
            // axum extractors, and the dry-run endpoint's conversion of a
            // client-authored draft, which maps its failure to a 400).
            // G8 answered 400 on the theory that inbound
            // parses landed here too; they do not, and a 400 hid these
            // server bugs from 5xx error-rate SLOs. The raw serde message
            // carries byte offsets and type names, so it is still not
            // surfaced (`log_internal_detail` records it).
            OrionError::Serialization(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::SERIALIZATION_ERROR,
                Cow::Borrowed("An internal serialization error occurred"),
            ),
        }
    }

    /// The client-safe message alone. Use this anywhere an error string is
    /// embedded in a response body outside `IntoResponse` — notably the bulk
    /// import handlers, which report per-item failures inside a 200.
    pub fn client_message(&self) -> String {
        self.response_parts().2.into_owned()
    }

    /// Emit the internal detail to the log for the variants whose client-facing
    /// message is redacted. No-op for variants that surface their own message.
    fn log_internal_detail(&self) {
        match self {
            OrionError::Internal {
                context,
                source: None,
            } => {
                tracing::error!(error.category = "internal", error = %context, "internal error")
            }
            OrionError::Config { message } => {
                tracing::error!(error.category = "config", error = %message, "config error")
            }
            OrionError::Storage(e) => {
                tracing::error!(error.category = "storage", error = %e, "storage error")
            }
            OrionError::Serialization(e) => {
                tracing::error!(error.category = "serialization", error = %e, "serialization error")
            }
            // G15: the exception to "no-op for variants that surface their own
            // message" — the client does see the byte counts, but the operator
            // who owns the cap gets no server-side record without this, and
            // the cap firing is an operations signal (raise the cap or shrink
            // the result), not a client mistake. Which cap fired is named by
            // the construction site's detail, not here — this mapper serves
            // every site, so a knob name hardcoded here becomes a lie the day
            // a second one appears.
            OrionError::ResponseTooLarge(detail) => {
                tracing::error!(error.category = "response_too_large", error = %detail, "response exceeded a configured size cap")
            }
            OrionError::Internal {
                context,
                source: Some(source),
            } => tracing::error!(
                error.category = "internal",
                error.context = %context,
                error.source = %source,
                "Internal error"
            ),
            OrionError::Engine(e) => {
                tracing::error!(error.category = "engine", error = %e, "Engine error")
            }
            _ => {}
        }
    }
}

impl OrionError {
    /// The structured field errors a validation refusal carries, empty for
    /// every other variant.
    ///
    /// Exists so a caller can report them *as* structured errors. The set check
    /// used to reach for `to_string()` here, which flattened a whole vector of
    /// paths and codes into one prose line.
    pub fn field_errors(&self) -> &[FieldError] {
        match self {
            OrionError::Validation { details, .. } => details,
            _ => &[],
        }
    }
}

impl IntoResponse for OrionError {
    fn into_response(self) -> Response {
        self.log_internal_detail();
        let (status, code, message) = self.response_parts();
        let message = message.into_owned();
        let retry_after = match &self {
            OrionError::ServiceUnavailable { reason, .. } => reason.retry_after_secs(),
            _ => None,
        };
        // What the body and headers need beyond the message, moved out of the
        // error rather than copied: the field errors and the bearer challenge.
        let (details, bearer_challenge) = match self {
            OrionError::Validation { details, .. } => (details, None),
            OrionError::UnauthorizedToken { challenge, .. } => {
                (Vec::new(), Some(challenge.header_value()))
            }
            _ => (Vec::new(), None),
        };

        // The shared envelope's serde attributes carry the compat rules this
        // used to spell imperatively: `details` is omitted when empty (v0.1
        // clients), `request_id` when the per-request scope isn't populated
        // (e.g. unit tests calling IntoResponse directly, or an inbound
        // request with no x-request-id for the SetRequestIdLayer).
        let body = ErrorEnvelope {
            error: ErrorBody {
                code: code.to_string(),
                message,
                details,
                request_id: crate::request_context::request_id(),
            },
        };

        let mut response = (status, axum::Json(body)).into_response();
        // A 429 always carries `retry-after`. It used to be added by the rate
        // limit middleware, which was the only producer; the per-channel limit
        // now refuses from the ingress guards (S15), so the header belongs to
        // the error rather than to one of its two call sites.
        if status == StatusCode::TOO_MANY_REQUESTS {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
            );
        }
        // A 503 says the same thing, when there is something to say. Before
        // `Unavailable` there was nothing to say it *with*: four causes shared
        // one string variant, so every 503 either carried no hint or the wrong
        // one. A cause that will not clear on its own — a quarantined channel,
        // a queue whose consumer died — deliberately sends no header rather
        // than inviting a retry loop against a node an operator has to fix.
        if let Some(secs) = retry_after {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from(secs),
            );
        }
        // A refused bearer token answers with the RFC 6750 challenge — the
        // same error-owns-its-header principle as `retry-after` above.
        if let Some(challenge) = bearer_challenge
            && let Ok(value) = axum::http::HeaderValue::from_str(&challenge)
        {
            response
                .headers_mut()
                .insert(axum::http::header::WWW_AUTHENTICATE, value);
        }
        response
    }
}

/// The codes Orion's functions give a service error
/// ([`dataflow_rs::Error::service`]). The engine never interprets them; the
/// error's `Display` is its caller-safe message alone, and the operator-only
/// detail rides as its source, so `to_string()` on one of these is always
/// safe to hand to an untrusted caller.
pub mod kind {
    /// An open circuit breaker shed the request before it reached a connector.
    pub const CIRCUIT_OPEN: &str = "circuit_open";
    /// A refusal whose text names a connector or other internal topology. G3:
    /// "operation 'delete' is disabled on connector 'prod-billing-db'" hands
    /// out connector inventory to an anonymous caller, so the caller gets a
    /// generic message and the real text rides in `detail`.
    pub const CONNECTOR_DETAIL: &str = "connector_detail";
    /// N16: a target channel's own ingress guards refused an in-process
    /// `channel_call`. One kind per outcome, so the status is declared by the
    /// producer rather than re-derived from a number.
    pub const CHANNEL_RATE_LIMITED: &str = "channel_rate_limited";
    pub const CHANNEL_FORBIDDEN: &str = "channel_forbidden";
    pub const CHANNEL_CONFLICT: &str = "channel_conflict";
    pub const CHANNEL_UNAVAILABLE: &str = "channel_unavailable";
    /// A `channel_call` refused by the recursion guards: a cycle, or a chain
    /// past the maximum depth. A configuration loop, so neither the caller's
    /// fault nor retryable.
    pub const CHANNEL_LOOP: &str = "channel_loop";
    /// The backend was reached and refused a statement against a rule its own
    /// schema declares. One kind per rule, so a workflow can answer a
    /// duplicate differently from a dangling reference — a single
    /// "integrity" code would only move the problem one level down.
    ///
    /// These are the four constraint kinds `sqlx::error::ErrorKind` names,
    /// which is what makes them portable: every driver maps its own SQLSTATE
    /// or errno table onto that enum, so the classification is the same
    /// sentence on SQLite, MySQL and Postgres.
    pub const INTEGRITY_UNIQUE: &str = "integrity_unique";
    pub const INTEGRITY_FOREIGN_KEY: &str = "integrity_foreign_key";
    pub const INTEGRITY_NOT_NULL: &str = "integrity_not_null";
    pub const INTEGRITY_CHECK: &str = "integrity_check";
}

/// The label a class code's message opens with, written by
/// `engine::HandlerError`: taken off, the message is what the caller reads.
fn without_label<'m>(message: &'m str, label: &str) -> &'m str {
    message.strip_prefix(label).unwrap_or(message)
}

/// Map an engine error to its HTTP status, code and caller-safe message.
fn engine_error_response(e: &dataflow_rs::Error) -> (StatusCode, &'static str, String) {
    use dataflow_rs::ErrorKind;

    let message = e.message();
    match e.kind() {
        // A function-classified failure answers for itself, with no ordering
        // hazard between guards and no risk of a genuine downstream 503
        // relayed by `http_call` being mistaken for one of Orion's own
        // refusals.
        ErrorKind::Service { code } => {
            let (status, wire) = match code.as_ref() {
                kind::CIRCUIT_OPEN => (StatusCode::SERVICE_UNAVAILABLE, codes::CIRCUIT_OPEN),
                kind::CONNECTOR_DETAIL => (StatusCode::BAD_REQUEST, codes::VALIDATION_ERROR),
                kind::CHANNEL_RATE_LIMITED => (StatusCode::TOO_MANY_REQUESTS, codes::RATE_LIMITED),
                kind::CHANNEL_FORBIDDEN => (StatusCode::FORBIDDEN, codes::FORBIDDEN),
                kind::CHANNEL_CONFLICT => (StatusCode::CONFLICT, codes::CONFLICT),
                kind::CHANNEL_UNAVAILABLE => {
                    (StatusCode::SERVICE_UNAVAILABLE, codes::SERVICE_UNAVAILABLE)
                }
                // Verbatim, like a validation refusal: the chain it names is
                // what makes the loop findable from the response.
                kind::CHANNEL_LOOP => (StatusCode::LOOP_DETECTED, codes::LOOP_DETECTED),
                // A duplicate or a dangling reference is a conflict of state:
                // the request was well-formed and the caller retries it
                // differently.
                kind::INTEGRITY_UNIQUE | kind::INTEGRITY_FOREIGN_KEY => {
                    (StatusCode::CONFLICT, codes::CONFLICT)
                }
                // A missing required value or a failed CHECK is a value the
                // caller sent wrong — the same shape as a failed field rule.
                kind::INTEGRITY_NOT_NULL | kind::INTEGRITY_CHECK => {
                    (StatusCode::BAD_REQUEST, codes::VALIDATION_ERROR)
                }
                // A caller-fixable refusal or a configured cap: workflow
                // structural and safe — "max call depth 10 exceeded", "'delete'
                // has no filter" — and verbatim, because it is what makes a
                // misconfigured workflow diagnosable from the response.
                "VALIDATION_ERROR" => {
                    return (
                        StatusCode::BAD_REQUEST,
                        codes::VALIDATION_ERROR,
                        without_label(message, "Validation error: ").to_string(),
                    );
                }
                // G13: same wire code as `OrionError::Timeout` — a caller keying
                // on the 504 `code` must not have to know which layer timed out.
                "TIMEOUT_ERROR" => {
                    return (
                        StatusCode::GATEWAY_TIMEOUT,
                        codes::TIMEOUT,
                        without_label(message, "Timeout error: ").to_string(),
                    );
                }
                other => {
                    tracing::error!(code = %other, error = %e, "engine error mapped to 500");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        codes::ENGINE_ERROR,
                        "An internal engine error occurred".to_string(),
                    );
                }
            };
            (status, wire, message.to_string())
        }
        // `engine.ops_budget` crossed: a configured cap, so the same `400`
        // with the message preserved that every other limit gets, because
        // "N operations charged against a budget of M" is the guidance.
        ErrorKind::Budget => (
            StatusCode::BAD_REQUEST,
            codes::VALIDATION_ERROR,
            message.to_string(),
        ),
        ErrorKind::TimedOut => (
            StatusCode::GATEWAY_TIMEOUT,
            codes::TIMEOUT,
            message.to_string(),
        ),
        _ => {
            tracing::error!(error = %e, "engine error mapped to 500");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::ENGINE_ERROR,
                "An internal engine error occurred".to_string(),
            )
        }
    }
}

#[cfg(test)]
#[path = "errors_tests.rs"]
mod tests;

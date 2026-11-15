//! One classification for a handler failure, and one place that turns it into
//! an engine error.
//!
//! The classification answers *whose fault is this, is it retryable, and what
//! status does it get*: a caller's input is a 400 with the message preserved,
//! a backend failure a 500 with it replaced, an unreachable backend retried.
//! Chosen separately at each call site, that would be a retry policy set
//! invisibly at dozens of places, its reasoning in doc comments a new call
//! site has no reason to read.
//!
//! [`ErrorClass`] names the five judgements; [`HandlerError`] carries one; the
//! `From` impl below is the only code that turns a judgement into an engine
//! error. A call site says *what kind of failure this is*, which is a thing it
//! knows, instead of *which error the retry loop should see*, which is a thing
//! it should not have to.
//!
//! `to_exec_error`, `to_connect_error` and `to_limit_error` exist because at a
//! call site `.map_err(to_connect_error)` reads better than a struct literal;
//! they build a `HandlerError`, and `?` converts it.
//!
//! **One table, one code per class.** Each class becomes a service error
//! carrying the code for that class (`VALIDATION_ERROR`, `IO_ERROR`,
//! `FUNCTION_ERROR`, `TIMEOUT_ERROR`), its retryability, and a message
//! opening with the class's label ("Validation error: …"), so a workflow
//! branching on `errors[].code` and a DLQ retrying on retryability read the
//! same answer everywhere. An engine error converted into a `HandlerError` is
//! carried whole and handed back as itself.

use dataflow_rs::ErrorKind;

use crate::service::{ServiceDetail, detail_of};

/// What kind of failure a handler hit.
///
/// The order below is the order of blame: the first two are the caller's, the
/// next two the world's, and the last is nobody's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// The workflow or its caller can fix this — a missing field, a malformed
    /// value, an operation the connector's gates refuse.
    ///
    /// A 400 with the message preserved, because the message is the guidance.
    CallerInput,
    /// A configured cap was exceeded — a result set over `query.max_limit`,
    /// a response over `max_response_size`.
    ///
    /// Also a 400 with the message preserved: *"add a LIMIT to the query or
    /// raise the cap"* is useless once sanitised. Distinct from
    /// [`Self::CallerInput`] because the fix may be the operator's rather than
    /// the author's, and because a limit is worth counting separately.
    Limit,
    /// The backend could not be *reached* — pool acquisition, connection
    /// setup, DNS.
    ///
    /// Retryable, on every backend alike: a dead Postgres or MySQL
    /// database is the same outage as an unreachable HTTP host, and DLQ retry
    /// policy has no principled reason to differ between them.
    Connector,
    /// The backend was reached and the operation failed.
    ///
    /// Not retryable, and the message is replaced on the way out: a driver
    /// error names hosts, tables and sometimes values.
    Backend,
    /// The operation outlived its budget.
    Timeout,
}

impl ErrorClass {
    /// The code a class records and the label its message opens with, so a
    /// record reads "Task t error: <label><message>".
    fn face(self) -> (&'static str, &'static str) {
        match self {
            ErrorClass::CallerInput | ErrorClass::Limit => {
                ("VALIDATION_ERROR", "Validation error: ")
            }
            ErrorClass::Connector => ("IO_ERROR", "IO error: "),
            ErrorClass::Backend => ("FUNCTION_ERROR", "Function execution error: "),
            ErrorClass::Timeout => ("TIMEOUT_ERROR", "Timeout error: "),
        }
    }

    /// The class a code [`Self::face`] writes is read back as, with its
    /// label. `Limit` shares `CallerInput`'s code and reads back as it.
    fn of_code(code: &str) -> Option<(Self, &'static str)> {
        [
            Self::CallerInput,
            Self::Connector,
            Self::Backend,
            Self::Timeout,
        ]
        .into_iter()
        .find_map(|class| {
            let (written, label) = class.face();
            (written == code).then_some((class, label))
        })
    }

    /// Whether the engine's retry loop should try this again.
    ///
    /// Stated here rather than inferred from the variant it maps to, so the
    /// policy is readable in one place — and so a test can assert it directly.
    pub fn is_retryable(self) -> bool {
        matches!(self, ErrorClass::Connector | ErrorClass::Timeout)
    }
}

/// A handler failure, classified, with the message the classification says to
/// keep.
///
/// `msg` is bare: no handler-name prefix, no marker. The prefix is applied by
/// the conversion below, which is what lets a handler's static-validation path
/// reuse the same parser and read `msg` directly instead of formatting a
/// prefix on and stripping it back off.
#[derive(Debug)]
pub struct HandlerError {
    pub class: ErrorClass,
    pub msg: String,
    /// Extra context for the operator-facing log, never for the client body.
    pub detail: Option<String>,
    /// The engine error this was converted from, when it was.
    ///
    /// `ErrorClass` cannot express everything an engine error can — a
    /// service failure carries a service-owned code that decides its HTTP
    /// status, and re-deriving one from five classes would turn a 400 gate
    /// refusal into a 500. Keeping the original makes
    /// `Error → HandlerError → Error` the identity.
    ///
    /// Dropped by [`Self::prefixed`], because a rewritten message is no longer
    /// the one the original carried — with one exception, a budget refusal,
    /// whose variant is its classification and is carried through with the
    /// prefixed message.
    original: Option<Box<dataflow_rs::Error>>,
    /// The connector failure this reports, when it is one: attached to the
    /// engine error as its source, so the data plane answers it by class
    /// (`orion_common::errors::connector_failure_of`). Kept by
    /// [`Self::prefixed`]: the failure is the provider's, not the message's.
    pub connector: Option<Box<orion_common::errors::ConnectorFailure>>,
}

impl HandlerError {
    pub fn new(class: ErrorClass, msg: impl std::fmt::Display) -> Self {
        Self {
            class,
            msg: msg.to_string(),
            detail: None,
            original: None,
            connector: None,
        }
    }

    /// [`ErrorClass::CallerInput`]: the workflow or its caller can fix this.
    pub fn caller_input(msg: impl std::fmt::Display) -> Self {
        Self::new(ErrorClass::CallerInput, msg)
    }

    /// [`ErrorClass::Backend`]: the backend was reached and the operation
    /// failed; repeating it would fail the same way.
    pub fn backend(msg: impl std::fmt::Display) -> Self {
        Self::new(ErrorClass::Backend, msg)
    }

    /// [`ErrorClass::Connector`]: the backend could not be reached; retryable.
    pub fn connector(msg: impl std::fmt::Display) -> Self {
        Self::new(ErrorClass::Connector, msg)
    }

    /// [`ErrorClass::Timeout`]: the operation ran out of time; retryable.
    pub fn timeout(msg: impl std::fmt::Display) -> Self {
        Self::new(ErrorClass::Timeout, msg)
    }

    pub fn with_detail(mut self, detail: impl std::fmt::Display) -> Self {
        self.detail = Some(detail.to_string());
        self
    }

    /// Prefix the message with the handler's name.
    ///
    /// Applied on the execution path, where a message travels far from the
    /// task that produced it and needs to say which one that was. Deliberately
    /// *not* applied on the static-validation path, where the `FieldError`'s
    /// own path already carries that context.
    pub fn prefixed(mut self, handler: &str) -> Self {
        self.msg = format!("{handler}: {}", self.msg);
        // The original carried the message being replaced, so it is no longer
        // a faithful round-trip target — except where the *kind* is the
        // point. A budget refusal is classified by its kind (the code
        // `BUDGET_EXCEEDED`) and nothing in the class table can rebuild it, so
        // it is kept and prefixed with `wrap`, which keeps its kind: its
        // message is the original's under the handler's name, which is `msg`
        // unless the caller rewrote `msg` by hand.
        self.original = match self.original.take() {
            Some(original) if *original.kind() == ErrorKind::Budget => {
                Some(Box::new(original.wrap(handler)))
            }
            _ => None,
        };
        self
    }

    /// The engine error this class maps to, built from `msg`: the
    /// class's code and retryability (a connection failure and a timeout are
    /// retried), and `msg` under its label. The connector failure, or else
    /// `detail`, rides as the source, which the error's JSON never carries.
    fn into_engine_error(self) -> dataflow_rs::Error {
        let (code, label) = self.class.face();
        let error = dataflow_rs::Error::service(code, format!("{label}{}", self.msg))
            .with_retryable(self.class.is_retryable());
        // One source: a connector failure's is what decides the answer, and a
        // connector error carries no separate detail.
        match (self.connector, self.detail) {
            (Some(failure), _) => error.with_source(*failure),
            (None, Some(detail)) => error.with_source(ServiceDetail(detail)),
            (None, None) => error,
        }
    }
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

/// The code an upstream's non-2xx answer records, and the one an
/// [`http_error`] carries.
pub const HTTP_ERROR: &str = "HTTP_ERROR";

/// A non-2xx answer from an upstream, as an engine error carrying
/// [`HTTP_ERROR`], retryable for a 5xx, a 429, a 408, or no status at all,
/// and reading `"HTTP error: {status} - {message}"`. The status and the bare
/// message ride as the source, an [`HttpFailure`], so a caller reads the
/// status with [`http_status`].
pub fn http_error(status: u16, message: impl Into<String>) -> dataflow_rs::Error {
    let failure = HttpFailure {
        status,
        message: message.into(),
    };
    dataflow_rs::Error::service(HTTP_ERROR, failure.to_string())
        .with_retryable(failure.is_retryable())
        .with_source(failure)
}

/// The upstream status of an [`http_error`]; `None` for any other error.
pub fn http_status(e: &dataflow_rs::Error) -> Option<u16> {
    http_failure(e).map(|failure| failure.status)
}

/// The [`HttpFailure`] an [`http_error`] carries.
fn http_failure(e: &dataflow_rs::Error) -> Option<&HttpFailure> {
    std::error::Error::source(e)?.downcast_ref::<HttpFailure>()
}

/// What an [`http_error`] carries as its source: the upstream's status and
/// the bare message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpFailure {
    /// The upstream's status; `0` when there was none.
    pub status: u16,
    /// The message, without the `"HTTP error: {status} - "` the error's own
    /// text opens with.
    pub message: String,
}

impl HttpFailure {
    /// Retryable: a server error, a rate limit, a request timeout, or a
    /// connection that gave no status.
    fn is_retryable(&self) -> bool {
        self.status >= 500 || matches!(self.status, 429 | 408 | 0)
    }

    /// The class an upstream's answer is read as: a 4xx is the caller's,
    /// anything else the backend's.
    fn class(&self) -> ErrorClass {
        if (400..500).contains(&self.status) {
            ErrorClass::CallerInput
        } else {
            ErrorClass::Backend
        }
    }
}

/// The text an upstream's non-2xx answer records.
impl std::fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP error: {} - {}", self.status, self.message)
    }
}

impl std::error::Error for HttpFailure {}

/// The one place a classification becomes an engine error.
///
/// Each class records one code and its retryability, so a workflow branching
/// on `errors[].code` and a DLQ retrying on retryability read the same answer
/// everywhere.
impl From<HandlerError> for dataflow_rs::Error {
    fn from(mut e: HandlerError) -> Self {
        // An error that came from an engine and was not rewritten goes back as
        // that engine's error, with its own code: an evaluation failure stays
        // `LOGIC_ERROR`, a budget refusal `BUDGET_EXCEEDED`.
        match e.original.take() {
            Some(original) => *original,
            None => e.into_engine_error(),
        }
    }
}

/// An engine error's message as a `HandlerError` holds it: bare, with
/// the label of a class-table code taken off, so a rebuilt or prefixed
/// error carries the label once.
fn bare_message(e: &dataflow_rs::Error) -> &str {
    let message = e.message();
    match e.kind() {
        ErrorKind::Service { code } => ErrorClass::of_code(code)
            .and_then(|(_, label)| message.strip_prefix(label))
            .unwrap_or(message),
        _ => message,
    }
}

/// The inverse, for an engine error a function was handed: an
/// evaluation's, a nested run's, or one of Orion's own service errors.
///
/// The class is read from the kind, and from the code for a service error:
/// the four codes the class table writes come back as their classes, and
/// any other code by the retryability it declares. The original is kept, so the conversion back is the
/// identity; a service error's detail is read from its source. `msg` is the
/// message without a class label.
impl From<dataflow_rs::Error> for HandlerError {
    fn from(e: dataflow_rs::Error) -> Self {
        // An upstream's non-2xx answer is classified by its status and keeps
        // its bare message.
        if let Some(failure) = http_failure(&e) {
            return Self {
                class: failure.class(),
                msg: failure.message.clone(),
                detail: None,
                original: Some(Box::new(e)),
                connector: None,
            };
        }
        let class = match e.kind() {
            // A configured cap: the author's or the operator's to fix, and a
            // retry spends the same operations.
            ErrorKind::Budget => ErrorClass::Limit,
            ErrorKind::TimedOut => ErrorClass::Timeout,
            ErrorKind::Service { code } => match ErrorClass::of_code(code) {
                Some((class, _)) => class,
                // A service-classified failure declares its own
                // retryability, so honour that rather than re-deriving it.
                None if e.is_retryable() => ErrorClass::Connector,
                None => ErrorClass::Backend,
            },
            // An evaluation, a parse, a missing function, a config or
            // compile failure, a cancellation, and any kind added upstream
            // (`ErrorKind` is `#[non_exhaustive]`): a backend failure, the
            // conservative reading.
            _ => ErrorClass::Backend,
        };
        Self {
            class,
            msg: bare_message(&e).to_string(),
            detail: detail_of(&e).map(str::to_string),
            connector: orion_common::errors::connector_failure_of(&e)
                .cloned()
                .map(Box::new),
            original: Some(Box::new(e)),
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;

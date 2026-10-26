//! One classification for a handler failure, and one place that turns it into
//! an engine error.
//!
//! Four mechanisms used to answer *whose fault is this, is it retryable, and
//! what status does it get*. Two are already gone: the `LIMIT_MARKER` prefix
//! that smuggled a classification through an error string, and the
//! `ServiceUnavailable(String)` that conflated four unrelated causes. This is
//! the third.
//!
//! **What was wrong with three constructors.** `to_exec_error`,
//! `to_connect_error` and `to_limit_error` each picked an engine error
//! variant, and the variant is what decides retryability: `Io` is retried,
//! `FunctionExecution` with no source is not, `Validation` is a 400 with the
//! message preserved while the other two are 500s with it replaced. So a
//! retry policy was being set, correctly but invisibly, at 55 separate call
//! sites — and the reasoning for each choice lived in three doc comments that
//! a 56th call site had no reason to read.
//!
//! [`ErrorClass`] names the five judgements; [`HandlerError`] carries one; the
//! `From` impl below is the only code that turns a judgement into a variant. A
//! call site now says *what kind of failure this is*, which is a thing it
//! knows, instead of *which error variant the retry loop should see*, which is
//! a thing it should not have to.
//!
//! The three constructors are kept, because at a call site `.map_err(
//! to_connect_error)` reads better than a struct literal — they just build a
//! `HandlerError` now, and `?` converts it.
//!
//! **One table, 1.12's codes.** Each class becomes a service error carrying
//! the code 1.12 recorded for it (`VALIDATION_ERROR`, `IO_ERROR`,
//! `FUNCTION_ERROR`, `TIMEOUT_ERROR`), its retryability, and a message opening
//! with the label 1.12 printed ("Validation error: …"), so a workflow branching
//! on `errors[].code` and a DLQ retrying on retryability read what they always
//! did. An engine error converted into a `HandlerError` is carried whole and
//! handed back as itself.

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
    /// Retryable. Before F42 these went out as `FunctionExecution`, so a dead
    /// Postgres, Redis or MongoDB was a non-retryable 500 while the identical
    /// HTTP outage was a retryable `Io` — DLQ retry policy diverged by backend
    /// for no principled reason.
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
    /// The code a class records and the label its message opens with: 1.12's,
    /// so a record reads "Task t error: <label><message>" as it always did.
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
}

impl HandlerError {
    pub fn new(class: ErrorClass, msg: impl std::fmt::Display) -> Self {
        Self {
            class,
            msg: msg.to_string(),
            detail: None,
            original: None,
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

    /// The engine error this class maps to, built from `msg` (I184): the
    /// class's code and retryability (a connection failure and a timeout are
    /// retried), and `msg` under its label. `detail` rides as the
    /// source, which the error's JSON never carries.
    fn into_v4_from_class(self) -> dataflow_rs::Error {
        let (code, label) = self.class.face();
        let error = dataflow_rs::Error::service(code, format!("{label}{}", self.msg))
            .with_retryable(self.class.is_retryable());
        match self.detail {
            Some(detail) => error.with_source(ServiceDetail(detail)),
            None => error,
        }
    }
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

/// The code 1.12 recorded for an upstream's non-2xx answer, and the one an
/// [`http_error`] carries.
pub const HTTP_ERROR: &str = "HTTP_ERROR";

/// A non-2xx answer from an upstream, as an engine error whose code
/// ([`HTTP_ERROR`]), retryability (a 5xx, a 429, a 408, or no status at
/// all) and recorded text (`"HTTP error: {status} - {message}"`) are
/// 1.12's. The status and the bare message ride as the source, an
/// [`HttpFailure`], so a caller reads the status with [`http_status`].
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
    /// 1.12's rule: a server error, a rate limit, a
    /// request timeout, or a connection that gave no status.
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

/// The text 1.12 recorded for an upstream's non-2xx answer.
impl std::fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP error: {} - {}", self.status, self.message)
    }
}

impl std::error::Error for HttpFailure {}

/// The one place a classification becomes an engine error (I184).
///
/// The code each class records, and its retryability, are 1.12's, so a
/// workflow branching on `errors[].code` and a DLQ retrying on retryability
/// read the answer they always did.
impl From<HandlerError> for dataflow_rs::Error {
    fn from(mut e: HandlerError) -> Self {
        // An error that came from an engine and was not rewritten goes back as
        // that engine's error, with its own code: an evaluation failure stays
        // `LOGIC_ERROR`, a budget refusal `BUDGET_EXCEEDED`.
        match e.original.take() {
            Some(original) => *original,
            None => e.into_v4_from_class(),
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
            original: Some(Box::new(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResolvedSecrets;
    use crate::operators::orion_exprs;
    use dataflow_rs::datavalue::OwnedDataValue;
    use serde_json::json;

    const CLASSES: [ErrorClass; 5] = [
        ErrorClass::CallerInput,
        ErrorClass::Limit,
        ErrorClass::Connector,
        ErrorClass::Backend,
        ErrorClass::Timeout,
    ];

    /// What 1.12 recorded for each class: its code, its retryability, and
    /// the label its message opened with. Written out, because these are the
    /// values workflows branch on and DLQ policies retry on.
    fn recorded(class: ErrorClass) -> (&'static str, bool, &'static str) {
        match class {
            ErrorClass::CallerInput | ErrorClass::Limit => {
                ("VALIDATION_ERROR", false, "Validation error: ")
            }
            ErrorClass::Connector => ("IO_ERROR", true, "IO error: "),
            ErrorClass::Backend => ("FUNCTION_ERROR", false, "Function execution error: "),
            ErrorClass::Timeout => ("TIMEOUT_ERROR", true, "Timeout error: "),
        }
    }

    /// A budget refusal, as the engine makes one: the only way there is,
    /// since only the engine builds an `ErrorKind::Budget`.
    fn budget_error() -> dataflow_rs::Error {
        let heavy = json!({"map": [{"var": "data.xs"}, {"+": [{"var": ""}, 1]}]});
        let context = json!({"data": {"xs": (0..100).collect::<Vec<u32>>()}});
        let bounded = orion_exprs(&ResolvedSecrets::empty(), 10).expect("builds");
        let expr = bounded.compile::<OwnedDataValue>(&heavy).expect("compiles");
        bounded
            .eval(&expr, &context)
            .expect_err("crosses the ceiling")
    }

    /// An evaluation failure, as the engine makes one.
    fn eval_error() -> dataflow_rs::Error {
        let exprs = orion_exprs(&ResolvedSecrets::empty(), 0).expect("builds");
        let expr = exprs
            .compile::<OwnedDataValue>(&json!({"hex_decode": [{"var": "data.x"}]}))
            .expect("compiles");
        exprs
            .eval(&expr, &json!({"data": {"x": "zz"}}))
            .expect_err("not hex")
    }

    /// The retry policy each class implies, stated on the class itself.
    #[test]
    fn each_class_has_its_retry_policy() {
        for class in CLASSES {
            assert_eq!(class.is_retryable(), recorded(class).1, "{class:?}");
        }
    }

    /// Each class records 1.12's code, retryability and labelled message.
    #[test]
    fn each_class_records_1_12s_code_and_retryability() {
        for class in CLASSES {
            let (code, retryable, label) = recorded(class);
            let e = dataflow_rs::Error::from(HandlerError::new(class, "boom"));
            assert_eq!(e.code(), code, "{class:?}");
            assert_eq!(e.is_retryable(), retryable, "{class:?}");
            assert_eq!(e.message(), format!("{label}boom"), "{class:?}");
        }
    }

    /// A caller-fixable failure keeps its text, because the text is the
    /// guidance.
    #[test]
    fn a_caller_fixable_failure_keeps_its_message() {
        let e = dataflow_rs::Error::from(HandlerError::new(
            ErrorClass::Limit,
            "add a LIMIT to the query or raise the cap",
        ));
        assert_eq!(
            e.message(),
            "Validation error: add a LIMIT to the query or raise the cap"
        );
    }

    /// The prefix is a rendering step, not part of the message — which is what
    /// lets the static path read `msg` instead of stripping a prefix back off.
    #[test]
    fn prefixing_is_separable_from_the_message() {
        let bare = HandlerError::new(ErrorClass::CallerInput, "'algorithm' is not supported");
        assert_eq!(bare.msg, "'algorithm' is not supported");
        assert_eq!(
            bare.prefixed("crypto").msg,
            "crypto: 'algorithm' is not supported"
        );
    }

    /// A class-built error's detail rides as its source, never in its JSON.
    #[test]
    fn a_class_built_errors_detail_is_its_source_and_not_its_json() {
        let e = dataflow_rs::Error::from(
            HandlerError::new(ErrorClass::Backend, "query failed").with_detail("table secrets"),
        );
        assert_eq!(detail_of(&e), Some("table secrets"));
        assert_eq!(
            serde_json::to_value(&e).expect("serializes"),
            json!({
                "code": "FUNCTION_ERROR",
                "message": "Function execution error: query failed",
                "retryable": false
            })
        );
    }

    /// The label is written once: reading a class-built error back strips
    /// it, so `msg` is bare, and a prefixed or rebuilt error carries it once.
    #[test]
    fn a_class_label_is_never_doubled() {
        for class in CLASSES {
            let (code, _, label) = recorded(class);
            let back =
                HandlerError::from(dataflow_rs::Error::from(HandlerError::new(class, "bad")));
            assert_eq!(back.msg, "bad", "{class:?}");

            let again = dataflow_rs::Error::from(back.prefixed("crypto"));
            assert_eq!(again.message(), format!("{label}crypto: bad"), "{class:?}");
            assert_eq!(again.code(), code, "{class:?}");

            let rebuilt = dataflow_rs::Error::from(HandlerError::from(again));
            assert_eq!(
                rebuilt.message(),
                format!("{label}crypto: bad"),
                "carried whole: {class:?}"
            );
        }
    }

    /// An error the engine handed a function goes back as itself: its own
    /// code, not one re-derived from a class.
    #[test]
    fn an_engine_error_passes_through_with_its_own_code() {
        let back: dataflow_rs::Error = HandlerError::from(eval_error()).into();
        assert_eq!(back.code(), "LOGIC_ERROR");
        assert_eq!(back.message(), eval_error().message());
        let back: dataflow_rs::Error = HandlerError::from(budget_error()).into();
        assert_eq!(back.kind(), &ErrorKind::Budget);
        assert_eq!(back.code(), "BUDGET_EXCEEDED");
    }

    /// The class each kind of engine error is read as.
    #[test]
    fn an_engine_error_is_read_as_its_class() {
        let cases = [
            (budget_error(), ErrorClass::Limit),
            (eval_error(), ErrorClass::Backend),
            (
                crate::service::circuit_open("c", "ch"),
                ErrorClass::Connector,
            ),
            (crate::service::connector_detail("d"), ErrorClass::Backend),
        ];
        for (e, class) in cases {
            assert_eq!(HandlerError::from(e).class, class);
        }
        for class in [
            ErrorClass::CallerInput,
            ErrorClass::Connector,
            ErrorClass::Backend,
            ErrorClass::Timeout,
        ] {
            let built = dataflow_rs::Error::from(HandlerError::new(class, "m"));
            assert_eq!(HandlerError::from(built).class, class);
        }
    }

    /// `prefixed` keeps a budget refusal's kind, with the prefixed message —
    /// so the code stays `BUDGET_EXCEEDED` and the refusal does not read as a
    /// backend fault — and rebuilds every other error from its class.
    #[test]
    fn prefixed_keeps_a_budget_errors_kind_and_rebuilds_the_rest() {
        let budget = budget_error();
        let e = dataflow_rs::Error::from(HandlerError::from(budget_error()).prefixed("crypto"));
        assert_eq!(e.kind(), &ErrorKind::Budget);
        assert_eq!(e.code(), "BUDGET_EXCEEDED");
        assert_eq!(e.message(), format!("crypto: {}", budget.message()));
        assert!(!e.is_retryable());

        let rebuilt = [
            (
                eval_error(),
                "FUNCTION_ERROR",
                false,
                format!(
                    "Function execution error: http_call: {}",
                    eval_error().message()
                ),
            ),
            (
                crate::service::connector_detail("nope"),
                "FUNCTION_ERROR",
                false,
                // The detail stays the operator's; the message is the
                // caller-safe half.
                "Function execution error: http_call: Request validation failed".to_string(),
            ),
            (
                dataflow_rs::Error::from(HandlerError::new(ErrorClass::Connector, "refused")),
                "IO_ERROR",
                true,
                "IO error: http_call: refused".to_string(),
            ),
        ];
        for (e, code, retryable, message) in rebuilt {
            let e = dataflow_rs::Error::from(HandlerError::from(e).prefixed("http_call"));
            assert_eq!(e.code(), code, "{message}");
            assert_eq!(e.is_retryable(), retryable, "{message}");
            assert_eq!(e.message(), message);
        }
    }

    /// A service error survives the round trip with its code, detail and
    /// retryability: the property `QueryBudget::run` and the gate refusals
    /// rest on. A connector gate's refusal is a 400 only because of its
    /// service-owned code, which no class can express, and an integrity
    /// refusal's code decides between a `409` and a `500` (#297).
    #[test]
    fn a_service_error_survives_the_round_trip() {
        let back: dataflow_rs::Error =
            HandlerError::from(crate::service::connector_detail("disabled")).into();
        assert_eq!(back.code(), crate::errors::kind::CONNECTOR_DETAIL);
        assert_eq!(detail_of(&back), Some("disabled"));

        let back: dataflow_rs::Error = HandlerError::from(crate::service::integrity(
            crate::service::IntegrityKind::ForeignKey,
            "FOREIGN KEY constraint failed",
        ))
        .into();
        assert_eq!(back.code(), crate::errors::kind::INTEGRITY_FOREIGN_KEY);
        assert!(!back.is_retryable());
    }

    /// Rewriting the message drops the original, because it is no longer the
    /// error that message came from.
    #[test]
    fn prefixing_gives_up_the_round_trip() {
        let original = crate::service::connector_detail("nope");
        let back: dataflow_rs::Error = HandlerError::from(original).prefixed("crypto").into();
        assert_eq!(back.code(), "FUNCTION_ERROR", "rebuilt from its class");
    }

    /// An upstream's non-2xx answer records 1.12's code, retryability and
    /// text at every status, its status stays readable, and it reads back as
    /// the caller's failure for a 4xx and the backend's otherwise.
    #[test]
    fn an_http_error_is_1_12s() {
        for status in [0, 301, 400, 401, 404, 408, 429, 500, 502, 503] {
            let e = http_error(status, "HTTP 503 from upstream: down");
            let retryable = status >= 500 || matches!(status, 0 | 408 | 429);
            assert_eq!(e.code(), HTTP_ERROR, "{status}");
            assert_eq!(e.is_retryable(), retryable, "{status}");
            assert_eq!(
                e.message(),
                format!("HTTP error: {status} - HTTP 503 from upstream: down")
            );
            assert_eq!(http_status(&e), Some(status));
            assert_eq!(detail_of(&e), None, "an HTTP failure carries no detail");

            let handler = HandlerError::from(e);
            let class = if (400..500).contains(&status) {
                ErrorClass::CallerInput
            } else {
                ErrorClass::Backend
            };
            assert_eq!(handler.class, class, "{status}");
            assert_eq!(handler.msg, "HTTP 503 from upstream: down");
            let back = dataflow_rs::Error::from(handler);
            assert_eq!(http_status(&back), Some(status), "carried whole");
        }
        assert_eq!(
            http_status(&dataflow_rs::Error::service(HTTP_ERROR, "x")),
            None
        );
    }

    /// How a probe fails: with a class, or with the engine error an
    /// evaluation hands a function, optionally prefixed with its name.
    #[derive(Clone, Copy, Debug)]
    enum Failure {
        Class(ErrorClass),
        Eval,
        Budget,
    }

    /// A function failing as its `Failure` says.
    struct Fails(Failure, bool);

    impl dataflow_rs::Function for Fails {
        type Config = ();

        async fn call(
            &self,
            _ctx: &mut dataflow_rs::Ctx<'_>,
            _cfg: &(),
        ) -> dataflow_rs::Result<dataflow_rs::Outcome> {
            let e = match self.0 {
                Failure::Class(class) => HandlerError::new(class, "the backend said no"),
                Failure::Eval => HandlerError::from(eval_error()),
                Failure::Budget => HandlerError::from(budget_error()),
            };
            Err(if self.1 { e.prefixed("probe") } else { e }.into())
        }
    }

    /// The code and text of the record a failing task `t` writes: the record
    /// the engine composes itself.
    async fn record(probe: Fails) -> (String, String) {
        let engine = dataflow_rs::Engine::builder()
            .register("probe", probe)
            .workflow_json(&json!({"id": "w", "tasks": [{
                "id": "t",
                "function": {"name": "probe", "input": {}},
                "on_failure": "continue"
            }]}))
            .build()
            .expect("the engine builds");
        let mut message = dataflow_rs::Message::new(OwnedDataValue::Null);
        engine.run(&mut message).await;
        let record = message.errors().first().expect("the failure is recorded");
        (record.code.clone(), record.message.clone())
    }

    /// The record a failing task writes is 1.12's, code and text: each class,
    /// bare and prefixed with the function's name, and the evaluation and
    /// budget errors a function passes on whole, which keep their own text.
    #[tokio::test]
    async fn a_failing_tasks_record_is_what_1_12_recorded() {
        let (eval, budget) = (eval_error(), budget_error());
        for class in CLASSES {
            let (code, _, label) = recorded(class);
            for (prefixed, prefix) in [(false, ""), (true, "probe: ")] {
                assert_eq!(
                    record(Fails(Failure::Class(class), prefixed)).await,
                    (
                        code.to_string(),
                        format!("Task t error: {label}{prefix}the backend said no")
                    ),
                    "{class:?}, prefixed: {prefixed}"
                );
            }
        }
        let whole = [
            (
                Failure::Eval,
                false,
                "LOGIC_ERROR",
                format!("Logic evaluation error: {}", eval.message()),
            ),
            (
                Failure::Eval,
                true,
                "FUNCTION_ERROR",
                format!("Function execution error: probe: {}", eval.message()),
            ),
            (
                Failure::Budget,
                false,
                "BUDGET_EXCEEDED",
                budget.message().to_string(),
            ),
            (
                Failure::Budget,
                true,
                "BUDGET_EXCEEDED",
                format!("probe: {}", budget.message()),
            ),
        ];
        for (failure, prefixed, code, message) in whole {
            assert_eq!(
                record(Fails(failure, prefixed)).await,
                (code.to_string(), format!("Task t error: {message}")),
                "{failure:?}, prefixed: {prefixed}"
            );
        }
    }

    /// How an engine error this module builds reaches an HTTP caller,
    /// through `OrionError`'s mapping. Here rather than in `errors.rs`,
    /// which sits below `engine` and may not name it.
    mod http_mapping {
        use super::*;
        use crate::errors::OrionError;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use orion_api::codes;

        #[test]
        fn a_caller_input_failure_answers_400() {
            let err =
                OrionError::Engine(HandlerError::new(ErrorClass::CallerInput, "bad input").into());
            let (status, code, message) = err.response_parts();
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(code, codes::VALIDATION_ERROR);
            assert_eq!(message, "bad input", "the class label is taken off");
        }

        #[test]
        fn a_timeout_answers_504() {
            let err =
                OrionError::Engine(HandlerError::new(ErrorClass::Timeout, "timed out").into());
            let (status, code, message) = err.response_parts();
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
            assert_eq!(code, codes::TIMEOUT);
            assert_eq!(message, "timed out", "the class label is taken off");
        }

        /// An HTTP error from a downstream call keeps the generic engine
        /// mapping — the `kind`, not the status, is what identifies an
        /// Orion-side channel refusal, and a relayed 429 carries none.
        #[test]
        fn a_downstream_429_is_not_reported_as_a_channel_refusal() {
            let err = OrionError::Engine(http_error(429, "Too Many Requests"));
            assert_eq!(
                err.into_response().status(),
                StatusCode::INTERNAL_SERVER_ERROR
            );
        }

        /// A genuine 503 relayed by `http_call` keeps the generic engine
        /// mapping rather than reading as an open circuit.
        #[tokio::test]
        async fn a_downstream_503_is_not_reported_as_circuit_open() {
            let err = OrionError::Engine(http_error(503, "Service Unavailable"));
            let response = err.into_response();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("body");
            let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
            assert_eq!(body["error"]["code"], "ENGINE_ERROR");
        }
    }
}

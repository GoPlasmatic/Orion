//! One classification for a handler failure, and one place that turns it into
//! a `DataflowError`.
//!
//! Four mechanisms used to answer *whose fault is this, is it retryable, and
//! what status does it get*. Two are already gone: the `LIMIT_MARKER` prefix
//! that smuggled a classification through an error string, and the
//! `ServiceUnavailable(String)` that conflated four unrelated causes. This is
//! the third.
//!
//! **What was wrong with three constructors.** `to_exec_error`,
//! `to_connect_error` and `to_limit_error` each picked a `DataflowError`
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
//! **Both engines, one table.** While dataflow-rs 3 and 4 run side by side, a
//! `HandlerError` converts into either engine's error. Into dataflow-rs 4 each
//! class becomes a service error carrying the code the dataflow-rs 3 variant
//! it maps to records (`VALIDATION_ERROR`, `IO_ERROR`, `FUNCTION_ERROR`,
//! `TIMEOUT_ERROR`) with that variant's retryability, so a client sees the
//! same code whichever engine ran the task. An error converted *from* either
//! engine is carried whole and handed back as itself.

use dataflow_rs::run::ErrorKind;
use dataflow_rs_v3::DataflowError;

use crate::errors::service::{ServiceDetail, detail_of};

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
    /// `DataflowError → HandlerError → DataflowError` the identity, and the
    /// same for dataflow-rs 4's `Error`.
    ///
    /// Dropped by [`Self::prefixed`], because a rewritten message is no longer
    /// the one the original carried — with one exception, a budget refusal,
    /// whose variant is its classification and is carried through with the
    /// prefixed message.
    original: Option<Original>,
}

/// The engine error a [`HandlerError`] was converted from. Not `Clone`, since
/// dataflow-rs 4's `Error` is not, and so neither is `HandlerError`.
#[derive(Debug)]
enum Original {
    V3(Box<DataflowError>),
    V4(Box<dataflow_rs::Error>),
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
        // a faithful round-trip target — except where the *variant* is the
        // point. A budget refusal is classified by its variant (the code
        // `BUDGET_EXCEEDED`, non-retryable upstream) and nothing in the class
        // table can rebuild it, so it is carried through with the prefixed
        // message rather than dropped into `FunctionExecution`.
        //
        // dataflow-rs 4 builds a budget error only inside the engine, so the
        // one it handed over is kept and prefixed with `wrap`, which keeps its
        // kind: its message is the original's under the handler's name, which
        // is `msg` unless the caller rewrote `msg` by hand. Wrap the `Error`
        // before converting it, rather than editing `msg` after.
        self.original = match self.original.take() {
            Some(Original::V3(original))
                if matches!(*original, DataflowError::BudgetExceeded(_)) =>
            {
                Some(Original::V3(Box::new(DataflowError::BudgetExceeded(
                    self.msg.clone(),
                ))))
            }
            Some(Original::V4(original)) if *original.kind() == ErrorKind::Budget => {
                Some(Original::V4(Box::new(original.wrap(handler))))
            }
            _ => None,
        };
        self
    }

    /// The dataflow-rs 4 error this class maps to, built from `msg` (I184):
    /// the code its dataflow-rs 3 variant records, and that variant's
    /// retryability. `detail` rides as the source, which the error's JSON
    /// never carries.
    fn into_v4_from_class(self) -> dataflow_rs::Error {
        let error = match self.class {
            // 400, message preserved.
            ErrorClass::CallerInput | ErrorClass::Limit => {
                dataflow_rs::Error::service("VALIDATION_ERROR", self.msg)
            }
            // Retryable: could not reach the backend.
            ErrorClass::Connector => {
                dataflow_rs::Error::service("IO_ERROR", self.msg).with_retryable(true)
            }
            // Not retryable.
            ErrorClass::Backend => dataflow_rs::Error::service("FUNCTION_ERROR", self.msg),
            ErrorClass::Timeout => {
                dataflow_rs::Error::service("TIMEOUT_ERROR", self.msg).with_retryable(true)
            }
        };
        match self.detail {
            Some(detail) => error.with_source(ServiceDetail(detail)),
            None => error,
        }
    }

    /// The dataflow-rs 3 variant this class maps to, built from `msg`.
    fn into_v3_from_class(self) -> DataflowError {
        match self.class {
            // 400, message preserved.
            ErrorClass::CallerInput | ErrorClass::Limit => DataflowError::Validation(self.msg),
            // Retryable: could not reach the backend.
            ErrorClass::Connector => DataflowError::Io(self.msg),
            // Not retryable: `function_execution` with `source: None` is what
            // dataflow-rs classifies as terminal.
            ErrorClass::Backend => DataflowError::function_execution(self.msg, None),
            ErrorClass::Timeout => DataflowError::Timeout(self.msg),
        }
    }
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

/// The one place a classification becomes a `DataflowError` variant.
///
/// Every mapping here is the one its constructor made before, so retryability
/// and status are unchanged — what changed is that there is now a single table
/// to read them from, and to change if dataflow-rs revises its semantics again.
impl From<HandlerError> for DataflowError {
    fn from(mut e: HandlerError) -> Self {
        // An error that came from a `DataflowError` and was not rewritten goes
        // back as itself. Anything else is built from its class.
        match e.original.take() {
            Some(Original::V3(original)) => *original,
            Some(Original::V4(original)) => v4_into_v3(*original, e),
            None => e.into_v3_from_class(),
        }
    }
}

/// A dataflow-rs 4 error, for a dataflow-rs 3 handler that was handed one
/// through a shared helper: the variant that records the same code.
///
/// The four codes the class table writes come back as the variants they
/// stand for, so dataflow-rs 3's status mapping reads them as it always did;
/// any other service code is Orion's own kind and stays a service error, its
/// detail and retryability kept. `rest` is the `HandlerError` the error
/// travelled in, for the kinds with no variant of their own.
fn v4_into_v3(original: dataflow_rs::Error, rest: HandlerError) -> DataflowError {
    let message = original.message().to_string();
    match original.kind() {
        ErrorKind::Budget => DataflowError::BudgetExceeded(message),
        ErrorKind::Eval => DataflowError::LogicEvaluation(message),
        ErrorKind::TimedOut => DataflowError::Timeout(message),
        ErrorKind::Parse => DataflowError::Deserialization(message),
        ErrorKind::FunctionNotFound => DataflowError::FunctionNotFound(message),
        ErrorKind::Service { code } => match code.as_ref() {
            "VALIDATION_ERROR" => DataflowError::Validation(message),
            "IO_ERROR" => DataflowError::Io(message),
            "TIMEOUT_ERROR" => DataflowError::Timeout(message),
            "FUNCTION_ERROR" => DataflowError::function_execution(message, None),
            kind => {
                let mut built =
                    DataflowError::service(kind, message).retryable(original.is_retryable());
                if let Some(detail) = detail_of(&original) {
                    built = built.detail(detail);
                }
                built.build()
            }
        },
        // Config, compile and cancellation have no variant: the class says
        // what the failure was.
        _ => HandlerError {
            original: None,
            ..rest
        }
        .into_v3_from_class(),
    }
}

/// The one place a classification becomes a dataflow-rs 4 error (I184).
///
/// The code each class records is the one its dataflow-rs 3 variant
/// records, and its retryability that variant's, so a workflow branching on
/// `errors[].code` and a DLQ retrying on retryability read the same answer
/// from either engine.
impl From<HandlerError> for dataflow_rs::Error {
    fn from(mut e: HandlerError) -> Self {
        // An error that came from an engine and was not rewritten goes back as
        // that engine's error, with its own code: an evaluation failure stays
        // `LOGIC_ERROR`, a budget refusal `BUDGET_EXCEEDED`.
        match e.original.take() {
            Some(Original::V4(original)) => *original,
            Some(Original::V3(original)) => v3_into_v4(*original),
            None => e.into_v4_from_class(),
        }
    }
}

/// A dataflow-rs 3 error, for a v4 function that was handed one through a
/// shared helper: a service error with the code dataflow-rs 3 records for it
/// (its own classifier, through `ErrorInfo`), its retryability, its
/// caller-facing message and, as the source, its detail.
fn v3_into_v4(original: DataflowError) -> dataflow_rs::Error {
    let retryable = original.retryable();
    let detail = original.detail().map(str::to_string);
    let code = dataflow_rs_v3::ErrorInfo::new(None, None, original.clone()).code;
    // The payload, as `From<DataflowError>` reads it, never `Display`'s
    // variant-prefixed text.
    let message = HandlerError::from(original).msg;
    let error = dataflow_rs::Error::service(code, message).with_retryable(retryable);
    match detail {
        Some(detail) => error.with_source(ServiceDetail(detail)),
        None => error,
    }
}

/// The inverse, for a dataflow-rs 4 error a function was handed: an
/// evaluation's, a nested run's, or one of Orion's own service errors.
///
/// The class is read from the kind, and from the code for a service error:
/// the four codes the class table writes come back as their classes, and
/// any other code by the retryability it declares, as a dataflow-rs 3
/// service error is. The original is kept, so the conversion back is the
/// identity; a service error's detail is read from its source.
impl From<dataflow_rs::Error> for HandlerError {
    fn from(e: dataflow_rs::Error) -> Self {
        let class = match e.kind() {
            // A configured cap: the author's or the operator's to fix, and a
            // retry spends the same operations — the class a dataflow-rs 3
            // `BudgetExceeded` takes.
            ErrorKind::Budget => ErrorClass::Limit,
            ErrorKind::TimedOut => ErrorClass::Timeout,
            ErrorKind::Service { code } => match code.as_ref() {
                "VALIDATION_ERROR" => ErrorClass::CallerInput,
                "IO_ERROR" => ErrorClass::Connector,
                "TIMEOUT_ERROR" => ErrorClass::Timeout,
                "FUNCTION_ERROR" => ErrorClass::Backend,
                // A service-classified failure declares its own
                // retryability, so honour that rather than re-deriving it.
                _ if e.is_retryable() => ErrorClass::Connector,
                _ => ErrorClass::Backend,
            },
            // An evaluation, a parse, a missing function, a config or
            // compile failure, a cancellation, and any kind added upstream
            // (`ErrorKind` is `#[non_exhaustive]`): a backend failure, the
            // conservative reading, as for dataflow-rs 3.
            _ => ErrorClass::Backend,
        };
        Self {
            class,
            msg: e.message().to_string(),
            detail: detail_of(&e).map(str::to_string),
            original: Some(Original::V4(Box::new(e))),
        }
    }
}

/// The inverse, for a helper that still speaks `DataflowError`.
///
/// A handler converted to `HandlerError` still calls shared code that has not
/// been — the secret resolver, say — and `?` needs this to keep working.
///
/// The message comes from the variant's *payload*, never from `to_string()`:
/// `Display` prepends the variant name ("Validation error: …"), and a message
/// carrying that prefix is exactly the noise this type exists to keep out. That
/// is not hypothetical — `strip_handler_prefix` stopped working the day it
/// started matching at position 0, because the string it was handed began with
/// `"Validation error: "` rather than with the handler's name.
impl From<DataflowError> for HandlerError {
    fn from(e: DataflowError) -> Self {
        // `Service` is the only variant with operator-only text beside the
        // caller-safe message, and dropping it would lose the *specific*
        // refusal — "Request validation failed" with "operation 'read' is
        // disabled on connector 'c'" thrown away.
        let detail = match &e {
            DataflowError::Service { detail, .. } => detail.clone(),
            _ => None,
        };
        let original = e.clone();
        let (class, msg) = match e {
            DataflowError::Validation(m) => (ErrorClass::CallerInput, m),
            DataflowError::Timeout(m) => (ErrorClass::Timeout, m),
            DataflowError::Io(m) => (ErrorClass::Connector, m),
            DataflowError::FunctionExecution { context, .. } => (ErrorClass::Backend, context),
            DataflowError::Service {
                message, retryable, ..
            } => (
                // A service-classified failure declares its own retryability,
                // so honour that rather than re-deriving it from the text.
                if retryable {
                    ErrorClass::Connector
                } else {
                    ErrorClass::Backend
                },
                message,
            ),
            DataflowError::Http { status, message } => (
                // 4xx is the caller's; anything else is the backend's.
                if (400..500).contains(&status) {
                    ErrorClass::CallerInput
                } else {
                    ErrorClass::Backend
                },
                message,
            ),
            DataflowError::Workflow(m)
            | DataflowError::Task(m)
            | DataflowError::FunctionNotFound(m)
            | DataflowError::Deserialization(m)
            | DataflowError::LogicEvaluation(m)
            | DataflowError::Unknown(m) => (ErrorClass::Backend, m),
            // `engine.ops_budget` crossed while a handler resolved one of its
            // template fields. A configured cap, like a result set over
            // `max_limit`: the author's or the operator's to fix, the message
            // says which number was crossed, and a retry spends the same
            // operations — so a `Limit`, and the variant is kept through
            // `prefixed` below so the code the retry loop and the trace read
            // stays `BUDGET_EXCEEDED`.
            DataflowError::BudgetExceeded(m) => (ErrorClass::Limit, m),
            // `DataflowError` is `#[non_exhaustive]`. A variant added upstream
            // is classified as a backend failure — not retryable, message
            // replaced — because that is the conservative reading of an error
            // whose semantics this build does not know. `to_string()` here
            // rather than a payload, since there is no arm to destructure.
            other => (ErrorClass::Backend, other.to_string()),
        };
        Self {
            class,
            msg,
            detail,
            original: Some(Original::V3(Box::new(original))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The retry policy each class implies. Written as a table because this is
    /// the property the whole type exists to make readable — and because it was
    /// previously spread over 55 construction sites.
    #[test]
    fn each_class_maps_to_the_variant_its_constructor_used_to_pick() {
        let cases = [
            (ErrorClass::CallerInput, false),
            (ErrorClass::Limit, false),
            (ErrorClass::Connector, true),
            (ErrorClass::Backend, false),
            (ErrorClass::Timeout, true),
        ];
        for (class, retryable) in cases {
            assert_eq!(
                class.is_retryable(),
                retryable,
                "{class:?} changed its retry policy"
            );
        }
    }

    #[test]
    fn the_variant_mapping_is_the_one_the_three_constructors_made() {
        let of = |class| DataflowError::from(HandlerError::new(class, "boom"));
        assert!(matches!(
            of(ErrorClass::CallerInput),
            DataflowError::Validation(_)
        ));
        assert!(matches!(
            of(ErrorClass::Limit),
            DataflowError::Validation(_)
        ));
        assert!(matches!(of(ErrorClass::Connector), DataflowError::Io(_)));
        assert!(matches!(
            of(ErrorClass::Backend),
            DataflowError::FunctionExecution { .. }
        ));
        assert!(matches!(of(ErrorClass::Timeout), DataflowError::Timeout(_)));
    }

    /// A `Validation` failure keeps its text, because the text is the guidance.
    #[test]
    fn a_caller_fixable_failure_keeps_its_message() {
        let err = DataflowError::from(HandlerError::new(
            ErrorClass::Limit,
            "add a LIMIT to the query or raise the cap",
        ));
        let DataflowError::Validation(msg) = err else {
            unreachable!("Limit maps to Validation")
        };
        assert_eq!(msg, "add a LIMIT to the query or raise the cap");
    }

    /// The prefix is a rendering step, not part of the message — which is what
    /// lets the static path read `msg` instead of stripping a prefix back off.
    #[test]
    fn prefixing_is_separable_from_the_message() {
        let bare = HandlerError::new(ErrorClass::CallerInput, "'from' is not a valid address");
        assert_eq!(bare.msg, "'from' is not a valid address");
        assert_eq!(
            bare.prefixed("send_email").msg,
            "send_email: 'from' is not a valid address"
        );
    }
}

#[cfg(test)]
mod round_trip_tests {
    use super::*;

    /// `DataflowError → HandlerError → DataflowError` must be the identity for
    /// a `Service` failure.
    ///
    /// The regression this exists for: a cache connector with `read` disabled
    /// refuses through `connector_detail_error`, which is a `Service` error
    /// whose service-owned `kind` is what makes it a 400. Round-tripping it
    /// through the five `ErrorClass` values re-derived it as a backend failure
    /// — a 500 `ENGINE_ERROR` — because no class can express a `kind`.
    #[test]
    fn a_service_error_survives_the_round_trip() {
        let original = crate::errors::connector_detail_error("operation 'read' is disabled");
        let back: DataflowError = HandlerError::from(original).into();
        match back {
            DataflowError::Service { kind, detail, .. } => {
                assert_eq!(kind, crate::errors::kind::CONNECTOR_DETAIL);
                assert_eq!(detail.as_deref(), Some("operation 'read' is disabled"));
            }
            other => unreachable!("a Service error must return as one, got {other:?}"),
        }
    }

    /// The same property for an integrity refusal, which arrives the same way
    /// — built as a `Service` error inside `QueryBudget::run`, converted to a
    /// `HandlerError` so the two other arms of that match still typecheck, and
    /// converted straight back.
    ///
    /// Worth its own case rather than trusting the one above: this is the
    /// error whose `kind` decides between a `409` and a `500`, and the whole
    /// point of #297 is that the two used to be indistinguishable.
    #[test]
    fn an_integrity_error_survives_the_round_trip() {
        let original = crate::errors::integrity_dataflow_error(
            crate::errors::IntegrityKind::ForeignKey,
            "data_write query failed: FOREIGN KEY constraint failed",
        );
        let back: DataflowError = HandlerError::from(original).into();
        match back {
            DataflowError::Service {
                kind, retryable, ..
            } => {
                assert_eq!(kind, crate::errors::kind::INTEGRITY_FOREIGN_KEY);
                assert!(!retryable, "an integrity failure is never retried");
            }
            other => unreachable!("a Service error must return as one, got {other:?}"),
        }
    }

    /// Rewriting the message drops the original, because it is no longer the
    /// error that message came from.
    #[test]
    fn prefixing_gives_up_the_round_trip() {
        let original = crate::errors::connector_detail_error("nope");
        let back: DataflowError = HandlerError::from(original).prefixed("crypto").into();
        assert!(
            matches!(back, DataflowError::FunctionExecution { .. }),
            "a rewritten Service error is rebuilt from its class, got {back:?}"
        );
    }
}

#[cfg(test)]
mod budget_round_trip {
    use super::*;

    /// The one variant `prefixed` must not drop: a handler that resolves a
    /// template under `engine.ops_budget` gets `BudgetExceeded` from the
    /// engine, names itself, and hands it back — and the executor must still
    /// see the variant, or the code on the message becomes `FUNCTION_ERROR`
    /// and the refusal reads as a backend fault.
    #[test]
    fn a_budget_refusal_keeps_its_variant_through_a_prefixed_handler() {
        let from_engine = DataflowError::BudgetExceeded(
            "501 operations charged against a budget of 50".to_string(),
        );
        let handler: HandlerError = from_engine.into();
        assert_eq!(handler.class, ErrorClass::Limit);
        assert!(!handler.class.is_retryable());
        let back: DataflowError = handler.prefixed("crypto").into();
        assert!(
            matches!(
                back,
                DataflowError::BudgetExceeded(ref m)
                    if m == "crypto: 501 operations charged against a budget of 50"
            ),
            "the variant was dropped: {back:?}"
        );
    }

    /// Every other original is still dropped by `prefixed`, as before: a
    /// rewritten message is not the one the original carried.
    #[test]
    fn prefixing_still_rebuilds_other_errors_from_their_class() {
        let handler: HandlerError = DataflowError::Io("connect refused".to_string()).into();
        let back: DataflowError = handler.prefixed("http_call").into();
        assert!(matches!(back, DataflowError::Io(ref m) if m == "http_call: connect refused"));
    }
}

/// The dataflow-rs 4 half (I184): each class against what dataflow-rs 3
/// records for it, and the identities the dataflow-rs 3 half above pins.
#[cfg(test)]
mod v4_tests {
    use super::*;
    use crate::engine::ResolvedSecrets;
    use crate::engine::operators::orion_exprs;
    use dataflow_rs::datavalue::OwnedDataValue;
    use serde_json::json;

    /// The code dataflow-rs 3 records for `e`, through its own classifier.
    fn v3_code(e: &DataflowError) -> String {
        dataflow_rs_v3::ErrorInfo::new(None, None, e.clone()).code
    }

    /// A budget refusal, as dataflow-rs 4's engine makes one: the only way
    /// there is, since only the engine builds an `ErrorKind::Budget`.
    fn budget_error() -> dataflow_rs::Error {
        let heavy = json!({"map": [{"var": "data.xs"}, {"+": [{"var": ""}, 1]}]});
        let context = json!({"data": {"xs": (0..100).collect::<Vec<u32>>()}});
        let bounded = orion_exprs(&ResolvedSecrets::empty(), 10).expect("builds");
        let expr = bounded.compile::<OwnedDataValue>(&heavy).expect("compiles");
        bounded
            .eval(&expr, &context)
            .expect_err("crosses the ceiling")
    }

    /// An evaluation failure, as dataflow-rs 4's engine makes one.
    fn eval_error() -> dataflow_rs::Error {
        let exprs = orion_exprs(&ResolvedSecrets::empty(), 0).expect("builds");
        let expr = exprs
            .compile::<OwnedDataValue>(&json!({"hex_decode": [{"var": "data.x"}]}))
            .expect("compiles");
        exprs
            .eval(&expr, &json!({"data": {"x": "zz"}}))
            .expect_err("not hex")
    }

    /// Each class records dataflow-rs 3's code for it, with its retryability
    /// and its message — computed from dataflow-rs 3's conversion of the same
    /// class and message, not written out.
    #[test]
    fn each_class_records_v3s_code_and_retryability() {
        for class in [
            ErrorClass::CallerInput,
            ErrorClass::Limit,
            ErrorClass::Connector,
            ErrorClass::Backend,
            ErrorClass::Timeout,
        ] {
            let v3 = DataflowError::from(HandlerError::new(class, "boom"));
            let v4 = dataflow_rs::Error::from(HandlerError::new(class, "boom"));
            assert_eq!(v4.code(), v3_code(&v3), "{class:?}");
            assert_eq!(v4.is_retryable(), v3.retryable(), "{class:?}");
            assert_eq!(v4.is_retryable(), class.is_retryable(), "{class:?}");
            assert_eq!(v4.message(), "boom", "{class:?}");
        }
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
            json!({"code": "FUNCTION_ERROR", "message": "query failed", "retryable": false})
        );
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

    /// The classes a dataflow-rs 4 error is read as: dataflow-rs 3's for the
    /// variant each kind corresponds to.
    #[test]
    fn a_v4_error_is_read_as_the_class_of_its_v3_variant() {
        let cases = [
            (budget_error(), DataflowError::BudgetExceeded("m".into())),
            (eval_error(), DataflowError::LogicEvaluation("m".into())),
            (
                crate::errors::service::circuit_open("c", "ch"),
                crate::errors::circuit_open_dataflow_error("c", "ch"),
            ),
            (
                crate::errors::service::connector_detail("d"),
                crate::errors::connector_detail_error("d"),
            ),
        ];
        for (v4, v3) in cases {
            let (v4_class, v3_class) = (HandlerError::from(v4).class, HandlerError::from(v3).class);
            assert_eq!(v4_class, v3_class);
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

    /// `prefixed` keeps a budget refusal's kind, with the prefixed message,
    /// and rebuilds every other error from its class — dataflow-rs 3's rule,
    /// so the codes and messages are dataflow-rs 3's for the same input.
    #[test]
    fn prefixed_keeps_a_budget_errors_kind_and_rebuilds_the_rest() {
        let budget = budget_error();
        let v3_budget = DataflowError::BudgetExceeded(budget.message().to_string());
        let v4 = dataflow_rs::Error::from(HandlerError::from(budget_error()).prefixed("crypto"));
        let v3 = DataflowError::from(HandlerError::from(v3_budget).prefixed("crypto"));
        assert_eq!(v4.kind(), &ErrorKind::Budget);
        assert_eq!(v4.code(), v3_code(&v3));
        assert_eq!(v4.message(), v3.to_string());
        assert_eq!(v4.is_retryable(), v3.retryable());

        let rebuilt = [
            (
                eval_error(),
                DataflowError::LogicEvaluation(eval_error().message().to_string()),
            ),
            (
                crate::errors::service::connector_detail("nope"),
                crate::errors::connector_detail_error("nope"),
            ),
            (
                dataflow_rs::Error::from(HandlerError::new(ErrorClass::Connector, "refused")),
                DataflowError::Io("refused".to_string()),
            ),
        ];
        for (v4, v3) in rebuilt {
            let v4 = dataflow_rs::Error::from(HandlerError::from(v4).prefixed("http_call"));
            let v3 = DataflowError::from(HandlerError::from(v3).prefixed("http_call"));
            assert_eq!(v4.code(), v3_code(&v3), "{v3:?}");
            assert_eq!(v4.is_retryable(), v3.retryable(), "{v3:?}");
            let v3_message = HandlerError::from(v3).msg;
            assert_eq!(v4.message(), v3_message);
        }
    }

    /// A service error survives the round trip with its code, detail and
    /// retryability: the property `QueryBudget::run` and the gate refusals
    /// rest on.
    #[test]
    fn a_service_error_survives_the_round_trip() {
        let back: dataflow_rs::Error =
            HandlerError::from(crate::errors::service::connector_detail("disabled")).into();
        assert_eq!(back.code(), crate::errors::kind::CONNECTOR_DETAIL);
        assert_eq!(detail_of(&back), Some("disabled"));

        let back: dataflow_rs::Error = HandlerError::from(crate::errors::service::integrity(
            crate::errors::IntegrityKind::ForeignKey,
            "FOREIGN KEY constraint failed",
        ))
        .into();
        assert_eq!(back.code(), crate::errors::kind::INTEGRITY_FOREIGN_KEY);
        assert!(!back.is_retryable());
    }

    /// A dataflow-rs 3 error a v4 function was handed keeps the code,
    /// retryability, message and detail dataflow-rs 3 gives it.
    #[test]
    fn a_v3_error_carried_into_v4_keeps_its_code() {
        for v3 in [
            DataflowError::Validation("bad".into()),
            DataflowError::Io("down".into()),
            DataflowError::LogicEvaluation("eval".into()),
            DataflowError::BudgetExceeded("over".into()),
            DataflowError::http(503, "upstream"),
            crate::errors::connector_detail_error("op disabled"),
            crate::errors::circuit_open_dataflow_error("c", "ch"),
        ] {
            let v4 = dataflow_rs::Error::from(HandlerError::from(v3.clone()));
            assert_eq!(v4.code(), v3_code(&v3), "{v3:?}");
            assert_eq!(v4.is_retryable(), v3.retryable(), "{v3:?}");
            assert_eq!(detail_of(&v4), v3.detail(), "{v3:?}");
            assert_eq!(v4.message(), HandlerError::from(v3.clone()).msg, "{v3:?}");
        }
    }

    /// A dataflow-rs 4 error a v3 handler was handed, through a helper both
    /// share, comes back as the dataflow-rs 3 error that records the same
    /// code — the class codes as their variants, Orion's kinds as service
    /// errors with their detail.
    #[test]
    fn a_v4_error_carried_into_v3_records_the_same_code() {
        for v4 in [
            budget_error(),
            eval_error(),
            crate::errors::service::circuit_open("c", "ch"),
            crate::errors::service::connector_detail("op disabled"),
            dataflow_rs::Error::from(HandlerError::new(ErrorClass::CallerInput, "bad")),
            dataflow_rs::Error::from(HandlerError::new(ErrorClass::Connector, "down")),
            dataflow_rs::Error::from(HandlerError::new(ErrorClass::Backend, "failed")),
            dataflow_rs::Error::from(HandlerError::new(ErrorClass::Timeout, "slow")),
        ] {
            let (code, retryable, detail) = (
                v4.code().to_string(),
                v4.is_retryable(),
                detail_of(&v4).map(str::to_string),
            );
            let v3 = DataflowError::from(HandlerError::from(v4));
            assert_eq!(v3_code(&v3), code, "{v3:?}");
            assert_eq!(v3.retryable(), retryable, "{v3:?}");
            assert_eq!(v3.detail().map(str::to_string), detail, "{v3:?}");
        }
        assert!(matches!(
            DataflowError::from(HandlerError::from(dataflow_rs::Error::from(
                HandlerError::new(ErrorClass::CallerInput, "bad")
            ))),
            DataflowError::Validation(ref m) if m == "bad"
        ));
    }
}

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

use dataflow_rs::DataflowError;

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
#[derive(Debug, Clone)]
pub struct HandlerError {
    pub class: ErrorClass,
    pub msg: String,
    /// Extra context for the operator-facing log, never for the client body.
    pub detail: Option<String>,
}

impl HandlerError {
    pub fn new(class: ErrorClass, msg: impl std::fmt::Display) -> Self {
        Self {
            class,
            msg: msg.to_string(),
            detail: None,
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
        self
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
    fn from(e: HandlerError) -> Self {
        match e.class {
            // 400, message preserved.
            ErrorClass::CallerInput | ErrorClass::Limit => DataflowError::Validation(e.msg),
            // Retryable: could not reach the backend.
            ErrorClass::Connector => DataflowError::Io(e.msg),
            // Not retryable: `function_execution` with `source: None` is what
            // dataflow-rs classifies as terminal.
            ErrorClass::Backend => DataflowError::function_execution(e.msg, None),
            ErrorClass::Timeout => DataflowError::Timeout(e.msg),
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
            detail: None,
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

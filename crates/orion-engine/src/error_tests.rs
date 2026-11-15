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
        let back = HandlerError::from(dataflow_rs::Error::from(HandlerError::new(class, "bad")));
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
/// A connector failure rides as the engine error's source through every
/// step it takes on the way to the data plane: the conversion, a
/// handler's prefix, a round trip back into a `HandlerError` (a nested
/// `channel_call`) and the engine's own `wrap`.
#[test]
fn connector_error_carries_its_failure_as_the_source() {
    use orion_extension_api::{ConnectorError, ConnectorErrorClass, EffectStatus};

    let err = crate::functions::connector_call::connector_error(ConnectorError::new(
        "ECHO_REFUSED",
        ConnectorErrorClass::ProviderFailure,
        "no route to db-7",
        EffectStatus::NotStarted,
    ));
    assert_eq!(err.class, ErrorClass::Backend);
    let engine: dataflow_rs::Error = err.prefixed("connector_call").into();
    assert_eq!(
        engine.kind(),
        &ErrorKind::Service {
            code: "FUNCTION_ERROR".into()
        },
        "the recorded task code is unchanged"
    );
    let failure = orion_common::errors::connector_failure_of(&engine).expect("the failure rides");
    assert_eq!(failure.provider_code, "ECHO_REFUSED");
    assert!(failure.from_provider);

    let nested: dataflow_rs::Error = HandlerError::from(engine).prefixed("channel_call").into();
    let nested = nested.wrap("parent");
    let failure = orion_common::errors::connector_failure_of(&nested).expect("it survives nesting");
    assert_eq!(failure.class, ConnectorErrorClass::ProviderFailure);
    assert_eq!(failure.message, "no route to db-7");
}

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
        let err = OrionError::Engine(HandlerError::new(ErrorClass::Timeout, "timed out").into());
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

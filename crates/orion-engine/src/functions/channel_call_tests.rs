use dataflow_rs::function::testing;
use dataflow_rs::program::Severity;
use serde_json::json;

use super::*;
use crate::functions::twin;

/// `engine.max_channel_call_depth`'s default.
const MAX_DEPTH: u32 = 10;

#[tokio::test]
async fn a_child_result_moves_into_the_parent_without_copying_its_buffer() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ProducesData(Arc<AtomicUsize>);
    impl Function for ProducesData {
        type Config = ();
        async fn call(&self, ctx: &mut Ctx<'_>, _: &()) -> dataflow_rs::Result<Outcome> {
            let text = "x".repeat(128 * 1024);
            self.0.store(text.as_ptr() as usize, Ordering::SeqCst);
            ctx.set(
                &kit::DATA,
                OwnedDataValue::Object(vec![
                    ("blob".to_string(), OwnedDataValue::String(text)),
                    ("amount".to_string(), OwnedDataValue::from(&json!(2.0))),
                ]),
            )?;
            ctx.set(
                &"metadata.marker".parse().expect("valid path"),
                OwnedDataValue::from("child"),
            )?;
            ctx.set(
                &"temp_data.private".parse().expect("valid path"),
                OwnedDataValue::from(true),
            )?;
            Ok(Outcome::Ok)
        }
    }
    struct Host(Arc<dataflow_rs::Engine>);
    impl ChannelCallHost for Host {
        fn resolve(&self, _: &str) -> Result<Box<dyn ChannelCallTarget>, HandlerError> {
            Ok(Box::new(Self(self.0.clone())))
        }
    }
    #[async_trait::async_trait]
    impl ChannelCallTarget for Host {
        async fn admit(&self, _: ChildRequest<'_>) -> dataflow_rs::Result<ChildAdmission> {
            Ok(ChildAdmission {
                timeout_ms: None,
                permit: None,
            })
        }
        fn engine(&self) -> &dataflow_rs::Engine {
            &self.0
        }
    }

    let original = Arc::new(AtomicUsize::new(0));
    let engine = dataflow_rs::Engine::builder()
        .register("produce", ProducesData(original.clone()))
        .workflow_json(&json!({"id":"child","channel":"child","tasks":[
            {"id":"produce","function":{"name":"produce"}}
        ]}))
        .build()
        .expect("child engine builds");
    let called = testing::call(
        ChannelCallFunction {
            host: Arc::new(Host(Arc::new(engine))),
            max_call_depth: MAX_DEPTH,
            default_timeout_ms: 30000,
        },
        json!({"channel":"child","output":"data.result"}),
    )
    .exprs(kit::test_exprs())
    .name(NAME)
    .message(Message::from_parts(
        OwnedDataValue::Null,
        OwnedDataValue::from(&json!({"kept":true})),
        OwnedDataValue::from(&json!({"marker":"parent"})),
    ))
    .await;
    assert!(called.outcome.is_ok(), "{:?}", called.outcome);
    let result = called
        .message
        .context()
        .data()
        .get("result")
        .expect("child result");
    let blob = result
        .get("blob")
        .and_then(OwnedDataValue::as_str)
        .expect("child string");
    assert_eq!(blob.len(), 128 * 1024);
    assert_eq!(
        blob.as_ptr() as usize,
        original.load(Ordering::SeqCst),
        "the child buffer must move across the call boundary"
    );
    assert_eq!(Value::from(result)["amount"], json!(2.0));
    assert_eq!(Value::from(called.message.context().data())["kept"], true);
    assert_eq!(
        Value::from(called.message.context().metadata())["marker"],
        "parent"
    );
    assert!(
        called
            .message
            .context()
            .temp_data()
            .get("private")
            .is_none()
    );
}

/// Declaration order is the field table's (I179); `channel` is
/// required (I183); `output` defaults to `data`; `data` and
/// `timeout_ms` written as `null` are unset, as serde read 1.12's
/// `Option<Template>`.
#[test]
fn the_config_compiles_and_checks_its_inputs() {
    let compile =
        |input: Value| testing::config_with::<ChannelCallConfig>(&kit::test_exprs(), input);
    let cfg = compile(json!({"channel": "orders"})).expect("compiles");
    assert_eq!(
        cfg.channel.constant(),
        Some(&OwnedDataValue::from("orders"))
    );
    assert_eq!(
        cfg.output.constant().map(ToString::to_string).as_deref(),
        Some("data")
    );
    assert!(cfg.data.is_none() && cfg.timeout_ms.is_none());
    let cfg = compile(json!({"channel": {"var": "data.to"}, "data": null,
                                 "output": null, "timeout_ms": null}))
    .expect("compiles");
    assert!(cfg.channel.constant().is_none());
    assert!(read::present(&cfg.data).is_none() && read::present(&cfg.timeout_ms).is_none());
    assert_eq!(
        cfg.output.constant().map(ToString::to_string).as_deref(),
        Some("data")
    );
}

/// What 1.12 refused when it built its engine is refused at load, in
/// its words: a `channel` that is a non-string scalar
/// (`check_channel_shape`, which `compile_input` ran). A `null` one,
/// which 1.12 read as no channel and refused on every call, is refused
/// with that call's words (a named break). A computed or literal name
/// loads, as 1.12 loaded it.
#[test]
fn a_channel_that_can_name_no_channel_is_refused_at_load_in_1_12s_words() {
    for channel in [json!(7), json!(true), json!(1.5)] {
        let expected = format!(
            "channel_call 'channel' must be a channel name or an expression \
                 producing one, not {channel}"
        );
        let issues = testing::config_with::<ChannelCallConfig>(
            &kit::test_exprs(),
            json!({ "channel": channel }),
        )
        .expect_err("refused at load");
        let refused: Vec<_> = issues
            .iter()
            .filter(|i| i.severity == Severity::Rejected)
            .map(|i| (i.path.to_string(), i.code, i.message.clone()))
            .collect();
        assert_eq!(
            refused,
            [("channel".to_string(), IssueCode::InputParse, expected)],
            "{channel}"
        );
    }
    // `null` is no channel: 1.12 failed every call with its refusal.
    let expected = "channel_call requires 'channel'".to_string();
    let issues =
        testing::config_with::<ChannelCallConfig>(&kit::test_exprs(), json!({"channel": null}))
            .expect_err("refused at load");
    assert_eq!(
        issues
            .iter()
            .map(|i| (i.path.to_string(), i.code, i.message.clone()))
            .collect::<Vec<_>>(),
        [("channel".to_string(), IssueCode::InputParse, expected)]
    );
    for channel in [json!("orders"), json!({"var": "data.to"}), json!(["a"])] {
        testing::config_with::<ChannelCallConfig>(
            &kit::test_exprs(),
            json!({ "channel": channel }),
        )
        .expect("loads, as on 1.12");
    }
}

/// The child's deadline is the earlier bound, the run's on a tie, and
/// an own bound past the clock's range is no bound.
#[test]
fn the_childs_deadline_is_the_earlier_bound_and_a_tie_is_the_runs() {
    let now = std::time::Instant::now();
    let soon = now + Duration::from_millis(10);
    let late = now + Duration::from_millis(20);
    let limits = |own, run| ChildLimits::new(own, run);
    assert_eq!(
        limits(Some(soon), Some(late)),
        ChildLimits {
            deadline: Some(soon),
            own: true
        }
    );
    assert_eq!(
        limits(Some(late), Some(soon)),
        ChildLimits {
            deadline: Some(soon),
            own: false
        }
    );
    assert_eq!(
        limits(Some(soon), Some(soon)),
        ChildLimits {
            deadline: Some(soon),
            own: false
        }
    );
    assert_eq!(
        limits(Some(soon), None),
        ChildLimits {
            deadline: Some(soon),
            own: true
        }
    );
    assert_eq!(
        limits(None, Some(late)),
        ChildLimits {
            deadline: Some(late),
            own: false
        }
    );
    assert_eq!(
        limits(None, None),
        ChildLimits {
            deadline: None,
            own: false
        }
    );
    assert!(own_deadline(1).is_some());
}

/// A call through `function::testing` on a node that serves nothing
/// refuses as a target nothing serves.
#[tokio::test]
async fn a_call_through_testing_refuses_a_target_the_node_lacks() {
    let called = testing::call(
        ChannelCallFunction {
            host: Arc::new(kit::NoChannels),
            max_call_depth: MAX_DEPTH,
            default_timeout_ms: 30_000,
        },
        json!({"channel": "orders"}),
    )
    .exprs(kit::test_exprs())
    .name(NAME)
    .await;
    let failure = twin::seen(&called).expect_err("refused");
    let expected: dataflow_rs::Error = not_found("orders").into();
    assert_eq!(failure.message, expected.to_string());
}

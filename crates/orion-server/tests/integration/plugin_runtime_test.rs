//! The one plugin-runtime test that needs a whole node.
//!
//! The rest of the sandbox's end-to-end suite lives in `orion-plugin-host`,
//! beside the code it exercises: it needs the plugin host and an engine, not a
//! served app, and building the server's integration binary for it was the
//! bulk of the `plugin-sdk` CI job's wall time (P-29).

/// The node's scheduler is sized from `engine.native_work_max_in_flight` and
/// reported under `native_scheduler` in the runtime capabilities.
/// The node's scheduler is sized from `engine.native_work_max_in_flight` and
/// reported under `native_scheduler` in the runtime capabilities.
#[tokio::test]
async fn the_native_work_bound_comes_from_config() {
    use tower::ServiceExt;

    let mut config = orion_common::config::AppConfig::default();
    config.engine.native_work_max_in_flight = 3;
    let app = crate::common::test_app_with_config(config).await;
    let resp = app
        .oneshot(crate::common::json_request(
            "GET",
            "/api/v2/admin/runtime-capabilities",
            None,
        ))
        .await
        .expect("responds");
    let caps = crate::common::body_json(resp).await;
    assert_eq!(caps["native_scheduler"]["max_in_flight"], 3, "{caps}");
    assert_eq!(caps["native_scheduler"]["in_flight"], 0, "{caps}");
}

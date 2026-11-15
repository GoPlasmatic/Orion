use super::*;
use crate::connector::circuit_breaker::CircuitBreakerConfig;
use crate::connector::{ConnectorConfig, HttpOperationGates};
use crate::{ErrorClass, HandlerError};
use dataflow_rs::Function;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::function::testing;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};

/// One call as its caller sees it: whether the work ran, and the error's
/// code, retryability and the text its record carries.
type Seen = (bool, Option<(String, bool, String)>);

/// What each call of a plan does: succeed, or fail with a class.
type Plan<'p> = &'p [Option<ErrorClass>];

#[test]
fn retry_projection_preserves_the_host_safety_distinctions() {
    use dataflow_rs::function::RetrySafety as EngineSafety;
    for safety in [
        schema::RetrySafety::Pure,
        schema::RetrySafety::Read,
        schema::RetrySafety::IdempotentWrite,
    ] {
        assert_eq!(retry(safety), EngineSafety::Idempotent);
    }
    assert_eq!(
        retry(schema::RetrySafety::UnsafeWrite),
        EngineSafety::NotIdempotent
    );
    assert_eq!(
        retry(schema::RetrySafety::DependsOn { input: "operation" }),
        EngineSafety::Unknown
    );
}

fn breakers(threshold: u32) -> ConnectorRegistry {
    ConnectorRegistry::new(CircuitBreakerConfig {
        enabled: true,
        failure_threshold: threshold,
        recovery_timeout_secs: 300,
        max_breakers: 100,
    })
}

/// `plan` through the kit's shell, connector `c` on channel `ch`.
async fn through_guarded(registry: &ConnectorRegistry, plan: Plan<'_>) -> Vec<Seen> {
    let mut seen = Vec::new();
    for step in plan {
        let ran = AtomicBool::new(false);
        let result = guarded("probe", registry, "c", "ch", async {
            ran.store(true, Ordering::Relaxed);
            match step {
                None => Ok(Outcome::Ok),
                Some(class) => Err(HandlerError::new(*class, "down").into()),
            }
        })
        .await;
        let error = result.err().map(|e| {
            (
                e.code().to_string(),
                e.is_retryable(),
                e.message().to_string(),
            )
        });
        seen.push((ran.load(Ordering::Relaxed), error));
    }
    seen
}

/// The exposition lines of the connector and breaker counters `run`
/// emitted, on a recorder of its own: what Prometheus is told, less the
/// latencies, which differ run to run.
fn counters(run: impl FnOnce() + Send) -> Vec<String> {
    let exposition = crate::metrics::render_local(run);
    let mut lines: Vec<String> = exposition
        .lines()
        .filter(|l| {
            l.starts_with("orion_connector_requests_total")
                || l.starts_with("orion_circuit_breaker_")
        })
        .map(str::to_string)
        .collect();
    lines.sort();
    lines
}

fn block_on<T>(f: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(f)
}

/// `test_breaker_trips_after_threshold`, `test_breaker_reset_via_admin_api`
/// and the shed request's code, at the shell: three retryable failures
/// open the breaker, the next calls never reach the work and fail
/// `circuit_open`, and a reset closes it — every outcome, every breaker
/// state and every counter as 1.12's `guarded_handler` gave them.
#[test]
fn guarded_trips_and_resets_the_breaker() {
    let connect = Some(ErrorClass::Connector);
    let plan = [connect, connect, connect, None, None];
    let mut seen = Vec::new();
    let mut states = Vec::new();
    let lines = counters(|| {
        block_on(async {
            let registry = breakers(3);
            seen.extend(through_guarded(&registry, &plan).await);
            states.push(registry.circuit_breaker_states());
            assert!(registry.reset_circuit_breaker("ch:c"));
            seen.extend(through_guarded(&registry, &[None]).await);
            states.push(registry.circuit_breaker_states());
        });
    });

    assert_eq!(seen.iter().filter(|(ran, _)| *ran).count(), 4, "{seen:?}");
    let shed = seen[3].1.as_ref().expect("shed");
    assert_eq!(shed.0, crate::errors::kind::CIRCUIT_OPEN);
    assert!(shed.1, "an open breaker is retryable");
    assert_eq!(
        shed.2,
        "Circuit breaker open for connector 'c' on channel 'ch'"
    );
    assert_eq!(states[0].get("ch:c").map(String::as_str), Some("open"));
    assert_eq!(states[1].get("ch:c").map(String::as_str), Some("closed"));
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("orion_circuit_breaker_trips_total"))
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("orion_circuit_breaker_rejections_total"))
    );
}

/// `test_breaker_disabled_by_default`: with the breaker off, every call
/// reaches the work, and no breaker is made.
#[tokio::test]
async fn guarded_is_a_pass_through_with_the_breaker_off() {
    let plan = [Some(ErrorClass::Connector); 10];
    let registry = ConnectorRegistry::new(CircuitBreakerConfig::default());
    let kit = through_guarded(&registry, &plan).await;
    assert!(registry.circuit_breaker_states().is_empty());
    assert!(kit.iter().all(|(ran, _)| *ran));
    assert!(
        kit.iter()
            .all(|(_, e)| e.as_ref().map(|e| e.0.as_str()) == Some("IO_ERROR")),
        "{kit:?}"
    );
}

/// `a_rejected_query_does_not_trip_the_breaker`: a failure the caller
/// caused is no evidence about the dependency, at a threshold of one.
#[tokio::test]
async fn a_failure_that_is_not_retryable_never_trips_the_breaker() {
    let plan = [
        Some(ErrorClass::CallerInput),
        Some(ErrorClass::Backend),
        Some(ErrorClass::Limit),
        Some(ErrorClass::Backend),
    ];
    let registry = breakers(1);
    let kit = through_guarded(&registry, &plan).await;
    let states = registry.circuit_breaker_states();
    assert_eq!(states.get("ch:c").map(String::as_str), Some("closed"));
    assert!(kit.iter().all(|(ran, _)| *ran));
}

/// `test_breaker_half_open_recovery`: past the cooldown the next call is
/// let through as a probe, and a failed probe opens the breaker again.
#[tokio::test(start_paused = true)]
async fn after_the_cooldown_a_probe_reaches_the_work() {
    let connect = Some(ErrorClass::Connector);
    let registry = breakers(2);
    let mut seen = through_guarded(&registry, &[connect, connect, connect]).await;
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    seen.extend(through_guarded(&registry, &[connect, connect]).await);
    let ran: Vec<bool> = seen.iter().map(|(ran, _)| *ran).collect();
    assert_eq!(ran, [true, true, false, true, false]);
    let states = registry.circuit_breaker_states();
    assert_eq!(states.get("ch:c").map(String::as_str), Some("open"));
}

/// Writes `kit::channel(ctx)` at `data.channel`.
struct Channel;

impl Function for Channel {
    type Config = ();

    async fn call(&self, ctx: &mut Ctx<'_>, _cfg: &()) -> dataflow_rs::Result<Outcome> {
        let channel = OwnedDataValue::String(channel(ctx).to_string());
        ctx.set(&"data.channel".parse()?, channel)?;
        Ok(Outcome::Ok)
    }
}

#[tokio::test]
async fn channel_reads_the_runs_channel_and_falls_back_to_unknown() {
    let at: dataflow_rs::Path = "data.channel".parse().expect("a path");
    let written = |called: &testing::Called| called.message.context().get(&at).map(Value::from);
    let on = testing::call(Channel, json!({})).channel("orders").await;
    assert_eq!(written(&on), Some(json!("orders")));
    let off = testing::call(Channel, json!({})).await;
    assert_eq!(written(&off), Some(json!("unknown")));
}

/// The code, retryability, recorded text and detail of an error.
fn facts(e: &dataflow_rs::Error) -> (String, bool, String, Option<String>) {
    (
        e.code().to_string(),
        e.is_retryable(),
        e.message().to_string(),
        crate::service::detail_of(e).map(str::to_string),
    )
}

#[tokio::test]
async fn the_connector_lookups_refuse_as_1_12_did() {
    let registry = ConnectorRegistry::new(CircuitBreakerConfig::default());
    let kit = resolve_connector(&registry, "missing")
        .await
        .expect_err("absent");
    assert_eq!(
        facts(&kit.into()),
        (
            "FUNCTION_ERROR".to_string(),
            false,
            "Function execution error: Connector 'missing' not found".to_string(),
            None
        )
    );

    let cache: ConnectorConfig =
        serde_json::from_str(r#"{"type":"cache","backend":"memory"}"#).expect("cache config");
    let kit = require_connector::<crate::connector::kind::Db>(&cache, "search")
        .expect_err("not a database");
    assert_eq!(
        facts(&kit.into()),
        (
            crate::errors::kind::CONNECTOR_DETAIL.to_string(),
            false,
            "Request validation failed".to_string(),
            Some(format!(
                "Connector 'search' is not {}",
                <crate::connector::kind::Db as crate::connector::kind::ConnectorTarget>::noun()
            ))
        )
    );
    assert!(require_connector::<crate::connector::kind::Cache>(&cache, "search").is_ok());
}

/// A probe with the fields a sample is attributed by.
#[derive(Debug, dataflow_rs::function::FunctionConfig)]
struct Target {
    connector: Option<String>,
    connection: Option<String>,
    channel: Option<dataflow_rs::Expr<String>>,
}

/// Answers `Ok`, after a nested sample of its own when asked.
struct Probe {
    nested: bool,
}

impl Function for Probe {
    type Config = Target;

    async fn call(&self, _ctx: &mut Ctx<'_>, _cfg: &Target) -> dataflow_rs::Result<Outcome> {
        if self.nested {
            crate::profile::record("db_read", Some("inner-db"), async {}).await;
        }
        Ok(Outcome::Ok)
    }
}

/// One call of `probe` under the layer, as `name`, with a collector in
/// scope when `profiled`; the collector's rendered profile.
async fn profiled(probe: Probe, name: &str, input: Value, profiled: bool) -> Value {
    let collector = crate::profile::ProfileCollector::new();
    let call = testing::call(probe, input).name(name).layer(ProfileLayer);
    let called = if profiled {
        crate::profile::ORION_PROFILE
            .scope(Arc::clone(&collector), call.into_future())
            .await
    } else {
        call.await
    };
    assert!(called.issues.is_empty(), "{:?}", called.issues);
    assert_eq!(called.outcome.expect("the probe answers"), Outcome::Ok);
    collector.to_json()
}

#[tokio::test]
async fn profile_layer_records_a_sample_per_call_with_a_collector_in_scope() {
    let profile = profiled(
        Probe { nested: false },
        "db_read",
        json!({"connector": "db1"}),
        true,
    )
    .await;
    let handlers = profile["handlers"].as_array().expect("handlers");
    assert_eq!(handlers.len(), 1, "{profile}");
    assert_eq!(handlers[0]["function"], "db_read", "named by ctx.info()");
    assert_eq!(handlers[0]["connector"], "db1", "attributed by ctx.input()");
    assert_eq!(profile["by_function"]["db_read"]["count"], 1);
    assert_eq!(profile["by_connector"]["db1"]["count"], 1);

    // `connector_call`'s label is the `connection` it targets.
    let profile = profiled(
        Probe { nested: false },
        "connector_call",
        json!({"connection": "stripe"}),
        true,
    )
    .await;
    assert_eq!(profile["by_connector"]["stripe"]["count"], 1, "{profile}");

    // `channel_call`'s label is the channel it reaches, computed or not.
    let profile = profiled(
        Probe { nested: false },
        "channel_call",
        json!({"channel": {"cat": ["downstream", "-ch"]}}),
        true,
    )
    .await;
    assert_eq!(
        profile["by_connector"]["downstream-ch"]["count"], 1,
        "{profile}"
    );

    // A function with neither field is profiled too, unattributed.
    let profile = profiled(Probe { nested: false }, "crypto", json!({}), true).await;
    assert_eq!(profile["handlers"][0]["function"], "crypto");
    assert!(profile["handlers"][0].get("connector").is_none());
}

/// A sample taken inside the call nests under the layer's, as a
/// `channel_call`'s child calls nest under it.
#[tokio::test]
async fn a_sample_inside_the_call_nests_under_the_layers() {
    let profile = profiled(
        Probe { nested: true },
        "channel_call",
        json!({"channel": "orders"}),
        true,
    )
    .await;
    let handlers = profile["handlers"].as_array().expect("handlers");
    assert_eq!(
        handlers.len(),
        1,
        "only the outer call is top-level: {profile}"
    );
    assert_eq!(handlers[0]["function"], "channel_call");
    assert_eq!(handlers[0]["nested"][0]["function"], "db_read");
    assert_eq!(handlers[0]["nested"][0]["depth"], 1);
}

#[tokio::test]
async fn profile_layer_records_nothing_without_a_collector() {
    let profile = profiled(
        Probe { nested: false },
        "db_read",
        json!({"connector": "db1"}),
        false,
    )
    .await;
    assert_eq!(profile["handlers"], json!([]), "{profile}");
}

/// The gates refuse as 1.12's did: the code, retryability, recorded text
/// and operator-only detail, open and closed.
#[test]
fn the_gates_refuse_as_1_12_did() {
    let refused = |detail: &str| {
        Err((
            crate::errors::kind::CONNECTOR_DETAIL.to_string(),
            false,
            "Request validation failed".to_string(),
            Some(detail.to_string()),
        ))
    };
    assert_eq!(
        require_op(true, "publish", "events").map_err(|e| facts(&e.into())),
        Ok(())
    );
    assert_eq!(
        require_op(false, "publish", "events").map_err(|e| facts(&e.into())),
        refused("operation 'publish' is disabled on connector 'events'")
    );
    // An empty list allows everything; a listed method matches whatever
    // its case.
    for (methods, allowed) in [
        (vec![], ["GET", "POST", "PUT"].as_slice()),
        (vec!["GET".to_string()], ["GET"].as_slice()),
        (
            vec!["get".to_string(), "PUT".to_string()],
            ["GET", "PUT"].as_slice(),
        ),
    ] {
        let gates = HttpOperationGates { methods };
        for method in ["GET", "POST", "PUT"] {
            let got = require_method_allowed(&gates, method, "api").map_err(|e| facts(&e.into()));
            let expected = if allowed.contains(&method) {
                Ok(())
            } else {
                refused(&format!(
                    "HTTP method '{method}' is not allowed on connector 'api' (allowed: {})",
                    gates.methods.join(", ")
                ))
            };
            assert_eq!(got, expected, "{method} against {:?}", gates.methods);
        }
    }
}

/// Each function `register_functions` registers carries its `schema.rs`
/// row: the description, the category, the retry safety mapped (I179),
/// and the connector reference (I187).
#[test]
fn spec_is_the_schema_row() {
    use dataflow_rs::function::RetrySafety;
    let offline = OfflineDeps::new();
    let mut registry = dataflow_rs::Registry::new(test_exprs());
    crate::register_functions(&mut registry, offline.deps());
    let mut seen = 0;
    for info in registry.catalogue().filter(|info| !info.builtin) {
        let row = schema::registry()
            .iter()
            .find(|row| row.name == info.name)
            .expect("a row");
        assert_eq!(info.spec.description, row.description, "{}", info.name);
        assert_eq!(info.spec.category, row.category, "{}", info.name);
        let retry = match row.retry_safety {
            schema::RetrySafety::UnsafeWrite => RetrySafety::NotIdempotent,
            schema::RetrySafety::DependsOn { .. } => RetrySafety::Unknown,
            _ => RetrySafety::Idempotent,
        };
        assert_eq!(info.spec.retry, retry, "{}", info.name);
        let references: Vec<_> = info
            .spec
            .references
            .iter()
            .map(|r| (r.field.to_string(), r.kind.to_string()))
            .collect();
        let expected: Vec<_> = row
            .connector
            .iter()
            .map(|_| ("connector".to_string(), row.name.to_string()))
            .collect();
        assert_eq!(references, expected, "{}", info.name);
        seen += 1;
    }
    assert!(seen > 0, "register_functions registered nothing");
}

/// `register_functions` registers every host function over the inert
/// dependencies, each under its 1.12 name.
#[tokio::test]
async fn offline_deps_register_every_function() {
    let offline = OfflineDeps::new();
    let mut registry = dataflow_rs::Registry::new(test_exprs());
    crate::register_functions(&mut registry, offline.deps());
    let hosts: Vec<&str> = registry
        .catalogue()
        .filter(|info| !info.builtin)
        .map(|info| info.name)
        .collect();
    let mut ported: Vec<_> = schema::registry().iter().map(|row| row.name).collect();
    ported.sort_unstable();
    assert_eq!(hosts, ported, "the catalogue lists host functions by name");
    // The engine every test compiles on: Orion's operators, no secrets.
    assert!(
        test_exprs()
            .operators()
            .iter()
            .any(|op| op.name == "hex_encode")
    );
    assert!(test_exprs().secrets().names().next().is_none());
}

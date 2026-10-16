pub mod channel_call;
pub mod connector_handler;
pub mod connector_helpers;
pub mod http_call;
pub mod http_common;
pub mod publish_kafka;
pub mod registry;
pub mod schema;
pub mod secret_ref;
pub mod stub;
pub mod templated_input;

pub mod cache_delete;
pub mod cache_incr;
pub mod cache_invalidate;
pub mod cache_read;
pub mod cache_write;
pub mod crypto;
pub mod data_query;
pub mod data_write;
pub mod db_read;
pub mod db_write;
pub mod jwt_sign;
pub mod jwt_verify;
pub mod model_infer;
pub mod mongo_aggregate;
pub mod mongo_common;
pub mod mongo_read;
pub mod mongo_write;
pub mod send_email;
pub mod storage_head;
pub mod storage_presign;

use dataflow_rs_v3::engine::message::Message;
// Only the `#[cfg(test)]` harness below needs this now that the retry loop
// itself lives upstream.
#[cfg(test)]
use serde_json::Value;

/// Convert a dataflow `HttpMethod` to a reqwest `Method`.
///
/// `HttpMethod::as_str` (dataflow-rs 3.1) is tied by an upstream test to the
/// enum's own `Deserialize` spelling, so the token is always a valid method
/// name and `from_bytes` cannot actually fail. The fallback keeps this
/// infallible rather than asserting on that.
pub fn to_reqwest_method(method: &dataflow_rs_v3::HttpMethod) -> reqwest::Method {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET)
}

/// Extract the channel name from a message's metadata.
pub fn extract_channel(message: &Message) -> &str {
    message
        .metadata()
        .get("channel")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
}

/// Test-only shared harness: run `tasks` through a real engine with `fns`
/// registered, seed `data` (unless null) into the message context, and return
/// the message's final `data` — or the first task error, formatted. The
/// per-handler test modules each used to hand-roll these ~25 lines.
#[cfg(test)]
pub(crate) async fn run_test_tasks(
    fns: std::collections::HashMap<String, dataflow_rs_v3::BoxedFunctionHandler>,
    tasks: Value,
    data: Value,
) -> Result<Value, String> {
    let workflow: dataflow_rs_v3::Workflow = serde_json::from_value(serde_json::json!({
        "id": "w", "name": "w", "condition": true, "tasks": tasks
    }))
    .map_err(|e| e.to_string())?;
    let engine = dataflow_rs_v3::Engine::new(vec![workflow], fns).map_err(|e| e.to_string())?;
    let mut message = dataflow_rs_v3::Message::from_value(&serde_json::json!({}));
    if !data.is_null() {
        dataflow_rs_v3::engine::utils::set_nested_value(
            &mut message.context,
            "data",
            dataflow_rs_v3::datavalue::OwnedDataValue::from(&data),
        );
    }
    engine
        .process_message(&mut message)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(err) = message.errors().first() {
        return Err(format!("{err:?}"));
    }
    Ok(message.data().into())
}

/// One-task convenience over [`run_test_tasks`].
#[cfg(test)]
pub(crate) async fn run_test_task(
    name: &str,
    handler: dataflow_rs_v3::BoxedFunctionHandler,
    input: Value,
    data: Value,
) -> Result<Value, String> {
    let mut fns: std::collections::HashMap<String, dataflow_rs_v3::BoxedFunctionHandler> =
        Default::default();
    fns.insert(name.to_string(), handler);
    run_test_tasks(
        fns,
        serde_json::json!([{"id": "t", "name": "t", "function": {"name": name, "input": input}}]),
        data,
    )
    .await
}

/// Test-only: one task through each engine, as a client reads the result, so
/// a dataflow-rs 4 function's test holds it to its dataflow-rs 3 handler over
/// the same input — the same writes, outcome and error (the port rules).
///
/// What is compared is what leaves the engine: the message's `data` after
/// the task, or the failure that stopped it, with its code, the text its
/// record carries after `"Task t error: "`, its retryability and its
/// operator-only detail.
#[cfg(test)]
pub(crate) mod twin {
    use std::sync::Arc;

    use dataflow_rs::datavalue::OwnedDataValue;
    use dataflow_rs::function::testing;
    use dataflow_rs::run::ErrorKind;
    use serde_json::{Value, json};

    use crate::connector::{ConnectorConfig, ConnectorRegistry};

    /// A task's failure, as its record and a retry read it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Failure {
        pub code: String,
        /// The record's text after `"Task t error: "`.
        pub message: String,
        pub retryable: bool,
        pub detail: Option<String>,
    }

    /// The message's `data` after the task, or the failure that stopped it.
    pub(crate) type Seen = Result<Value, Failure>;

    /// A connector registry holding `connectors`, each a connector config's
    /// JSON (`{"type": "http", …}`), with the circuit breaker off.
    pub(crate) async fn connectors(connectors: &[(&str, Value)]) -> Arc<ConnectorRegistry> {
        let registry = Arc::new(ConnectorRegistry::new(Default::default()));
        for (name, config) in connectors {
            let config: ConnectorConfig =
                serde_json::from_value(config.clone()).expect("a connector config");
            registry.insert_for_test(name, config).await;
        }
        registry
    }

    /// A SQLite database of its own, in memory, behind the `db` connector
    /// `db`, as a function reaches it: its pool cache, the registry holding
    /// the connector, and the pool itself, for a test to seed or to hold.
    pub(crate) struct Sqlite {
        pub pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
        pub registry: Arc<ConnectorRegistry>,
        pub pool: sqlx::SqlitePool,
    }

    /// [`Sqlite`], seeded by running `seed` in order. The connector holds
    /// one connection to a shared-cache database, so the database lives as
    /// long as the pool does and a test holding that connection makes the
    /// next acquire wait. `config` adds to the connector's config (a
    /// `query_timeout_ms`, its `operations`).
    pub(crate) async fn sqlite(config: Value, seed: &[&str]) -> Sqlite {
        use crate::connector::pool_cache::{SqlPool, SqlPoolCache};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut connector = json!({
            "type": "db",
            "connection_string": format!(
                "sqlite:file:twin_{}_{n}?mode=memory&cache=shared",
                std::process::id()
            ),
            "max_connections": 1,
        });
        if let (Some(connector), Some(config)) = (connector.as_object_mut(), config.as_object()) {
            connector.extend(config.clone());
        }
        let registry = connectors(&[("db", connector.clone())]).await;
        let Ok(ConnectorConfig::Db(db)) = serde_json::from_value::<ConnectorConfig>(connector)
        else {
            unreachable!("a db connector")
        };
        let pool_cache = Arc::new(SqlPoolCache::new(8));
        let SqlPool::Sqlite(pool) = pool_cache
            .get_pool("db", &db)
            .await
            .expect("the pool opens")
        else {
            unreachable!("a sqlite connection string opens a sqlite pool")
        };
        for statement in seed {
            sqlx::query(sqlx::AssertSqlSafe(*statement))
                .execute(&pool)
                .await
                .expect("the seed runs");
        }
        Sqlite {
            pool_cache,
            registry,
            pool,
        }
    }

    /// An in-memory cache of its own behind the cache connector `c`, as a
    /// function reaches it: its pool, the registry holding the connector,
    /// and the connector's config, for a test to seed or to read.
    pub(crate) struct Cache {
        pub pool: Arc<crate::connector::cache_backend::CachePool>,
        pub registry: Arc<ConnectorRegistry>,
        pub config: crate::connector::CacheConnectorConfig,
    }

    /// [`Cache`], holding `seed`: each key and the string stored at it.
    /// `config` adds to the connector's config (its `operations`).
    pub(crate) async fn cache(config: Value, seed: &[(&str, &str)]) -> Cache {
        let mut connector = json!({"type": "cache", "backend": "memory"});
        if let (Some(connector), Some(config)) = (connector.as_object_mut(), config.as_object()) {
            connector.extend(config.clone());
        }
        let registry = connectors(&[("c", connector.clone())]).await;
        let Ok(ConnectorConfig::Cache(config)) =
            serde_json::from_value::<ConnectorConfig>(connector)
        else {
            unreachable!("a cache connector")
        };
        let cache = Cache {
            pool: Arc::new(crate::connector::cache_backend::CachePool::new(4, 60, 128)),
            registry,
            config,
        };
        let backend = cache.backend().await;
        for (key, value) in seed {
            backend.set(key, value).await.expect("the seed is stored");
        }
        cache
    }

    impl Cache {
        /// The workflow store `cache_read` and its kin reach through `c`.
        pub(crate) async fn backend(
            &self,
        ) -> Arc<dyn crate::connector::cache_backend::CacheBackend> {
            self.pool
                .get_backend(
                    crate::connector::cache_backend::CachePurpose::Workflow,
                    "c",
                    &self.config,
                )
                .await
                .expect("a memory backend opens")
        }

        /// What the store holds at each of `keys`, in order.
        pub(crate) async fn held(&self, keys: &[&str]) -> Vec<Option<String>> {
            let backend = self.backend().await;
            let mut held = Vec::with_capacity(keys.len());
            for key in keys {
                held.push(backend.get(key).await.expect("a memory read"));
            }
            held
        }
    }

    /// `handler` as the one task `t` of a dataflow-rs 3 workflow, over a
    /// message whose `data` is `data` (unless `null`).
    pub(crate) async fn v3(
        name: &str,
        handler: dataflow_rs_v3::BoxedFunctionHandler,
        input: Value,
        data: Value,
    ) -> Seen {
        v3_status(name, handler, input, data).await.0
    }

    /// [`v3`], and the status the task's audit entry records when it
    /// finished: `200`, or the status it answered with (a partial bulk
    /// write's `207`).
    pub(crate) async fn v3_status(
        name: &str,
        handler: dataflow_rs_v3::BoxedFunctionHandler,
        input: Value,
        data: Value,
    ) -> (Seen, Option<usize>) {
        let mut fns: std::collections::HashMap<String, dataflow_rs_v3::BoxedFunctionHandler> =
            Default::default();
        fns.insert(name.to_string(), handler);
        let workflow: dataflow_rs_v3::Workflow = serde_json::from_value(json!({
            "id": "w", "name": "w", "condition": true,
            "tasks": [{"id": "t", "name": "t", "function": {"name": name, "input": input}}]
        }))
        .expect("a workflow");
        let engine = dataflow_rs_v3::Engine::new(vec![workflow], fns).expect("the engine builds");
        let mut message = dataflow_rs_v3::Message::from_value(&json!({}));
        if !data.is_null() {
            dataflow_rs_v3::engine::utils::set_nested_value(
                &mut message.context,
                "data",
                OwnedDataValue::from(&data),
            );
        }
        let seen = match engine.process_message(&mut message).await {
            Ok(()) => Ok(message.data().into()),
            Err(e) => {
                let record = message.errors().first().expect("the failure is recorded");
                let failure = Failure {
                    code: record.code.clone(),
                    message: e.to_string(),
                    retryable: e.retryable(),
                    detail: record.detail.clone(),
                };
                // What the harness reads off the error is what the record says.
                assert_eq!(record.message, format!("Task t error: {}", failure.message));
                Err(failure)
            }
        };
        let status = message.audit_trail().last().map(|entry| entry.status);
        (seen, status)
    }

    /// One call of `f`, registered as `name`, on Orion's expressions, over a
    /// message whose `data` is `data`. The config must compile.
    pub(crate) async fn v4<F: dataflow_rs::Function>(
        name: &str,
        f: F,
        input: Value,
        data: Value,
    ) -> Seen {
        let message = dataflow_rs::Message::from_parts(
            OwnedDataValue::Null,
            OwnedDataValue::from(&data),
            OwnedDataValue::Null,
        );
        let called = testing::call(f, input)
            .exprs(crate::engine::kit::test_exprs())
            .name(name)
            .message(message)
            .await;
        seen(&called)
    }

    /// What a call did, as [`Seen`]. A call whose config was refused fails
    /// the test, naming the issues.
    pub(crate) fn seen(called: &testing::Called) -> Seen {
        match &called.outcome {
            Ok(_) => Ok(Value::from(called.message.context().data())),
            Err(e) => {
                assert!(
                    *e.kind() != ErrorKind::Compile,
                    "the config was refused: {:?}",
                    called.issues
                );
                Err(failure(e))
            }
        }
    }

    /// A dataflow-rs 4 error as its record and a retry read it: a service
    /// error's message as it is, any other kind's under the label the
    /// recorder gives that kind, as dataflow-rs 3's `Display` did.
    pub(crate) fn failure(e: &dataflow_rs::Error) -> Failure {
        let label = match e.kind() {
            ErrorKind::Eval => "Logic evaluation error: ",
            ErrorKind::Parse => "Deserialization error: ",
            ErrorKind::TimedOut => "Timeout error: ",
            ErrorKind::FunctionNotFound => "Function not found: ",
            _ => "",
        };
        Failure {
            code: e.code().to_string(),
            message: format!("{label}{}", e.message()),
            retryable: e.is_retryable(),
            detail: crate::errors::service::detail_of(e).map(str::to_string),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dataflow_rs_v3::engine::error::DataflowError;
    use std::sync::Arc;
    use std::time::Duration;

    /// Five one-assert tests used to sit here, one per variant, and a new
    /// upstream variant would have gone untested. Driving off `HttpMethod::ALL`
    /// covers whatever the enum holds, and the `unwrap_or(GET)` fallback in
    /// `to_reqwest_method` means a name reqwest cannot parse is silent — so
    /// assert the round trip, not just that a `Method` came back.
    #[test]
    fn every_http_method_maps_to_the_same_reqwest_verb() {
        for method in dataflow_rs_v3::HttpMethod::ALL {
            assert_eq!(
                to_reqwest_method(method).as_str(),
                method.as_str(),
                "{method} did not round-trip through reqwest::Method"
            );
        }
    }

    #[test]
    fn test_extract_channel_with_channel() {
        let mut message = Message::from_value(&serde_json::json!({"key": "val"}));
        dataflow_rs_v3::engine::utils::set_nested_value(
            &mut message.context,
            "metadata.channel",
            dataflow_rs_v3::datavalue::OwnedDataValue::from("orders".to_string()),
        );
        assert_eq!(extract_channel(&message), "orders");
    }

    #[test]
    fn test_extract_channel_without_channel() {
        let message = Message::from_value(&serde_json::json!({}));
        assert_eq!(extract_channel(&message), "unknown");
    }

    /// F8: the loop must stop retrying once the deadline is spent, rather
    /// than running attempts + backoff far past the caller's budget. The loop
    /// is `engine::retry`, vendored from dataflow-rs 3.15.
    #[tokio::test]
    async fn test_retry_policy_honours_deadline() {
        use crate::engine::retry::{RetryPolicy, retry_with_policy};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();
        let start = std::time::Instant::now();
        let result = retry_with_policy(
            RetryPolicy {
                max_retries: 10,
                retry_delay_ms: 200,
                deadline: Some(Duration::from_millis(250)),
            },
            "deadline test",
            || {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Err::<Value, _>(DataflowError::Timeout("nope".into()))
                }
            },
        )
        .await;
        assert!(result.is_err());
        // 10 retries at 200ms exponential backoff would take minutes.
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "loop must stop at the deadline, took {:?}",
            start.elapsed()
        );
        assert!(
            attempts.load(Ordering::SeqCst) < 5,
            "deadline must cut the attempt count, got {}",
            attempts.load(Ordering::SeqCst)
        );
    }
}

#[cfg(test)]
mod message_shape_pin {
    //! The prefix contract, pinned before the parsers were made to yield bare
    //! messages.
    //!
    //! `crypto`, `send_email` and `storage_presign` share their parsers between
    //! two paths: `execute`, whose errors travel far from the task that
    //! produced them and so name the handler, and `validate_static_input`,
    //! whose errors become `FieldError`s where the field path already supplies
    //! that context. The prefix used to be formatted on and then stripped back
    //! off (`strip_handler_prefix`).
    //!
    //! Making the parsers bare and re-prefixing once at the shell is the fix,
    //! and the way it goes wrong is silent: an execution error that loses its
    //! prefix, or a static message that gains one. Asserted for one input per
    //! handler, so a half-converted handler fails.
    //!
    //! Written before the conversion, where it failed on all three — which is
    //! how it turned up that `strip_handler_prefix` had stopped working.
    //! `DataflowError::Validation` renders as `"Validation error: {0}"`, so a
    //! `strip_prefix("crypto: ")` never matches at position 0 and returned the
    //! whole string. The `split_once` it replaced *did* match mid-string, so
    //! hardening the function against a mid-sentence cut turned it into a
    //! no-op, and these messages have read `"Validation error: crypto: …"`
    //! ever since. Unreleased; this is the repair.

    use serde_json::json;

    /// Every static-validation message is bare — no `"{handler}: "` anywhere.
    fn assert_no_prefix(handler: &str, errors: &[(&'static str, &'static str, String)]) {
        let needle = format!("{handler}: ");
        for (field, code, message) in errors {
            assert!(
                !message.contains(&needle),
                "{handler}.validate_static_input returned a prefixed message for \
                 ({field}, {code}): {message:?}"
            );
        }
    }

    /// The other half of the contract: on the execution path the message
    /// *keeps* the handler's name, because there it travels away from the task
    /// that produced it.
    ///
    /// Both halves are asserted because the conversion can fail in either
    /// direction, and a lost prefix is the quieter of the two — nothing breaks,
    /// the message is just less useful in a trace than it was.
    #[tokio::test]
    async fn crypto_execution_messages_keep_the_handler_name() {
        let err = super::run_test_task(
            "crypto",
            Box::new(super::crypto::CryptoHandler),
            json!({"op": "hash", "data": "x", "algorithm": "not-an-algorithm"}),
            json!({}),
        )
        .await
        .expect_err("an unknown algorithm must fail the task");
        assert!(
            err.contains("crypto: "),
            "an execution-path message must name its handler: {err:?}"
        );
    }

    #[test]
    fn crypto_static_messages_are_bare() {
        let obj = json!({"op": "hash", "data": "x", "algorithm": "not-an-algorithm"});
        let errors = super::crypto::validate_static_input(obj.as_object().expect("object"));
        assert!(!errors.is_empty(), "an unknown algorithm must be reported");
        assert_no_prefix("crypto", &errors);
    }

    #[test]
    fn send_email_static_messages_are_bare() {
        let obj = json!({"connector": "c", "to": "not an address", "subject": "s", "body": "b"});
        let errors = super::send_email::validate_static_input(obj.as_object().expect("object"));
        assert!(!errors.is_empty(), "an invalid address must be reported");
        assert_no_prefix("send_email", &errors);
    }

    #[test]
    fn storage_presign_static_messages_are_bare() {
        let obj = json!({"connector": "c", "bucket": "b", "key": "k", "method": "TELEPORT"});
        let errors =
            super::storage_presign::validate_static_input(obj.as_object().expect("object"));
        assert!(!errors.is_empty(), "an unknown method must be reported");
        assert_no_prefix("storage_presign", &errors);
    }
}

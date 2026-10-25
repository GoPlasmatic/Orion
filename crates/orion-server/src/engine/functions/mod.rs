pub mod channel_call;
pub mod connector_call;
pub mod connector_helpers;
pub mod http_call;
pub mod http_common;
pub mod registry;
pub mod schema;
pub mod secret_ref;
pub mod stub;

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
pub mod storage_head;
pub mod storage_presign;

/// A `validate_static_input`'s refusals as a dataflow-rs 4 config check's
/// issues: each Rejected, `INPUT_PARSE`, at the field it names, with its
/// message. 1.12's own code for each (`unknown_op`, `unknown_stage`, …) has
/// no place on an `Issue`, whose codes are the engine's.
pub(super) fn report_static(
    refusals: Vec<(&'static str, &'static str, String)>,
    cx: &mut dataflow_rs::expr::CompileCx<'_>,
) {
    for (field, _code, message) in refusals {
        if field.is_empty() {
            cx.issue(dataflow_rs::program::IssueCode::InputParse, message);
        } else {
            cx.at(field, |cx| {
                cx.issue(dataflow_rs::program::IssueCode::InputParse, message)
            });
        }
    }
}

// Only the `#[cfg(test)]` harness below needs this.
#[cfg(test)]
use serde_json::Value;

/// Test-only: one task through the engine, as a client reads the result, and
/// the record of what 1.12 answered for the same input.
///
/// What is compared is what leaves the engine: the message's `data` after
/// the task, or the failure that stopped it, with its code, the text its
/// record carries after `"Task t error: "`, its retryability and its
/// operator-only detail.
///
/// The records under `tests/fixtures/function-records/` were the twin tests'
/// oracle while each function had a dataflow-rs 3 handler beside it: they
/// are the answers both gave, taken when the two last agreed on every case.
/// A test asserts against them with [`pinned`]; `ORION_BLESS=1` rewrites the
/// records it reaches, for a behaviour change made on purpose.
#[cfg(test)]
pub(crate) mod twin {
    use std::sync::Arc;

    use dataflow_rs::ErrorKind;
    use dataflow_rs::datavalue::OwnedDataValue;
    use dataflow_rs::function::testing;
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
            pool: Arc::new(crate::connector::cache_backend::CachePool::new(60, 128)),
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
            ErrorKind::TimedOut => "Timeout error: ",
            _ => "",
        };
        Failure {
            code: e.code().to_string(),
            message: format!("{label}{}", e.message()),
            retryable: e.is_retryable(),
            detail: crate::errors::service::detail_of(e).map(str::to_string),
        }
    }

    /// What a call saw, as its record holds it: `{"ok": data}` or
    /// `{"err": failure}`.
    fn record_of(seen: &Seen) -> Value {
        match seen {
            Ok(data) => json!({"ok": data}),
            Err(f) => json!({"err": {
                "code": f.code,
                "message": f.message,
                "retryable": f.retryable,
                "detail": f.detail,
            }}),
        }
    }

    /// Where the records of the module at `file` (`src/engine/functions/x.rs`)
    /// live, and the name they go by.
    fn records_path(file: &str) -> (String, std::path::PathBuf) {
        let module = file
            .rsplit_once("src/")
            .map_or(file, |(_, rest)| rest)
            .trim_end_matches(".rs")
            .replace('/', ".");
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/function-records")
            .join(format!("{module}.json"));
        (module, path)
    }

    fn read_records(path: &std::path::Path) -> std::collections::BTreeMap<String, Value> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).expect("a records file is JSON"),
            Err(_) => Default::default(),
        }
    }

    /// Assert that `seen` is what the calling module's record `key` holds:
    /// what 1.12 answered for that case. Under `ORION_BLESS=1` the record is
    /// written instead, and a key written twice in one run with two different
    /// answers fails, so a loop cannot pin one case over another.
    #[track_caller]
    pub(crate) fn pinned(seen: &Seen, key: impl std::fmt::Display) {
        use std::collections::{BTreeMap, HashMap};
        use std::sync::{Mutex, OnceLock};

        static BLESSED: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
        static LOADED: OnceLock<Mutex<HashMap<std::path::PathBuf, BTreeMap<String, Value>>>> =
            OnceLock::new();

        let (module, path) = records_path(std::panic::Location::caller().file());
        let key = key.to_string();
        let got = record_of(seen);

        if std::env::var_os("ORION_BLESS").is_some() {
            let mut blessed = BLESSED.get_or_init(Default::default).lock().expect("lock");
            let id = format!("{module}::{key}");
            if let Some(earlier) = blessed.insert(id.clone(), got.clone()) {
                assert_eq!(earlier, got, "{id} was pinned twice with two answers");
            }
            let mut records = read_records(&path);
            records.insert(key, got);
            std::fs::create_dir_all(path.parent().expect("a directory"))
                .expect("the records directory is writable");
            let mut text = serde_json::to_string_pretty(&records).expect("serializes");
            text.push('\n');
            std::fs::write(&path, text).expect("the records file is writable");
            return;
        }

        let mut loaded = LOADED.get_or_init(Default::default).lock().expect("lock");
        let records = loaded
            .entry(path.clone())
            .or_insert_with(|| read_records(&path));
        assert!(
            records.contains_key(&key),
            "no record `{key}` in {}; run with ORION_BLESS=1 to write it",
            path.display()
        );
        assert_eq!(
            &got, &records[&key],
            "{module}: `{key}` is not what 1.12 answered"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

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
                    Err::<Value, _>(
                        dataflow_rs::Error::service("TIMEOUT_ERROR", "nope").with_retryable(true),
                    )
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
    //! `crypto` and `storage_presign` share their parsers between
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
        // A key reference that resolves to nothing fails at execution; an
        // unknown algorithm no longer gets that far, it is refused at load.
        let err = super::twin::v4(
            super::crypto::NAME,
            super::crypto::CryptoFunction,
            json!({"op": "hmac", "data": "x", "key": "env://ORION_TEST_UNSET_HMAC_KEY_7f3a"}),
            json!({}),
        )
        .await
        .expect_err("an unresolvable key must fail the task")
        .message;
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
    fn storage_presign_static_messages_are_bare() {
        let obj = json!({"connector": "c", "bucket": "b", "key": "k", "method": "TELEPORT"});
        let errors =
            super::storage_presign::validate_static_input(obj.as_object().expect("object"));
        assert!(!errors.is_empty(), "an unknown method must be reported");
        assert_no_prefix("storage_presign", &errors);
    }
}

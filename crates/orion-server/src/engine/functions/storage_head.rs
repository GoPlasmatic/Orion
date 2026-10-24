//! `storage_head` — bounded object metadata from a storage connector (#265).
//!
//! The one network call the storage surface makes: a SigV4-signed HEAD.
//! A missing object is *data*, not failure — "is it there yet?" is the
//! question this function exists to ask — so 404 answers
//! `{ "exists": false }` while auth failures, timeouts and other statuses
//! fail the task. One attempt inside the breaker shell; like every non-HTTP
//! connector there is no retry machinery, and a workflow can loop if it
//! wants polling.

use std::sync::Arc;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::Output;
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use serde_json::{Value, json};

use super::schema::{FieldKind, FieldSchema};
use crate::connector::{ConnectorRegistry, StorageConnectorConfig, sigv4};
use crate::engine::kit::{self, read};
use crate::engine::{ErrorClass, HandlerError};

/// This function's name, for the free functions below (F48).
pub(crate) const NAME: &str = "storage_head";

/// The signed HEAD, against a resolved and gated storage connector: the
/// object's metadata, `{ "exists": false }` for a 404. Shared by the
/// dataflow-rs 3 handler and the dataflow-rs 4 function, so both ask the
/// same question.
async fn head(
    client: &reqwest::Client,
    storage: &StorageConnectorConfig,
    connector: &str,
    key: &str,
) -> Result<Value, HandlerError> {
    let (scheme, host, path) = storage
        .address(Some(key))
        .map_err(|e| HandlerError::new(ErrorClass::CallerInput, e))?;
    let url = format!("{scheme}://{host}{path}");

    // S6: the one network call the storage surface makes gets the same
    // private-address posture as every other egress.
    if !storage.allow_private_urls
        && let Err(msg) = crate::validation::validate_url_not_private(&url).await
    {
        return Err(HandlerError::new(
            ErrorClass::Backend,
            format!("SSRF protection: {msg}"),
        ));
    }

    let amz_date = sigv4::amz_date_now();
    let sig_ctx = sigv4::SigningContext::for_storage(storage, &host, &path, &amz_date);
    let mut req = client
        .head(&url)
        .timeout(std::time::Duration::from_millis(storage.timeout_ms));
    for (name, value) in sigv4::sign_headers(&sig_ctx, "HEAD") {
        req = req.header(name, value);
    }

    let response = req.send().await.map_err(|e| {
        if e.is_timeout() {
            HandlerError::new(
                ErrorClass::Timeout,
                format!("storage_head via '{connector}' timed out"),
            )
        } else {
            // `without_url`: this message names the connector rather than
            // the endpoint on purpose, and reqwest's `Display` would put
            // the URL back (#281).
            HandlerError::new(
                ErrorClass::Connector,
                format!("storage_head via '{connector}' failed: {}", e.without_url()),
            )
        }
    })?;

    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        // A missing object is data, not failure — "is it there yet?" is
        // the question this function exists to ask.
        return Ok(json!({ "exists": false }));
    }
    if !status.is_success() {
        // 403 and friends are task errors: unlike absence, they say the
        // *question* could not be asked.
        return Err(HandlerError::new(
            ErrorClass::Backend,
            format!("storage_head via '{connector}': HTTP {status}"),
        ));
    }

    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    Ok(json!({
        "exists": true,
        "size": header("content-length").and_then(|v| v.parse::<u64>().ok()),
        "etag": header("etag").map(|v| v.trim_matches('"').to_string()),
        "last_modified": header("last-modified"),
        "content_type": header("content-type"),
    }))
}

/// `storage_head`'s input on dataflow-rs 4: the connector, and the key read
/// by `kit::read`'s twin of `resolve_required_str` (I183). A task missing
/// either is refused at load (I183).
#[derive(Debug, FunctionConfig)]
pub struct StorageHeadConfig {
    /// The storage connector asked.
    connector: String,
    /// The object key.
    key: Expr<OwnedDataValue>,
    /// Where `{ exists, size, etag, last_modified, content_type }` is written.
    #[output(default = "data")]
    output: Output,
}

/// `storage_head` as a dataflow-rs 4 function: the HEAD `StorageHeadHandler`
/// sends, read in the same order.
pub struct StorageHeadFunction {
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
}

impl Function for StorageHeadFunction {
    type Config = StorageHeadConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &StorageHeadConfig,
    ) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, before the key is read: an output that
        // names no path asks nothing.
        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
        let key = ctx.eval(&cfg.key)?;
        let key = read::required_str(Some(&key), "key", NAME)?.into_owned();
        let mut metadata = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let storage =
                kit::require_connector::<crate::connector::kind::Storage>(&config, &cfg.connector)?;
            kit::require_op(storage.operations.head, "head", &cfg.connector)?;
            metadata = Some(head(&self.client, storage, &cfg.connector, &key).await?);
            Ok(Outcome::Ok)
        })
        .await?;
        if let Some(metadata) = metadata {
            ctx.set(&output, OwnedDataValue::from(&metadata))?;
        }
        Ok(Outcome::Ok)
    }
}

// -- Input schema (F53) --

pub(super) const STORAGE_HEAD_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the storage connector.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key",
        description: "Object key within the connector's bucket.",
        kind: FieldKind::String,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where { exists, size, etag, last_modified, \
                      content_type } is stored (404 means { exists: false }, not an \
                      error). Defaults to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::connector::{ConnectorConfig, StorageConnectorConfig};
    use serde_json::json;

    /// A mock store: answers HEAD with metadata when the object "exists",
    /// 404 when not, and asserts a SigV4 authorization header arrived.
    async fn spawn_store(exists: bool, status_override: Option<u16>) -> std::net::SocketAddr {
        use axum::http::{HeaderMap, StatusCode};
        let handler = move |headers: HeaderMap| async move {
            assert!(
                headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.starts_with("AWS4-HMAC-SHA256 Credential=")),
                "the HEAD must arrive signed"
            );
            assert!(headers.contains_key("x-amz-date"), "x-amz-date missing");
            if let Some(code) = status_override {
                return (StatusCode::from_u16(code).expect("test"), HeaderMap::new());
            }
            if exists {
                let mut out = HeaderMap::new();
                out.insert("content-length", "1048576".parse().expect("test"));
                out.insert("etag", "\"abc123\"".parse().expect("test"));
                out.insert(
                    "last-modified",
                    "Wed, 19 Aug 2026 07:00:00 GMT".parse().expect("test"),
                );
                out.insert("content-type", "video/mp4".parse().expect("test"));
                (StatusCode::OK, out)
            } else {
                (StatusCode::NOT_FOUND, HeaderMap::new())
            }
        };
        let app = axum::Router::new().route("/{*path}", axum::routing::head(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test");
        let addr = listener.local_addr().expect("test");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test");
        });
        addr
    }

    fn storage_config(addr: std::net::SocketAddr) -> StorageConnectorConfig {
        StorageConnectorConfig {
            provider: crate::connector::StorageProvider::S3,
            endpoint: format!("http://{addr}"),
            region: "us-east-1".to_string(),
            bucket: "media".to_string(),
            access_key: "AK".to_string(),
            secret_key: "sk".to_string(),
            session_token: None,
            // Virtual-hosted would prepend the bucket to 127.0.0.1 and
            // resolve nowhere; path-style is also what real self-hosted
            // stores want.
            force_path_style: true,
            allow_private_urls: true, // tests use localhost
            timeout_ms: 5_000,
            operations: Default::default(),
        }
    }

    // -- the config, and calls against a store mock, held to 1.12's records --

    use crate::engine::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};

    /// Declaration order is the field table's (I179); `connector` and `key`
    /// are required (I183), `connector` a literal, an unknown key refused
    /// (break 2a), and `output` defaults to `data`.
    #[test]
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = StorageHeadConfig::fields()
            .iter()
            .map(|f| &*f.name)
            .collect();
        let table: Vec<&str> = STORAGE_HEAD_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

        let compile =
            |input: Value| testing::config_with::<StorageHeadConfig>(&kit::test_exprs(), input);
        let cfg =
            compile(json!({"connector": "media", "key": {"var": "data.key"}})).expect("compiles");
        assert!(cfg.key.constant().is_none());
        assert_eq!(
            cfg.output.constant().map(ToString::to_string).as_deref(),
            Some("data")
        );
        let rejected = |input: Value| -> Vec<(String, IssueCode)> {
            compile(input)
                .expect_err("refused")
                .into_iter()
                .filter(|i| i.severity == Severity::Rejected)
                .map(|i| (i.path.to_string(), i.code))
                .collect()
        };
        assert_eq!(
            rejected(json!({"connector": "media"})),
            [("key".to_string(), IssueCode::InputParse)]
        );
        assert_eq!(
            rejected(json!({"key": "k"})),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        assert_eq!(
            rejected(json!({"connector": {"var": "data.c"}, "key": "k"})),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        let issues = rejected(json!({"connector": "media", "key": "k", "bucket": "b"}));
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].0, "bucket");
    }

    /// One task against its own store mock.
    async fn run(
        input: Value,
        exists: bool,
        status_override: Option<u16>,
        config: impl Fn(std::net::SocketAddr) -> StorageConnectorConfig,
    ) -> Seen {
        let addr = spawn_store(exists, status_override).await;
        let registry =
            std::sync::Arc::new(crate::connector::ConnectorRegistry::new(Default::default()));
        registry
            .insert_for_test("media", ConnectorConfig::Storage(config(addr)))
            .await;
        let f = StorageHeadFunction {
            registry,
            client: reqwest::Client::new(),
        };
        let seen = twin::v4(NAME, f, input, Value::Null).await;
        // The mock has its own port, which a failure may name.
        let port = format!("127.0.0.1:{}", addr.port());
        seen.map_err(|f| twin::Failure {
            message: f.message.replace(&port, "<store>"),
            ..f
        })
    }

    #[tokio::test]
    async fn an_existing_object_answers_its_metadata() {
        let input = json!({"connector": "media", "key": "video/out.mp4", "output": "data.meta"});
        let v4 = run(input, true, None, storage_config).await;
        twin::pinned(&v4, "an_existing_object_answers_its_metadata");
        assert_eq!(
            v4.expect("test")["meta"],
            json!({
                "exists": true,
                "size": 1048576,
                "etag": "abc123",
                "last_modified": "Wed, 19 Aug 2026 07:00:00 GMT",
                "content_type": "video/mp4",
            })
        );
    }

    #[tokio::test]
    async fn a_missing_object_is_data_not_failure() {
        let input = json!({"connector": "media", "key": "nope.mp4", "output": "data.meta"});
        let v4 = run(input, false, None, storage_config).await;
        twin::pinned(&v4, "a_missing_object_is_data_not_failure");
        assert_eq!(
            v4.expect("404 must not fail the task")["meta"],
            json!({"exists": false})
        );
    }

    #[tokio::test]
    async fn a_denied_request_is_an_error_and_the_gate_applies() {
        let input = json!({"connector": "media", "key": "k", "output": "data.meta"});
        let v4 = run(input.clone(), true, Some(403), storage_config).await;
        twin::pinned(&v4, "a_denied_request/403");
        let failure = v4.expect_err("403 says the question could not be asked");
        assert!(failure.message.contains("403"), "{failure:?}");

        let gated = |addr| {
            let mut config = storage_config(addr);
            config.operations.head = false;
            config
        };
        let v4 = run(input, true, None, gated).await;
        twin::pinned(&v4, "a_denied_request/gated");
        v4.expect_err("head=false must refuse");

        // A key that resolves to nothing, and a connector that is not there.
        for input in [
            json!({"connector": "media", "key": {"var": "data.none"}}),
            json!({"connector": "absent", "key": "k"}),
        ] {
            let v4 = run(input.clone(), true, None, storage_config).await;
            twin::pinned(&v4, format!("a_denied_request/{input}"));
            assert!(v4.is_err(), "{input}");
        }
    }

    /// A computed `output` is resolved before the key is read and before the
    /// HEAD, as 1.12's prologue resolved it: one that names no path is 1.12's
    /// `VALIDATION_ERROR` with its text, and the store sees no request; one
    /// that names a path is written there after one request.
    #[tokio::test]
    async fn a_computed_output_is_resolved_before_the_head_as_1_12_did() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let data = json!({"n": 3, "at": "data.meta"});
        for (output, asked) in [
            (json!({"var": "data.n"}), 0),
            (json!({"var": "data.missing.deeper"}), 1),
            (json!({"var": "data.at"}), 1),
        ] {
            let input = json!({"connector": "media", "key": "k", "output": output});
            let requests = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&requests);
            let app = axum::Router::new().fallback(move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::NOT_FOUND
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
            let registry =
                std::sync::Arc::new(crate::connector::ConnectorRegistry::new(Default::default()));
            registry
                .insert_for_test("media", ConnectorConfig::Storage(storage_config(addr)))
                .await;
            let client = reqwest::Client::new();
            let seen = twin::v4(
                NAME,
                StorageHeadFunction { registry, client },
                input.clone(),
                data.clone(),
            )
            .await;
            twin::pinned(&seen, format!("computed_output/{output}"));
            assert_eq!(requests.load(Ordering::SeqCst), asked, "{output}");
            if asked == 0 {
                let failure = seen.as_ref().expect_err("refused");
                assert_eq!(failure.code, "VALIDATION_ERROR");
                assert_eq!(
                    failure.message,
                    "Validation error: 'output' must resolve to a non-empty dotted path, got number"
                );
            }
        }
    }
}

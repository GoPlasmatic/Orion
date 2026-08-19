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

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::{Value, json};

use super::connector_helpers::{
    ConnectorCall, apply_output, require_op, require_storage_connector, resolve_required_str,
};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::{ConnectorRegistry, sigv4};

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "storage_head";

/// Workflow function handler for object-metadata lookups.
pub struct StorageHeadHandler {
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
}

#[async_trait]
impl AsyncFunctionHandler for StorageHeadHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        let call = ConnectorCall::begin(NAME, input, ctx)?;
        let key = resolve_required_str(input, "key", NAME, ctx)?;

        call.run(&self.registry, async {
            let connector_config = call.resolve(&self.registry, None).await?;
            let storage = require_storage_connector(&connector_config, call.connector)?;
            require_op(storage.operations.head, "head", call.connector)?;

            let (scheme, host, path) = storage
                .address(Some(&key))
                .map_err(DataflowError::Validation)?;
            let url = format!("{scheme}://{host}{path}");

            // S6: the one network call the storage surface makes gets the
            // same private-address posture as every other egress.
            if !storage.allow_private_urls
                && let Err(msg) = crate::validation::validate_url_not_private(&url).await
            {
                return Err(DataflowError::function_execution(
                    format!("SSRF protection: {msg}"),
                    None,
                ));
            }

            let amz_date = sigv4::amz_date_now();
            let sig_ctx = sigv4::SigningContext {
                access_key: &storage.access_key,
                secret_key: &storage.secret_key,
                session_token: storage.session_token.as_deref(),
                region: &storage.region,
                service: "s3",
                host: &host,
                path: &path,
                amz_date: &amz_date,
            };
            let mut req = self
                .client
                .head(&url)
                .timeout(std::time::Duration::from_millis(storage.timeout_ms));
            for (name, value) in sigv4::sign_headers(&sig_ctx, "HEAD") {
                req = req.header(name, value);
            }

            let response = req.send().await.map_err(|e| {
                if e.is_timeout() {
                    DataflowError::Timeout(format!(
                        "storage_head via '{}' timed out",
                        call.connector
                    ))
                } else {
                    DataflowError::Io(format!("storage_head via '{}' failed: {e}", call.connector))
                }
            })?;

            let status = response.status();
            let result = if status == reqwest::StatusCode::NOT_FOUND {
                json!({ "exists": false })
            } else if status.is_success() {
                let header = |name: &str| {
                    response
                        .headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                };
                json!({
                    "exists": true,
                    "size": header("content-length")
                        .and_then(|v| v.parse::<u64>().ok()),
                    "etag": header("etag").map(|v| v.trim_matches('"').to_string()),
                    "last_modified": header("last-modified"),
                    "content_type": header("content-type"),
                })
            } else {
                // 403 and friends are task errors: unlike absence, they say
                // the *question* could not be asked.
                return Err(DataflowError::function_execution(
                    format!("storage_head via '{}': HTTP {status}", call.connector),
                    None,
                ));
            };

            apply_output(ctx, call.output, result);
            Ok(TaskOutcome::Success)
        })
        .await
    }
}

// -- Input schema (F53) --

pub(super) const STORAGE_HEAD_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the storage connector.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "key",
        description: "Object key within the connector's bucket.",
        kind: FieldKind::String,
        required: true,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where { exists, size, etag, last_modified, \
                      content_type } is stored (404 means { exists: false }, not an \
                      error). Defaults to \"data\".",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
];

#[cfg(test)]
mod tests {
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

    async fn run(input: Value, config: StorageConnectorConfig) -> Result<Value, String> {
        let registry =
            std::sync::Arc::new(crate::connector::ConnectorRegistry::new(Default::default()));
        registry
            .insert_for_test("media", ConnectorConfig::Storage(config))
            .await;
        let workflow: dataflow_rs::Workflow = serde_json::from_value(json!({
            "id": "w", "name": "w", "condition": true,
            "tasks": [{"id": "t", "name": "t",
                       "function": {"name": NAME, "input": input}}]
        }))
        .map_err(|e| e.to_string())?;
        let mut fns: std::collections::HashMap<String, dataflow_rs::BoxedFunctionHandler> =
            Default::default();
        fns.insert(
            NAME.to_string(),
            Box::new(StorageHeadHandler {
                registry,
                client: reqwest::Client::new(),
            }),
        );
        let engine = dataflow_rs::Engine::new(vec![workflow], fns).map_err(|e| e.to_string())?;
        let mut message = dataflow_rs::Message::from_value(&json!({}));
        engine
            .process_message(&mut message)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(err) = message.errors().first() {
            return Err(format!("{err:?}"));
        }
        Ok(message.data().into())
    }

    #[tokio::test]
    async fn an_existing_object_answers_its_metadata() {
        let addr = spawn_store(true, None).await;
        let out = run(
            json!({"connector": "media", "key": "video/out.mp4", "output": "data.meta"}),
            storage_config(addr),
        )
        .await
        .expect("test");
        assert_eq!(
            out["meta"],
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
        let addr = spawn_store(false, None).await;
        let out = run(
            json!({"connector": "media", "key": "nope.mp4", "output": "data.meta"}),
            storage_config(addr),
        )
        .await
        .expect("404 must not fail the task");
        assert_eq!(out["meta"], json!({"exists": false}));
    }

    #[tokio::test]
    async fn a_denied_request_is_an_error_and_the_gate_applies() {
        let addr = spawn_store(true, Some(403)).await;
        let err = run(
            json!({"connector": "media", "key": "k", "output": "data.meta"}),
            storage_config(addr),
        )
        .await
        .expect_err("403 says the question could not be asked");
        assert!(err.contains("403"), "{err}");

        // Gate refusal is caller-sanitized; the behavioral assertion is that
        // the gated call errs where the tests above succeed ungated.
        let addr = spawn_store(true, None).await;
        let mut config = storage_config(addr);
        config.operations.head = false;
        run(
            json!({"connector": "media", "key": "k", "output": "data.meta"}),
            config,
        )
        .await
        .expect_err("head=false must refuse");
    }
}

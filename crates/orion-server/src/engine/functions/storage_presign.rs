//! `storage_presign` — a time-limited URL for one object, computed locally
//! (#265).
//!
//! Zero bytes move: SigV4 presigning is arithmetic over the connector's
//! credentials, and the client then talks to the object store directly. That
//! invariant — pure computation, no data path — is the storage connector's
//! scoping rule.

use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::connector_helpers::{
    ConnectorCall, apply_output, parse_duration_secs, require_connector, require_op,
    resolve_duration_secs, resolve_optional_str, resolve_required_str,
};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::{ConnectorRegistry, sigv4};

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "storage_presign";

/// S3's own ceiling on presigned-URL lifetime: 7 days, in seconds.
const MAX_EXPIRES_SECS: u64 = 604_800;

/// Workflow function handler that presigns object-storage URLs.
pub struct StoragePresignHandler {
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl AsyncFunctionHandler for StoragePresignHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // F48/F58: literal prologue first.
        let call = ConnectorCall::begin(NAME, input, ctx)?;
        let method = presign_method(input)?;
        check_method_fields(input, method)?;

        let key = resolve_required_str(input, "key", NAME, ctx)?;
        let expires_secs = expires_in(input, ctx)?;
        let response_content_type =
            resolve_optional_str(input, "response_content_type", NAME, ctx)?;
        let response_content_disposition =
            resolve_optional_str(input, "response_content_disposition", NAME, ctx)?;
        let content_type = resolve_optional_str(input, "content_type", NAME, ctx)?;

        call.run(&self.registry, async {
            let connector_config = call.resolve(&self.registry, None).await?;
            let storage = require_connector::<crate::connector::kind::Storage>(
                &connector_config,
                call.connector,
            )?;
            let gate = match method {
                PresignMethod::Get => storage.operations.presign_get,
                PresignMethod::Put => storage.operations.presign_put,
            };
            require_op(gate, method.gate_name(), call.connector)?;

            let (scheme, host, path) = storage
                .address(Some(&key))
                .map_err(DataflowError::Validation)?;

            // Response overrides ride the signed query; a PUT content-type is
            // a signed header the client must then send verbatim.
            let mut extra_query: Vec<(String, String)> = Vec::new();
            if let Some(v) = &response_content_type {
                extra_query.push(("response-content-type".to_string(), v.clone()));
            }
            if let Some(v) = &response_content_disposition {
                extra_query.push(("response-content-disposition".to_string(), v.clone()));
            }
            let mut extra_headers: Vec<(String, String)> = Vec::new();
            if let Some(v) = &content_type {
                extra_headers.push(("content-type".to_string(), v.clone()));
            }

            let amz_date = sigv4::amz_date_now();
            let sig_ctx = sigv4::SigningContext::for_storage(storage, &host, &path, &amz_date);
            let url = sigv4::presign_url(
                &sig_ctx,
                &scheme,
                method.as_str(),
                expires_secs,
                &extra_query,
                &extra_headers,
            );

            apply_output(ctx, call.output, Value::String(url));
            Ok(TaskOutcome::Success)
        })
        .await
    }
}

fn validation(msg: &str) -> DataflowError {
    DataflowError::Validation(format!("{NAME}: {msg}"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PresignMethod {
    Get,
    Put,
}

impl PresignMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Put => "PUT",
        }
    }

    /// The connector gate this method answers to.
    fn gate_name(self) -> &'static str {
        match self {
            Self::Get => "presign_get",
            Self::Put => "presign_put",
        }
    }
}

/// The `method` value table — an open set: a presigned DELETE would be a new
/// value here, never a new function.
fn presign_method(input: &Value) -> Result<PresignMethod, DataflowError> {
    match input.get("method").and_then(Value::as_str) {
        None | Some("GET") => Ok(PresignMethod::Get),
        Some("PUT") => Ok(PresignMethod::Put),
        Some(other) => Err(validation(&format!(
            "method '{other}' is not supported — GET (default) or PUT"
        ))),
    }
}

/// The per-method field rules: response overrides are GET's, the upload
/// content-type constraint is PUT's. Naming a field on the wrong method is a
/// misunderstanding worth refusing, not ignoring.
fn check_method_fields(input: &Value, method: PresignMethod) -> Result<(), DataflowError> {
    let present = |field: &str| input.get(field).is_some_and(|v| !v.is_null());
    match method {
        PresignMethod::Get if present("content_type") => Err(validation(
            "'content_type' applies to PUT only — for GET use 'response_content_type'",
        )),
        PresignMethod::Put
            if present("response_content_type") || present("response_content_disposition") =>
        {
            Err(validation(
                "'response_content_type'/'response_content_disposition' apply to GET only",
            ))
        }
        _ => Ok(()),
    }
}

/// The TTL: integer seconds or a single-unit duration string, bounded by
/// S3's own 7-day cap.
fn expires_in(input: &Value, ctx: &TaskContext<'_>) -> Result<u64, DataflowError> {
    let Some(raw) = input.get("expires_in") else {
        return Err(validation(
            "requires 'expires_in' (seconds, or \"<n>s|m|h|d\")",
        ));
    };
    let secs = resolve_duration_secs(raw, ctx, NAME, "expires_in")?;
    if secs == 0 || secs > MAX_EXPIRES_SECS {
        return Err(validation(&format!(
            "'expires_in' must be between 1 second and {MAX_EXPIRES_SECS} (7 days — \
             S3's own presign ceiling), got {secs}"
        )));
    }
    Ok(secs)
}

// -- Authoring-time validation (shared with schema::validate_input) --

/// Cross-field checks over a *static* input: the method value table, the
/// per-method field rules, and literal `expires_in` bounds.
pub(super) fn validate_static_input(
    obj: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    let mut errors: Vec<(&'static str, &'static str, String)> = Vec::new();
    let input = Value::Object(obj.clone());

    let method = match presign_method(&input) {
        Ok(m) => Some(m),
        Err(e) => {
            errors.push(("method", "INVALID", strip_name(&e)));
            None
        }
    };
    if let Some(method) = method
        && let Err(e) = check_method_fields(&input, method)
    {
        errors.push(("method", "INVALID", strip_name(&e)));
    }

    match obj.get("expires_in") {
        None => errors.push((
            "expires_in",
            "REQUIRED",
            "storage_presign requires 'expires_in' (seconds, or \"<n>s|m|h|d\")".to_string(),
        )),
        Some(Value::Number(n)) => {
            if !n
                .as_u64()
                .is_some_and(|secs| (1..=MAX_EXPIRES_SECS).contains(&secs))
            {
                errors.push((
                    "expires_in",
                    "INVALID",
                    format!("'expires_in' must be between 1 and {MAX_EXPIRES_SECS} seconds"),
                ));
            }
        }
        Some(Value::String(s)) => match parse_duration_secs(s) {
            Ok(secs) if (1..=MAX_EXPIRES_SECS).contains(&secs) => {}
            Ok(secs) => errors.push((
                "expires_in",
                "INVALID",
                format!(
                    "'expires_in' must be between 1 second and {MAX_EXPIRES_SECS} \
                     (7 days), got {secs}"
                ),
            )),
            Err(e) => errors.push(("expires_in", "INVALID", e)),
        },
        // A {"var": ..} node — checked at request time.
        Some(_) => {}
    }

    errors
}

fn strip_name(e: &DataflowError) -> String {
    super::schema::strip_handler_prefix(NAME, e)
}

// -- Input schema (F53) --

pub(super) const STORAGE_PRESIGN_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the storage connector.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "method",
        description: "GET (default) presigns a download; PUT presigns a direct client \
                      upload. Each answers to its own connector gate.",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key",
        description: "Object key within the connector's bucket.",
        kind: FieldKind::String,
        required: true,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "expires_in",
        description: "URL lifetime: integer seconds or \"<n>s|m|h|d\"; at most 7 days \
                      (S3's own ceiling).",
        kind: FieldKind::Any,
        required: true,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "response_content_type",
        description: "GET only: forces the Content-Type the store answers with; signed, \
                      so the client cannot alter it.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "response_content_disposition",
        description: "GET only: forces Content-Disposition (e.g. a download filename); \
                      signed.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "content_type",
        description: "PUT only: the Content-Type the uploader must send — a signed \
                      header, so an upload with any other type is refused by the store.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the presigned URL (string) is stored. Defaults \
                      to \"data\".",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::{ConnectorConfig, StorageConnectorConfig};
    use serde_json::json;

    fn storage_config(force_path_style: bool) -> StorageConnectorConfig {
        StorageConnectorConfig {
            provider: crate::connector::StorageProvider::S3,
            endpoint: "https://ap-south-1.linodeobjects.com".to_string(),
            region: "ap-south-1".to_string(),
            bucket: "media".to_string(),
            access_key: "AKEXAMPLE".to_string(),
            secret_key: "sk".to_string(),
            session_token: None,
            force_path_style,
            allow_private_urls: false,
            timeout_ms: 10_000,
            operations: Default::default(),
        }
    }

    async fn run(input: Value, config: StorageConnectorConfig) -> Result<Value, String> {
        let registry =
            std::sync::Arc::new(crate::connector::ConnectorRegistry::new(Default::default()));
        registry
            .insert_for_test("media", ConnectorConfig::Storage(config))
            .await;
        crate::engine::functions::run_test_task(
            NAME,
            Box::new(StoragePresignHandler { registry }),
            input,
            Value::Null,
        )
        .await
    }

    #[tokio::test]
    async fn presigns_a_virtual_hosted_get() {
        let out = run(
            json!({"connector": "media", "key": "video/topic 1/output.m3u8",
                   "expires_in": "7d", "output": "data.url"}),
            storage_config(false),
        )
        .await
        .expect("test");
        let url = out["url"].as_str().expect("test");
        assert!(
            url.starts_with(
                "https://media.ap-south-1.linodeobjects.com/video/topic%201/output.m3u8?"
            ),
            "{url}"
        );
        assert!(url.contains("X-Amz-Expires=604800"), "{url}");
        assert!(url.contains("X-Amz-Signature="), "{url}");
        assert!(url.contains("X-Amz-Credential=AKEXAMPLE%2F"), "{url}");
    }

    #[tokio::test]
    async fn path_style_addresses_through_the_endpoint_host() {
        let out = run(
            json!({"connector": "media", "key": "k.txt", "expires_in": 60,
                   "output": "data.url"}),
            storage_config(true),
        )
        .await
        .expect("test");
        let url = out["url"].as_str().expect("test");
        assert!(
            url.starts_with("https://ap-south-1.linodeobjects.com/media/k.txt?"),
            "{url}"
        );
    }

    #[tokio::test]
    async fn put_respects_its_gate_and_signs_the_content_type() {
        let mut config = storage_config(false);
        config.operations.presign_put = false;
        // The refusal is caller-sanitized ("Request validation failed" — the
        // gate name stays server-side), so the assertion is behavioral: the
        // gated call errs where the identical ungated call below succeeds.
        run(
            json!({"connector": "media", "method": "PUT", "key": "up.mp4",
                   "expires_in": 900}),
            config,
        )
        .await
        .expect_err("presign_put=false must refuse");

        let out = run(
            json!({"connector": "media", "method": "PUT", "key": "up.mp4",
                   "content_type": "video/mp4", "expires_in": 900,
                   "output": "data.url"}),
            storage_config(false),
        )
        .await
        .expect("test");
        let url = out["url"].as_str().expect("test");
        assert!(
            url.contains("X-Amz-SignedHeaders=content-type%3Bhost"),
            "{url}"
        );
    }

    #[tokio::test]
    async fn argument_mistakes_are_named() {
        for (input, expected) in [
            (
                json!({"connector": "media", "key": "k", "expires_in": "8d"}),
                "7 days",
            ),
            (
                json!({"connector": "media", "key": "k", "expires_in": "soon"}),
                "not a duration",
            ),
            (
                json!({"connector": "media", "key": "k", "expires_in": 0}),
                "between 1 second",
            ),
            (
                json!({"connector": "media", "method": "POST", "key": "k", "expires_in": 60}),
                "GET (default) or PUT",
            ),
            (
                json!({"connector": "media", "key": "k", "expires_in": 60,
                       "content_type": "video/mp4"}),
                "applies to PUT only",
            ),
            (
                json!({"connector": "media", "method": "PUT", "key": "k", "expires_in": 60,
                       "response_content_disposition": "attachment"}),
                "apply to GET only",
            ),
            (json!({"connector": "media", "key": "k"}), "expires_in"),
        ] {
            let err = run(input.clone(), storage_config(false))
                .await
                .expect_err("test");
            assert!(err.contains(expected), "{input}: {err}");
        }
    }

    #[test]
    fn static_validation_reads_the_same_rules() {
        let obj = json!({"connector": "m", "key": "k", "expires_in": "30d"});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(
            errs.iter()
                .any(|(f, c, _)| *f == "expires_in" && *c == "INVALID"),
            "{errs:?}"
        );

        let obj = json!({"connector": "m", "method": "DELETE", "key": "k", "expires_in": 60});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(
            errs.iter()
                .any(|(f, c, _)| *f == "method" && *c == "INVALID"),
            "{errs:?}"
        );

        // A {"var"} expires_in is a request-time concern.
        let obj = json!({"connector": "m", "key": "k", "expires_in": {"var": "data.ttl"}});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(errs.is_empty(), "{errs:?}");
    }
}

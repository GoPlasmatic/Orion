//! `storage_presign` — a time-limited URL for one object, computed locally
//! (#265).
//!
//! Zero bytes move: SigV4 presigning is arithmetic over the connector's
//! credentials, and the client then talks to the object store directly. That
//! invariant — pure computation, no data path — is the storage connector's
//! scoping rule.

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::Output;
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::program::{AuthoredPath, IssueCode, Severity};
use dataflow_rs::{Ctx, Expr, Function, Issue, Outcome};
use dataflow_rs_v3::engine::error::DataflowError;
use dataflow_rs_v3::engine::task_context::TaskContext;
use serde_json::Value;

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, parse_duration_secs, require_op, resolve_duration_secs, resolve_optional_str,
    resolve_optional_u64, resolve_required_str,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::FieldSource;
use super::templated_input::{Scope, TemplatedInput};
use crate::connector::{ConnectorRegistry, StorageConnectorConfig, sigv4};
use crate::engine::kit::{self, read};
use crate::engine::{ErrorClass, HandlerError};

/// S3's own ceiling on presigned-URL lifetime: 7 days, in seconds.
const MAX_EXPIRES_SECS: u64 = 604_800;

/// This handler's name, for the free functions below — a reference to the one
/// place it is written (F48), not a second spelling of it.
const NAME: &str = <StoragePresignHandler as ConnectorHandler>::NAME;

/// Workflow function handler that presigns object-storage URLs.
pub struct StoragePresignHandler {
    pub registry: Arc<ConnectorRegistry>,
}

/// The presign request, read from the task and folded against the message.
///
/// The method is parsed here rather than in the body because it decides *which*
/// gate applies, and a gate chosen after the URL is signed is not a gate.
pub struct Presign {
    method: PresignMethod,
    key: String,
    expires_secs: u64,
    response_content_type: Option<String>,
    response_content_disposition: Option<String>,
    content_type: Option<String>,
    content_length: Option<u64>,
}

#[async_trait]
impl ConnectorHandler for StoragePresignHandler {
    const NAME: &'static str = "storage_presign";
    type Kind = crate::connector::kind::Storage;
    type Input = TemplatedInput;
    type Parsed = Presign;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &Scope<'_, '_>,
    ) -> Result<Self::Parsed, HandlerError> {
        // Both read literal fields, and both are shared with the
        // authoring-time validator, which has only the authored JSON.
        let raw = input.raw();
        let method = presign_method(raw.get("method").and_then(Value::as_str)).map_err(named)?;
        check_method_fields(method, |field| raw.get(field).is_some_and(|v| !v.is_null()))
            .map_err(named)?;
        Ok(Presign {
            method,
            key: resolve_required_str(input, "key", call.name, ctx)?,
            expires_secs: expires_in(input, ctx)?,
            response_content_type: resolve_optional_str(
                input,
                "response_content_type",
                call.name,
                ctx,
            )?,
            response_content_disposition: resolve_optional_str(
                input,
                "response_content_disposition",
                call.name,
                ctx,
            )?,
            content_type: resolve_optional_str(input, "content_type", call.name, ctx)?,
            content_length: content_length(input, ctx)?,
        })
    }

    fn gate(
        presign: &Self::Parsed,
        conn: &StorageConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        Ok(require_op(
            presign.method.allowed(conn),
            presign.method.gate_name(),
            connector,
        )?)
    }

    async fn run(
        &self,
        presign: Self::Parsed,
        storage: &StorageConnectorConfig,
        _call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        Ok(Value::String(presign_url(&presign, storage)?).into())
    }
}

/// The URL, signed against a resolved and gated storage connector: pure
/// computation over its credentials, no data path. Shared by the
/// dataflow-rs 3 handler and the dataflow-rs 4 function.
fn presign_url(
    presign: &Presign,
    storage: &StorageConnectorConfig,
) -> Result<String, HandlerError> {
    let (scheme, host, path) = storage
        .address(Some(&presign.key))
        .map_err(|e| validation(&e))?;

    // Response overrides ride the signed query; a PUT's content-type and
    // content-length are signed headers the client must then send
    // verbatim.
    let mut extra_query: Vec<(String, String)> = Vec::new();
    if let Some(v) = &presign.response_content_type {
        extra_query.push(("response-content-type".to_string(), v.clone()));
    }
    if let Some(v) = &presign.response_content_disposition {
        extra_query.push(("response-content-disposition".to_string(), v.clone()));
    }
    let mut extra_headers: Vec<(String, String)> = Vec::new();
    if let Some(v) = &presign.content_type {
        extra_headers.push(("content-type".to_string(), v.clone()));
    }
    if let Some(n) = presign.content_length {
        extra_headers.push(("content-length".to_string(), n.to_string()));
    }

    let amz_date = sigv4::amz_date_now();
    let sig_ctx = sigv4::SigningContext::for_storage(storage, &host, &path, &amz_date);
    Ok(sigv4::presign_url(
        &sig_ctx,
        &scheme,
        presign.method.as_str(),
        presign.expires_secs,
        &extra_query,
        &extra_headers,
    ))
}

/// `storage_presign`'s input on dataflow-rs 4, each field read as today
/// (I183): `method` the literal `presign_method` reads, any JSON, so a
/// non-string or `null` is GET as it was, and the rest expressions
/// read by `kit::read`'s twins of `resolve_required_str`,
/// `resolve_duration_secs`, `resolve_optional_str` and `resolve_optional_u64`.
/// A task missing `connector`, `key` or `expires_in` is refused at load
/// (I183); the method's table and its per-method fields stay the call's
/// rules, with their messages.
#[derive(Debug, FunctionConfig)]
pub struct StoragePresignConfig {
    /// The storage connector whose credentials sign the URL.
    connector: String,
    /// `GET` (the default) or `PUT`; anything but a string is GET.
    method: Option<Value>,
    /// The object key.
    key: Expr<OwnedDataValue>,
    /// The URL's lifetime: seconds, or `"<n>s|m|h|d"`.
    expires_in: Expr<OwnedDataValue>,
    /// GET only: the Content-Type the store answers with.
    response_content_type: Option<Expr<OwnedDataValue>>,
    /// GET only: the Content-Disposition the store answers with.
    response_content_disposition: Option<Expr<OwnedDataValue>>,
    /// PUT only: the Content-Type the uploader must send.
    content_type: Option<Expr<OwnedDataValue>>,
    /// PUT only: the exact number of bytes the uploader must send.
    content_length: Option<Expr<OwnedDataValue>>,
    /// Where the URL is written.
    #[output(default = "data")]
    output: Output,
}

impl StoragePresignConfig {
    /// Whether the task sets `field` to anything but `null`, as
    /// `check_method_fields` asks of the authored input.
    fn sets(&self, field: &str) -> bool {
        let value = match field {
            "response_content_type" => &self.response_content_type,
            "response_content_disposition" => &self.response_content_disposition,
            "content_type" => &self.content_type,
            "content_length" => &self.content_length,
            _ => return false,
        };
        read::present(value).is_some()
    }
}

/// I185's Defect: the URL is a string, which no context root takes (break
/// 21), so an `output` that is one fails every call.
const ROOT_OUTPUT: &str = "`storage_presign` writes a URL string, and a context root takes \
    only an object (break 21): set `output` to a path below it, such as `data.url`";

/// An optional string field for this call, by `resolve_optional_str`'s
/// reading.
fn optional_str(
    ctx: &Ctx<'_>,
    field: &Option<Expr<OwnedDataValue>>,
    name: &str,
) -> Result<Option<String>, HandlerError> {
    let value = field.as_ref().map(|expr| ctx.eval(expr)).transpose()?;
    Ok(read::optional_str(value.as_deref(), name, NAME)?.map(Cow::into_owned))
}

/// The presign the task describes, read as `StoragePresignHandler::parse`
/// reads it, in its order.
fn presign(ctx: &Ctx<'_>, cfg: &StoragePresignConfig) -> Result<Presign, HandlerError> {
    let method = presign_method(cfg.method.as_ref().and_then(Value::as_str)).map_err(named)?;
    check_method_fields(method, |field| cfg.sets(field)).map_err(named)?;
    let key = ctx.eval(&cfg.key)?;
    let key = read::required_str(Some(&key), "key", NAME)?.into_owned();
    let expires = ctx.eval(&cfg.expires_in)?;
    let expires_secs = expires_secs(read::duration_secs(Some(&expires), "expires_in", NAME)?)?;
    let response_content_type =
        optional_str(ctx, &cfg.response_content_type, "response_content_type")?;
    let response_content_disposition = optional_str(
        ctx,
        &cfg.response_content_disposition,
        "response_content_disposition",
    )?;
    let content_type = optional_str(ctx, &cfg.content_type, "content_type")?;
    let length = cfg
        .content_length
        .as_ref()
        .map(|expr| ctx.eval(expr))
        .transpose()?;
    let content_length = exact_length(read::optional_u64(
        length.as_deref(),
        "content_length",
        NAME,
    )?)?;
    Ok(Presign {
        method,
        key,
        expires_secs,
        response_content_type,
        response_content_disposition,
        content_type,
        content_length,
    })
}

/// `storage_presign` as a dataflow-rs 4 function: the URL
/// `StoragePresignHandler` signs, read in the same order.
pub struct StoragePresignFunction {
    pub registry: Arc<ConnectorRegistry>,
}

impl Function for StoragePresignFunction {
    type Config = StoragePresignConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    /// I185: an `output` that is a literal context root, the default `data`
    /// included, is a Defect at `output`.
    fn prepare(&self, cfg: &mut StoragePresignConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        match cfg.output.constant() {
            Some(path) if path.is_root() => {
                let mut at = AuthoredPath::root();
                at.push("output");
                Err(vec![
                    Issue::new(IssueCode::InputParse, at, ROOT_OUTPUT)
                        .with_severity(Severity::Defect),
                ])
            }
            _ => Ok(()),
        }
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &StoragePresignConfig,
    ) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, before the presign is read: an output that
        // names no path signs nothing.
        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
        let presign = presign(ctx, cfg)?;
        let mut url = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let storage =
                kit::require_connector::<crate::connector::kind::Storage>(&config, &cfg.connector)?;
            kit::require_op(
                presign.method.allowed(storage),
                presign.method.gate_name(),
                &cfg.connector,
            )?;
            url = Some(presign_url(&presign, storage)?);
            Ok(Outcome::Ok)
        })
        .await?;
        if let Some(url) = url {
            ctx.set(&output, OwnedDataValue::String(url))?;
        }
        Ok(Outcome::Ok)
    }
}

/// A caller-fixable problem with the task's input, message **bare**.
///
/// Shared by `execute` and `validate_static_input`. The static path turns it
/// into a `FieldError`, whose own path already says which field is wrong, so a
/// `"storage_presign: "` prefix there is noise; the execution path adds the
/// name through [`named`]. Formatting it on here and stripping it back off was
/// what `strip_handler_prefix` did, and it had silently stopped doing it.
fn validation(msg: &str) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, msg)
}

/// A bare parser error on its way out through `execute`, named once.
fn named(e: HandlerError) -> HandlerError {
    e.prefixed(NAME)
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

    /// Whether `storage` hands out this method's links. GET and PUT are
    /// gated separately: a connector that may hand out read links has said
    /// nothing about handing out write ones.
    fn allowed(self, storage: &StorageConnectorConfig) -> bool {
        match self {
            Self::Get => storage.operations.presign_get,
            Self::Put => storage.operations.presign_put,
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
fn presign_method(method: Option<&str>) -> Result<PresignMethod, HandlerError> {
    match method {
        None | Some("GET") => Ok(PresignMethod::Get),
        Some("PUT") => Ok(PresignMethod::Put),
        Some(other) => Err(validation(&format!(
            "method '{other}' is not supported — GET (default) or PUT"
        ))),
    }
}

/// The per-method field rules: response overrides are GET's, the upload
/// constraints (`content_type`, `content_length`) are PUT's. Naming a field on
/// the wrong method is a misunderstanding worth refusing, not ignoring.
/// `present` says whether the task sets a field to anything but `null`.
fn check_method_fields(
    method: PresignMethod,
    present: impl Fn(&str) -> bool,
) -> Result<(), HandlerError> {
    match method {
        PresignMethod::Get if present("content_type") => Err(validation(
            "'content_type' applies to PUT only — for GET use 'response_content_type'",
        )),
        // No GET counterpart to redirect to: a download's length is the
        // object's, and nothing about the request can constrain it.
        PresignMethod::Get if present("content_length") => {
            Err(validation("'content_length' applies to PUT only"))
        }
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
fn expires_in(
    input: &TemplatedInput,
    ctx: &(impl FieldSource + ?Sized),
) -> Result<u64, DataflowError> {
    Ok(expires_secs(resolve_duration_secs(
        input,
        ctx,
        NAME,
        "expires_in",
    )?)?)
}

/// `expires_in`'s rules over the seconds the field read as: it is set, and
/// within S3's 7-day cap.
fn expires_secs(secs: Option<u64>) -> Result<u64, HandlerError> {
    let Some(secs) = secs else {
        return Err(named(validation(
            "requires 'expires_in' (seconds, or \"<n>s|m|h|d\")",
        )));
    };
    if secs == 0 || secs > MAX_EXPIRES_SECS {
        return Err(named(validation(&format!(
            "'expires_in' must be between 1 second and {MAX_EXPIRES_SECS} (7 days — \
             S3's own presign ceiling), got {secs}"
        ))));
    }
    Ok(secs)
}

/// The exact upload size a PUT presign binds, signed as a `content-length`
/// header so the store refuses any other length — the size bound the
/// after-the-fact `storage_head` check cannot make, because by then the bytes
/// have landed (#367).
///
/// Zero is refused rather than signed: AWS's front end rewrites an incoming
/// `Content-Length: 0` to an empty value, so a URL signed for zero bytes can
/// never match its own signature. That is a URL that fails with
/// `SignatureDoesNotMatch` and no way to tell why, which is worse than
/// refusing it here.
fn content_length(
    input: &TemplatedInput,
    ctx: &(impl FieldSource + ?Sized),
) -> Result<Option<u64>, DataflowError> {
    Ok(exact_length(resolve_optional_u64(
        input,
        "content_length",
        NAME,
        ctx,
    )?)?)
}

/// `content_length`'s rule over the value the field read as: never zero.
fn exact_length(length: Option<u64>) -> Result<Option<u64>, HandlerError> {
    match length {
        Some(0) => Err(named(validation(ZERO_CONTENT_LENGTH))),
        other => Ok(other),
    }
}

/// The zero refusal, written once: the execution path and the authoring-time
/// validator must give the same answer, and this is the sentence they share.
const ZERO_CONTENT_LENGTH: &str = "'content_length' must be at least 1 — a signed \
    'Content-Length: 0' is rewritten by S3's front end to an empty value, so the \
    signature can never match";

// -- Authoring-time validation (shared with schema::validate_input) --

/// Cross-field checks over a *static* input: the method value table, the
/// per-method field rules, and literal `expires_in` bounds.
pub(super) fn validate_static_input(
    obj: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    let mut errors: Vec<(&'static str, &'static str, String)> = Vec::new();
    let input = Value::Object(obj.clone());

    let method = match presign_method(input.get("method").and_then(Value::as_str)) {
        Ok(m) => Some(m),
        Err(e) => {
            errors.push(("method", "INVALID", e.msg));
            None
        }
    };
    if let Some(method) = method
        && let Err(e) = check_method_fields(method, |field| {
            input.get(field).is_some_and(|v| !v.is_null())
        })
    {
        errors.push(("method", "INVALID", e.msg));
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

    // Only a literal is judged here: absent is fine, and a `{"var": ..}` node
    // is a request-time concern. A non-number literal is the field table's
    // TYPE_MISMATCH, not this validator's.
    if let Some(Value::Number(n)) = obj.get("content_length") {
        match n.as_u64() {
            Some(0) => {
                errors.push(("content_length", "INVALID", ZERO_CONTENT_LENGTH.to_string()));
            }
            Some(_) => {}
            None => errors.push((
                "content_length",
                "INVALID",
                "'content_length' must be a whole number of bytes, at least 1".to_string(),
            )),
        }
    }

    errors
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
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "expires_in",
        description: "URL lifetime: integer seconds or \"<n>s|m|h|d\"; at most 7 days \
                      (S3's own ceiling).",
        kind: FieldKind::Any,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "response_content_type",
        description: "GET only: forces the Content-Type the store answers with; signed, \
                      so the client cannot alter it.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "response_content_disposition",
        description: "GET only: forces Content-Disposition (e.g. a download filename); \
                      signed.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "content_type",
        description: "PUT only: the Content-Type the uploader must send — a signed \
                      header, so an upload with any other type is refused by the store.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "content_length",
        description: "PUT only: the exact number of bytes the uploader must send — a \
                      signed header, so an upload of any other size is refused by the \
                      store. At least 1; SigV4 can bind an exact length, not a range.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the presigned URL (string) is stored. Defaults \
                      to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
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
            Box::new(super::super::connector_handler::Connector(
                StoragePresignHandler { registry },
            )),
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

    /// The size bound is the *signature*, not a field the store reads: two
    /// lengths over otherwise identical inputs must sign differently, or the
    /// header is decoration and a client could send any number of bytes.
    ///
    /// The half this cannot assert is the store's, so it was verified by hand
    /// against a SigV4-verifying S3 implementation (SeaweedFS): a URL signed
    /// for 64 bytes stored a 64-byte body and answered
    /// `403 SignatureDoesNotMatch` to 65 bytes and to 4 KiB, while the same
    /// URL signed without a length took every size.
    #[tokio::test]
    async fn put_signs_an_exact_content_length() {
        let presign = |bytes: u64| async move {
            let out = run(
                json!({"connector": "media", "method": "PUT", "key": "up.mp4",
                       "content_type": "video/mp4", "content_length": bytes,
                       "expires_in": 900, "output": "data.url"}),
                storage_config(false),
            )
            .await
            .expect("test");
            out["url"].as_str().expect("test").to_string()
        };

        let url = presign(1_048_576).await;
        assert!(
            url.contains("X-Amz-SignedHeaders=content-length%3Bcontent-type%3Bhost"),
            "{url}"
        );
        let signature = |u: &str| {
            u.split("X-Amz-Signature=")
                .nth(1)
                .expect("test")
                .to_string()
        };
        assert_ne!(
            signature(&url),
            signature(&presign(1_048_577).await),
            "one byte's difference must change the signature"
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
            (
                json!({"connector": "media", "key": "k", "expires_in": 60,
                       "content_length": 1024}),
                "'content_length' applies to PUT only",
            ),
            (
                json!({"connector": "media", "method": "PUT", "key": "k",
                       "expires_in": 60, "content_length": 0}),
                "must be at least 1",
            ),
            (
                json!({"connector": "media", "method": "PUT", "key": "k",
                       "expires_in": 60, "content_length": -1}),
                "non-negative integer",
            ),
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

        // A literal zero length is refused at authoring time, with the same
        // sentence the execution path gives.
        let obj = json!({"connector": "m", "method": "PUT", "key": "k",
                         "expires_in": 60, "content_length": 0});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(
            errs.iter().any(|(f, c, m)| *f == "content_length"
                && *c == "INVALID"
                && m == ZERO_CONTENT_LENGTH),
            "{errs:?}"
        );

        let obj = json!({"connector": "m", "method": "PUT", "key": "k",
                         "expires_in": 60, "content_length": 1.5});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(
            errs.iter()
                .any(|(f, c, _)| *f == "content_length" && *c == "INVALID"),
            "{errs:?}"
        );

        // A {"var"} length, and a valid literal one, are both clean.
        let obj = json!({"connector": "m", "method": "PUT", "key": "k", "expires_in": 60,
                         "content_length": {"var": "temp_data.declared_bytes"}});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(errs.is_empty(), "{errs:?}");
        let obj = json!({"connector": "m", "method": "PUT", "key": "k", "expires_in": 60,
                         "content_length": 1});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(errs.is_empty(), "{errs:?}");
    }

    // -- dataflow-rs 4: the config, I185's Defect, and a twin of each test
    // above that runs the handler, held to it over the same input --

    use crate::engine::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;

    fn config_v4(input: Value) -> Result<StoragePresignConfig, Vec<dataflow_rs::Issue>> {
        testing::config_with(&kit::test_exprs(), input)
    }

    /// Declaration order is the field table's (I179).
    #[test]
    fn the_config_declares_the_field_table_in_its_order() {
        let fields: Vec<&str> = StoragePresignConfig::fields()
            .iter()
            .map(|f| &*f.name)
            .collect();
        let table: Vec<&str> = STORAGE_PRESIGN_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);
    }

    #[test]
    fn every_field_kind_compiles_and_the_required_ones_are_required() {
        let cfg = config_v4(json!({
            "connector": "media", "method": "PUT", "key": {"var": "data.key"},
            "expires_in": "15m", "content_type": "video/mp4",
            "content_length": {"var": "data.bytes"}, "output": "data.url"
        }))
        .expect("compiles");
        assert_eq!(cfg.method.as_ref().and_then(Value::as_str), Some("PUT"));
        assert!(cfg.key.constant().is_none() && cfg.expires_in.constant().is_some());
        assert!(cfg.sets("content_type") && cfg.sets("content_length"));
        assert!(!cfg.sets("response_content_type"));
        let output = cfg.output.constant().map(ToString::to_string);
        assert_eq!(output.as_deref(), Some("data.url"));
        // An authored `null` is not set, as the authored-JSON rule read it.
        let cfg = config_v4(json!({"connector": "m", "key": "k", "expires_in": 60,
                                   "content_type": null}))
        .expect("compiles");
        assert!(!cfg.sets("content_type") && cfg.method.is_none());

        let rejected = |input: Value| -> Vec<(String, IssueCode)> {
            config_v4(input)
                .expect_err("refused")
                .into_iter()
                .filter(|i| i.severity == Severity::Rejected)
                .map(|i| (i.path.to_string(), i.code))
                .collect()
        };
        let full = json!({"connector": "m", "key": "k", "expires_in": 60});
        for field in ["connector", "key", "expires_in"] {
            let mut input = full.clone();
            input.as_object_mut().expect("object").remove(field);
            assert_eq!(
                rejected(input),
                [(field.to_string(), IssueCode::InputParse)]
            );
        }
        let mut computed = full.clone();
        computed["connector"] = json!({"var": "data.store"});
        assert_eq!(
            rejected(computed),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        let mut unknown = full;
        unknown["bucket"] = json!("b");
        let issues = rejected(unknown);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].0, "bucket");
    }

    async fn media(config: StorageConnectorConfig) -> Arc<ConnectorRegistry> {
        let registry =
            std::sync::Arc::new(crate::connector::ConnectorRegistry::new(Default::default()));
        registry
            .insert_for_test("media", ConnectorConfig::Storage(config))
            .await;
        registry
    }

    /// I185: the URL is never an object, so a literal root `output` — the
    /// default `data` included — is a Defect at `output`, reported by
    /// `prepare`, which only a compile through the registry runs; a path
    /// below a root is not. The call still runs, and a root's write is
    /// refused (break 21), where 1.12 replaced the root with the string.
    #[tokio::test]
    async fn a_root_output_is_i185s_defect() {
        for (output, defect) in [
            (None, true),
            (Some(json!("data")), true),
            (Some(json!("temp_data")), true),
            (Some(json!(null)), true),
            (Some(json!("temp_data.url")), false),
            (Some(json!("data.url")), false),
            (Some(json!({"cat": ["data", ".url"]})), false),
        ] {
            let mut input = json!({"connector": "media", "key": "k.txt", "expires_in": 60});
            if let Some(output) = &output {
                input["output"] = output.clone();
            }
            let called = testing::call(
                StoragePresignFunction {
                    registry: media(storage_config(false)).await,
                },
                input,
            )
            .exprs(kit::test_exprs())
            .name(NAME)
            .await;
            let defects: Vec<_> = called
                .issues
                .iter()
                .filter(|i| i.severity == Severity::Defect)
                .map(|i| (i.path.to_string(), i.code, i.message.as_str()))
                .collect();
            if defect {
                assert_eq!(
                    defects,
                    [("output".to_string(), IssueCode::InputParse, ROOT_OUTPUT)],
                    "{output:?}"
                );
                let error = called.outcome.expect_err("a string at a root is refused");
                assert_eq!(error.code(), "CONFIG_ERROR", "{output:?}");
            } else {
                assert!(defects.is_empty(), "{output:?}: {defects:?}");
                assert_eq!(called.outcome.expect("signed"), Outcome::Ok, "{output:?}");
            }
        }
    }

    /// `seen` with the URL's clock-dependent parts, its timestamp and the
    /// signature over it, spelled out of the comparison.
    fn unclocked(seen: &Seen) -> Seen {
        fn strip(v: &mut Value) {
            match v {
                Value::String(url) if url.contains("X-Amz-Signature=") => {
                    let kept: Vec<&str> = url
                        .split('&')
                        .filter(|p| {
                            !p.starts_with("X-Amz-Date=") && !p.starts_with("X-Amz-Signature=")
                        })
                        .map(|p| p.split("X-Amz-Credential=").next().unwrap_or(p))
                        .collect();
                    *url = kept.join("&");
                }
                Value::Object(map) => map.values_mut().for_each(strip),
                _ => {}
            }
        }
        seen.clone().map(|mut data| {
            strip(&mut data);
            data
        })
    }

    /// One task through each engine against the same connector config.
    async fn both(input: Value, config: StorageConnectorConfig) -> (Seen, Seen) {
        let v3 = twin::v3(
            NAME,
            Box::new(super::super::connector_handler::Connector(
                StoragePresignHandler {
                    registry: media(config.clone()).await,
                },
            )),
            input.clone(),
            Value::Null,
        )
        .await;
        let v4 = twin::v4(
            NAME,
            StoragePresignFunction {
                registry: media(config).await,
            },
            input,
            Value::Null,
        )
        .await;
        (v3, v4)
    }

    #[tokio::test]
    async fn presigns_a_virtual_hosted_get_on_v4() {
        let input = json!({"connector": "media", "key": "video/topic 1/output.m3u8",
                           "expires_in": "7d", "output": "data.url"});
        let (v3, v4) = both(input, storage_config(false)).await;
        assert_eq!(unclocked(&v4), unclocked(&v3));
        let out = v4.expect("test");
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
    async fn path_style_addresses_through_the_endpoint_host_on_v4() {
        let input = json!({"connector": "media", "key": "k.txt", "expires_in": 60,
                           "output": "data.url"});
        let (v3, v4) = both(input, storage_config(true)).await;
        assert_eq!(unclocked(&v4), unclocked(&v3));
        let out = v4.expect("test");
        let url = out["url"].as_str().expect("test");
        assert!(
            url.starts_with("https://ap-south-1.linodeobjects.com/media/k.txt?"),
            "{url}"
        );
    }

    #[tokio::test]
    async fn put_respects_its_gate_and_signs_the_content_type_on_v4() {
        let mut config = storage_config(false);
        config.operations.presign_put = false;
        let (v3, v4) = both(
            json!({"connector": "media", "method": "PUT", "key": "up.mp4",
                   "expires_in": 900}),
            config,
        )
        .await;
        assert_eq!(v4, v3);
        v4.expect_err("presign_put=false must refuse");

        let (v3, v4) = both(
            json!({"connector": "media", "method": "PUT", "key": "up.mp4",
                   "content_type": "video/mp4", "expires_in": 900,
                   "output": "data.url"}),
            storage_config(false),
        )
        .await;
        assert_eq!(unclocked(&v4), unclocked(&v3));
        let out = v4.expect("test");
        let url = out["url"].as_str().expect("test");
        assert!(
            url.contains("X-Amz-SignedHeaders=content-type%3Bhost"),
            "{url}"
        );
    }

    #[tokio::test]
    async fn put_signs_an_exact_content_length_on_v4() {
        let presign = |bytes: u64| async move {
            let (v3, v4) = both(
                json!({"connector": "media", "method": "PUT", "key": "up.mp4",
                       "content_type": "video/mp4", "content_length": bytes,
                       "expires_in": 900, "output": "data.url"}),
                storage_config(false),
            )
            .await;
            assert_eq!(unclocked(&v4), unclocked(&v3));
            v4.expect("test")["url"].as_str().expect("test").to_string()
        };

        let url = presign(1_048_576).await;
        assert!(
            url.contains("X-Amz-SignedHeaders=content-length%3Bcontent-type%3Bhost"),
            "{url}"
        );
        let signature = |u: &str| {
            u.split("X-Amz-Signature=")
                .nth(1)
                .expect("test")
                .to_string()
        };
        assert_ne!(
            signature(&url),
            signature(&presign(1_048_577).await),
            "one byte's difference must change the signature"
        );
    }

    #[tokio::test]
    async fn argument_mistakes_are_named_on_v4() {
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
            // 1.12 refused a task with no `expires_in` on each call; it is
            // refused at load now (I183), and the call's refusal is the one
            // a `null` gets.
            (
                json!({"connector": "media", "key": "k", "expires_in": null}),
                "expires_in",
            ),
            (
                json!({"connector": "media", "key": "k", "expires_in": 60,
                       "content_length": 1024}),
                "'content_length' applies to PUT only",
            ),
            (
                json!({"connector": "media", "method": "PUT", "key": "k",
                       "expires_in": 60, "content_length": 0}),
                "must be at least 1",
            ),
            (
                json!({"connector": "media", "method": "PUT", "key": "k",
                       "expires_in": 60, "content_length": -1}),
                "non-negative integer",
            ),
            // The refusals the cases above do not reach.
            (
                json!({"connector": "media", "key": {"var": "data.none"}, "expires_in": 60}),
                "'key'",
            ),
            (
                json!({"connector": "media", "key": "k", "expires_in": 60,
                       "response_content_type": 7}),
                "must resolve to a string",
            ),
            (
                json!({"connector": "absent", "key": "k", "expires_in": 60}),
                "Connector 'absent' not found",
            ),
        ] {
            let (v3, v4) = both(input.clone(), storage_config(false)).await;
            assert_eq!(v4, v3, "{input}");
            let failure = v4.expect_err("test");
            assert!(failure.message.contains(expected), "{input}: {failure:?}");
        }
    }

    /// A `method` that is not a string is GET, as 1.12 read it: the config
    /// loads, and the call answers to the `presign_get` gate, not
    /// `presign_put`, on both engines.
    #[tokio::test]
    async fn a_method_that_is_not_a_string_is_get() {
        for method in [
            json!(5),
            json!(true),
            json!({"a": 1}),
            json!(["PUT"]),
            json!(null),
        ] {
            let input = json!({"connector": "media", "method": method, "key": "k.txt",
                               "expires_in": 60, "output": "data.url"});
            config_v4(input.clone()).expect("not a load refusal");

            let mut no_put = storage_config(false);
            no_put.operations.presign_put = false;
            let (v3, v4) = both(input.clone(), no_put).await;
            assert_eq!(unclocked(&v4), unclocked(&v3), "{method}");
            assert!(
                v4.is_ok(),
                "{method}: GET does not answer to presign_put: {v4:?}"
            );

            let mut no_get = storage_config(false);
            no_get.operations.presign_get = false;
            let (v3, v4) = both(input, no_get).await;
            assert_eq!(v4, v3, "{method}");
            assert!(v4.is_err(), "{method}: GET answers to presign_get");
        }
    }

    /// A computed `output` is resolved before anything is read or signed, as
    /// 1.12's prologue resolved it: one that names no path is 1.12's
    /// `VALIDATION_ERROR` with its text, ahead of the task's own refusals;
    /// one that names a path is written there.
    #[tokio::test]
    async fn a_computed_output_is_resolved_first_as_1_12_did() {
        for (input, written) in [
            (
                json!({"connector": "media", "key": "k", "expires_in": 60,
                    "output": {"var": "data.n"}}),
                false,
            ),
            (
                json!({"connector": "media", "key": "k", "expires_in": 60,
                    "output": {"var": "data.empty"}}),
                false,
            ),
            // Ahead of the method's own refusal, as in 1.12.
            (
                json!({"connector": "media", "method": "POST", "key": "k", "expires_in": 60,
                    "output": {"var": "data.n"}}),
                false,
            ),
            (
                json!({"connector": "media", "key": "k", "expires_in": 60,
                    "output": {"var": "data.at"}}),
                true,
            ),
        ] {
            let data = json!({"n": 3, "empty": "", "at": "data.url"});
            let v3 = twin::v3(
                NAME,
                Box::new(super::super::connector_handler::Connector(
                    StoragePresignHandler {
                        registry: media(storage_config(false)).await,
                    },
                )),
                input.clone(),
                data.clone(),
            )
            .await;
            let f = StoragePresignFunction {
                registry: media(storage_config(false)).await,
            };
            let v4 = twin::v4(NAME, f, input.clone(), data).await;
            assert_eq!(unclocked(&v4), unclocked(&v3), "{input}");
            if written {
                assert!(v4.expect("signed")["url"].is_string());
            } else {
                let failure = v4.expect_err("refused");
                assert_eq!(failure.code, "VALIDATION_ERROR", "{input}");
                assert!(
                    failure.message.contains("'output' must resolve"),
                    "{failure:?}"
                );
            }
        }
    }
}

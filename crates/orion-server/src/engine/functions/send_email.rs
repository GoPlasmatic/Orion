//! `send_email` — transactional mail through an SMTP connector (#262).
//!
//! Transport and credentials live on the connector; message logic lives here.
//! The one deliberate departure from the other connector handlers: **no
//! automatic retries**. A timeout after `DATA` is indistinguishable from an
//! accepted message, and SMTP has no idempotency key — a retry *is* a
//! duplicate email (the same F8 reasoning that gates non-idempotent HTTP,
//! with no escape hatch). The circuit breaker still applies through the
//! shared `ConnectorCall` shell.
//!
//! Privacy: recipient addresses and bodies are workflow data like any other —
//! trace capture is the channel's policy — but this handler never logs
//! message content or addresses, and error messages carry only the server's
//! reply.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use lettre::message::header::{HeaderName, HeaderValue};
use lettre::message::{Mailbox, MultiPart, SinglePart};
use lettre::{AsyncTransport, Message};
use serde_json::{Value, json};

use super::connector_helpers::{
    ConnectorCall, apply_output, require_smtp_connector, resolve_value, to_connect_error,
};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::smtp_pool::SmtpPoolCache;
use crate::connector::{ConnectorRegistry, SmtpConnectorConfig};

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "send_email";

/// Header names owned by the structured fields — a task `headers` entry naming
/// one is refused (at create and here), so the two surfaces cannot fight over
/// one header. Matched case-insensitively.
pub(crate) const PROTECTED_HEADERS: &[&str] = &[
    "from",
    "to",
    "cc",
    "bcc",
    "subject",
    "date",
    "message-id",
    "content-type",
    "content-transfer-encoding",
    "mime-version",
    "reply-to",
];

/// Workflow function handler for sending mail through an SMTP connector.
pub struct SendEmailHandler {
    pub registry: Arc<ConnectorRegistry>,
    pub smtp_pool: Arc<SmtpPoolCache>,
}

#[async_trait]
impl AsyncFunctionHandler for SendEmailHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // F48/F58: the literal prologue first — connector presence, then the
        // literal-field checks no property of the message can change.
        let call = ConnectorCall::begin(NAME, input, ctx)?;
        check_headers_field(input)?;

        // Resolve the message-dependent fields before the body takes the
        // breaker shell (and before `ctx` is reborrowed).
        let to = address_list(input, "to", ctx)?
            .ok_or_else(|| validation("requires 'to' (an address or an array of addresses)"))?;
        let cc = address_list(input, "cc", ctx)?.unwrap_or_default();
        let bcc = address_list(input, "bcc", ctx)?.unwrap_or_default();
        let subject = resolved_string(input, "subject", ctx)?
            .ok_or_else(|| validation("requires 'subject'"))?;
        let text = resolved_string(input, "text", ctx)?;
        let html = resolved_string(input, "html", ctx)?;
        if text.is_none() && html.is_none() {
            return Err(validation("requires at least one of 'text' or 'html'"));
        }
        let from_override = resolved_string(input, "from", ctx)?;
        let reply_to = resolved_string(input, "reply_to", ctx)?
            .map(|s| parse_mailbox("reply_to", &s))
            .transpose()?;

        call.run(&self.registry, async {
            let connector_config = call.resolve(&self.registry, None).await?;
            let smtp_config = require_smtp_connector(&connector_config, call.connector)?;

            let from = sender(smtp_config, from_override.as_deref(), call.connector)?;
            let message_id = generated_message_id(&from);
            let message = build_message(
                &from,
                &message_id,
                &to,
                &cc,
                &bcc,
                &subject,
                reply_to.clone(),
                input.get("headers"),
                text.clone(),
                html.clone(),
            )?;

            let transport = self
                .smtp_pool
                .get_transport(call.connector, smtp_config)
                .await
                .map_err(to_connect_error)?;

            // One attempt, no retry loop — see the module comment. The
            // breaker (via `call.run`) still counts this failure.
            let response = transport.send(message).await.map_err(|e| {
                DataflowError::function_execution(
                    format!("SMTP send via '{}' failed: {e}", call.connector),
                    None,
                )
            })?;

            let result = json!({
                "message_id": message_id,
                "response": response.message().collect::<Vec<&str>>().join(" "),
            });
            apply_output(ctx, call.output, result);
            Ok(TaskOutcome::Success)
        })
        .await
    }
}

fn validation(msg: &str) -> DataflowError {
    DataflowError::Validation(format!("{NAME}: {msg}"))
}

/// A resolvable string field: absent/null → `None`; non-string after
/// resolution is an error naming the field.
fn resolved_string(
    input: &Value,
    field: &str,
    ctx: &TaskContext<'_>,
) -> Result<Option<String>, DataflowError> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(raw) => match resolve_value(raw, ctx) {
            Value::String(s) => Ok(Some(s)),
            Value::Null => Ok(None),
            _ => Err(validation(&format!("'{field}' must resolve to a string"))),
        },
    }
}

/// An address field: one address or an array, each `addr@x` or
/// `Name <addr@x>`. `None` when the field is absent.
fn address_list(
    input: &Value,
    field: &str,
    ctx: &TaskContext<'_>,
) -> Result<Option<Vec<Mailbox>>, DataflowError> {
    let raw = match input.get(field) {
        None | Some(Value::Null) => return Ok(None),
        Some(raw) => resolve_value(raw, ctx),
    };
    let parsed = match raw {
        Value::String(s) => vec![parse_mailbox(field, &s)?],
        Value::Array(items) => {
            if items.is_empty() {
                return Ok(None);
            }
            items
                .iter()
                .enumerate()
                .map(|(i, item)| match item {
                    Value::String(s) => parse_mailbox(&format!("{field}[{i}]"), s),
                    _ => Err(validation(&format!("'{field}[{i}]' must be a string"))),
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        Value::Null => return Ok(None),
        _ => {
            return Err(validation(&format!(
                "'{field}' must resolve to an address or an array of addresses"
            )));
        }
    };
    Ok(Some(parsed))
}

/// Parse `addr@x` / `Name <addr@x>`, naming the field (and index) on failure.
pub(crate) fn parse_mailbox(field: &str, s: &str) -> Result<Mailbox, DataflowError> {
    Mailbox::from_str(s.trim())
        .map_err(|e| validation(&format!("'{field}' is not a valid email address: {e}")))
}

/// The effective sender: the connector default, or the task's `from` when the
/// connector opts in.
fn sender(
    config: &SmtpConnectorConfig,
    from_override: Option<&str>,
    connector: &str,
) -> Result<Mailbox, DataflowError> {
    match from_override {
        Some(from) => {
            if !config.allow_from_override {
                return Err(validation(&format!(
                    "connector '{connector}' does not allow a per-send 'from' \
                     (set allow_from_override on the connector)"
                )));
            }
            parse_mailbox("from", from)
        }
        None => parse_mailbox("connector 'from'", &config.from),
    }
}

/// A fresh RFC 5322 Message-ID in the sender's domain — generated here rather
/// than left to the library so the workflow gets it back for correlation.
fn generated_message_id(from: &Mailbox) -> String {
    format!("<{}@{}>", uuid::Uuid::new_v4(), from.email.domain())
}

/// Literal check on the `headers` field: an object of string values whose
/// names are not owned by the structured fields. Shared with authoring-time
/// validation via [`validate_static_input`].
fn check_headers_field(input: &Value) -> Result<(), DataflowError> {
    match input.get("headers") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(map)) => {
            for (name, value) in map {
                if PROTECTED_HEADERS
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(name))
                {
                    return Err(validation(&format!(
                        "'headers' may not set '{name}' — use the structured field instead"
                    )));
                }
                if !value.is_string() {
                    return Err(validation(&format!("'headers.{name}' must be a string")));
                }
            }
            Ok(())
        }
        Some(_) => Err(validation("'headers' must be an object of string values")),
    }
}

/// Assemble the RFC 5322 message.
#[allow(clippy::too_many_arguments)]
fn build_message(
    from: &Mailbox,
    message_id: &str,
    to: &[Mailbox],
    cc: &[Mailbox],
    bcc: &[Mailbox],
    subject: &str,
    reply_to: Option<Mailbox>,
    headers: Option<&Value>,
    text: Option<String>,
    html: Option<String>,
) -> Result<Message, DataflowError> {
    let mut builder = Message::builder()
        .from(from.clone())
        .subject(subject)
        .message_id(Some(message_id.to_string()));
    for mbox in to {
        builder = builder.to(mbox.clone());
    }
    for mbox in cc {
        builder = builder.cc(mbox.clone());
    }
    for mbox in bcc {
        builder = builder.bcc(mbox.clone());
    }
    if let Some(mbox) = reply_to {
        builder = builder.reply_to(mbox);
    }

    let build_error = |e: lettre::error::Error| {
        DataflowError::Validation(format!("{NAME}: message could not be assembled: {e}"))
    };
    let mut message = match (text, html) {
        (Some(text), Some(html)) => builder
            .multipart(MultiPart::alternative_plain_html(text, html))
            .map_err(build_error)?,
        (Some(text), None) => builder
            .singlepart(SinglePart::plain(text))
            .map_err(build_error)?,
        (None, Some(html)) => builder
            .singlepart(SinglePart::html(html))
            .map_err(build_error)?,
        (None, None) => unreachable!("checked in the prologue"),
    };

    // Extra headers, already shape-checked by `check_headers_field`.
    if let Some(Value::Object(map)) = headers {
        for (name, value) in map {
            let header_name = HeaderName::new_from_ascii(name.clone()).map_err(|e| {
                validation(&format!("'headers.{name}' is not a valid header name: {e}"))
            })?;
            let raw = value.as_str().unwrap_or_default().to_string();
            message
                .headers_mut()
                .insert_raw(HeaderValue::new(header_name, raw));
        }
    }
    Ok(message)
}

// -- Authoring-time validation (shared with schema::validate_input) --

/// Cross-field checks over a *static* `send_email` input. Returns
/// `(path-suffix, code, message)` triples like the crypto function's.
pub(super) fn validate_static_input(
    obj: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    let mut errors: Vec<(&'static str, &'static str, String)> = Vec::new();
    let input = Value::Object(obj.clone());

    // At least one body, when both are statically absent. A `{"var": ..}`
    // value counts as present — it only exists per message.
    if obj.get("text").is_none_or(Value::is_null) && obj.get("html").is_none_or(Value::is_null) {
        errors.push((
            "",
            "REQUIRED",
            "send_email requires at least one of 'text' or 'html'".to_string(),
        ));
    }

    // The headers shape and the protected-name rule are fully literal.
    if let Err(e) = check_headers_field(&input) {
        errors.push(("headers", "INVALID", strip_name(&e)));
    }

    // Static addresses parse now instead of on the first send.
    for field in ["to", "cc", "bcc", "from", "reply_to"] {
        let Some(raw) = obj.get(field) else { continue };
        let addresses: Vec<&str> = match raw {
            Value::String(s) => vec![s.as_str()],
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            // A `{"var": ..}` node or another shape — checked at send time.
            _ => continue,
        };
        for s in addresses {
            if let Err(e) = parse_mailbox(field, s) {
                errors.push((field_name(field), "INVALID", strip_name(&e)));
            }
        }
    }

    errors
}

/// Map a field key to its `&'static str` from the schema table.
fn field_name(key: &str) -> &'static str {
    SEND_EMAIL_FIELDS
        .iter()
        .map(|f| f.name)
        .find(|n| *n == key)
        .unwrap_or("to")
}

/// Drop the `"send_email: "` prefix [`validation`] adds — as a `FieldError`
/// message the field path already carries the context.
fn strip_name(e: &DataflowError) -> String {
    let s = e.to_string();
    match s.split_once(&format!("{NAME}: ")) {
        Some((_, msg)) => msg.to_string(),
        None => s,
    }
}

// -- Input schema (F53) --

pub(super) const SEND_EMAIL_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the SMTP connector to send through.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "to",
        description: "Recipient address or array of addresses; each is 'addr@example.com' \
                      or 'Name <addr@example.com>'.",
        kind: FieldKind::Any,
        required: true,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "cc",
        description: "Carbon-copy recipients; same forms as 'to'.",
        kind: FieldKind::Any,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "bcc",
        description: "Blind-carbon-copy recipients; same forms as 'to'.",
        kind: FieldKind::Any,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "subject",
        description: "Message subject (UTF-8).",
        kind: FieldKind::String,
        required: true,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "text",
        description: "Plain-text body. At least one of 'text'/'html' is required; both \
                      together send multipart/alternative.",
        kind: FieldKind::String,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "html",
        description: "HTML body. At least one of 'text'/'html' is required.",
        kind: FieldKind::String,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "from",
        description: "Per-send sender override; honored only when the connector sets \
                      allow_from_override. Default: the connector's 'from'.",
        kind: FieldKind::String,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "reply_to",
        description: "Reply-To address.",
        kind: FieldKind::String,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "headers",
        description: "Extra headers (string values). Structured names (From, To, Subject, \
                      Content-Type, ...) are rejected; intended for List-Unsubscribe, \
                      Auto-Submitted, correlation IDs.",
        kind: FieldKind::Object,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where { message_id, response } is stored. Defaults to \
                      \"data\".",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::{ConnectorConfig, SmtpAuth, SmtpTls};
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // -- A minimal SMTP server: enough protocol for one send, no TLS/auth. --
    //
    // The same idea as http_common's axum mocks: the wire behavior is tested
    // against a real listener, not a mocked transport. Returns the bound
    // address and a receiver that yields the raw DATA payload of the first
    // message accepted.
    async fn spawn_smtp_mock() -> (std::net::SocketAddr, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test");
        let addr = listener.local_addr().expect("test");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("test");
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            write.write_all(b"220 mock ESMTP\r\n").await.expect("test");
            let mut data = String::new();
            let mut in_data = false;
            while let Ok(Some(line)) = lines.next_line().await {
                if in_data {
                    if line == "." {
                        in_data = false;
                        write
                            .write_all(b"250 2.0.0 OK queued as mock-42\r\n")
                            .await
                            .expect("test");
                    } else {
                        data.push_str(&line);
                        data.push('\n');
                    }
                    continue;
                }
                let upper = line.to_ascii_uppercase();
                let reply: &[u8] = if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                    b"250-mock\r\n250 8BITMIME\r\n"
                } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
                    b"250 OK\r\n"
                } else if upper.starts_with("DATA") {
                    in_data = true;
                    b"354 go ahead\r\n"
                } else if upper.starts_with("QUIT") {
                    write.write_all(b"221 bye\r\n").await.expect("test");
                    break;
                } else {
                    b"250 OK\r\n"
                };
                write.write_all(reply).await.expect("test");
            }
            let _ = tx.send(data);
        });
        (addr, rx)
    }

    fn smtp_config(addr: std::net::SocketAddr) -> SmtpConnectorConfig {
        SmtpConnectorConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            tls: SmtpTls::None,
            auth: SmtpAuth::None,
            from: "Orion Test <noreply@example.test>".to_string(),
            allow_from_override: false,
            allow_private_urls: true, // tests use localhost
            timeout_ms: 5_000,
        }
    }

    /// Run one send_email task through a real engine against the mock server.
    async fn run(input: Value, config: SmtpConnectorConfig, data: Value) -> Result<Value, String> {
        let registry =
            std::sync::Arc::new(crate::connector::ConnectorRegistry::new(Default::default()));
        registry
            .insert_for_test("mailer", ConnectorConfig::Smtp(config))
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
            Box::new(SendEmailHandler {
                registry,
                smtp_pool: std::sync::Arc::new(SmtpPoolCache::new(4)),
            }),
        );
        let engine = dataflow_rs::Engine::new(vec![workflow], fns).map_err(|e| e.to_string())?;
        let mut message = dataflow_rs::Message::from_value(&json!({}));
        dataflow_rs::engine::utils::set_nested_value(
            &mut message.context,
            "data",
            dataflow_rs::datavalue::OwnedDataValue::from(&data),
        );
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
    async fn sends_a_multipart_message_through_a_real_smtp_exchange() {
        let (addr, rx) = spawn_smtp_mock().await;
        let out = run(
            json!({
                "connector": "mailer",
                "to": {"var": "data.email"},
                "cc": ["Audit <audit@example.test>"],
                "subject": "Your verification code",
                "text": "Your OTP is 123456",
                "html": "<p>Your OTP is <b>123456</b></p>",
                "headers": {"Auto-Submitted": "auto-generated"},
                "output": "data.mail"
            }),
            smtp_config(addr),
            json!({"email": "User <user@example.test>"}),
        )
        .await
        .expect("test");

        // The task result carries the generated Message-ID and the server line.
        let message_id = out["mail"]["message_id"].as_str().expect("test");
        assert!(
            message_id.starts_with('<') && message_id.ends_with("@example.test>"),
            "{message_id}"
        );
        assert!(
            out["mail"]["response"]
                .as_str()
                .expect("test")
                .contains("mock-42"),
            "{}",
            out["mail"]
        );

        // And the wire payload is a real multipart/alternative message with
        // the custom header and both bodies.
        let wire = rx.await.expect("test");
        assert!(wire.contains("multipart/alternative"), "{wire}");
        assert!(wire.contains("Auto-Submitted: auto-generated"), "{wire}");
        assert!(wire.contains("Your OTP is 123456"), "{wire}");
        assert!(wire.contains("Subject: Your verification code"), "{wire}");
        assert!(wire.contains("Cc: "), "{wire}");
        assert!(wire.contains(message_id), "{wire}");
    }

    #[tokio::test]
    async fn from_override_needs_the_connector_gate() {
        let (addr, _rx) = spawn_smtp_mock().await;
        let input = json!({
            "connector": "mailer",
            "to": "user@example.test",
            "subject": "s",
            "text": "b",
            "from": "spoof@example.test"
        });
        let err = run(input.clone(), smtp_config(addr), json!({}))
            .await
            .expect_err("test");
        assert!(err.contains("allow_from_override"), "{err}");

        let (addr, _rx) = spawn_smtp_mock().await;
        let mut config = smtp_config(addr);
        config.allow_from_override = true;
        run(input, config, json!({})).await.expect("test");
    }

    #[tokio::test]
    async fn message_shape_errors_are_named() {
        let (addr, _rx) = spawn_smtp_mock().await;
        let config = smtp_config(addr);

        // No body at all.
        let err = run(
            json!({"connector": "mailer", "to": "a@b.test", "subject": "s"}),
            config.clone(),
            json!({}),
        )
        .await
        .expect_err("test");
        assert!(err.contains("'text' or 'html'"), "{err}");

        // A malformed address names the field and index.
        let err = run(
            json!({"connector": "mailer", "to": ["ok@b.test", "not an address"],
                   "subject": "s", "text": "b"}),
            config.clone(),
            json!({}),
        )
        .await
        .expect_err("test");
        assert!(err.contains("to[1]"), "{err}");

        // A protected header is refused before anything is sent.
        let err = run(
            json!({"connector": "mailer", "to": "a@b.test", "subject": "s",
                   "text": "b", "headers": {"Subject": "override"}}),
            config,
            json!({}),
        )
        .await
        .expect_err("test");
        assert!(err.contains("structured field"), "{err}");
    }

    #[test]
    fn static_validation_reads_the_same_rules() {
        let obj = json!({"connector": "m", "to": "user@example.test", "subject": "s"});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(
            errs.iter()
                .any(|(_, c, m)| *c == "REQUIRED" && m.contains("'text' or 'html'")),
            "{errs:?}"
        );

        let obj = json!({"connector": "m", "to": "not an address",
                         "subject": "s", "text": "b"});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(
            errs.iter().any(|(f, c, _)| *f == "to" && *c == "INVALID"),
            "{errs:?}"
        );

        // A {"var"} body counts as present; a {"var"} recipient is not
        // statically checkable.
        let obj = json!({"connector": "m", "to": {"var": "data.email"},
                         "subject": "s", "text": {"var": "data.body"}});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(errs.is_empty(), "{errs:?}");

        let obj = json!({"connector": "m", "to": "a@b.test", "subject": "s",
                         "text": "b", "headers": {"Message-ID": "<x@y>"}});
        let errs = validate_static_input(obj.as_object().expect("test"));
        assert!(
            errs.iter()
                .any(|(f, c, _)| *f == "headers" && *c == "INVALID"),
            "{errs:?}"
        );
    }

    #[test]
    fn mailbox_forms_parse_and_name_the_field_on_failure() {
        assert!(parse_mailbox("to", "user@example.test").is_ok());
        assert!(parse_mailbox("to", "Ada Lovelace <ada@example.test>").is_ok());
        let err = parse_mailbox("reply_to", "nope")
            .expect_err("test")
            .to_string();
        assert!(err.contains("reply_to"), "{err}");
    }
}

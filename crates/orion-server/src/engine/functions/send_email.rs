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

use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_builder::headers::raw::Raw;
use mail_send::smtp::AssertReply;
use serde_json::{Value, json};

use super::connector_helpers::{
    ConnectorCall, apply_output, require_connector, resolve_optional_str, resolve_value,
    to_connect_error,
};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::smtp_pool::{PooledClient, SmtpPoolCache};
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
        let subject = resolve_optional_str(input, "subject", NAME, ctx)?
            .ok_or_else(|| validation("requires 'subject'"))?;
        let text = resolve_optional_str(input, "text", NAME, ctx)?;
        let html = resolve_optional_str(input, "html", NAME, ctx)?;
        if text.is_none() && html.is_none() {
            return Err(validation("requires at least one of 'text' or 'html'"));
        }
        let from_override = resolve_optional_str(input, "from", NAME, ctx)?;
        let reply_to = resolve_optional_str(input, "reply_to", NAME, ctx)?
            .map(|s| parse_mailbox("reply_to", &s))
            .transpose()?;

        call.run(&self.registry, async {
            let connector_config = call.resolve(&self.registry, None).await?;
            let smtp_config = require_connector::<crate::connector::kind::Smtp>(
                &connector_config,
                call.connector,
            )?;

            let from = sender(smtp_config, from_override.as_deref(), call.connector)?;
            let bare_id = generated_message_id(&from);
            let message = build_message(MessageParts {
                from: &from,
                message_id: &bare_id,
                to: &to,
                cc: &cc,
                subject: &subject,
                reply_to,
                headers: input.get("headers"),
                text,
                html,
            })?;

            // Bcc rides the envelope only — it is deliberately absent from the
            // headers `build_message` rendered, so a blind copy stays blind.
            let recipients: Vec<&str> = to
                .iter()
                .chain(cc.iter())
                .chain(bcc.iter())
                .map(|mbox| mbox.email.as_str())
                .collect();

            let pool = self
                .smtp_pool
                .get_pool(call.connector, smtp_config)
                .await
                .map_err(to_connect_error)?;
            let mut client = pool.checkout().await.map_err(to_connect_error)?;

            // One attempt, no retry loop — see the module comment. The
            // breaker (via `call.run`) still counts this failure.
            let response = match deliver(&mut client, &from, &recipients, &message).await {
                Ok(reply) => {
                    pool.checkin(client).await;
                    reply
                }
                Err(e) => {
                    // Deliberately not returned to the pool: after a failure
                    // part-way through a transaction the connection's protocol
                    // state is unknown, and the next send would inherit it.
                    return Err(DataflowError::function_execution(
                        format!("SMTP send via '{}' failed: {e}", call.connector),
                        None,
                    ));
                }
            };

            let result = json!({
                "message_id": format!("<{bare_id}>"),
                "response": response,
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

/// One parsed address. Orion's own type because mail-builder models an
/// address as data to *render* and never parses one, and lettre's `Mailbox` —
/// which did both — is what this replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Mailbox {
    pub name: Option<String>,
    pub email: String,
}

impl Mailbox {
    /// The domain half, for the generated Message-ID. Parsing guarantees the
    /// `@`, so the fallback is unreachable rather than meaningful.
    fn domain(&self) -> &str {
        self.email.rsplit_once('@').map_or("", |(_, domain)| domain)
    }

    fn to_address(&self) -> Address<'static> {
        Address::new_address(self.name.clone(), self.email.clone())
    }
}

/// Parse `addr@x` / `Name <addr@x>`, naming the field (and index) on failure.
///
/// The grammar comes from mail-parser rather than a hand-rolled split, so
/// quoted display names and comments behave the way a receiving MTA will read
/// them. Two checks are Orion's own on top:
///
/// * a line break is refused before parsing, because the header-injection
///   vector is precisely a parser that stops at the newline and accepts the
///   prefix as valid;
/// * the address shape is then enforced, because mail-parser is a *parser* of
///   real-world mail and is deliberately lenient — it will hand back a
///   nonsense local part rather than reject a header a live MTA emitted.
pub(crate) fn parse_mailbox(field: &str, s: &str) -> Result<Mailbox, DataflowError> {
    let s = s.trim();
    let invalid = |why: &str| validation(&format!("'{field}' is not a valid email address: {why}"));

    if s.is_empty() {
        return Err(invalid("empty"));
    }
    if s.contains(['\r', '\n']) {
        return Err(invalid("contains a line break"));
    }

    // A synthetic header, because mail-parser's address grammar is reachable
    // only through a header parse. `with_address_headers` is required: a bare
    // `MessageParser::new()` registers no field parsers and yields raw text.
    let raw = format!("To:{s}\r\n\r\n");
    let parsed = mail_parser::MessageParser::new()
        .with_address_headers()
        .parse_headers(raw.as_bytes())
        .ok_or_else(|| invalid("unparseable"))?;
    let Some(mail_parser::Address::List(list)) = parsed.to() else {
        return Err(invalid("expected a single address"));
    };
    let [addr] = list.as_slice() else {
        return Err(invalid("expected exactly one address"));
    };

    let email = addr
        .address
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .ok_or_else(|| invalid("no address part"))?;
    let Some((local, domain)) = email.split_once('@') else {
        return Err(invalid("missing '@'"));
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err(invalid("malformed address"));
    }
    if email.contains(|c: char| c.is_whitespace() || c.is_control() || "<>,;:\"\\".contains(c)) {
        return Err(invalid("malformed address"));
    }

    Ok(Mailbox {
        name: addr
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        email: email.to_string(),
    })
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
///
/// Returned *bare*, without the angle brackets: mail-builder's `MessageId`
/// writes its own `<`/`>`, so passing a bracketed id would emit `<<id>>`. The
/// brackets go back on for the task result, which is the form a workflow
/// correlates against what the receiving server logs.
fn generated_message_id(from: &Mailbox) -> String {
    format!("{}@{}", uuid::Uuid::new_v4(), from.domain())
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
                // These two were lettre's `HeaderName::new_from_ascii` and
                // `HeaderValue` doing the checking. mail-builder's `Raw`
                // writes both halves verbatim, so the injection guard is
                // Orion's now: a CR or LF ends the header and begins one the
                // caller chose, and a ':' in a name splits it in two.
                if name.is_empty() || !name.bytes().all(|b| b.is_ascii_graphic() && b != b':') {
                    return Err(validation(&format!(
                        "'headers.{name}' is not a valid header name"
                    )));
                }
                if value.as_str().is_some_and(|v| v.contains(['\r', '\n'])) {
                    return Err(validation(&format!(
                        "'headers.{name}' may not contain a line break"
                    )));
                }
            }
            Ok(())
        }
        Some(_) => Err(validation("'headers' must be an object of string values")),
    }
}

/// Everything [`build_message`] assembles, gathered so the call site reads by
/// name instead of by a nine-argument positional list (two adjacent
/// `&[Mailbox]`s would make a swapped `to`/`cc` invisible).
///
/// No `bcc`: a blind copy is an envelope recipient and nothing else, so it
/// never reaches the rendered headers. The caller adds it to `RCPT TO`.
struct MessageParts<'a> {
    from: &'a Mailbox,
    message_id: &'a str,
    to: &'a [Mailbox],
    cc: &'a [Mailbox],
    subject: &'a str,
    reply_to: Option<Mailbox>,
    headers: Option<&'a Value>,
    text: Option<String>,
    html: Option<String>,
}

/// Assemble the RFC 5322 message as the bytes that follow `DATA`.
///
/// mail-builder supplies Date and MIME-Version, picks the transfer encoding
/// per part, and renders `text` + `html` as multipart/alternative.
fn build_message(parts: MessageParts<'_>) -> Result<Vec<u8>, DataflowError> {
    let MessageParts {
        from,
        message_id,
        to,
        cc,
        subject,
        reply_to,
        headers,
        text,
        html,
    } = parts;

    let addresses =
        |boxes: &[Mailbox]| Address::List(boxes.iter().map(Mailbox::to_address).collect());

    let mut builder = MessageBuilder::new()
        .from(from.to_address())
        .subject(subject)
        .message_id(message_id.to_string());
    if !to.is_empty() {
        builder = builder.to(addresses(to));
    }
    if !cc.is_empty() {
        builder = builder.cc(addresses(cc));
    }
    if let Some(mbox) = reply_to {
        builder = builder.reply_to(mbox.to_address());
    }

    builder = match (text, html) {
        (Some(text), Some(html)) => builder.text_body(text).html_body(html),
        (Some(text), None) => builder.text_body(text),
        (None, Some(html)) => builder.html_body(html),
        (None, None) => unreachable!("checked in the prologue"),
    };

    // Extra headers, already name- and value-checked by `check_headers_field`.
    if let Some(Value::Object(map)) = headers {
        for (name, value) in map {
            builder = builder.header(
                name.clone(),
                Raw::new(value.as_str().unwrap_or_default().to_string()),
            );
        }
    }

    builder.write_to_vec().map_err(|e| {
        DataflowError::Validation(format!("{NAME}: message could not be assembled: {e}"))
    })
}

/// Drive one SMTP transaction and return the server's final reply line.
///
/// mail-send's own `send` throws every reply away — `mail_from`, `rcpt_to` and
/// `data` all return `()` — and Orion hands the queue line back to the
/// workflow, so the envelope is driven here. `write_message` still does the
/// dot-stuffing and the terminating `.`, which is the part worth borrowing.
async fn deliver(
    client: &mut PooledClient,
    from: &Mailbox,
    recipients: &[&str],
    message: &[u8],
) -> Result<String, mail_send::Error> {
    client
        .cmd(format!("MAIL FROM:<{}>\r\n", from.email).as_bytes())
        .await?
        .assert_positive_completion()?;
    for rcpt in recipients {
        client
            .cmd(format!("RCPT TO:<{rcpt}>\r\n").as_bytes())
            .await?
            .assert_positive_completion()?;
    }
    client.cmd(b"DATA\r\n").await?.assert_code(354)?;

    // The body and its reply share one timeout, the way mail-send's `data`
    // does: a relay that accepts DATA and then stalls must not hang the task.
    let timeout = client.timeout;
    let reply = tokio::time::timeout(timeout, async {
        client.write_message(message).await?;
        client.read().await
    })
    .await
    .map_err(|_| mail_send::Error::Timeout)??;

    if !reply.is_positive_completion() {
        return Err(mail_send::Error::UnexpectedReply(reply));
    }
    Ok(reply.message)
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

fn field_name(key: &str) -> &'static str {
    super::schema::static_field_name(SEND_EMAIL_FIELDS, key, "to")
}

fn strip_name(e: &DataflowError) -> String {
    super::schema::strip_handler_prefix(NAME, e)
}

// -- Input schema (F53) --

pub(super) const SEND_EMAIL_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the SMTP connector to send through.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "to",
        description: "Recipient address or array of addresses; each is 'addr@example.com' \
                      or 'Name <addr@example.com>'.",
        kind: FieldKind::Any,
        required: true,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "cc",
        description: "Carbon-copy recipients; same forms as 'to'.",
        kind: FieldKind::Any,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "bcc",
        description: "Blind-carbon-copy recipients; same forms as 'to'.",
        kind: FieldKind::Any,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "subject",
        description: "Message subject (UTF-8).",
        kind: FieldKind::String,
        required: true,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "text",
        description: "Plain-text body. At least one of 'text'/'html' is required; both \
                      together send multipart/alternative.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "html",
        description: "HTML body. At least one of 'text'/'html' is required.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "from",
        description: "Per-send sender override; honored only when the connector sets \
                      allow_from_override. Default: the connector's 'from'.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "reply_to",
        description: "Reply-To address.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "headers",
        description: "Extra headers (string values). Structured names (From, To, Subject, \
                      Content-Type, ...) are rejected; intended for List-Unsubscribe, \
                      Auto-Submitted, correlation IDs.",
        kind: FieldKind::Object,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where { message_id, response } is stored. Defaults to \
                      \"data\".",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
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

    /// What one mock server saw: how many TCP sessions were opened, and every
    /// line the client sent across all of them (envelope commands and DATA
    /// payload alike, which is what lets a test tell a Bcc *envelope*
    /// recipient from a Bcc *header*).
    #[derive(Default)]
    struct MockLog {
        connections: usize,
        transcript: String,
    }

    /// Like [`spawn_smtp_mock`], but serves any number of sessions and records
    /// them, for the pooling and envelope assertions.
    async fn spawn_logging_smtp_mock() -> (std::net::SocketAddr, Arc<std::sync::Mutex<MockLog>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test");
        let addr = listener.local_addr().expect("test");
        let log = Arc::new(std::sync::Mutex::new(MockLog::default()));
        let accept_log = log.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                accept_log.lock().expect("test").connections += 1;
                let session_log = accept_log.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    let _ = write.write_all(b"220 mock ESMTP\r\n").await;
                    let mut in_data = false;
                    while let Ok(Some(line)) = lines.next_line().await {
                        {
                            let mut log = session_log.lock().expect("test");
                            log.transcript.push_str(&line);
                            log.transcript.push('\n');
                        }
                        if in_data {
                            if line == "." {
                                in_data = false;
                                let _ =
                                    write.write_all(b"250 2.0.0 OK queued as mock-42\r\n").await;
                            }
                            continue;
                        }
                        let upper = line.to_ascii_uppercase();
                        let reply: &[u8] = if upper.starts_with("EHLO") || upper.starts_with("HELO")
                        {
                            b"250-mock\r\n250 8BITMIME\r\n"
                        } else if upper.starts_with("DATA") {
                            in_data = true;
                            b"354 go ahead\r\n"
                        } else if upper.starts_with("QUIT") {
                            let _ = write.write_all(b"221 bye\r\n").await;
                            break;
                        } else {
                            b"250 OK\r\n"
                        };
                        let _ = write.write_all(reply).await;
                    }
                });
            }
        });
        (addr, log)
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
        crate::engine::functions::run_test_task(
            NAME,
            Box::new(SendEmailHandler {
                registry,
                smtp_pool: std::sync::Arc::new(SmtpPoolCache::new(4)),
            }),
            input,
            data,
        )
        .await
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

    /// Send `count` messages through one shared pool, returning the mock's log.
    async fn send_n(addr: std::net::SocketAddr, input: Value, count: usize) -> Arc<SmtpPoolCache> {
        let registry =
            std::sync::Arc::new(crate::connector::ConnectorRegistry::new(Default::default()));
        registry
            .insert_for_test("mailer", ConnectorConfig::Smtp(smtp_config(addr)))
            .await;
        let smtp_pool = std::sync::Arc::new(SmtpPoolCache::new(4));
        for _ in 0..count {
            crate::engine::functions::run_test_task(
                NAME,
                Box::new(SendEmailHandler {
                    registry: registry.clone(),
                    smtp_pool: smtp_pool.clone(),
                }),
                input.clone(),
                json!({}),
            )
            .await
            .expect("test");
        }
        smtp_pool
    }

    /// A blind copy is an envelope recipient and nothing else. lettre enforced
    /// this by excluding Bcc from the rendered message; mail-builder's `bcc()`
    /// would happily write the header, so `build_message` is never given one
    /// and the address is added to `RCPT TO` instead. If that ever regresses,
    /// every recipient learns who was blind-copied.
    #[tokio::test]
    async fn bcc_reaches_the_envelope_but_never_the_headers() {
        let (addr, log) = spawn_logging_smtp_mock().await;
        let pool = send_n(
            addr,
            json!({"connector": "mailer", "to": "user@example.test",
                   "bcc": ["blind@example.test"], "subject": "s", "text": "b"}),
            1,
        )
        .await;
        drop(pool); // close the session so the transcript is complete

        let transcript = log.lock().expect("test").transcript.clone();
        assert!(
            transcript.contains("RCPT TO:<blind@example.test>"),
            "bcc must be an envelope recipient: {transcript}"
        );
        assert!(
            !transcript.to_ascii_lowercase().contains("bcc:"),
            "bcc must never be a header: {transcript}"
        );
    }

    /// The pool's whole reason to exist: a second send reuses the parked
    /// connection instead of paying another handshake.
    #[tokio::test]
    async fn a_second_send_reuses_the_pooled_connection() {
        let (addr, log) = spawn_logging_smtp_mock().await;
        let pool = send_n(
            addr,
            json!({"connector": "mailer", "to": "user@example.test",
                   "subject": "s", "text": "b"}),
            2,
        )
        .await;
        drop(pool);

        let log = log.lock().expect("test");
        assert_eq!(
            log.connections, 1,
            "two sends should share one connection: {}",
            log.transcript
        );
        // And the reuse was verified rather than assumed.
        assert!(
            log.transcript.contains("RSET"),
            "reuse must probe with RSET: {}",
            log.transcript
        );
    }

    /// mail-builder's `Raw` header writes both halves verbatim, so the CRLF
    /// checks lettre's typed `HeaderName`/`HeaderValue` used to perform are
    /// Orion's now. A newline here would end the header and begin one the
    /// caller chose.
    #[test]
    fn header_injection_attempts_are_refused() {
        for (name, value) in [
            ("X-Evil", "ok\r\nBcc: attacker@example.test"),
            ("X-Evil", "ok\nSubject: replaced"),
            ("X:Evil", "ok"),
            ("X Evil", "ok"),
        ] {
            let obj = json!({"connector": "m", "to": "a@b.test", "subject": "s",
                             "text": "b", "headers": {name: value}});
            let errs = validate_static_input(obj.as_object().expect("test"));
            assert!(
                errs.iter()
                    .any(|(f, c, _)| *f == "headers" && *c == "INVALID"),
                "{name}: {value:?} should be refused, got {errs:?}"
            );
        }
    }

    /// An address field is equally a header, and equally injectable.
    #[test]
    fn addresses_with_line_breaks_are_refused() {
        let err = parse_mailbox("to", "a@b.test\r\nBcc: attacker@example.test")
            .expect_err("test")
            .to_string();
        assert!(err.contains("line break"), "{err}");
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

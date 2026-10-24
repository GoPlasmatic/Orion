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

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_builder::headers::raw::Raw;
use mail_send::smtp::AssertReply;
use serde_json::{Value, json};

use super::connector_helpers::to_connect_error;
use super::schema::{FieldKind, FieldSchema};
use crate::connector::smtp_pool::{PooledClient, SmtpPoolCache};
use crate::connector::{ConnectorRegistry, SmtpConnectorConfig};
use crate::engine::kit::{self, Members, read};
use crate::engine::{ErrorClass, HandlerError};

/// This handler's name, for the free functions below — a reference to the one
/// place it is written (F48), not a second spelling of it.
pub(crate) const NAME: &str = "send_email";

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

/// The mail as the task describes it, every field already folded against the
/// message. What is *not* here is the sender: it depends on the connector,
/// which is not resolved yet.
pub struct Mail {
    to: Vec<Mailbox>,
    cc: Vec<Mailbox>,
    bcc: Vec<Mailbox>,
    subject: String,
    text: Option<String>,
    html: Option<String>,
    from_override: Option<String>,
    reply_to: Option<Mailbox>,
    headers: Vec<(String, String)>,
}

/// The send, against a resolved SMTP connector: the sender, the generated
/// Message-ID, the rendered message, the pooled session and the one SMTP
/// transaction. The task result, `{ message_id, response }`. Shared by the
/// dataflow-rs 3 handler and the dataflow-rs 4 function, so both send the
/// same mail.
async fn deliver_mail(
    smtp_pool: &SmtpPoolCache,
    mail: Mail,
    smtp_config: &SmtpConnectorConfig,
    connector: &str,
) -> Result<Value, HandlerError> {
    let from = sender(smtp_config, mail.from_override.as_deref(), connector)?;
    let bare_id = generated_message_id(&from);
    let message = build_message(MessageParts {
        from: &from,
        message_id: &bare_id,
        to: &mail.to,
        cc: &mail.cc,
        subject: &mail.subject,
        reply_to: mail.reply_to,
        headers: &mail.headers,
        text: mail.text,
        html: mail.html,
    })?;

    // Bcc rides the envelope only — it is deliberately absent from the
    // headers `build_message` rendered, so a blind copy stays blind.
    let recipients: Vec<&str> = mail
        .to
        .iter()
        .chain(mail.cc.iter())
        .chain(mail.bcc.iter())
        .map(|mbox| mbox.email.as_str())
        .collect();

    let pool = smtp_pool
        .get_pool(connector, smtp_config)
        .await
        .map_err(to_connect_error)?;
    let mut client = pool.checkout().await.map_err(to_connect_error)?;

    // One attempt, no retry loop — see the module comment. The breaker
    // (via the handler shell) still counts this failure.
    let response = match deliver(&mut client, &from, &recipients, &message).await {
        Ok(reply) => {
            pool.checkin(client).await;
            reply
        }
        Err(e) => {
            // Deliberately not returned to the pool: after a failure
            // part-way through a transaction the connection's protocol
            // state is unknown, and the next send would inherit it.
            return Err(HandlerError::new(
                ErrorClass::Backend,
                format!("SMTP send via '{connector}' failed: {e}"),
            ));
        }
    };

    Ok(json!({
        "message_id": format!("<{bare_id}>"),
        "response": response,
    }))
}

/// `send_email`'s input on dataflow-rs 4, each field in the position it
/// executes in today (I183): `to`, `cc` and `bcc` are documents whose
/// `{"var": …}` holes are filled and then read by `address_list`, as
/// dataflow-rs 3 folded them; every other field is an expression read by
/// `kit::read`'s twin of the reader it went through. A task missing
/// `connector`, `to` or `subject` is refused at load (I183), where 1.12
/// failed each call; every other rule is still the call's, with its message.
#[derive(Debug, FunctionConfig)]
pub struct SendEmailConfig {
    /// The SMTP connector sent through.
    connector: String,
    /// The recipients: an address or an array of them.
    to: Interpolated,
    /// The carbon-copy recipients.
    cc: Option<Interpolated>,
    /// The blind-carbon-copy recipients, on the envelope only.
    bcc: Option<Interpolated>,
    /// The subject.
    subject: Expr<OwnedDataValue>,
    /// The plain-text body.
    text: Option<Expr<OwnedDataValue>>,
    /// The HTML body.
    html: Option<Expr<OwnedDataValue>>,
    /// A per-send sender, when the connector allows one.
    from: Option<Expr<OwnedDataValue>>,
    /// The Reply-To address.
    reply_to: Option<Expr<OwnedDataValue>>,
    /// Extra headers, each value computed from the message, kept in the
    /// order written: 1.12 judged and rendered them in that order.
    headers: Option<Members<Expr<OwnedDataValue>>>,
    /// Where `{ message_id, response }` is written.
    #[output(default = "data")]
    output: Output,
}

/// An optional string field for this call, by `resolve_optional_str`'s
/// reading.
fn optional_str(
    ctx: &Ctx<'_>,
    field: &Option<Expr<OwnedDataValue>>,
    name: &str,
) -> Result<Option<String>, HandlerError> {
    let value = field.as_ref().map(|expr| ctx.eval(expr)).transpose()?;
    Ok(read::optional_str(value.as_deref(), name, NAME)?.map(std::borrow::Cow::into_owned))
}

/// An address document with its holes filled, through [`address_list`].
fn rendered_addresses(
    ctx: &Ctx<'_>,
    doc: &Interpolated,
    field: &str,
) -> Result<Option<Vec<Mailbox>>, HandlerError> {
    let filled = serde_json::to_value(ctx.render(doc))
        .map_err(|e| HandlerError::new(ErrorClass::Backend, e))?;
    address_list(field, filled)
}

/// The mail the task describes, read as `SendEmailHandler::parse` reads it,
/// in its order: the headers' literal check, `to`, `subject`, the bodies and
/// their rule, `cc`, `bcc`, `from`, `reply_to`, then each header's value.
fn mail(ctx: &Ctx<'_>, cfg: &SendEmailConfig) -> Result<Mail, HandlerError> {
    let headers = cfg
        .headers
        .as_ref()
        .map(Members::iter)
        .into_iter()
        .flatten();
    check_headers(headers.clone().map(|(name, expr)| (name, expr.raw()))).map_err(named)?;

    let to = rendered_addresses(ctx, &cfg.to, "to")
        .map_err(named)?
        .ok_or_else(|| validation("requires 'to' (an address or an array of addresses)"))?;
    let subject = ctx.eval(&cfg.subject)?;
    let subject = read::optional_str(Some(&subject), "subject", NAME)?
        .ok_or_else(|| validation("requires 'subject'"))?
        .into_owned();
    let text = optional_str(ctx, &cfg.text, "text")?;
    let html = optional_str(ctx, &cfg.html, "html")?;
    if text.is_none() && html.is_none() {
        return Err(named(validation(
            "requires at least one of 'text' or 'html'",
        )));
    }

    let cc = match &cfg.cc {
        Some(doc) => rendered_addresses(ctx, doc, "cc").map_err(named)?,
        None => None,
    };
    let bcc = match &cfg.bcc {
        Some(doc) => rendered_addresses(ctx, doc, "bcc").map_err(named)?,
        None => None,
    };
    Ok(Mail {
        to,
        cc: cc.unwrap_or_default(),
        bcc: bcc.unwrap_or_default(),
        subject,
        text,
        html,
        from_override: optional_str(ctx, &cfg.from, "from")?,
        reply_to: optional_str(ctx, &cfg.reply_to, "reply_to")?
            .map(|s| parse_mailbox("reply_to", &s))
            .transpose()?,
        headers: headers
            .map(|(name, expr)| {
                let value = Value::from(&*ctx.eval(expr)?);
                Ok((name.to_string(), header_value(name, &value)?.to_string()))
            })
            .collect::<Result<_, HandlerError>>()?,
    })
}

/// `send_email` as a dataflow-rs 4 function: the mail `SendEmailHandler`
/// sends, read in the same order, through the same delivery.
pub struct SendEmailFunction {
    pub registry: Arc<ConnectorRegistry>,
    pub smtp_pool: Arc<SmtpPoolCache>,
}

impl Function for SendEmailFunction {
    type Config = SendEmailConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &SendEmailConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, before the mail is read: an output that
        // names no path sends nothing.
        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
        let mail = mail(ctx, cfg)?;
        let mut sent = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let smtp =
                kit::require_connector::<crate::connector::kind::Smtp>(&config, &cfg.connector)?;
            sent = Some(deliver_mail(&self.smtp_pool, mail, smtp, &cfg.connector).await?);
            Ok(Outcome::Ok)
        })
        .await?;
        if let Some(result) = sent {
            ctx.set(&output, OwnedDataValue::from(&result))?;
        }
        Ok(Outcome::Ok)
    }
}

/// A caller-fixable problem with the task's input, message **bare**.
///
/// Shared by `execute`, `validate_static_input` and the SMTP connector
/// validator (which calls [`parse_mailbox`] on a stored `from`). The static and
/// connector paths present the message beside a field path that already says
/// what is wrong; the execution path adds the handler's name through [`named`].
fn validation(msg: &str) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, msg)
}

/// A bare parser error on its way out through the execution path, named once.
///
/// The static-validation path does not call this: there the `FieldError`'s own
/// field path already says which task input was wrong.
fn named(e: HandlerError) -> HandlerError {
    e.prefixed(NAME)
}

/// An address field's value, its holes filled: one address or an array,
/// each `addr@x` or `Name <addr@x>`. `None` for `null` or an empty array.
/// Today's executed reading of `to`, `cc` and `bcc` (I183), whichever engine
/// filled the holes.
fn address_list(field: &str, raw: Value) -> Result<Option<Vec<Mailbox>>, HandlerError> {
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
pub(crate) fn parse_mailbox(field: &str, s: &str) -> Result<Mailbox, HandlerError> {
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
) -> Result<Mailbox, HandlerError> {
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
fn check_headers_field(input: &Value) -> Result<(), HandlerError> {
    match input.get("headers") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(map)) => check_headers(map.iter().map(|(k, v)| (k.as_str(), v))),
        Some(_) => Err(validation("'headers' must be an object of string values")),
    }
}

/// The literal half of each `headers` member, as authored: its name is not
/// a structured field's, and a scalar value is a string with no line break
/// under a valid name. An object or an array may be an operator call, and
/// what it produces is checked when it is resolved.
fn check_headers<'a>(
    members: impl IntoIterator<Item = (&'a str, &'a Value)>,
) -> Result<(), HandlerError> {
    for (name, value) in members {
        if PROTECTED_HEADERS
            .iter()
            .any(|p| p.eq_ignore_ascii_case(name))
        {
            return Err(validation(&format!(
                "'headers' may not set '{name}' — use the structured field instead"
            )));
        }
        // A scalar is unambiguously itself in JSONLogic, so it is
        // judged here; an object or array may be an operator call, and
        // what it produces is `resolve_headers`'s to check.
        if value.is_object() || value.is_array() {
            continue;
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

/// A resolved header value, as the JSON it resolved to: a string with no
/// line break.
fn header_value<'v>(name: &str, value: &'v Value) -> Result<&'v str, HandlerError> {
    let Some(text) = value.as_str() else {
        return Err(validation(&format!(
            "'headers.{name}' must resolve to a string"
        )));
    };
    if text.contains(['\r', '\n']) {
        return Err(validation(&format!(
            "'headers.{name}' may not contain a line break"
        )));
    }
    Ok(text)
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
    headers: &'a [(String, String)],
    text: Option<String>,
    html: Option<String>,
}

/// Assemble the RFC 5322 message as the bytes that follow `DATA`.
///
/// mail-builder supplies Date and MIME-Version, picks the transfer encoding
/// per part, and renders `text` + `html` as multipart/alternative.
fn build_message(parts: MessageParts<'_>) -> Result<Vec<u8>, HandlerError> {
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

    // Extra headers: names checked at authoring by `check_headers_field`,
    // values checked by `resolve_headers` *after* evaluation — which is where
    // the CRLF guard has to be once a value can be computed.
    for (name, value) in headers {
        builder = builder.header(name.clone(), Raw::new(value.clone()));
    }

    builder
        .write_to_vec()
        .map_err(|e| validation(&format!("{NAME}: message could not be assembled: {e}")))
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
        errors.push(("headers", "INVALID", e.msg));
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
                errors.push((field_name(field), "INVALID", e.msg));
            }
        }
    }

    errors
}

fn field_name(key: &str) -> &'static str {
    super::schema::static_field_name(SEND_EMAIL_FIELDS, key, "to")
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
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "cc",
        description: "Carbon-copy recipients; same forms as 'to'.",
        kind: FieldKind::Any,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "bcc",
        description: "Blind-carbon-copy recipients; same forms as 'to'.",
        kind: FieldKind::Any,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "subject",
        description: "Message subject (UTF-8).",
        kind: FieldKind::String,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "text",
        description: "Plain-text body. At least one of 'text'/'html' is required; both \
                      together send multipart/alternative.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "html",
        description: "HTML body. At least one of 'text'/'html' is required.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "from",
        description: "Per-send sender override; honored only when the connector sets \
                      allow_from_override. Default: the connector's 'from'.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "reply_to",
        description: "Reply-To address.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "headers",
        description: "Extra headers (string values). Structured names (From, To, Subject, \
                      Content-Type, ...) are rejected; intended for List-Unsubscribe, \
                      Auto-Submitted, correlation IDs.",
        kind: FieldKind::Object,
        template_at: &["*"],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where { message_id, response } is stored. Defaults to \
                      \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
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

    // -- dataflow-rs 4: the config, and a twin of each test above that runs
    // the handler, held to it over the same input --

    use crate::engine::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};

    fn config_v4(input: Value) -> Result<SendEmailConfig, Vec<dataflow_rs::Issue>> {
        testing::config_with(&kit::test_exprs(), input)
    }

    /// Declaration order is the field table's (I179).
    #[test]
    fn the_config_declares_the_field_table_in_its_order() {
        let fields: Vec<&str> = SendEmailConfig::fields().iter().map(|f| &*f.name).collect();
        let table: Vec<&str> = SEND_EMAIL_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);
    }

    #[test]
    fn every_field_kind_compiles() {
        let cfg = config_v4(json!({
            "connector": "mailer",
            "to": [{"var": "data.email"}, "Audit <audit@example.test>"],
            "cc": {"var": "data.cc"},
            "bcc": "blind@example.test",
            "subject": {"cat": ["Order ", {"var": "data.id"}]},
            "text": "t", "html": "<p>h</p>",
            "from": "noreply@example.test", "reply_to": {"var": "data.reply"},
            "headers": {"X-Id": {"var": "data.id"}},
            "output": "data.mail"
        }))
        .expect("compiles");
        assert_eq!(cfg.connector, "mailer");
        assert_eq!(cfg.to.holes().count(), 1);
        assert_eq!(cfg.cc.as_ref().map(|d| d.holes().count()), Some(1));
        assert!(cfg.subject.constant().is_none() && cfg.text.is_some());
        assert_eq!(cfg.headers.as_ref().map(Members::len), Some(1));
        let output = cfg.output.constant().map(ToString::to_string);
        assert_eq!(output.as_deref(), Some("data.mail"));

        // `output` defaults to `data`, as the field table says.
        let cfg = config_v4(json!({"connector": "m", "to": "a@b.test", "subject": "s"}))
            .expect("compiles");
        assert_eq!(
            cfg.output.constant().map(ToString::to_string).as_deref(),
            Some("data")
        );
        assert!(cfg.cc.is_none() && cfg.headers.is_none());
    }

    /// A literal `connector`; `connector`, `to` and `subject` required (I183);
    /// an unknown key refused (break 2a). Every rule beyond the shape stays
    /// the call's, so a task with no body still compiles.
    #[test]
    fn the_required_fields_and_an_unknown_key_are_refused() {
        let rejected = |input: Value| -> Vec<(String, IssueCode)> {
            config_v4(input)
                .expect_err("refused")
                .into_iter()
                .filter(|i| i.severity == Severity::Rejected)
                .map(|i| (i.path.to_string(), i.code))
                .collect()
        };
        let full = json!({"connector": "m", "to": "a@b.test", "subject": "s"});
        for field in ["connector", "to", "subject"] {
            let mut input = full.clone();
            input.as_object_mut().expect("object").remove(field);
            assert_eq!(
                rejected(input),
                [(field.to_string(), IssueCode::InputParse)]
            );
        }
        let mut computed = full.clone();
        computed["connector"] = json!({"var": "data.mailer"});
        assert_eq!(
            rejected(computed),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        let mut unknown = full.clone();
        unknown["body"] = json!("b");
        let issues = rejected(unknown);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].0, "body");
        config_v4(full).expect("no body is the call's to refuse");
    }

    async fn mailer(config: SmtpConnectorConfig) -> Arc<ConnectorRegistry> {
        let registry =
            std::sync::Arc::new(crate::connector::ConnectorRegistry::new(Default::default()));
        registry
            .insert_for_test("mailer", ConnectorConfig::Smtp(config))
            .await;
        registry
    }

    /// One task against its own one-session mock: what it saw, and the
    /// mock's DATA payload when the send reached it.
    async fn run(
        input: Value,
        config: impl Fn(std::net::SocketAddr) -> SmtpConnectorConfig,
        data: Value,
    ) -> (Seen, Option<String>) {
        let (addr, rx) = spawn_smtp_mock().await;
        let registry = mailer(config(addr)).await;
        let pool = Arc::new(SmtpPoolCache::new(4));
        let f = SendEmailFunction {
            registry,
            smtp_pool: pool,
        };
        let seen = twin::v4(NAME, f, input, data).await;
        // The mock reports its payload once the session ends, which the
        // pool's drop at the end of the run does; a refused call never
        // reached it.
        let mut rx = rx;
        let wire = match &seen {
            Ok(_) => tokio::time::timeout(std::time::Duration::from_secs(5), rx)
                .await
                .ok()
                .and_then(Result::ok),
            Err(_) => rx.try_recv().ok(),
        }
        .filter(|wire| !wire.is_empty());
        (seen, wire)
    }

    /// `seen` with each generated Message-ID, which is fresh per send, in a
    /// shape check and then masked.
    fn masked(seen: &Seen) -> Seen {
        fn mask(v: &mut Value) {
            match v {
                Value::Object(map) => {
                    for (k, v) in map.iter_mut() {
                        if k == "message_id" {
                            let id = v.as_str().expect("a string id");
                            assert!(
                                id.starts_with('<') && id.ends_with("@example.test>"),
                                "{id}"
                            );
                            *v = json!("<masked>");
                        } else {
                            mask(v);
                        }
                    }
                }
                Value::Array(items) => items.iter_mut().for_each(mask),
                _ => {}
            }
        }
        seen.clone().map(|mut data| {
            mask(&mut data);
            data
        })
    }

    #[tokio::test]
    async fn sends_a_multipart_message_through_a_real_smtp_exchange() {
        let input = json!({
            "connector": "mailer",
            "to": {"var": "data.email"},
            "cc": ["Audit <audit@example.test>"],
            "subject": "Your verification code",
            "text": "Your OTP is 123456",
            "html": "<p>Your OTP is <b>123456</b></p>",
            "headers": {"Auto-Submitted": "auto-generated"},
            "output": "data.mail"
        });
        let data = json!({"email": "User <user@example.test>"});
        let (v4, wire) = run(input, smtp_config, data).await;
        twin::pinned(&masked(&v4), "sends_a_multipart_message");

        let out = v4.expect("sent");
        let message_id = out["mail"]["message_id"].as_str().expect("test");
        assert!(
            out["mail"]["response"]
                .as_str()
                .expect("test")
                .contains("mock-42"),
            "{}",
            out["mail"]
        );
        let wire = wire.expect("the payload reached the server");
        assert!(wire.contains("multipart/alternative"), "{wire}");
        assert!(wire.contains("Auto-Submitted: auto-generated"), "{wire}");
        assert!(wire.contains("Your OTP is 123456"), "{wire}");
        assert!(wire.contains("Subject: Your verification code"), "{wire}");
        assert!(wire.contains("Cc: "), "{wire}");
        assert!(wire.contains(message_id), "{wire}");
    }

    #[tokio::test]
    async fn from_override_needs_the_connector_gate() {
        let input = json!({
            "connector": "mailer",
            "to": "user@example.test",
            "subject": "s",
            "text": "b",
            "from": "spoof@example.test"
        });
        let (v4, _) = run(input.clone(), smtp_config, json!({})).await;
        twin::pinned(&v4, "from_override/refused");
        let failure = v4.expect_err("test");
        assert!(
            failure.message.contains("allow_from_override"),
            "{failure:?}"
        );

        let allowing = |addr| SmtpConnectorConfig {
            allow_from_override: true,
            ..smtp_config(addr)
        };
        let (v4, wire) = run(input, allowing, json!({})).await;
        twin::pinned(&masked(&v4), "from_override/allowed");
        assert!(v4.is_ok() && wire.is_some_and(|w| w.contains("spoof@example.test")));
    }

    #[tokio::test]
    async fn message_shape_errors_are_named() {
        for (input, expected) in [
            (
                json!({"connector": "mailer", "to": "a@b.test", "subject": "s"}),
                "'text' or 'html'",
            ),
            (
                json!({"connector": "mailer", "to": ["ok@b.test", "not an address"],
                       "subject": "s", "text": "b"}),
                "to[1]",
            ),
            (
                json!({"connector": "mailer", "to": "a@b.test", "subject": "s",
                       "text": "b", "headers": {"Subject": "override"}}),
                "structured field",
            ),
            // The refusals the three above do not reach, each in its turn.
            (
                json!({"connector": "mailer", "to": null, "subject": "s", "text": "b"}),
                "requires 'to'",
            ),
            (
                json!({"connector": "mailer", "to": [], "subject": "s", "text": "b"}),
                "requires 'to'",
            ),
            (
                json!({"connector": "mailer", "to": "a@b.test", "subject": {"var": "data.none"},
                       "text": "b"}),
                "requires 'subject'",
            ),
            (
                json!({"connector": "mailer", "to": "a@b.test", "subject": 7, "text": "b"}),
                "must resolve to a string",
            ),
            (
                json!({"connector": "mailer", "to": "a@b.test", "subject": "s", "text": "b",
                       "reply_to": "nope"}),
                "reply_to",
            ),
            (
                json!({"connector": "mailer", "to": "a@b.test", "subject": "s", "text": "b",
                       "headers": {"X-Id": {"cat": ["a\r\n", "Bcc: x@y.test"]}}}),
                "line break",
            ),
            (
                json!({"connector": "mailer", "to": "a@b.test", "subject": "s", "text": "b",
                       "headers": {"X-Id": {"var": "data.none"}}}),
                "must resolve to a string",
            ),
            (
                json!({"connector": "mailer", "to": {"var": "data.none"}, "cc": 7,
                       "subject": "s", "text": "b"}),
                "requires 'to'",
            ),
            (
                json!({"connector": "missing", "to": "a@b.test", "subject": "s", "text": "b"}),
                "Connector 'missing' not found",
            ),
        ] {
            let (v4, wire) = run(input.clone(), smtp_config, json!({})).await;
            twin::pinned(&v4, format!("message_shape/{input}"));
            let failure = v4.expect_err("refused");
            assert!(failure.message.contains(expected), "{input}: {failure:?}");
            assert!(wire.is_none(), "nothing is sent: {input}");
        }
    }

    /// Send `count` messages through one shared pool, returning the pool.
    async fn send_n(addr: std::net::SocketAddr, input: Value, count: usize) -> Arc<SmtpPoolCache> {
        let registry = mailer(smtp_config(addr)).await;
        let smtp_pool = std::sync::Arc::new(SmtpPoolCache::new(4));
        for _ in 0..count {
            let f = SendEmailFunction {
                registry: registry.clone(),
                smtp_pool: smtp_pool.clone(),
            };
            twin::v4(NAME, f, input.clone(), json!({}))
                .await
                .expect("sent");
        }
        smtp_pool
    }

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
        drop(pool);

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
        assert!(
            log.transcript.contains("RSET"),
            "reuse must probe with RSET: {}",
            log.transcript
        );
    }

    /// A computed `output` is resolved before the mail is read, as 1.12's
    /// prologue resolved it: one that names no path is 1.12's
    /// `VALIDATION_ERROR`, with its text, and no SMTP session is opened; one
    /// that resolves to `null` is the default `data`, and one that names a
    /// path is written there.
    #[tokio::test]
    async fn a_computed_output_is_resolved_before_the_send_as_1_12_did() {
        let data = json!({"n": 3, "flag": true, "empty": "", "at": "data.mail"});
        for (output, sent) in [
            (json!({"var": "data.n"}), false),
            (json!({"var": "data.flag"}), false),
            (json!({"var": "data.empty"}), false),
            (json!({"var": "data.none"}), true),
            (json!({"var": "data.at"}), true),
        ] {
            let input = json!({"connector": "mailer", "to": "a@b.test", "subject": "s",
                               "text": "b", "output": output});
            let (addr, log) = spawn_logging_smtp_mock().await;
            let registry = mailer(smtp_config(addr)).await;
            let f = SendEmailFunction {
                registry,
                smtp_pool: Arc::new(SmtpPoolCache::new(4)),
            };
            let seen = masked(&twin::v4(NAME, f, input.clone(), data.clone()).await);
            let connections = log.lock().expect("test").connections;
            twin::pinned(&seen, format!("computed_output/{output}"));
            assert_eq!(connections, usize::from(sent), "{output}");
            if !sent {
                let failure = seen.as_ref().expect_err("refused");
                assert_eq!(failure.code, "VALIDATION_ERROR", "{output}");
                assert!(
                    failure.message.starts_with(
                        "Validation error: 'output' must resolve to a non-empty dotted path"
                    ),
                    "{failure:?}"
                );
            }
        }
    }

    /// Extra headers reach the message in the order written, and of several
    /// bad ones the first written is the one refused, literal or computed, as
    /// 1.12's.
    #[tokio::test]
    async fn the_headers_keep_the_order_written() {
        let input = json!({"connector": "mailer", "to": "a@b.test", "subject": "s", "text": "b",
                           "headers": {"X-B": "2", "X-A": "1", "X-C": {"var": "data.c"}}});
        let (v4, wire) = run(input, smtp_config, json!({"c": "3"})).await;
        twin::pinned(&masked(&v4), "headers_keep_the_order_written");
        let at = |wire: &str, header: &str| wire.find(header).expect(header);
        let wire = wire.expect("sent");
        assert!(
            at(&wire, "X-B: 2") < at(&wire, "X-A: 1") && at(&wire, "X-A: 1") < at(&wire, "X-C: 3"),
            "{wire}"
        );

        for headers in [
            json!({"X-Evil": "a\nb", "Subject": "s"}),
            json!({"X-2": {"var": "data.none"}, "X-1": {"var": "data.crlf"}}),
            json!({"Z-Bad Name": "v", "A-Bad Name": "v"}),
        ] {
            let input = json!({"connector": "mailer", "to": "a@b.test", "subject": "s",
                               "text": "b", "headers": headers});
            let (v4, wire) = run(input, smtp_config, json!({"crlf": "a\r\nb"})).await;
            twin::pinned(&v4, format!("bad_headers/{headers}"));
            assert!(v4.is_err() && wire.is_none(), "{headers}");
        }
    }
}

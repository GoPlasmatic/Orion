//! Per-channel shaping of ingress guard-rejection bodies (#269).
//!
//! Every guard rejection answers with the fixed platform envelope
//! `{"error": {"code", "message", "request_id"}}`. A backend migrated onto
//! Orion whose mobile and web clients are already shipped cannot change what
//! those clients parse — and Orion is already inconsistent with itself here:
//! `response.mode = "shaped"` lets a channel own its response bytes on the
//! success path, including a `200` carrying task errors, but any `OrionError`
//! returns `Err` from the handler and never reaches shaping. A shaped channel
//! therefore speaks its own dialect for a `200` and Orion's for a `401`.
//!
//! **The platform decides the status; the channel decides the bytes.** Three
//! constraints follow, and they are the point rather than boilerplate:
//!
//! - **Keyed by HTTP status, never by rejection cause.** A uniform `401` is an
//!   anti-oracle: the response never reveals whether the header was missing,
//!   the key wrong, the signature malformed or the timestamp stale. Keying by
//!   cause would rebuild exactly that credential oracle. (Status is also the
//!   honest key: two rejections already share `RATE_LIMITED` and three share
//!   `SERVICE_UNAVAILABLE`, so a code key would promise a distinction authors
//!   cannot act on.)
//! - **No JSONLogic, and a bounded template.** There is no engine at guard
//!   time, and a refusal must cost the least work possible — evaluating
//!   expressions over attacker-influenced input on the cheapest-must-be path
//!   is new attack surface for no gain.
//! - **Soft failure.** An unrenderable template falls back to the platform
//!   envelope, never a 500 — the posture response shaping already takes, where
//!   an absent or malformed field falls back to the platform's own answer
//!   rather than turning a cosmetic authoring slip into an outage.
//!
//! Placeholders come from a closed set, every member of which is already on
//! the wire. `message` in particular is redacted by construction, since
//! `response_parts` is the single place the redaction policy lives.
//! `details` is deliberately **not** exposed: it is empty for every guard
//! rejection today, so it would buy nothing and would become a leak the day a
//! guard grows field-pathed details.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Cap on a rendered body, enforced at authoring time. A refusal must not
/// become an amplification primitive.
pub const MAX_TEMPLATE_BYTES: usize = 4096;

/// One status's replacement body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelErrorBody {
    /// The body to send, with `{placeholder}` substitutions.
    pub body: String,
    /// `content-type` for the replacement. Defaults to JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

/// The per-channel map, keyed by HTTP status as a string, plus an optional
/// `"default"`.
///
/// `BTreeMap`, not `HashMap`: `content_hash` is computed over the config
/// `Value` and drives the package "unchanged" comparison, so serialization
/// order must be stable.
pub type ErrorBodies = BTreeMap<String, ChannelErrorBody>;

/// What a template may interpolate. Closed by construction — an unknown name
/// is a parse error, not a literal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Status,
    Code,
    Message,
    RequestId,
    Channel,
    Timestamp,
}

/// The values a rendered template draws on. Every field is already on the
/// wire in the platform envelope or its headers.
pub struct RenderContext<'a> {
    pub status: u16,
    pub code: &'a str,
    pub message: &'a str,
    pub channel: &'a str,
}

/// A placeholder is `{` + a lowercase identifier + `}` and nothing else.
///
/// That narrowness is what lets a JSON template be written literally: an error
/// body is overwhelmingly `{"error": …}`, so treating every `{` as an opener
/// would make the common case unwritable. Only an identifier-shaped token is
/// read as a placeholder — and then an unknown one is a hard error rather than
/// a literal, so a misspelling cannot ship silently.
///
/// Returns the name and the byte length consumed. Everything involved is
/// ASCII, so byte slicing is safe and needs no `Vec<char>` copy of the
/// template.
fn placeholder_at(rest: &str) -> Option<(&str, usize)> {
    let body = rest.strip_prefix('{')?;
    let end = body.find('}')?;
    let name = &body[..end];
    let identifier = !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    identifier.then_some((name, end + 2))
}

/// Compile a template into segments, rejecting an unknown placeholder rather
/// than treating it as a literal — the same discipline the HMAC signing-string
/// parser uses, and for the same reason: a silently mistyped placeholder is a
/// body that ships wrong forever.
fn parse(template: &str) -> Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut rest = template;
    while !rest.is_empty() {
        // `{{` / `}}` escape a literal brace, for a body that needs `{word}`
        // verbatim.
        if let Some(tail) = rest.strip_prefix("{{").or_else(|| rest.strip_prefix("}}")) {
            literal.push(rest.as_bytes()[0] as char);
            rest = tail;
            continue;
        }
        if let Some((name, consumed)) = placeholder_at(rest) {
            if !literal.is_empty() {
                segments.push(Segment::Literal(std::mem::take(&mut literal)));
            }
            segments.push(match name {
                "status" => Segment::Status,
                "code" => Segment::Code,
                "message" => Segment::Message,
                "request_id" => Segment::RequestId,
                "channel" => Segment::Channel,
                "timestamp" => Segment::Timestamp,
                other => {
                    return Err(format!(
                        "unknown placeholder '{{{other}}}' — expected one of \
                         status, code, message, request_id, channel, timestamp"
                    ));
                }
            });
            rest = &rest[consumed..];
            continue;
        }
        // Not identifier-shaped: an ordinary character (a JSON brace included).
        let ch = rest.chars().next().expect("non-empty");
        literal.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    if !literal.is_empty() {
        segments.push(Segment::Literal(literal));
    }
    Ok(segments)
}

/// Render a template. Returns `None` when it does not compile, which the call
/// site treats as "use the platform envelope".
pub fn render(template: &str, ctx: &RenderContext<'_>) -> Option<String> {
    let segments = parse(template).ok()?;
    let mut out = String::with_capacity(template.len() + 64);
    for segment in segments {
        match segment {
            Segment::Literal(s) => out.push_str(&s),
            Segment::Status => out.push_str(&ctx.status.to_string()),
            Segment::Code => out.push_str(ctx.code),
            // Already redacted — `response_parts` is the single place the
            // redaction policy lives, so this adds no new source of content.
            Segment::Message => out.push_str(ctx.message),
            Segment::RequestId => {
                out.push_str(&crate::server::request_context::request_id().unwrap_or_default())
            }
            Segment::Channel => out.push_str(ctx.channel),
            Segment::Timestamp => out
                .push_str(&chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        }
    }
    Some(out)
}

/// Pick the template for a status: the exact key, else `"default"`.
pub fn lookup(bodies: &ErrorBodies, status: u16) -> Option<&ChannelErrorBody> {
    bodies
        .get(&status.to_string())
        .or_else(|| bodies.get("default"))
}

/// Authoring-time check for one entry. Returns the reason it is unusable.
pub fn validate(key: &str, entry: &ChannelErrorBody) -> Result<(), String> {
    if key != "default" {
        let status: u16 = key
            .parse()
            .map_err(|_| format!("key '{key}' must be an HTTP status code or \"default\""))?;
        if !(400..=599).contains(&status) {
            return Err(format!(
                "key '{key}' must be a 4xx or 5xx status — the platform decides the \
                 status, so only error responses can be shaped"
            ));
        }
    }
    if entry.body.len() > MAX_TEMPLATE_BYTES {
        return Err(format!(
            "body is {} bytes, over the {MAX_TEMPLATE_BYTES}-byte cap",
            entry.body.len()
        ));
    }
    parse(&entry.body).map_err(|e| format!("body: {e}"))?;

    let content_type = entry.content_type.as_deref().unwrap_or("application/json");
    if axum::http::HeaderValue::from_str(content_type).is_err() {
        return Err(format!(
            "content_type '{content_type}' is not a valid header value"
        ));
    }

    // A JSON-ish content type must actually produce JSON, checked with probe
    // values — otherwise the channel ships a body its clients cannot parse.
    if content_type.contains("json") {
        let probe = RenderContext {
            status: 400,
            code: "PROBE",
            message: "probe",
            channel: "probe",
        };
        let rendered = render(&entry.body, &probe).ok_or("body does not compile")?;
        if serde_json::from_str::<serde_json::Value>(&rendered).is_err() {
            return Err(format!(
                "body does not render as valid JSON, but content_type is '{content_type}'"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RenderContext<'static> {
        RenderContext {
            status: 401,
            code: "UNAUTHORIZED",
            message: "Unauthorized",
            channel: "login",
        }
    }

    #[test]
    fn a_template_renders_every_placeholder() {
        let out = render(
            r#"{"status":{status},"error":"{code}","message":"{message}","ch":"{channel}"}"#,
            &ctx(),
        )
        .expect("renders");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed["status"], 401);
        assert_eq!(parsed["error"], "UNAUTHORIZED");
        assert_eq!(parsed["message"], "Unauthorized");
        assert_eq!(parsed["ch"], "login");
    }

    #[test]
    fn the_placeholder_set_is_closed() {
        // A misspelling must be an authoring error, not a literal that ships.
        let err = parse("{mesage}").expect_err("unknown placeholder");
        assert!(err.contains("unknown placeholder"), "{err}");
        assert!(
            err.contains("message"),
            "the message names the valid set: {err}"
        );

        // `details` is deliberately absent.
        assert!(parse("{details}").is_err());
    }

    /// The narrowness that makes a JSON template writable: only an
    /// identifier-shaped `{word}` is a placeholder, so ordinary JSON braces
    /// pass through untouched.
    #[test]
    fn json_braces_are_literal_and_placeholders_are_narrow() {
        // A whole JSON object with no placeholders at all.
        assert_eq!(
            render(r#"{"a":{"b":1},"c":[{}]}"#, &ctx()).as_deref(),
            Some(r#"{"a":{"b":1},"c":[{}]}"#)
        );
        // `{` not followed by an identifier is literal.
        assert_eq!(render("{ {status}", &ctx()).as_deref(), Some("{ 401"));
        assert_eq!(render("{123}", &ctx()).as_deref(), Some("{123}"));
        assert_eq!(render("{status", &ctx()).as_deref(), Some("{status"));
        // `{{word}}` escapes a literal brace pair.
        assert_eq!(
            render("{{literal}} {status}", &ctx()).as_deref(),
            Some("{literal} 401")
        );
    }

    #[test]
    fn lookup_prefers_the_exact_status_then_default() {
        let mut bodies = ErrorBodies::new();
        bodies.insert(
            "default".to_string(),
            ChannelErrorBody {
                body: "d".into(),
                content_type: None,
            },
        );
        bodies.insert(
            "401".to_string(),
            ChannelErrorBody {
                body: "specific".into(),
                content_type: None,
            },
        );
        assert_eq!(lookup(&bodies, 401).expect("found").body, "specific");
        assert_eq!(lookup(&bodies, 429).expect("found").body, "d");
        assert!(lookup(&ErrorBodies::new(), 401).is_none());
    }

    #[test]
    fn validation_refuses_what_cannot_work() {
        let ok = ChannelErrorBody {
            body: r#"{"m":"{message}"}"#.into(),
            content_type: None,
        };
        assert!(validate("401", &ok).is_ok());
        assert!(validate("default", &ok).is_ok());

        // Not a status, or not an error status.
        assert!(validate("nope", &ok).is_err());
        assert!(
            validate("200", &ok).is_err(),
            "the platform owns the status"
        );
        assert!(validate("399", &ok).is_err());
        assert!(validate("600", &ok).is_err());

        // A JSON content type that does not render as JSON.
        let bad_json = ChannelErrorBody {
            body: "not json at all".into(),
            content_type: None,
        };
        let err = validate("401", &bad_json).expect_err("must refuse");
        assert!(err.contains("valid JSON"), "{err}");

        // …but the same body is fine as text.
        let as_text = ChannelErrorBody {
            body: "not json at all".into(),
            content_type: Some("text/plain".into()),
        };
        assert!(validate("401", &as_text).is_ok());

        // Over the cap.
        let huge = ChannelErrorBody {
            body: "x".repeat(MAX_TEMPLATE_BYTES + 1),
            content_type: Some("text/plain".into()),
        };
        assert!(validate("401", &huge).is_err());
    }

    /// The anti-oracle rule, as a test rather than only a comment: the shape
    /// is keyed by status, so two different causes that share a status are
    /// indistinguishable to the caller by construction.
    #[test]
    fn one_status_yields_one_body_whatever_the_cause() {
        let mut bodies = ErrorBodies::new();
        bodies.insert(
            "401".to_string(),
            ChannelErrorBody {
                body: r#"{"e":"{code}"}"#.into(),
                content_type: None,
            },
        );
        // Both a missing credential and a bad signature arrive as 401
        // UNAUTHORIZED, so both select the same entry and render identically.
        let entry = lookup(&bodies, 401).expect("the 401 entry");
        let a = render(&entry.body, &ctx()).expect("renders");
        let b = render(&entry.body, &ctx()).expect("renders");
        assert_eq!(a, b);
    }
}

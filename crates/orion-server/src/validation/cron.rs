//! What a cron channel may and may not declare.
//!
//! Two halves, and they refuse for opposite reasons.
//!
//! [`cron_transport_errors`] compiles the schedule. It is the authoring-time
//! twin of what `ChannelLoader` does at load: the same
//! [`CronTransportConfig::compile`](crate::channel::CronTransportConfig), so a
//! definition the admin API accepts is a definition that will load, and one it
//! refuses could only ever have quarantined.
//!
//! [`cron_config_errors`] refuses the *caller-shaped* guards. Every key it
//! names is a control over somebody making a request — who they are, where they
//! came from, how often they may ask, what to send back. A schedule has no
//! requester, so each of those would be a setting an author wrote and Orion
//! silently never applied. The alternative to refusing them is a channel whose
//! stored `auth` block reads like protection and protects nothing, which is the
//! same failure mode `deny_unknown_fields` exists to prevent one level up.
//!
//! The refused set is exactly the `false` column of
//! [`Transport::Cron`](crate::channel::guards::Transport)'s guard row, and
//! `cron_guards_and_validation_agree` in `channel/guards/mod.rs` keeps the two
//! from drifting: a guard switched on here without being switched on there
//! would be a config Orion accepts and ignores.

use serde_json::Value;

use crate::channel::{CronIdentity, CronTransportConfig};
use crate::errors::FieldError;

/// Per-channel `config` keys a cron channel may not set, and what to do
/// instead.
///
/// `rate_limit` is on this list and the design did not put it there. A rate
/// limit answers "how often may this caller ask?", and the schedule already
/// answers "how often does this run?" — a cron channel that is both scheduled
/// every minute and limited to one request a minute has said the same thing
/// twice, and one of the two silently wins. Use the schedule.
const REFUSED_CONFIG_KEYS: &[(&str, &str)] = &[
    (
        "auth",
        "a scheduled run has no caller to authenticate; the schedule is the only \
         thing that can start this channel",
    ),
    (
        "origin_allow_list",
        "the allow-list checks an HTTP `Origin` header, and a scheduled run sends \
         no request",
    ),
    (
        "rate_limit",
        "the schedule already decides how often this runs — set the cron \
         expression instead",
    ),
    (
        "deduplication",
        "deduplication suppresses a redelivered ingress event; scheduled work is \
         made unique by its occurrence identity, and overlap is controlled with \
         transport_config.concurrency",
    ),
    (
        "cache",
        "there is no response to cache: a scheduled run answers nobody",
    ),
    (
        "request",
        "`request` shapes an HTTP body into `data`, and a scheduled run's data is \
         transport_config.payload",
    ),
    (
        "response",
        "`response` shapes an HTTP reply, and a scheduled run has no reply to shape",
    ),
    (
        "oauth2_login",
        "both legs of the sign-in flow are browser redirects, which a schedule \
         cannot carry",
    ),
];

/// Fields that belong to another protocol's routing and must be absent here.
///
/// A cron channel that carried a `route_pattern` would look routable in every
/// listing and export, and be reachable at none of them.
const REFUSED_ROUTING_FIELDS: &[(&str, &str)] = &[
    ("channel.methods", "methods"),
    ("channel.route_pattern", "route_pattern"),
    ("channel.topic", "topic"),
    ("channel.consumer_group", "consumer_group"),
];

/// The routing fields a cron channel must leave unset, plus the `channel_type`
/// rule.
pub(super) fn cron_routing_errors(
    channel_type: Option<&str>,
    methods: Option<&[String]>,
    route_pattern: Option<&str>,
    topic: Option<&str>,
    consumer_group: Option<&str>,
) -> Vec<FieldError> {
    let present: [bool; 4] = [
        methods.is_some_and(|m| !m.is_empty()),
        route_pattern.is_some_and(|r| !r.trim().is_empty()),
        topic.is_some_and(|t| !t.trim().is_empty()),
        consumer_group.is_some_and(|c| !c.trim().is_empty()),
    ];
    let mut out: Vec<FieldError> = REFUSED_ROUTING_FIELDS
        .iter()
        .zip(present)
        .filter(|(_, present)| *present)
        .map(|((path, name), _)| {
            FieldError::new(
                *path,
                orion_api::error::field_codes::INVALID,
                format!(
                    "a cron channel must not declare {name}: it is started by its \
                     schedule and registers no HTTP route and no Kafka subscription"
                ),
            )
        })
        .collect();

    // Not a style rule. A cron channel answers nobody, so `sync` would promise
    // a caller a result there is no caller to receive — and `channel_type` is
    // read by the route table, the Kafka topic merge and the trace policy, each
    // of which would draw a different conclusion from it.
    if let Some(channel_type) = channel_type
        && !channel_type.eq_ignore_ascii_case(crate::storage::models::CHANNEL_TYPE_ASYNC)
    {
        out.push(FieldError::new(
            "channel.channel_type",
            orion_api::error::field_codes::INVALID,
            format!(
                "a cron channel must be channel_type \"async\" (got \"{channel_type}\"): \
                 nothing waits for a scheduled run, and its result is read from its \
                 trace and its occurrence"
            ),
        ));
    }
    out
}

/// Compile the `transport_config`, reporting every problem.
pub(super) fn cron_transport_errors(
    transport_config: &Value,
    channel_id: Option<&str>,
) -> Vec<FieldError> {
    // The stored identity is not known on every path (a create request may let
    // the server mint the id), and none of the compiled *checks* depend on it —
    // it only supplies the default singleton key. A placeholder keeps the
    // validation total.
    let identity = CronIdentity {
        channel_id: channel_id.unwrap_or("channel").to_string(),
        channel_name: String::new(),
        version: 0,
        workflow_id: None,
    };
    let parsed: CronTransportConfig = match serde_json::from_value(transport_config.clone()) {
        Ok(parsed) => parsed,
        Err(e) => {
            // `deny_unknown_fields` lands here, and it is the case worth
            // naming: a key this struct does not know is a scheduling decision
            // that would never have been applied.
            let message = e.to_string();
            let code = if message.contains("unknown field") {
                orion_api::error::field_codes::UNKNOWN_FIELD
            } else {
                orion_api::error::field_codes::INVALID
            };
            return vec![FieldError::new(
                "channel.transport_config",
                code,
                format!("cron transport_config does not parse: {message}"),
            )];
        }
    };

    let mut errors = parsed.compile(identity).err().unwrap_or_default();
    if let Some(payload) = parsed.payload.as_ref() {
        errors.extend(payload_secret_errors(payload));
    }
    errors
}

/// The per-channel `config` keys a cron channel may not set.
pub(super) fn cron_config_errors(config: &Value) -> Vec<FieldError> {
    let Some(map) = config.as_object() else {
        return Vec::new();
    };
    REFUSED_CONFIG_KEYS
        .iter()
        .filter(|(key, _)| map.get(*key).is_some_and(|v| !v.is_null()))
        .map(|(key, why)| {
            FieldError::new(
                format!("channel.config.{key}"),
                orion_api::error::field_codes::INVALID,
                format!("a cron channel must not set {key}: {why}"),
            )
        })
        .collect()
}

/// Reference schemes that resolve *somewhere else* in Orion and resolve nowhere
/// here.
///
/// A connector config resolves `env://` and `vault://` at load; a channel's
/// `config` resolves `var://`. `transport_config.payload` resolves nothing — it
/// is handed to the workflow as `data` verbatim — so one of these strings would
/// travel to whatever the workflow sends `data` to, as the literal text
/// `env://STRIPE_KEY`.
const UNRESOLVED_SCHEMES: [&str; 4] = ["env://", "vault://", "secret://", "var://"];

/// Refuse key material in the authored payload.
///
/// The payload is recorded verbatim as every occurrence's trace `input_json`,
/// so a secret placed here is a secret at rest in the traces table, readable
/// through the admin trace API by anyone who can read traces. Workflows reach
/// key material through the engine's secret store, which exists precisely so it
/// is never recorded.
fn payload_secret_errors(payload: &Value) -> Vec<FieldError> {
    let mut out = Vec::new();
    walk_payload(payload, "channel.transport_config.payload", &mut out);
    out
}

fn walk_payload(value: &Value, path: &str, out: &mut Vec<FieldError>) {
    // The engine's own predicate, so this recognises exactly the shape a
    // `secret` operator would have resolved — the one-element array spelling
    // included.
    if let Some(name) = crate::engine::functions::secret_ref::secret_name(value) {
        out.push(FieldError::new(
            path.to_string(),
            orion_api::error::field_codes::UNRESOLVED_SECRET_REF,
            format!(
                "the authored payload must not carry {{\"secret\": \"{name}\"}}: it is \
                 stored in the channel definition and recorded in every occurrence's \
                 trace input. Read the secret inside the workflow instead, where the \
                 engine resolves it without recording it."
            ),
        ));
        return;
    }
    match value {
        Value::String(s) => {
            if let Some(scheme) = UNRESOLVED_SCHEMES
                .iter()
                .find(|scheme| s.starts_with(**scheme))
            {
                out.push(FieldError::new(
                    path.to_string(),
                    orion_api::error::field_codes::UNRESOLVED_SECRET_REF,
                    format!(
                        "\"{scheme}\" references are not resolved in a cron payload — the \
                         value reaches the workflow as the literal string. Use \
                         metadata.vars for deployment values, or the engine secret store \
                         for credentials."
                    ),
                ));
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk_payload(item, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                walk_payload(item, &format!("{path}.{key}"), out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(errors: &[FieldError]) -> Vec<&str> {
        errors.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn routing_fields_of_other_protocols_are_refused() {
        let errors = cron_routing_errors(
            Some("async"),
            Some(&["POST".to_string()]),
            Some("/nightly"),
            Some("orders"),
            Some("group-a"),
        );
        assert_eq!(
            paths(&errors),
            vec![
                "channel.methods",
                "channel.route_pattern",
                "channel.topic",
                "channel.consumer_group",
            ]
        );
    }

    #[test]
    fn an_empty_routing_field_is_not_a_declaration() {
        assert!(
            cron_routing_errors(Some("async"), Some(&[]), Some("  "), None, Some("")).is_empty()
        );
    }

    #[test]
    fn a_sync_cron_channel_is_refused() {
        let errors = cron_routing_errors(Some("sync"), None, None, None, None);
        assert_eq!(paths(&errors), vec!["channel.channel_type"]);
        assert!(cron_routing_errors(Some("ASYNC"), None, None, None, None).is_empty());
    }

    #[test]
    fn every_caller_shaped_guard_is_refused() {
        let config = json!({
            "auth": {"mode": "api_key"},
            "origin_allow_list": ["https://example.com"],
            "rate_limit": {"requests_per_second": 1},
            "deduplication": {"header": "idempotency-key"},
            "cache": {"ttl_secs": 60},
            "request": {"body_mode": "envelope"},
            "response": {"mode": "raw"},
            "oauth2_login": {"connector": "idp"},
        });
        let errors = cron_config_errors(&config);
        assert_eq!(errors.len(), REFUSED_CONFIG_KEYS.len(), "{errors:?}");
        assert!(errors.iter().all(|e| e.path.starts_with("channel.config.")));
    }

    /// The guards that *do* apply are not refused, and neither is anything
    /// that is not a guard at all.
    #[test]
    fn the_guards_a_schedule_can_use_are_left_alone() {
        let config = json!({
            "timeout_ms": 1_800_000,
            "validation_logic": {"!!": {"var": "data.window"}},
            "backpressure": {"max_concurrent_per_node": 1},
            "tracing": {"mode": "sync", "task_details": true},
        });
        assert!(cron_config_errors(&config).is_empty());
    }

    #[test]
    fn a_null_guard_is_not_a_declaration() {
        assert!(cron_config_errors(&json!({"auth": null})).is_empty());
    }

    #[test]
    fn an_unknown_transport_key_names_itself() {
        let errors = cron_transport_errors(
            &json!({"schedule": "0 15 2 * * *", "misfire_polcy": "skip"}),
            Some("nightly"),
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, orion_api::error::field_codes::UNKNOWN_FIELD);
        assert!(errors[0].message.contains("misfire_polcy"), "{errors:?}");
    }

    #[test]
    fn a_valid_transport_config_reports_nothing() {
        let errors = cron_transport_errors(
            &json!({
                "schedule": "0 15 2 * * *",
                "timezone": "Asia/Kolkata",
                "payload": {"window": "previous_day"},
                "misfire_policy": "latest",
                "concurrency": {"policy": "forbid", "key": "order-rollup"},
            }),
            Some("nightly"),
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn a_secret_node_in_the_payload_is_refused_wherever_it_sits() {
        for payload in [
            json!({"key": {"secret": "stripe"}}),
            json!({"nested": {"list": [{"secret": "stripe"}]}}),
        ] {
            let errors = cron_transport_errors(
                &json!({"schedule": "0 15 2 * * *", "payload": payload}),
                Some("nightly"),
            );
            assert_eq!(errors.len(), 1, "{payload}: {errors:?}");
            assert_eq!(
                errors[0].code,
                orion_api::error::field_codes::UNRESOLVED_SECRET_REF
            );
        }
    }

    #[test]
    fn an_unresolvable_reference_string_is_refused() {
        let errors = cron_transport_errors(
            &json!({
                "schedule": "0 15 2 * * *",
                "payload": {"token": "env://STRIPE_KEY"},
            }),
            Some("nightly"),
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "channel.transport_config.payload.token");
    }

    /// A member *named* `secret` holding data is data — the same rule the
    /// workflow scanner applies, so the two cannot disagree about what a secret
    /// node looks like.
    #[test]
    fn a_payload_member_that_merely_mentions_a_secret_is_data() {
        let errors = cron_transport_errors(
            &json!({
                "schedule": "0 15 2 * * *",
                "payload": {"columns": ["secret", "name"], "secret_count": 3},
            }),
            Some("nightly"),
        );
        assert!(errors.is_empty(), "{errors:?}");
    }
}

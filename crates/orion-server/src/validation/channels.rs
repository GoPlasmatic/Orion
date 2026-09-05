use crate::errors::OrionError;
use crate::storage::models::{Channel, ChannelProtocol};
use crate::storage::repositories::channels::{CreateChannelRequest, UpdateChannelRequest};
use dataflow_rs::datalogic_rs;

use super::common::{validate_description, validate_id, validate_name};

pub fn validate_create_channel(req: &CreateChannelRequest) -> Result<(), OrionError> {
    if let Some(ref id) = req.channel_id {
        validate_id(id, "channel.channel_id")?;
    }
    validate_name(&req.name, "channel.name")?;
    if let Some(ref desc) = req.description {
        validate_description(desc, "channel.description")?;
    }
    // B1: collect all protocol-conditional missing-field errors in one
    // response (instead of failing on the first). Channel authors get
    // the full list of what to fix instead of one round-trip per issue.
    check_protocol_required_fields(&ProtocolFields {
        protocol: req.protocol,
        channel_type: Some(req.channel_type.as_str()),
        methods: req.methods.as_deref(),
        route_pattern: req.route_pattern.as_deref(),
        topic: req.topic.as_deref(),
        consumer_group: req.consumer_group.as_deref(),
    })?;
    // `transport_config` is a free-form blob with no strict parse of its own,
    // so an uncompiled `$from` in it would be stored verbatim and read as a
    // transport setting at reload. `config` gets the same check inside
    // `validate_channel_config_blob`, which the update path shares.
    let transport =
        super::common::uncompiled_source_errors(&req.transport_config, "channel.transport_config");
    if !transport.is_empty() {
        return Err(uncompiled_channel(transport));
    }
    // B2: strict-validate the per-channel `config` blob at create time, so
    // authors learn at the CRUD boundary with field-pathed errors, not at
    // first request. The registry applies the same strict parse at reload —
    // a stored config that no longer parses (unknown keys included, per
    // N24/N25) quarantines the channel rather than serving it with a guard
    // silently absent; `orion-server preflight` names affected channels
    // before an upgrade.
    validate_channel_config_blob(&req.config)?;
    validate_oauth2_login_routing(&req.config, req.protocol, req.route_pattern.as_deref())?;
    check_cron_channel(
        req.protocol,
        &req.transport_config,
        &req.config,
        req.channel_id.as_deref(),
    )?;
    Ok(())
}

/// The cron half of channel validation: the compiled schedule and the guards a
/// schedule cannot use.
///
/// Runs on create and on update alike, against the *effective* documents, so an
/// update that replaces only `transport_config` is checked against the config
/// it will actually serve with.
fn check_cron_channel(
    protocol: ChannelProtocol,
    transport_config: &serde_json::Value,
    config: &serde_json::Value,
    channel_id: Option<&str>,
) -> Result<(), OrionError> {
    if protocol != ChannelProtocol::Cron {
        return Ok(());
    }
    let mut details = super::cron::cron_transport_errors(transport_config, channel_id);
    details.extend(super::cron::cron_config_errors(config));
    if details.is_empty() {
        return Ok(());
    }
    Err(OrionError::Validation {
        code: "VALIDATION_ERROR",
        message: format!(
            "Cron channel has {} problem(s): a schedule that does not compile would \
             be quarantined at load rather than serving",
            details.len()
        ),
        details,
    })
}

/// R3: mirror the create-time checks on `PUT /channels/{id}`, which
/// previously ran no validation at all. `UpdateChannelRequest` carries no
/// protocol (it is immutable across versions), so the protocol-conditional
/// checks run against the merged (stored draft ⊕ request) view: a field
/// omitted from the request keeps its stored value, while an explicit `""`
/// or `[]` counts as emptying it and is rejected when the stored protocol
/// requires it.
pub fn validate_update_channel(
    stored: &Channel,
    req: &UpdateChannelRequest,
) -> Result<(), OrionError> {
    if let Some(ref name) = req.name {
        validate_name(name, "channel.name")?;
    }
    if let Some(ref desc) = req.description {
        validate_description(desc, "channel.description")?;
    }
    // A stored row whose `methods_json` fails to parse contributes no methods
    // (`Channel::methods` owns that rule), so the request must supply them.
    let stored_methods = stored.methods();
    // A stored protocol outside the known set (corrupt row) skips the
    // protocol-conditional checks rather than blocking unrelated updates.
    let stored_protocol: Option<ChannelProtocol> =
        serde_json::from_value(serde_json::Value::String(stored.protocol.clone())).ok();
    if let Some(protocol) = stored_protocol {
        check_protocol_required_fields(&ProtocolFields {
            protocol,
            channel_type: Some(stored.channel_type.as_str()),
            methods: req.methods.as_deref().or(stored_methods.as_deref()),
            route_pattern: req
                .route_pattern
                .as_deref()
                .or(stored.route_pattern.as_deref()),
            topic: req.topic.as_deref().or(stored.topic.as_deref()),
            consumer_group: req
                .consumer_group
                .as_deref()
                .or(stored.consumer_group.as_deref()),
        })?;
    }
    if let Some(ref config) = req.config {
        validate_channel_config_blob(config)?;
    }
    // Against the merged view, the way `check_protocol_required_fields` above
    // already is — *not* nested inside `req.config`. Nested, a request carrying
    // only `{"route_pattern": "…"}` skipped this entirely and could set the
    // channel's route equal to its own stored `oauth2_login.callback_path`,
    // which is the exact collision the check exists to refuse.
    //
    // Nothing downstream catches that: `ensure_route_is_unclaimed` compares a
    // draft against *other* channels and never a channel's two routes against
    // each other, so activation succeeds and the `RouteTable` gets two entries
    // for one channel on one path. Whichever wins decides the leg, and sign-ins
    // either loop or complete against the wrong one.
    if let Some(protocol) = stored_protocol {
        // A stored blob that will not parse contributes no `oauth2_login`, so
        // the check no-ops rather than blocking an unrelated update — the same
        // rule `Channel::methods` applies to a corrupt `methods_json`.
        let stored_config = serde_json::from_str::<serde_json::Value>(&stored.config_json).ok();
        if let Some(config) = req.config.as_ref().or(stored_config.as_ref()) {
            validate_oauth2_login_routing(
                config,
                protocol,
                req.route_pattern
                    .as_deref()
                    .or(stored.route_pattern.as_deref()),
            )?;
        }
    }
    if let Some(protocol) = stored_protocol {
        // The merged view, for the same reason the routing checks above use
        // one: a request replacing only `transport_config` still has to be
        // checked against the `config` it will serve with, and vice versa. A
        // stored blob that no longer parses contributes nothing rather than
        // blocking an unrelated update.
        let stored_transport =
            serde_json::from_str::<serde_json::Value>(&stored.transport_config_json).ok();
        let stored_config = serde_json::from_str::<serde_json::Value>(&stored.config_json).ok();
        let empty = serde_json::Value::Object(serde_json::Map::new());
        check_cron_channel(
            protocol,
            req.transport_config
                .as_ref()
                .or(stored_transport.as_ref())
                .unwrap_or(&empty),
            req.config
                .as_ref()
                .or(stored_config.as_ref())
                .unwrap_or(&empty),
            Some(stored.channel_id.as_str()),
        )?;
    }
    Ok(())
}

/// The two things an `oauth2_login` block needs from the channel *around* it
/// (#307), which [`validate_channel_config_blob`] cannot see because it holds
/// the config alone.
///
/// Both legs of the flow are routes, so a channel with no route has no flow:
/// reachable only by name, it would answer a redirect at
/// `/api/v1/data/{name}` and have nowhere for the identity provider to send
/// the browser back to.
fn validate_oauth2_login_routing(
    config: &serde_json::Value,
    protocol: ChannelProtocol,
    route_pattern: Option<&str>,
) -> Result<(), OrionError> {
    let Some(login) = config.get("oauth2_login").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    if protocol != ChannelProtocol::Rest {
        return Err(OrionError::invalid_field(
            "channel.config.oauth2_login",
            "REQUIRED_FOR_PROTOCOL",
            format!(
                "oauth2_login needs protocol 'rest' — both legs are browser redirects to \
                 registered paths, and '{}' registers none",
                protocol.as_str()
            ),
        ));
    }
    let Some(pattern) = route_pattern.map(str::trim).filter(|p| !p.is_empty()) else {
        return Err(OrionError::invalid_field(
            "channel.route_pattern",
            "REQUIRED_FOR_PROTOCOL",
            "a channel with oauth2_login must declare a route_pattern: it is the \
             authorize leg, the path a user is sent to in order to begin signing in",
        ));
    };
    // The two legs are two routes on one channel; the same path for both would
    // make the callback shadow the authorize leg (or the reverse, by priority),
    // and a sign-in would loop.
    if let Some(callback) = login
        .get("callback_path")
        .and_then(serde_json::Value::as_str)
        && crate::channel::routing::canonical_route(callback)
            == crate::channel::routing::canonical_route(pattern)
    {
        return Err(OrionError::invalid_field(
            "channel.config.oauth2_login.callback_path",
            "INVALID",
            format!(
                "callback_path '{callback}' is the channel's own route_pattern \
                 '{pattern}'. They are two different requests — one begins a sign-in, \
                 the other completes it — and must be two different paths"
            ),
        ));
    }
    Ok(())
}

/// The protocol-conditional fields of a channel, as seen by validation —
/// the request itself at create time, the merged stored ⊕ request view at
/// update time.
struct ProtocolFields<'a> {
    protocol: ChannelProtocol,
    /// `None` where the caller has no view of it — the update path, whose
    /// request cannot change it. Only the cron arm reads it.
    channel_type: Option<&'a str>,
    methods: Option<&'a [String]>,
    route_pattern: Option<&'a str>,
    topic: Option<&'a str>,
    consumer_group: Option<&'a str>,
}

/// D29: cap the fields MySQL stores as `varchar(255)` at the validation
/// boundary, so a value cannot store on SQLite/Postgres and fail on MySQL.
fn check_varchar_len(path: &'static str, value: &str) -> Option<crate::errors::FieldError> {
    let len = value.chars().count();
    (len > super::common::MAX_VARCHAR_FIELD_LEN).then(|| {
        crate::errors::FieldError::new(
            path,
            "TOO_LONG",
            format!(
                "{path} must be at most {} characters — MySQL stores this column as \
                 varchar(255), so a longer value cannot load on every supported backend \
                 (got {len})",
                super::common::MAX_VARCHAR_FIELD_LEN,
            ),
        )
    })
}

/// The HTTP methods a channel may declare.
///
/// R6: `methods` was checked for non-emptiness and nothing else, so
/// `["POTS"]` was created, activated and reloaded — and simply never matched,
/// because `RouteTable::match_route` compares against the request's real verb.
/// Nothing reported it. This is the set the data plane can actually route.
pub const VALID_HTTP_METHODS: &[&str] =
    &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// Structural check on a channel's declared HTTP methods (R6).
fn check_http_methods(methods: &[String]) -> Vec<crate::errors::FieldError> {
    use crate::errors::FieldError;
    let mut out = Vec::new();
    let mut seen: Vec<String> = Vec::with_capacity(methods.len());
    for method in methods {
        let upper = method.trim().to_ascii_uppercase();
        if !VALID_HTTP_METHODS.contains(&upper.as_str()) {
            out.push(
                FieldError::new(
                    "channel.methods",
                    "INVALID",
                    format!(
                        "'{method}' is not an HTTP method this channel can be reached by — \
                         a route declaring it is never matched"
                    ),
                )
                .with_expected(serde_json::Value::String(VALID_HTTP_METHODS.join(", ")))
                .with_got(serde_json::Value::String(method.clone())),
            );
        } else if seen.contains(&upper) {
            out.push(FieldError::new(
                "channel.methods",
                "INVALID",
                format!("HTTP method '{upper}' is listed more than once"),
            ));
        } else {
            seen.push(upper);
        }
    }
    out
}

/// Structural check on a channel's route pattern (R6).
///
/// `parse_route_pattern` is deliberately total — it never fails, because it
/// runs on the request path at reload time where there is nobody to report to.
/// That meant `"orders/{id"` parsed as a *static* segment named `{id`, and
/// `"{}"` as a parameter named `""`: the channel was created, activated and
/// reloaded, and was then unreachable forever with nothing saying so. The
/// grammar is checked here instead, at the one boundary that has a caller.
fn check_route_pattern(pattern: &str) -> Vec<crate::errors::FieldError> {
    use crate::errors::FieldError;
    let err = |message: String| {
        FieldError::new("channel.route_pattern", "INVALID", message)
            .with_expected(serde_json::Value::String(
                "a path like \"/orders/{id}/items\"".to_string(),
            ))
            .with_got(serde_json::Value::String(pattern.to_string()))
    };

    if !pattern.starts_with('/') {
        return vec![err(format!(
            "route_pattern must start with '/' (got \"{pattern}\")"
        ))];
    }
    if let Some(bad) = pattern.chars().find(|c| c.is_whitespace()) {
        return vec![err(format!(
            "route_pattern must not contain whitespace (found {bad:?})"
        ))];
    }
    if let Some(bad) = pattern.chars().find(|c| matches!(c, '?' | '#')) {
        return vec![err(format!(
            "route_pattern must not contain '{bad}' — it ends the path, so \
             everything after it is a query string or fragment and is never matched"
        ))];
    }
    // N10 (symmetry): requests are matched by their percent-decoded value,
    // but pattern segments are compared as written. An escape here (`%20`)
    // could therefore only ever match a double-encoded request — write the
    // character itself instead.
    if pattern.contains('%') {
        return vec![err(
            "route_pattern must not contain '%' — patterns are written literally \
             and requests match by their decoded value, so write the character \
             itself instead of a percent-escape"
                .to_string(),
        )];
    }

    let mut out = Vec::new();
    let mut params: Vec<&str> = Vec::new();
    // `[1..]` drops the leading '/' checked above; every remaining segment must
    // be non-empty, so `//` and a trailing '/' are rejected rather than silently
    // collapsed by `parse_route_pattern`'s empty-segment filter.
    for segment in pattern[1..].split('/') {
        if segment.is_empty() {
            out.push(err(
                "route_pattern has an empty path segment — remove the doubled or \
                 trailing '/'"
                    .to_string(),
            ));
            continue;
        }
        let has_brace = segment.contains('{') || segment.contains('}');
        if !has_brace {
            continue;
        }
        let Some(name) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
            out.push(err(format!(
                "segment \"{segment}\" has unbalanced braces — a parameter is a whole \
                 segment written as \"{{name}}\""
            )));
            continue;
        };
        if name.is_empty() {
            out.push(err(
                "route_pattern has an unnamed parameter \"{}\" — a parameter must be \
                 named so the workflow can read it from `metadata.params`"
                    .to_string(),
            ));
            continue;
        }
        if name.contains('{') || name.contains('}') {
            out.push(err(format!(
                "parameter name \"{name}\" contains a brace — nested parameters are \
                 not supported"
            )));
            continue;
        }
        if !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            out.push(err(format!(
                "parameter name \"{name}\" is not a valid identifier — it becomes a key \
                 in `metadata.params`, so it must match [A-Za-z_][A-Za-z0-9_]*"
            )));
            continue;
        }
        if params.contains(&name) {
            out.push(err(format!(
                "parameter \"{name}\" appears more than once — the later capture \
                 silently overwrites the earlier one"
            )));
            continue;
        }
        params.push(name);
    }
    out
}

/// Per-protocol required-field check. Emits one `FieldError` per missing
/// field, all with `code = "REQUIRED_FOR_PROTOCOL"` so clients can
/// distinguish "this field is conditionally required" from a generic
/// missing-field error.
fn check_protocol_required_fields(fields: &ProtocolFields) -> Result<(), OrionError> {
    use crate::errors::FieldError;
    let mut out = Vec::new();
    match fields.protocol {
        ChannelProtocol::Rest | ChannelProtocol::Http => {
            match fields.methods {
                // R6: non-empty is not the same as usable.
                Some(m) if !m.is_empty() => out.extend(check_http_methods(m)),
                _ => out.push(
                    FieldError::new(
                        "channel.methods",
                        "REQUIRED_FOR_PROTOCOL",
                        format!(
                            "REST/HTTP channels must specify at least one HTTP method (protocol=\"{}\")",
                            fields.protocol
                        ),
                    )
                    .with_expected(serde_json::Value::String(
                        "non-empty array of method names".to_string(),
                    )),
                ),
            }
            match fields.route_pattern {
                Some(r) if !r.trim().is_empty() => out.extend(check_route_pattern(r)),
                _ => out.push(
                    FieldError::new(
                        "channel.route_pattern",
                        "REQUIRED_FOR_PROTOCOL",
                        format!(
                            "REST/HTTP channels must specify a route_pattern (protocol=\"{}\")",
                            fields.protocol
                        ),
                    )
                    .with_expected(serde_json::Value::String(
                        "URL path pattern (e.g. \"/orders/{id}\")".to_string(),
                    )),
                ),
            }
        }
        ChannelProtocol::Kafka => {
            if fields.topic.is_none_or(|t| t.trim().is_empty()) {
                out.push(FieldError::new(
                    "channel.topic",
                    "REQUIRED_FOR_PROTOCOL",
                    "Kafka channels must specify a topic",
                ));
            }
        }
        // A cron channel is the one protocol with nothing *required* here: its
        // trigger lives entirely in `transport_config`, which
        // `cron_transport_errors` compiles. What this arm contributes is the
        // other half — the routing fields of the protocols it is not.
        ChannelProtocol::Cron => out.extend(super::cron::cron_routing_errors(
            fields.channel_type,
            fields.methods,
            fields.route_pattern,
            fields.topic,
            fields.consumer_group,
        )),
    }
    // D29: the varchar(255) caps are deliberately outside the protocol match —
    // all three fields are optional on the request and persist whatever the
    // protocol says, so an over-long `topic` on a REST channel (or
    // `route_pattern` on a Kafka one) would still store on SQLite/Postgres
    // and fail on MySQL if only the owning protocol's arm checked it.
    for (path, value) in [
        ("channel.route_pattern", fields.route_pattern),
        ("channel.topic", fields.topic),
        ("channel.consumer_group", fields.consumer_group),
    ] {
        if let Some(e) = value.and_then(|v| check_varchar_len(path, v)) {
            out.push(e);
        }
    }
    if out.is_empty() {
        return Ok(());
    }
    // R6 added structural checks alongside the required-field ones, so
    // "missing N required field(s)" stopped being true — a malformed
    // `route_pattern` is present and wrong, not absent.
    let missing = out
        .iter()
        .filter(|e| e.code == "REQUIRED_FOR_PROTOCOL")
        .count();
    Err(OrionError::Validation {
        code: "VALIDATION_ERROR",
        message: if missing == out.len() {
            format!(
                "Channel with protocol=\"{}\" is missing {missing} required field(s)",
                fields.protocol
            )
        } else {
            format!(
                "Channel with protocol=\"{}\" has {} unusable field(s): a route that \
                 cannot match is never reached",
                fields.protocol,
                out.len()
            )
        },
        details: out,
    })
}

/// Strict-validate the channel `config` Value: parses to `ChannelConfig` to
/// catch shape errors and compiles every embedded JSONLogic expression
/// (`validation_logic`, `rate_limit.key_logic`) so typos surface here
/// rather than at engine reload (where they downgrade to warnings).
/// The channel spelling of the shared refusal — see
/// [`super::common::uncompiled_source_errors`].
fn uncompiled_channel(details: Vec<crate::errors::FieldError>) -> OrionError {
    OrionError::Validation {
        code: "VALIDATION_ERROR",
        message: "Channel has not been compiled: it still contains shared-definition \
                  references"
            .to_string(),
        details,
    }
}

fn validate_channel_config_blob(config: &serde_json::Value) -> Result<(), OrionError> {
    // Empty object is the documented default for "no config" — skip parsing.
    if let Some(obj) = config.as_object()
        && obj.is_empty()
    {
        return Ok(());
    }
    // Before the strict parse. A `$from` here does reach the author by name,
    // but as `unknown field '$from'` — a message that reads as a typo and
    // sends them looking for the right spelling of a key that is not a key at
    // all. Say what it is instead.
    let source = super::common::uncompiled_source_errors(config, "channel.config");
    if !source.is_empty() {
        return Err(uncompiled_channel(source));
    }
    // H3: channel reads mask `auth` credentials, so a config still carrying
    // the sentinel is a copied-from-a-GET mistake (on create) or a mask the
    // update path could not match to a stored value. Persisting it would make
    // "******" the literal credential.
    if let Some(path) = crate::connector::find_masked_value(config) {
        return Err(OrionError::invalid_field(
            format!("channel.config.{path}"),
            "INVALID",
            "this field is the masked placeholder the channel read API returns, \
             not a real value. Send the actual secret, or omit the field to keep \
             the stored one.",
        ));
    }
    // A `var://` field is checked against the instance that declares it, at
    // load — not here, where the value is not knowable. Same call as leaving a
    // secret reference unresolved below, and for the same reason: a bundle has
    // to validate on a host that holds neither.
    let mut shape = config.clone();
    crate::config::vars::strip_var_references(&mut shape, &|key| key.ends_with("_logic"));
    let parsed: crate::channel::ChannelConfig = serde_json::from_value(shape).map_err(|e| {
        OrionError::invalid_field(
            "channel.config",
            "INVALID",
            format!("channel.config does not match the ChannelConfig shape: {e}"),
        )
    })?;

    // #264: the structural half of auth compilation runs here, so a broken
    // auth config — missing keys/secret, bad template, unknown preset, half a
    // replay guard — is a 400 naming the problem instead of a reload-time
    // quarantine. Secret references stay unresolved: a bundle must validate
    // on hosts that hold none of the production secrets.
    if let Some(ref auth) = parsed.auth {
        crate::channel::auth::CompiledAuth::validate_config(auth)
            .map_err(|e| OrionError::invalid_field("channel.config.auth", "INVALID", e))?;
    }

    // Operator parity with the runtime guard engine (bootstrap's shared
    // datalogic instance): what compiles there must compile here.
    let dl = crate::engine::operators::add_to_datalogic(datalogic_rs::Engine::builder()).build();
    if let Some(ref logic) = parsed.validation_logic {
        dl.compile(logic).map_err(|e| {
            OrionError::invalid_field(
                "channel.config.validation_logic",
                "INVALID",
                format!("validation_logic is not a valid JSONLogic expression: {e}"),
            )
        })?;
    }
    if let Some(ref rl) = parsed.rate_limit {
        if let Some(ref logic) = rl.key_logic {
            dl.compile(logic).map_err(|e| {
                OrionError::invalid_field(
                    "channel.config.rate_limit.key_logic",
                    "INVALID",
                    format!("rate_limit.key_logic is not a valid JSONLogic expression: {e}"),
                )
            })?;
        }
        validate_rate_limit(rl)?;
    }
    if let Some(ref cache) = parsed.cache {
        validate_cache_key_fields(cache)?;
    }
    if let Some(ref request) = parsed.request {
        validate_cookies_to_metadata(request)?;
    }
    if let Some(ref response) = parsed.response
        && let Some(ref bodies) = response.error_bodies
    {
        for (key, entry) in bodies {
            crate::channel::error_body::validate(key, entry).map_err(|e| {
                OrionError::invalid_field(
                    // Field-pathed to the offending entry, so an author with
                    // several templates is told which one.
                    format!("channel.config.response.error_bodies.{key}"),
                    "INVALID",
                    e,
                )
            })?;
        }
    }

    // #307. Everything judgeable without resolving a secret, so a broken
    // sign-in block is a `400` naming the field rather than a channel that
    // activates and then quarantines on the next reload. Shared with the
    // compile path, so the two cannot come to disagree about what is valid.
    if let Some(ref login) = parsed.oauth2_login {
        crate::channel::oauth2_login::validate_shape(login)
            .map_err(|e| OrionError::invalid_field("channel.config.oauth2_login", "INVALID", e))?;

        // The response cache is keyed on the request, never on who is making
        // it. A stored authorize `302` would replay one browser's state cookie
        // to the next visitor, and a stored callback would replay one user's
        // session — so this is not a preference, it is two ways to hand a
        // session to the wrong person.
        if parsed.cache.as_ref().is_some_and(|c| c.enabled) {
            return Err(OrionError::invalid_field(
                "channel.config.cache",
                "INVALID",
                "a channel with oauth2_login cannot also enable the response cache: both \
                 of its responses are per-user (a one-time state cookie, then a session), \
                 and the cache key carries no caller identity, so a hit would serve one \
                 visitor's sign-in to the next",
            ));
        }
    }
    Ok(())
}

/// Structurally check `request.cookies_to_metadata`.
///
/// Worth checking even though `claims_to_metadata` has no structural
/// validation at all: a name carrying `;`, `=` or whitespace can never match a
/// real cookie, so a channel carrying one silently never populates. A typo
/// here is an invisibly dead feature rather than a slightly wider claim set —
/// and unlike `body_mode`, serde cannot catch it.
fn validate_cookies_to_metadata(
    request: &crate::channel::ChannelRequestConfig,
) -> Result<(), OrionError> {
    let Some(ref names) = request.cookies_to_metadata else {
        return Ok(());
    };
    if names.is_empty() {
        return Err(OrionError::invalid_field(
            "channel.config.request.cookies_to_metadata",
            "INVALID",
            "cookies_to_metadata must not be empty; omit the field to expose nothing".to_string(),
        ));
    }
    for name in names {
        if name.trim().is_empty() {
            return Err(OrionError::invalid_field(
                "channel.config.request.cookies_to_metadata",
                "INVALID",
                "cookies_to_metadata entries must not be blank".to_string(),
            ));
        }
        if name.contains(';') || name.contains('=') || name.chars().any(char::is_whitespace) {
            return Err(OrionError::invalid_field(
                "channel.config.request.cookies_to_metadata",
                "INVALID",
                format!(
                    "cookies_to_metadata entry '{name}' cannot name a real cookie — \
                     ';', '=' and whitespace are jar delimiters, so this entry would \
                     never populate"
                ),
            ));
        }
    }
    Ok(())
}

/// Structurally check the non-expression parts of `rate_limit`.
///
/// Both rules exist because the alternative is a channel that loads clean and
/// enforces something other than what it says. `requests_per_second: 0` is
/// floored to 1 by `build_keyed_limiter`, so asking for "no admissions" quietly
/// gets one per second; and a `key_headers` entry that cannot name a real
/// header can never populate the key context, leaving `key_logic` to resolve to
/// `null` and every request refused. Create/update/import, `POST
/// /channels/validate` and `package apply` all funnel through here.
fn validate_rate_limit(rl: &crate::channel::ChannelRateLimitConfig) -> Result<(), OrionError> {
    if rl.requests_per_second == 0 {
        return Err(OrionError::invalid_field(
            "channel.config.rate_limit.requests_per_second",
            "INVALID",
            "requests_per_second must be at least 1; 0 is silently treated as 1 by the \
             limiter, so it can never mean 'admit nothing'. Archive the channel or remove \
             the rate_limit block instead."
                .to_string(),
        ));
    }

    let Some(ref names) = rl.key_headers else {
        return Ok(());
    };
    if names.is_empty() {
        return Err(OrionError::invalid_field(
            "channel.config.rate_limit.key_headers",
            "INVALID",
            "key_headers must not be empty; omit the field to use the built-in header set"
                .to_string(),
        ));
    }
    let mut seen: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(OrionError::invalid_field(
                "channel.config.rate_limit.key_headers",
                "INVALID",
                "key_headers entries must not be blank".to_string(),
            ));
        }
        // `HeaderName` is the authority on what can appear on the wire: an
        // entry it rejects can never match a real header, so a channel
        // carrying one would silently never populate that key.
        if axum::http::HeaderName::from_bytes(trimmed.as_bytes()).is_err() {
            return Err(OrionError::invalid_field(
                "channel.config.rate_limit.key_headers",
                "INVALID",
                format!("key_headers entry '{name}' is not a valid HTTP header name"),
            ));
        }
        let lowered = trimmed.to_ascii_lowercase();
        if seen.contains(&lowered) {
            return Err(OrionError::invalid_field(
                "channel.config.rate_limit.key_headers",
                "INVALID",
                format!(
                    "key_headers entry '{name}' is a duplicate; header names are \
                     case-insensitive"
                ),
            ));
        }
        seen.push(lowered);
    }
    Ok(())
}

/// Structurally check `cache.cache_key_fields`.
///
/// Whether a field *resolves* is a per-request property and cannot be decided
/// here — a payload that omits every declared field is refused the cache at
/// request time instead (see `guards::compute_cache_key`). What can be decided
/// here is that the operator wrote something capable of resolving at all: an
/// empty list, a blank name, or a path with an empty segment (`a..b`, `.id`)
/// can never match any payload, so a channel carrying one would silently never
/// cache. Refusing at create is the only point that failure is visible.
fn validate_cache_key_fields(cache: &crate::channel::ChannelCacheConfig) -> Result<(), OrionError> {
    let Some(ref fields) = cache.cache_key_fields else {
        return Ok(());
    };
    const PATH: &str = "channel.config.cache.cache_key_fields";
    if fields.is_empty() {
        return Err(OrionError::invalid_field(
            PATH,
            "INVALID",
            "cache_key_fields must name at least one field; omit it entirely to key on \
             the whole payload",
        ));
    }
    for (i, f) in fields.iter().enumerate() {
        if f.trim().is_empty() {
            return Err(OrionError::invalid_field(
                format!("{PATH}[{i}]"),
                "INVALID",
                "cache_key_fields entries must not be blank",
            ));
        }
        if f.contains('.') && f.split('.').any(|s| s.is_empty()) {
            return Err(OrionError::invalid_field(
                format!("{PATH}[{i}]"),
                "INVALID",
                format!(
                    "'{f}' has an empty path segment; use a literal payload key \
                     (`user_id`) or a dotted path (`user.id`)"
                ),
            ));
        }
    }
    Ok(())
}

pub fn validate_channel_id(id: &str) -> Result<(), OrionError> {
    validate_id(id, "channel.channel_id")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::common::MAX_ID_LEN;
    use serde_json::json;

    #[test]
    fn test_valid_channel() {
        assert!(validate_channel_id("orders").is_ok());
        assert!(validate_channel_id("my-channel.v2").is_ok());
    }

    #[test]
    fn test_invalid_channel() {
        assert!(validate_channel_id("").is_err());
        assert!(validate_channel_id("   ").is_err());
        assert!(validate_channel_id("has spaces").is_err());
    }

    #[test]
    fn test_channel_too_long() {
        let long_channel = "a".repeat(MAX_ID_LEN + 1);
        assert!(validate_channel_id(&long_channel).is_err());
    }

    /// D29: the varchar cap exists because MySQL stores these three channel
    /// columns as `varchar(255)`, and `schema_parity` cannot see the link
    /// (its normaliser folds declared widths). Pin the constant and the
    /// checked-field trio to the migration, so widening a column without
    /// moving the cap — or vice versa — fails here instead of silently
    /// re-opening the stores-on-two-backends-fails-on-the-third gap.
    #[test]
    fn varchar_cap_matches_the_mysql_channels_schema() {
        let sql = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/mysql/001_initial.sql"
        ));
        let channels = sql
            .split("CREATE TABLE")
            .find(|block| block.trim_start().starts_with("IF NOT EXISTS `channels`"))
            .expect("channels table in the mysql migration");
        let expected = format!("varchar({})", super::super::common::MAX_VARCHAR_FIELD_LEN);
        for column in ["route_pattern", "topic", "consumer_group"] {
            let line = channels
                .lines()
                .find(|l| l.trim_start().starts_with(&format!("`{column}`")));
            assert!(
                line.is_some_and(|l| l.contains(&expected)),
                "mysql `{column}` is missing or no longer {expected}; move \
                 MAX_VARCHAR_FIELD_LEN (and this cap) with it: {line:?}"
            );
        }
    }

    // -- R6: route patterns and methods are structurally checked --

    /// Every one of these was created, activated and reloaded before R6, and
    /// then never matched a request, with nothing reporting it.
    #[test]
    fn malformed_route_patterns_are_rejected() {
        for (pattern, hint) in [
            ("orders/{id}", "must start with '/'"),
            ("/orders/{id", "unbalanced braces"),
            ("/orders/id}", "unbalanced braces"),
            ("/orders/{}", "unnamed parameter"),
            ("/orders/{2id}", "not a valid identifier"),
            ("/orders/{my id}", "whitespace"),
            ("/orders//items", "empty path segment"),
            ("/orders/", "empty path segment"),
            ("/orders/{id}/items/{id}", "appears more than once"),
            ("/orders?filter=x", "'?'"),
            // N10 symmetry: requests match by decoded value, so a literal
            // escape in the pattern is only reachable double-encoded.
            ("/a%20b/{id}", "'%'"),
        ] {
            let errors = check_route_pattern(pattern);
            assert!(
                !errors.is_empty(),
                "\"{pattern}\" must be rejected — it is unreachable at runtime"
            );
            assert!(
                errors.iter().any(|e| e.message.contains(hint)),
                "\"{pattern}\" should be explained with {hint:?}, got {:?}",
                errors.iter().map(|e| &e.message).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn well_formed_route_patterns_are_accepted() {
        for pattern in [
            "/orders",
            "/orders/{id}",
            "/orders/{order_id}/items/{item_id}",
            "/_internal/health",
            "/v1/a-b.c~d",
            "/{id}",
        ] {
            let errors = check_route_pattern(pattern);
            assert!(
                errors.is_empty(),
                "\"{pattern}\" must be accepted, got {:?}",
                errors.iter().map(|e| &e.message).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn unroutable_http_methods_are_rejected() {
        // The typo the entry names: created, activated, never matched.
        let errors = check_http_methods(&["POTS".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.contains("POTS"),
            "{:?}",
            errors[0].message
        );

        // Duplicates are a typo signal too.
        let errors = check_http_methods(&["GET".to_string(), "get".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.contains("more than once"),
            "{:?}",
            errors[0].message
        );

        // Case is normalised at match time, so it is accepted here.
        assert!(check_http_methods(&["get".to_string(), "POST".to_string()]).is_empty());
        for m in VALID_HTTP_METHODS {
            assert!(check_http_methods(&[(*m).to_string()]).is_empty(), "{m}");
        }
    }

    /// D29: MySQL stores these three columns as `varchar(255)`; the cap must
    /// hold at the boundary or a value stores on SQLite/Postgres and fails
    /// on MySQL. 255 exactly passes, 256 fails, and the count is characters
    /// (a multi-byte char is one).
    #[test]
    fn varchar_backed_fields_are_capped_at_255_chars() {
        let at_limit = format!("/{}", "a".repeat(254));
        let over = format!("/{}", "a".repeat(255));

        // Multi-byte safety: 255 two-byte chars is 510 bytes but 255 chars.
        assert!(check_varchar_len("channel.topic", &"é".repeat(255)).is_none());
        assert!(check_varchar_len("channel.topic", &"é".repeat(256)).is_some());

        let kafka_req = |topic: String, consumer_group: Option<String>| CreateChannelRequest {
            tags: vec![],
            channel_id: None,
            name: "kafka-ch".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Async,
            protocol: ChannelProtocol::Kafka,
            methods: None,
            route_pattern: None,
            topic: Some(topic),
            consumer_group,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        let rest_req = |route_pattern: String, topic: Option<String>| CreateChannelRequest {
            tags: vec![],
            channel_id: None,
            name: "rest-ch".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Sync,
            protocol: ChannelProtocol::Rest,
            methods: Some(vec!["POST".to_string()]),
            route_pattern: Some(route_pattern),
            topic,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(
            validate_create_channel(&rest_req(at_limit, None)).is_ok(),
            "at-limit route_pattern accepted"
        );
        assert!(
            validate_create_channel(&rest_req(over.clone(), None)).is_err(),
            "long route_pattern refused"
        );
        assert!(
            validate_create_channel(&kafka_req("t".repeat(256), None)).is_err(),
            "long topic refused"
        );
        assert!(
            validate_create_channel(&kafka_req("orders".to_string(), Some("g".repeat(256))))
                .is_err(),
            "long consumer_group refused"
        );
        assert!(
            validate_create_channel(&kafka_req("orders".to_string(), Some("g".repeat(255))))
                .is_ok(),
            "at-limit consumer_group accepted"
        );

        // The caps are protocol-independent: both fields persist whatever the
        // protocol says, so the over-long value must be refused on the
        // "wrong" protocol too — it would otherwise store on SQLite/Postgres
        // and fail on MySQL, the exact break D29 closes.
        let mut kafka_with_route = kafka_req("orders".to_string(), None);
        kafka_with_route.route_pattern = Some(over);
        assert!(
            validate_create_channel(&kafka_with_route).is_err(),
            "long route_pattern on a Kafka channel refused"
        );
        assert!(
            validate_create_channel(&rest_req("/orders".to_string(), Some("t".repeat(256))))
                .is_err(),
            "long topic on a REST channel refused"
        );
    }

    /// R6's other half: the checks run on **update** as well as create. The
    /// entry calls this out because `PUT /channels/{id}` is how a working
    /// pattern gets broken.
    #[test]
    fn an_update_that_breaks_the_route_pattern_is_rejected() {
        let stored = Channel {
            tags_json: "[]".to_string(),
            channel_id: "orders".to_string(),
            name: "orders".to_string(),
            version: 1,
            status: "draft".to_string(),
            channel_type: "sync".to_string(),
            protocol: ChannelProtocol::Rest.as_str().to_string(),
            methods_json: Some(r#"["GET"]"#.to_string()),
            workflow_id: None,
            topic: None,
            consumer_group: None,
            route_pattern: Some("/orders/{id}".to_string()),
            description: None,
            transport_config_json: "{}".to_string(),
            config_json: "{}".to_string(),
            priority: 0,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        };
        let req = UpdateChannelRequest {
            route_pattern: Some("/orders/{id".to_string()),
            ..Default::default()
        };
        assert!(validate_update_channel(&stored, &req).is_err());

        let req = UpdateChannelRequest {
            methods: Some(vec!["POTS".to_string()]),
            ..Default::default()
        };
        assert!(validate_update_channel(&stored, &req).is_err());

        // A well-formed change still goes through.
        let req = UpdateChannelRequest {
            route_pattern: Some("/orders/{order_id}".to_string()),
            methods: Some(vec!["GET".to_string(), "POST".to_string()]),
            ..Default::default()
        };
        assert!(validate_update_channel(&stored, &req).is_ok());
    }

    #[test]
    fn test_validate_create_channel_sync_valid() {
        let req = CreateChannelRequest {
            tags: vec![],
            channel_id: Some("orders-sync".to_string()),
            name: "Orders Sync".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Sync,
            protocol: ChannelProtocol::Rest,
            methods: Some(vec!["POST".to_string()]),
            route_pattern: Some("/orders".to_string()),
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req).is_ok());
    }

    #[test]
    fn test_validate_create_channel_sync_missing_methods() {
        let req = CreateChannelRequest {
            tags: vec![],
            channel_id: None,
            name: "Bad Sync".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Sync,
            protocol: ChannelProtocol::Rest,
            methods: None,
            route_pattern: Some("/orders".to_string()),
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req).is_err());
    }

    #[test]
    fn test_validate_create_channel_sync_missing_route() {
        let req = CreateChannelRequest {
            tags: vec![],
            channel_id: None,
            name: "Bad Sync".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Sync,
            protocol: ChannelProtocol::Rest,
            methods: Some(vec!["POST".to_string()]),
            route_pattern: None,
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req).is_err());
    }

    #[test]
    fn test_validate_create_channel_async_valid() {
        let req = CreateChannelRequest {
            tags: vec![],
            channel_id: None,
            name: "Orders Async".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Async,
            protocol: ChannelProtocol::Kafka,
            methods: None,
            route_pattern: None,
            topic: Some("orders-topic".to_string()),
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req).is_ok());
    }

    #[test]
    fn test_validate_create_channel_async_missing_topic() {
        let req = CreateChannelRequest {
            tags: vec![],
            channel_id: None,
            name: "Bad Async".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Async,
            protocol: ChannelProtocol::Kafka,
            methods: None,
            route_pattern: None,
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req).is_err());
    }

    #[test]
    fn test_validate_create_channel_kafka_valid() {
        let req = CreateChannelRequest {
            tags: vec![],
            channel_id: None,
            name: "Kafka Channel".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Async,
            protocol: ChannelProtocol::Kafka,
            methods: None,
            route_pattern: None,
            topic: Some("kafka-topic".to_string()),
            consumer_group: Some("my-group".to_string()),
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req).is_ok());
    }

    #[test]
    fn test_validate_channel_id() {
        assert!(validate_channel_id("my-channel-1").is_ok());
        assert!(validate_channel_id("bad id!").is_err());
    }

    // -- validate_update_channel (R3) --

    fn stored_rest_channel() -> Channel {
        Channel {
            tags_json: "[]".to_string(),
            channel_id: "orders".to_string(),
            version: 1,
            name: "Orders".to_string(),
            description: None,
            channel_type: "sync".to_string(),
            protocol: "rest".to_string(),
            methods_json: Some("[\"POST\"]".to_string()),
            route_pattern: Some("/orders".to_string()),
            topic: None,
            consumer_group: None,
            transport_config_json: "{}".to_string(),
            workflow_id: None,
            config_json: "{}".to_string(),
            status: "draft".to_string(),
            priority: 0,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        }
    }

    fn empty_update() -> UpdateChannelRequest {
        UpdateChannelRequest {
            tags: None,
            name: None,
            description: None,
            methods: None,
            route_pattern: None,
            topic: None,
            consumer_group: None,
            transport_config: None,
            workflow_id: None,
            config: None,
            priority: None,
        }
    }

    /// A complete `oauth2_login` block with the given `callback_path`. The
    /// request side of an update goes through `validate_channel_config_blob`
    /// first, so a partial block would be refused for the wrong reason.
    fn login_block(callback_path: &str) -> serde_json::Value {
        serde_json::json!({
            "oauth2_login": {
                "authorize_url": "https://idp.example.com/authorize",
                "token_url": "https://idp.example.com/token",
                "client_id": "client-123",
                "client_secret": "the-client-secret",
                "redirect_uri": "https://app.example.com/v1/auth/idp/callback",
                "state_secret": "0123456789abcdef0123456789abcdef",
                "callback_path": callback_path
            }
        })
    }

    /// A request carrying only a `route_pattern` must still be judged against
    /// the channel's *stored* `oauth2_login`. Nested inside `req.config`, this
    /// check never ran on such an update, and the route could be set equal to
    /// the stored `callback_path` — the collision it exists to refuse.
    /// `ensure_route_is_unclaimed` does not catch it either: it compares a
    /// draft against other channels, never a channel's two routes against each
    /// other.
    #[test]
    fn test_update_routing_is_checked_against_the_stored_oauth2_login() {
        let mut stored = stored_rest_channel();
        stored.config_json = login_block("/v1/auth/idp/callback").to_string();

        // The collision, arriving with no `config` of its own.
        let req = UpdateChannelRequest {
            route_pattern: Some("/v1/auth/idp/callback".to_string()),
            ..empty_update()
        };
        let err = validate_update_channel(&stored, &req).expect_err("must refuse the collision");
        assert!(
            err.to_string().contains("callback_path"),
            "the error must name the field: {err}"
        );

        // A different route on the same stored block is still fine.
        let req = UpdateChannelRequest {
            route_pattern: Some("/v1/auth/idp".to_string()),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_ok());
    }

    /// The merge runs the other way too: a request supplying the `config`
    /// keeps being judged against the stored `route_pattern`.
    #[test]
    fn test_update_routing_uses_the_request_config_over_the_stored_one() {
        let mut stored = stored_rest_channel();
        stored.route_pattern = Some("/v1/auth/idp".to_string());
        stored.config_json = login_block("/other").to_string();

        let req = UpdateChannelRequest {
            config: Some(login_block("/v1/auth/idp")),
            ..empty_update()
        };
        let err = validate_update_channel(&stored, &req).expect_err("must refuse");
        assert!(err.to_string().contains("callback_path"), "{err}");
    }

    /// A stored blob that will not parse must not block an unrelated update —
    /// the same leniency `Channel::methods` applies to a corrupt column.
    #[test]
    fn test_update_with_an_unparseable_stored_config_is_not_blocked() {
        let mut stored = stored_rest_channel();
        stored.config_json = "{not json".to_string();

        let req = UpdateChannelRequest {
            route_pattern: Some("/somewhere".to_string()),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_ok());
    }

    #[test]
    fn test_update_omitted_fields_keep_stored_values() {
        let stored = stored_rest_channel();
        let req = UpdateChannelRequest {
            name: Some("New Name".to_string()),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_ok());
    }

    #[test]
    fn test_update_emptying_route_pattern_rejected() {
        let stored = stored_rest_channel();
        let req = UpdateChannelRequest {
            route_pattern: Some("".to_string()),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_err());
    }

    #[test]
    fn test_update_emptying_methods_rejected() {
        let stored = stored_rest_channel();
        let req = UpdateChannelRequest {
            methods: Some(vec![]),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_err());
    }

    #[test]
    fn test_update_emptying_topic_rejected_for_kafka() {
        let stored = Channel {
            protocol: "kafka".to_string(),
            methods_json: None,
            route_pattern: None,
            topic: Some("orders-topic".to_string()),
            ..stored_rest_channel()
        };
        let req = UpdateChannelRequest {
            topic: Some("  ".to_string()),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_err());
        // Omitting topic keeps the stored one
        assert!(validate_update_channel(&stored, &empty_update()).is_ok());
    }

    #[test]
    fn test_update_malformed_config_rejected() {
        let stored = stored_rest_channel();
        let req = UpdateChannelRequest {
            config: Some(json!({"rate_limit": 42})),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_err());
    }

    #[test]
    fn test_update_invalid_name_rejected() {
        let stored = stored_rest_channel();
        let req = UpdateChannelRequest {
            name: Some("   ".to_string()),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_err());
    }

    #[test]
    fn test_update_replacing_protocol_fields_with_valid_values_accepted() {
        let stored = stored_rest_channel();
        let req = UpdateChannelRequest {
            methods: Some(vec!["GET".to_string(), "POST".to_string()]),
            route_pattern: Some("/orders/{id}".to_string()),
            config: Some(json!({"timeout_ms": 5000})),
            ..empty_update()
        };
        assert!(validate_update_channel(&stored, &req).is_ok());
    }
}

use crate::errors::OrionError;
use crate::extensions::{ChannelDriver, ChannelDriverRegistry};
use crate::storage::models::CreateChannelRequest;
use dataflow_rs::datalogic_rs;

use super::common::{validate_description, validate_id, validate_name};

/// Validate a channel as `drivers` — the node's composition root — serves it.
///
/// The protocol's [`ChannelDriver`] owns everything protocol-specific: which
/// fields a channel of that protocol needs, its `transport_config` (a cron
/// schedule), and the `channel_type`s and sign-in flows its capabilities
/// allow. A protocol no registered driver serves is refused by field.
pub fn validate_create_channel(
    req: &CreateChannelRequest,
    drivers: &ChannelDriverRegistry,
) -> Result<(), OrionError> {
    if let Some(ref id) = req.channel_id {
        validate_id(id, "channel.channel_id")?;
    }
    validate_name(&req.name, "channel.name")?;
    if let Some(ref desc) = req.description {
        validate_description(desc, "channel.description")?;
    }
    let driver = drivers
        .resolve(&req.protocol)
        .map_err(|unknown| OrionError::Validation {
            code: "VALIDATION_ERROR",
            message: unknown.message.clone(),
            details: vec![unknown],
        })?;
    // `transport_config` is a free-form blob with no strict parse of its own,
    // so an uncompiled `$from` in it would be stored verbatim and read as a
    // transport setting at reload — and handed to the driver's transport
    // check below as if it were one. `config` gets the same check inside
    // `validate_channel_config_blob`.
    let transport =
        super::common::uncompiled_source_errors(&req.transport_config, "channel.transport_config");
    if !transport.is_empty() {
        return Err(uncompiled_channel(transport));
    }
    // B1: every protocol-conditional problem in one response (instead of
    // failing on the first), so channel authors get the full list of what to
    // fix instead of one round-trip per issue.
    check_protocol_fields(
        drivers,
        &ProtocolFields {
            protocol: &req.protocol,
            channel_type: Some(req.channel_type.as_str()),
            methods: req.methods.as_deref(),
            route_pattern: req.route_pattern.as_deref(),
            topic: req.topic.as_deref(),
            consumer_group: req.consumer_group.as_deref(),
        },
        &req.transport_config,
        &req.config,
        req.channel_id.as_deref(),
    )?;
    // B2: strict-validate the per-channel `config` blob at create time, so
    // authors learn at the CRUD boundary with field-pathed errors, not at
    // first request. The registry applies the same strict parse at reload —
    // a stored config that no longer parses (unknown keys included, per
    // N24/N25) quarantines the channel rather than serving it with a guard
    // silently absent.
    validate_channel_config_blob(&req.config)?;
    validate_oauth2_login_routing(&req.config, driver.as_ref(), req.route_pattern.as_deref())?;
    Ok(())
}

/// The two things an `oauth2_login` block needs from the channel *around* it
/// (#307), which [`validate_channel_config_blob`] cannot see because it holds
/// the config alone.
///
/// Both legs of the flow are routes, so a channel with no route has no flow:
/// reachable only by name, it would answer a redirect at
/// `/api/v2/data/{name}` and have nowhere for the identity provider to send
/// the browser back to.
fn validate_oauth2_login_routing(
    config: &serde_json::Value,
    driver: &dyn ChannelDriver,
    route_pattern: Option<&str>,
) -> Result<(), OrionError> {
    let Some(login) = config.get("oauth2_login").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    if !driver.capabilities().supports_oauth2_login {
        return Err(OrionError::invalid_field(
            "channel.config.oauth2_login",
            "REQUIRED_FOR_PROTOCOL",
            format!(
                "oauth2_login needs a protocol whose driver serves a sign-in flow ('rest' in \
                 Community) — both legs are browser redirects to registered paths, and \
                 '{}' does not serve them",
                driver.protocol_id()
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
    // #355: a multi-provider block picks the provider from a `{provider}` route
    // segment, so the authorize route must carry one. Without it, the guard has
    // no slug to resolve and every sign-in would 404. A block is multi-provider
    // when it declares a `providers` map or opts into instance-supplied ones.
    // (The callback's own `{provider}` requirement is checked by
    // `oauth2_login::validate_shape`.)
    let multi_provider = login.get("providers").is_some_and(|v| !v.is_null())
        || login
            .get("providers_from_instance")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
    if multi_provider
        && !crate::channel::routing::route_param_names(pattern)
            .iter()
            .any(|p| p == "provider")
    {
        return Err(OrionError::invalid_field(
            "channel.route_pattern",
            "INVALID",
            format!(
                "a multi-provider oauth2_login channel selects the provider from a \
                 {{provider}} route segment, but route_pattern '{pattern}' has none — add one, \
                 e.g. /v1/auth/{{provider}}"
            ),
        ));
    }
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
    protocol: &'a str,
    /// `None` where the caller has no view of it — the update path, whose
    /// request cannot change it. Only the cron arm reads it.
    channel_type: Option<&'a str>,
    methods: Option<&'a [String]>,
    route_pattern: Option<&'a str>,
    topic: Option<&'a str>,
    consumer_group: Option<&'a str>,
}

/// Cap a channel's free-text routing fields at the validation boundary.
fn check_varchar_len(path: &'static str, value: &str) -> Option<crate::errors::FieldError> {
    let len = value.chars().count();
    (len > super::common::MAX_VARCHAR_FIELD_LEN).then(|| {
        crate::errors::FieldError::new(
            path,
            "TOO_LONG",
            format!(
                "{path} must be at most {} characters (got {len})",
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
pub(crate) fn check_http_methods(methods: &[String]) -> Vec<crate::errors::FieldError> {
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
pub(crate) fn check_route_pattern(pattern: &str) -> Vec<crate::errors::FieldError> {
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

/// The protocol's driver over the channel ([`ChannelDriverRegistry::validate_channel`]:
/// its fields, its transport, its `channel_type` against the driver's
/// capabilities), plus the column caps every protocol shares. Emits one
/// `FieldError` per problem; a conditionally required field that is missing
/// carries `code = "REQUIRED_FOR_PROTOCOL"` so clients can distinguish it
/// from a generic missing-field error.
fn check_protocol_fields(
    drivers: &ChannelDriverRegistry,
    fields: &ProtocolFields,
    transport_config: &serde_json::Value,
    config: &serde_json::Value,
    channel_id: Option<&str>,
) -> Result<(), OrionError> {
    let driver_fields = crate::extensions::ChannelProtocolFields {
        protocol: fields.protocol,
        channel_type: fields.channel_type,
        methods: fields.methods,
        route_pattern: fields.route_pattern,
        topic: fields.topic,
        consumer_group: fields.consumer_group,
    };
    let mut out = drivers.validate_channel(&driver_fields, transport_config, config, channel_id);
    // The caps are deliberately outside the driver: all three fields are
    // optional and kept whatever the protocol says, so an over-long `topic` on
    // a REST channel (or `route_pattern` on a cron one) must be refused too.
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
                "Channel with protocol=\"{}\" has {} problem(s): a route that cannot \
                 match is never reached, and a schedule that does not compile would be \
                 quarantined at load rather than serving",
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
    // to validate on a host that holds neither. The typed shape keeps a
    // reference wherever a string belongs and stands a placeholder in
    // elsewhere, so a required field holding one is present rather than
    // missing — see `parse_with_unresolved_vars`.
    let parsed: crate::channel::ChannelConfig = crate::config::vars::parse_with_unresolved_vars(
        config,
        &crate::channel::registry::is_expression_field,
    )
    .map_err(|e| {
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
        validate_rate_limit(rl, "rate_limit")?;
    }
    if let Some(ref rl) = parsed.principal_rate_limit {
        // Required, not defaulted. The address limiter falls back to the
        // caller identity when no `key_logic` is given; a principal has no
        // such fallback, and inventing one — the address — would silently
        // turn a per-user quota into a per-address one.
        let Some(ref logic) = rl.key_logic else {
            return Err(OrionError::invalid_field(
                "channel.config.principal_rate_limit.key_logic",
                "REQUIRED",
                "principal_rate_limit requires a 'key_logic' naming the claim that identifies \
                 the principal (e.g. {\"var\": \"auth.sub\"}); there is no address to fall \
                 back to"
                    .to_string(),
            ));
        };
        dl.compile(logic).map_err(|e| {
            OrionError::invalid_field(
                "channel.config.principal_rate_limit.key_logic",
                "INVALID",
                format!("principal_rate_limit.key_logic is not a valid JSONLogic expression: {e}"),
            )
        })?;
        // `jwt` is the one mode that exposes claims; every other returns
        // `None`, so the key expression would have nothing to read and every
        // request on the channel would be refused for an uncomputable key.
        // Refuse the config instead of shipping a channel that 429s everything.
        let jwt = matches!(
            parsed.auth.as_ref().map(|a| a.mode),
            Some(crate::channel::config::AuthMode::Jwt)
        );
        if !jwt {
            return Err(OrionError::invalid_field(
                "channel.config.principal_rate_limit",
                "INVALID",
                "principal_rate_limit needs a verified principal, which only \
                 auth.mode = \"jwt\" provides — the other modes expose no claims, so the \
                 key could never be computed and every request would be refused"
                    .to_string(),
            ));
        }
        validate_rate_limit(rl, "principal_rate_limit")?;
    }
    if let Some(ref cache) = parsed.cache {
        validate_cache_key_fields(cache)?;
        validate_cache_namespaces(cache)?;
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
        crate::channel::oauth2_login::validate_shape(
            login,
            crate::channel::oauth2_login::ShapeCheck::Authoring,
        )
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
fn validate_rate_limit(
    rl: &crate::channel::ChannelRateLimitConfig,
    block: &str,
) -> Result<(), OrionError> {
    if rl.requests_per_second == 0 {
        return Err(OrionError::invalid_field(
            format!("channel.config.{block}.requests_per_second"),
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
            format!("channel.config.{block}.key_headers"),
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
                format!("channel.config.{block}.key_headers"),
                "INVALID",
                "key_headers entries must not be blank".to_string(),
            ));
        }
        // `HeaderName` is the authority on what can appear on the wire: an
        // entry it rejects can never match a real header, so a channel
        // carrying one would silently never populate that key.
        if axum::http::HeaderName::from_bytes(trimmed.as_bytes()).is_err() {
            return Err(OrionError::invalid_field(
                format!("channel.config.{block}.key_headers"),
                "INVALID",
                format!("key_headers entry '{name}' is not a valid HTTP header name"),
            ));
        }
        let lowered = trimmed.to_ascii_lowercase();
        if seen.contains(&lowered) {
            return Err(OrionError::invalid_field(
                format!("channel.config.{block}.key_headers"),
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

/// Structurally check `cache.namespaces`: 1 to 8 distinct names, each one
/// `channel::cache_namespace::check_name` accepts. An empty list would declare
/// a namespaced channel that no invalidation can reach.
fn validate_cache_namespaces(cache: &crate::channel::ChannelCacheConfig) -> Result<(), OrionError> {
    use crate::channel::cache_namespace::{MAX_NAMESPACES, check_name};
    let Some(ref namespaces) = cache.namespaces else {
        return Ok(());
    };
    const PATH: &str = "channel.config.cache.namespaces";
    if namespaces.is_empty() || namespaces.len() > MAX_NAMESPACES {
        return Err(OrionError::invalid_field(
            PATH,
            "INVALID",
            format!(
                "namespaces must name 1 to {MAX_NAMESPACES} namespaces; omit it for a cache \
                 that only expires"
            ),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for (i, ns) in namespaces.iter().enumerate() {
        check_name(ns)
            .map_err(|e| OrionError::invalid_field(format!("{PATH}[{i}]"), "INVALID", e))?;
        if !seen.insert(ns.as_str()) {
            return Err(OrionError::invalid_field(
                format!("{PATH}[{i}]"),
                "INVALID",
                format!("namespace '{ns}' is declared twice"),
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

    fn community() -> ChannelDriverRegistry {
        ChannelDriverRegistry::community()
    }
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

    /// 255 exactly passes, 256 fails, and the count is characters (a
    /// multi-byte char is one).
    #[test]
    fn varchar_backed_fields_are_capped_at_255_chars() {
        let at_limit = format!("/{}", "a".repeat(254));
        let over = format!("/{}", "a".repeat(255));

        // Multi-byte safety: 255 two-byte chars is 510 bytes but 255 chars.
        assert!(check_varchar_len("channel.topic", &"é".repeat(255)).is_none());
        assert!(check_varchar_len("channel.topic", &"é".repeat(256)).is_some());

        let rest_req = |route_pattern: String, topic: Option<String>| CreateChannelRequest {
            tags: vec![],
            channel_id: None,
            name: "rest-ch".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Sync,
            protocol: "rest".to_string(),
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
            validate_create_channel(&rest_req(at_limit, None), &community()).is_ok(),
            "at-limit route_pattern accepted"
        );
        assert!(
            validate_create_channel(&rest_req(over, None), &community()).is_err(),
            "long route_pattern refused"
        );
        let with_group = |group: String| CreateChannelRequest {
            consumer_group: Some(group),
            ..rest_req("/orders".to_string(), None)
        };
        assert!(
            validate_create_channel(&with_group("g".repeat(256)), &community()).is_err(),
            "long consumer_group refused"
        );
        assert!(
            validate_create_channel(&with_group("g".repeat(255)), &community()).is_ok(),
            "at-limit consumer_group accepted"
        );

        // The caps are protocol-independent: the over-long value is refused
        // on a protocol that does not read it too.
        assert!(
            validate_create_channel(
                &rest_req("/orders".to_string(), Some("t".repeat(256))),
                &community()
            )
            .is_err(),
            "long topic on a REST channel refused"
        );
    }

    #[test]
    fn test_validate_create_channel_sync_valid() {
        let req = CreateChannelRequest {
            tags: vec![],
            channel_id: Some("orders-sync".to_string()),
            name: "Orders Sync".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Sync,
            protocol: "rest".to_string(),
            methods: Some(vec!["POST".to_string()]),
            route_pattern: Some("/orders".to_string()),
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req, &community()).is_ok());
    }

    #[test]
    fn test_validate_create_channel_sync_missing_methods() {
        let req = CreateChannelRequest {
            tags: vec![],
            channel_id: None,
            name: "Bad Sync".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Sync,
            protocol: "rest".to_string(),
            methods: None,
            route_pattern: Some("/orders".to_string()),
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req, &community()).is_err());
    }

    #[test]
    fn test_validate_create_channel_sync_missing_route() {
        let req = CreateChannelRequest {
            tags: vec![],
            channel_id: None,
            name: "Bad Sync".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Sync,
            protocol: "rest".to_string(),
            methods: Some(vec!["POST".to_string()]),
            route_pattern: None,
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req, &community()).is_err());
    }

    #[test]
    fn test_validate_create_channel_async_valid() {
        let req = CreateChannelRequest {
            tags: vec![],
            channel_id: None,
            name: "Orders Async".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Async,
            protocol: "rest".to_string(),
            methods: Some(vec!["POST".to_string()]),
            route_pattern: Some("/orders".to_string()),
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        assert!(validate_create_channel(&req, &community()).is_ok());
    }

    /// The protocol is whatever the node's drivers serve: an unknown one is a
    /// field error naming them, and a driver registered on the registry is
    /// what makes its protocol valid.
    #[test]
    fn the_protocol_is_validated_by_the_registered_driver() {
        let req = |protocol: &str| CreateChannelRequest {
            tags: vec![],
            channel_id: None,
            name: "events".to_string(),
            description: None,
            channel_type: crate::storage::models::ChannelType::Async,
            protocol: protocol.to_string(),
            methods: None,
            route_pattern: None,
            topic: None,
            consumer_group: None,
            transport_config: json!({}),
            workflow_id: None,
            config: json!({}),
            priority: 0,
        };
        let err = validate_create_channel(&req("kafka"), &community()).expect_err("unknown");
        let details = err.field_errors();
        assert_eq!(details.len(), 1, "{details:?}");
        assert_eq!(details[0].path, "channel.protocol");
        assert_eq!(details[0].code, "INVALID");

        struct Kafka;
        impl ChannelDriver for Kafka {
            fn protocol_id(&self) -> &'static str {
                "kafka"
            }
            fn capabilities(&self) -> crate::extensions::ChannelDriverCapabilities {
                crate::extensions::ChannelDriverCapabilities {
                    supports_sync: false,
                    supports_async: true,
                    registers_http_routes: false,
                    supports_oauth2_login: false,
                }
            }
            fn validate_fields(
                &self,
                fields: &crate::extensions::ChannelProtocolFields<'_>,
            ) -> Vec<crate::errors::FieldError> {
                fields
                    .topic
                    .is_none()
                    .then(|| {
                        crate::errors::FieldError::new(
                            "channel.topic",
                            "REQUIRED_FOR_PROTOCOL",
                            "kafka channels name a topic",
                        )
                    })
                    .into_iter()
                    .collect()
            }
        }
        let mut with_kafka = crate::extensions::community_drivers();
        with_kafka.push(std::sync::Arc::new(Kafka));
        let drivers =
            crate::extensions::ChannelDriverRegistry::new(with_kafka).expect("distinct protocols");
        let err = validate_create_channel(&req("KAFKA"), &drivers).expect_err("no topic");
        assert_eq!(err.field_errors()[0].path, "channel.topic");
        let mut with_topic = req("kafka");
        with_topic.topic = Some("orders".to_string());
        validate_create_channel(&with_topic, &drivers).expect("the driver accepts it");

        // oauth2_login follows the driver's capability, not a protocol name:
        // refused outright on a driver with no sign-in flow, and on `rest`
        // held to the route it needs.
        let login = json!({"oauth2_login": {}});
        let err = validate_oauth2_login_routing(&login, &Kafka, None).expect_err("no flow");
        assert_eq!(err.field_errors()[0].path, "channel.config.oauth2_login");
        let err =
            validate_oauth2_login_routing(&login, &crate::extensions::RestChannelDriver, None)
                .expect_err("no route");
        assert_eq!(err.field_errors()[0].path, "channel.route_pattern");
    }

    #[test]
    fn test_validate_channel_id() {
        assert!(validate_channel_id("my-channel-1").is_ok());
        assert!(validate_channel_id("bad id!").is_err());
    }
}

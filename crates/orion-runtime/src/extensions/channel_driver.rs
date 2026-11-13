//! Native Rust channel driver interface and registry.
//!
//! A channel's `protocol` names a [`ChannelDriver`], and the driver registered
//! in the node's [`super::Composition`] under that name is what decides
//! whether a channel of that protocol is valid and whether it claims an HTTP
//! route:
//! - **Validation**: `validation::validate_create_channel` (the definition
//!   upload and `lint`) asks [`ChannelDriverRegistry::validate_channel`], which
//!   refuses a protocol no driver serves, runs the driver's field and
//!   transport checks, and holds `channel_type` to its
//!   [`ChannelDriverCapabilities`].
//! - **Routing**: the channel loader quarantines a stored channel whose
//!   protocol has no driver on this node, and the route table registers a
//!   route only for a driver that declares `registers_http_routes`.
//!
//! Community ([`community_drivers`]) registers `"rest"`, `"http"` and
//! `"cron"`. An edition that composes its own node registers more
//! (`"graphql"`, `"sse"`, …) with
//! [`super::CompositionBuilder::with_channel_driver`], without a closed
//! protocol enum or per-protocol match arms in core: the protocol is a string
//! id resolved here. What stays closed is `channel::guards::Transport`, which
//! names core's own ingress paths (the HTTP data plane, `channel_call`, the
//! cron worker) and the guards each applies, not which protocols exist. There is
//! no `"source"` driver: the source runner has no Community source provider to
//! drive, so a protocol for it would accept channels nothing runs.

use std::collections::BTreeMap;
use std::sync::Arc;

use orion_extension_api::{ChannelDriver, ChannelDriverCapabilities, ChannelProtocolFields};
use serde_json::Value;

use crate::errors::FieldError;

/// The `"rest"` protocol id.
pub const PROTOCOL_REST: &str = "rest";
/// The `"http"` protocol id.
pub const PROTOCOL_HTTP: &str = "http";
/// The `"cron"` protocol id.
pub const PROTOCOL_CRON: &str = "cron";

/// Community `"rest"` channel driver.
pub struct RestChannelDriver;

impl ChannelDriver for RestChannelDriver {
    fn protocol_id(&self) -> &'static str {
        PROTOCOL_REST
    }

    fn capabilities(&self) -> ChannelDriverCapabilities {
        ChannelDriverCapabilities::none()
            .sync()
            .async_submit()
            .http_routes()
            .oauth2_login()
    }

    fn validate_fields(&self, fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
        validate_http_like_fields(self.protocol_id(), fields)
    }
}

/// Community `"http"` channel driver.
pub struct HttpChannelDriver;

impl ChannelDriver for HttpChannelDriver {
    fn protocol_id(&self) -> &'static str {
        PROTOCOL_HTTP
    }

    fn capabilities(&self) -> ChannelDriverCapabilities {
        ChannelDriverCapabilities::none()
            .sync()
            .async_submit()
            .http_routes()
    }

    fn validate_fields(&self, fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
        validate_http_like_fields(self.protocol_id(), fields)
    }
}

fn validate_http_like_fields(
    protocol: &str,
    fields: &ChannelProtocolFields<'_>,
) -> Vec<FieldError> {
    let mut out = Vec::new();
    match fields.methods {
        Some(m) if !m.is_empty() => out.extend(crate::channel::field_checks::check_http_methods(m)),
        _ => out.push(
            FieldError::new(
                "channel.methods",
                "REQUIRED_FOR_PROTOCOL",
                format!(
                    "REST/HTTP channels must specify at least one HTTP method (protocol=\"{protocol}\")"
                ),
            )
            .with_expected(Value::String("non-empty array of method names".to_string())),
        ),
    }
    match fields.route_pattern {
        Some(r) if !r.trim().is_empty() => {
            out.extend(crate::channel::field_checks::check_route_pattern(r))
        }
        _ => out.push(
            FieldError::new(
                "channel.route_pattern",
                "REQUIRED_FOR_PROTOCOL",
                format!(
                    "REST/HTTP channels must specify a route_pattern (protocol=\"{protocol}\")"
                ),
            )
            .with_expected(Value::String(
                "URL path pattern (e.g. \"/orders/{id}\")".to_string(),
            )),
        ),
    }
    out
}

/// Community `"cron"` scheduler channel driver.
pub struct CronChannelDriver;

impl ChannelDriver for CronChannelDriver {
    fn protocol_id(&self) -> &'static str {
        PROTOCOL_CRON
    }

    fn capabilities(&self) -> ChannelDriverCapabilities {
        ChannelDriverCapabilities::none().async_submit()
    }

    fn validate_fields(&self, fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
        crate::channel::cron_validation::cron_routing_errors(
            fields.channel_type,
            fields.methods,
            fields.route_pattern,
            None,
            None,
        )
    }

    /// The compiled schedule, and the guards a schedule cannot use: a
    /// schedule that does not compile would be quarantined at load rather
    /// than serving, and a guard a cron occurrence never applies would be
    /// declared and silently ignored.
    fn validate_transport(
        &self,
        transport_config: &Value,
        config: &Value,
        channel_id: Option<&str>,
    ) -> Vec<FieldError> {
        let mut details =
            crate::channel::cron_validation::cron_transport_errors(transport_config, channel_id);
        details.extend(crate::channel::cron_validation::cron_config_errors(config));
        details
    }
}

/// The registry of native Rust [`ChannelDriver`]s a node serves:
/// the one answer to which channel protocols exist. Built once, by
/// [`super::CompositionBuilder::build`] (or [`Self::community`] for the
/// offline tooling), and frozen.
#[derive(Clone)]
pub struct ChannelDriverRegistry {
    drivers: Arc<BTreeMap<String, Arc<dyn ChannelDriver>>>,
}

impl std::fmt::Debug for ChannelDriverRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let protocols = self.protocol_ids();
        f.debug_struct("ChannelDriverRegistry")
            .field("protocols", &protocols)
            .finish()
    }
}

/// Community's drivers: `"rest"`, `"http"` and `"cron"`.
pub fn community_drivers() -> Vec<Arc<dyn ChannelDriver>> {
    vec![
        Arc::new(RestChannelDriver),
        Arc::new(HttpChannelDriver),
        Arc::new(CronChannelDriver),
    ]
}

impl ChannelDriverRegistry {
    /// A registry of `drivers`, refusing the first protocol id seen twice
    /// (compared case-insensitively).
    pub fn new(drivers: impl IntoIterator<Item = Arc<dyn ChannelDriver>>) -> Result<Self, String> {
        let mut map = BTreeMap::new();
        for driver in drivers {
            let id = driver.protocol_id().to_ascii_lowercase();
            if map.contains_key(&id) {
                return Err(id);
            }
            map.insert(id, driver);
        }
        Ok(Self {
            drivers: Arc::new(map),
        })
    }

    /// Community Edition channel driver registry: `"rest"`, `"http"` and
    /// `"cron"`. What the offline tooling (`lint`, `compile`) validates
    /// against; a running node validates against its composition's.
    pub fn community() -> Self {
        Self::new(community_drivers())
            .unwrap_or_else(|_| unreachable!("distinct community protocols"))
    }

    /// Look up a registered driver by its protocol identifier, exactly as
    /// written: a protocol is the lowercase word the driver declares, so
    /// `REST` is not `rest`.
    pub fn get(&self, protocol_id: &str) -> Option<Arc<dyn ChannelDriver>> {
        self.drivers.get(protocol_id).cloned()
    }

    /// List all registered protocol identifiers in sorted order.
    pub fn protocol_ids(&self) -> Vec<String> {
        self.drivers.keys().cloned().collect()
    }

    /// Whether `protocol_id` is registered and declares HTTP route registration.
    pub fn registers_http_routes(&self, protocol_id: &str) -> bool {
        self.get(protocol_id)
            .is_some_and(|d| d.capabilities().registers_http_routes)
    }

    /// The driver serving `protocol`, or the field error naming the protocols
    /// this node does serve (boxed: the error is large and the path is cold).
    pub fn resolve(&self, protocol: &str) -> Result<Arc<dyn ChannelDriver>, Box<FieldError>> {
        self.get(protocol).ok_or_else(|| {
            let registered = self.protocol_ids();
            Box::new(
                FieldError::new(
                    "channel.protocol",
                    "INVALID",
                    format!(
                        "unknown protocol '{protocol}': no channel driver serves it on this node \
                     (registered: {})",
                        registered.join(", ")
                    ),
                )
                .with_expected(Value::from(registered))
                .with_got(Value::String(protocol.to_string())),
            )
        })
    }

    /// Every problem the driver for `fields.protocol` finds in a channel: an
    /// unknown protocol, the driver's own field and transport checks, and a
    /// `channel_type` its [`ChannelDriverCapabilities`] do not serve. Empty
    /// when the channel is valid for its protocol.
    pub fn validate_channel(
        &self,
        fields: &ChannelProtocolFields<'_>,
        transport_config: &Value,
        config: &Value,
        channel_id: Option<&str>,
    ) -> Vec<FieldError> {
        let driver = match self.resolve(fields.protocol) {
            Ok(driver) => driver,
            Err(unknown) => return vec![*unknown],
        };
        let mut out = driver.validate_fields(fields);
        // A driver that names `channel_type` itself (cron does) has said it
        // better than the generic capability check would.
        if !out.iter().any(|e| e.path == "channel.channel_type")
            && let Some(problem) = channel_type_problem(driver.as_ref(), fields.channel_type)
        {
            out.push(problem);
        }
        out.extend(ingress_problems(driver.as_ref(), fields, config));
        out.extend(driver.validate_transport(transport_config, config, channel_id));
        out
    }
}

/// `config` keys an ingress protocol's deliveries never pass through,
/// whatever guards its driver declares: each answers a question about an HTTP
/// request — who sent it, from which page, what to answer, where to redirect —
/// and a delivery from a broker or a queue is not one.
const HTTP_ONLY_CONFIG_KEYS: &[(&str, &str)] = &[
    (
        "auth",
        "deliveries are authenticated by the transport's own connection, not per message",
    ),
    (
        "principal_rate_limit",
        "there is no authenticated caller to key it on",
    ),
    (
        "origin_allow_list",
        "the allow-list checks an HTTP `Origin` header, which a delivery does not carry",
    ),
    (
        "cache",
        "there is no caller waiting for a response to serve from a cache",
    ),
    (
        "request",
        "`request` shapes an HTTP body into `data`; the driver supplies `data`",
    ),
    (
        "response",
        "`response` shapes an HTTP reply, and a delivery has no reply to shape",
    ),
    (
        "oauth2_login",
        "both legs of the sign-in flow are browser redirects",
    ),
];

/// A channel of an ingress protocol ([`ChannelDriverCapabilities::ingress`])
/// may configure only the guards its driver runs. Anything else would be a
/// setting Orion accepts and never applies — an `auth` block that reads like
/// protection and protects nothing — so it is refused here, at upload and in
/// `lint`, rather than ignored at runtime. The row this mirrors is
/// `Transport::Extension`'s in `channel::guards`.
fn ingress_problems(
    driver: &dyn ChannelDriver,
    fields: &ChannelProtocolFields<'_>,
    config: &Value,
) -> Vec<FieldError> {
    let caps = driver.capabilities();
    let Some(guards) = caps.ingress else {
        return Vec::new();
    };
    let protocol = driver.protocol_id();
    let mut out = Vec::new();
    if let Some(map) = config.as_object() {
        let declared_off: [(&str, bool); 4] = [
            ("rate_limit", guards.rate_limit),
            ("deduplication", guards.deduplication),
            ("validation_logic", guards.validation),
            ("backpressure", guards.backpressure),
        ];
        let set = |key: &str| map.get(key).is_some_and(|v| !v.is_null());
        for (key, why) in HTTP_ONLY_CONFIG_KEYS {
            if set(key) {
                out.push(FieldError::new(
                    format!("channel.config.{key}"),
                    orion_api::error::field_codes::INVALID,
                    format!("a '{protocol}' channel must not set {key}: {why}"),
                ));
            }
        }
        for (key, runs) in declared_off {
            if !runs && set(key) {
                out.push(FieldError::new(
                    format!("channel.config.{key}"),
                    orion_api::error::field_codes::INVALID,
                    format!(
                        "a '{protocol}' channel must not set {key}: the protocol's \
                         deliveries do not pass that guard"
                    ),
                ));
            }
        }
    }
    if !caps.registers_http_routes {
        if fields.methods.is_some_and(|m| !m.is_empty()) {
            out.push(FieldError::new(
                "channel.methods",
                orion_api::error::field_codes::INVALID,
                format!(
                    "a '{protocol}' channel registers no HTTP route and must not declare methods"
                ),
            ));
        }
        if fields.route_pattern.is_some_and(|r| !r.trim().is_empty()) {
            out.push(FieldError::new(
                "channel.route_pattern",
                orion_api::error::field_codes::INVALID,
                format!(
                    "a '{protocol}' channel registers no HTTP route and must not declare \
                     route_pattern"
                ),
            ));
        }
    }
    out
}

/// `channel_type` against what the driver serves: a sync channel on a driver
/// with no caller to answer, or an async one on a driver that cannot queue.
fn channel_type_problem(
    driver: &dyn ChannelDriver,
    channel_type: Option<&str>,
) -> Option<FieldError> {
    let channel_type = channel_type?;
    let caps = driver.capabilities();
    let served = if channel_type.eq_ignore_ascii_case("sync") {
        caps.supports_sync
    } else if channel_type.eq_ignore_ascii_case("async") {
        caps.supports_async
    } else {
        return None;
    };
    (!served).then(|| {
        FieldError::new(
            "channel.channel_type",
            "INVALID",
            format!(
                "protocol '{}' does not serve {channel_type} channels",
                driver.protocol_id()
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct CustomGraphqlDriver;

    impl ChannelDriver for CustomGraphqlDriver {
        fn protocol_id(&self) -> &'static str {
            "graphql"
        }

        fn capabilities(&self) -> ChannelDriverCapabilities {
            ChannelDriverCapabilities::none().sync().http_routes()
        }

        fn validate_fields(&self, fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
            let mut out = Vec::new();
            if fields.route_pattern.is_none_or(|r| r.trim().is_empty()) {
                out.push(FieldError::new(
                    "channel.route_pattern",
                    "REQUIRED_FOR_PROTOCOL",
                    "GraphQL channels must specify a route_pattern",
                ));
            }
            out
        }
    }

    fn with_graphql() -> ChannelDriverRegistry {
        let mut drivers = community_drivers();
        drivers.push(Arc::new(CustomGraphqlDriver));
        ChannelDriverRegistry::new(drivers).expect("distinct protocols")
    }

    #[test]
    fn a_protocol_registered_twice_is_refused() {
        let mut drivers = community_drivers();
        drivers.push(Arc::new(CronChannelDriver));
        assert_eq!(
            ChannelDriverRegistry::new(drivers).err().as_deref(),
            Some("cron")
        );
    }

    #[test]
    fn community_registry_contains_foundation_drivers_only() {
        let reg = ChannelDriverRegistry::community();
        assert_eq!(reg.protocol_ids(), vec!["cron", "http", "rest"]);
        assert!(reg.get("kafka").is_none());
        assert!(reg.get("source").is_none());
        assert!(reg.registers_http_routes("rest"));
        assert!(reg.registers_http_routes("http"));
        assert!(!reg.registers_http_routes("cron"));
    }

    fn fields<'a>(protocol: &'a str, channel_type: &'a str) -> ChannelProtocolFields<'a> {
        let mut fields = ChannelProtocolFields::new(protocol);
        fields.channel_type = Some(channel_type);
        fields
    }

    #[test]
    fn external_enterprise_driver_registers_without_core_enum_changes() {
        let reg = with_graphql();

        assert!(reg.get("graphql").is_some());
        assert!(reg.registers_http_routes("graphql"));

        let errs = reg.validate_channel(&fields("graphql", "sync"), &json!({}), &json!({}), None);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].path, "channel.route_pattern");
    }

    #[test]
    fn an_unknown_protocol_is_a_field_error_naming_what_is_served() {
        let reg = ChannelDriverRegistry::community();
        let errs = reg.validate_channel(&fields("kafka", "async"), &json!({}), &json!({}), None);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].path, "channel.protocol");
        assert_eq!(errs[0].code, "INVALID");
        assert_eq!(errs[0].expected, Some(json!(["cron", "http", "rest"])));
        assert_eq!(errs[0].got, Some(json!("kafka")));
    }

    #[test]
    fn channel_type_is_held_to_the_driver_capabilities() {
        let reg = with_graphql();
        let mut f = fields("graphql", "async");
        f.route_pattern = Some("/graphql");
        let errs = reg.validate_channel(&f, &json!({}), &json!({}), None);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].path, "channel.channel_type");

        // Cron reports channel_type itself; the capability check adds nothing.
        let errs = reg.validate_channel(
            &fields("cron", "sync"),
            &json!({"schedule": "0 0 * * * *"}),
            &json!({}),
            None,
        );
        assert_eq!(
            errs.iter()
                .filter(|e| e.path == "channel.channel_type")
                .count(),
            1,
            "{errs:?}"
        );
    }

    /// A protocol whose driver runs its own listener and declares validation
    /// and deduplication only.
    struct QueueDriver;

    impl ChannelDriver for QueueDriver {
        fn protocol_id(&self) -> &'static str {
            "queue"
        }

        fn capabilities(&self) -> ChannelDriverCapabilities {
            ChannelDriverCapabilities::none().async_submit().ingress(
                orion_extension_api::GuardSet::none()
                    .validation()
                    .deduplication(),
            )
        }

        fn validate_fields(&self, _fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
            Vec::new()
        }
    }

    fn with_queue() -> ChannelDriverRegistry {
        let mut drivers = community_drivers();
        drivers.push(Arc::new(QueueDriver));
        ChannelDriverRegistry::new(drivers).expect("distinct protocols")
    }

    #[test]
    fn an_ingress_channel_may_configure_only_the_guards_its_driver_runs() {
        let reg = with_queue();
        let accepted = json!({
            "validation_logic": {"!!": [{"var": "data.id"}]},
            "deduplication": {"header": "x-id", "window_secs": 60},
            "timeout_ms": 1000,
        });
        let errs = reg.validate_channel(&fields("queue", "async"), &json!({}), &accepted, None);
        assert!(errs.is_empty(), "{errs:?}");

        let refused = json!({
            "auth": {"type": "api_key", "keys": ["k"]},
            "cache": {"enabled": true},
            "rate_limit": {"requests_per_second": 5},
            "backpressure": {"max_concurrent": 2},
        });
        let errs = reg.validate_channel(&fields("queue", "async"), &json!({}), &refused, None);
        let mut paths: Vec<&str> = errs.iter().map(|e| e.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            [
                "channel.config.auth",
                "channel.config.backpressure",
                "channel.config.cache",
                "channel.config.rate_limit",
            ],
            "{errs:?}"
        );
    }

    #[test]
    fn an_ingress_channel_without_http_routes_declares_no_route() {
        let reg = with_queue();
        let mut f = fields("queue", "async");
        f.route_pattern = Some("/orders");
        let errs = reg.validate_channel(&f, &json!({}), &json!({}), None);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].path, "channel.route_pattern");
    }

    #[test]
    fn the_cron_driver_validates_its_transport() {
        let reg = ChannelDriverRegistry::community();
        let errs = reg.validate_channel(&fields("cron", "async"), &json!({}), &json!({}), None);
        assert!(
            errs.iter()
                .any(|e| e.path.starts_with("channel.transport_config")),
            "a cron channel with no schedule must be refused: {errs:?}"
        );
    }
}

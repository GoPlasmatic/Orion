//! Connector endpoint validation (S6).
//!
//! Until 1.0 only the `http` connector's URL was scheme-checked, and the
//! runtime SSRF guard ([`crate::validation::validate_url_not_private`]) was
//! called from exactly two places — `http_common.rs` and the Elasticsearch
//! helper. No db, cache, mongo or kafka path checked anything, so a connector
//! with `connection_string: "postgres://…@169.254.169.254/…"` was accepted and
//! dialled.
//!
//! Two layers close that:
//!
//!   1. **Scheme allow-list at create/update** ([`validate_endpoint_schemes`]).
//!      Synchronous and offline: a connector must not need DNS to be stored,
//!      and an admin API that blocks on resolution is an admin API that hangs
//!      when the target is down.
//!   2. **Private-address check on the pool-open paths**
//!      ([`check_db_endpoint`]), with the same `allow_private_urls` opt-out
//!      the HTTP connector already had.
//!
//! **Why not just call `validate_url_not_private`.** S7 made an
//! `{http, https}` gate the *first* statement of that function, so calling it
//! on a `postgres://` URL would refuse every database connector in existence. These paths go through
//! [`crate::validation::ssrf::validate_hostport_not_private`] instead, which
//! judges an already-split host and port and is scheme-agnostic by
//! construction.

use crate::connector::{ConnectorConfig, DbConnectorConfig};
use crate::errors::OrionError;
use crate::validation::ssrf::validate_hostport_not_private;

/// Schemes a `db` connector may use for a SQL backend. `sqlite` opens a local
/// file rather than a socket — it is allowed because a single-node SQLite
/// connector is a supported shape, and it is skipped by the runtime address
/// check because there is no host to judge.
pub const DB_SQL_SCHEMES: &[&str] = &["postgres", "postgresql", "mysql", "mariadb", "sqlite"];

/// Default port per scheme, for connection strings that omit one. Used only to
/// build the address the private-IP check judges; the driver applies its own
/// default when it dials.
fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "postgres" | "postgresql" => Some(5432),
        "mysql" | "mariadb" => Some(3306),
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

/// The scheme of a connection string: everything before the first `:`.
///
/// Deliberately not `Url::parse` — `sqlite::memory:` and other driver-specific
/// forms are legal connection strings that are not legal URLs, and rejecting
/// them here would be a scheme check that fails on the shape rather than on
/// the scheme.
fn scheme_of(conn: &str) -> Option<String> {
    let (scheme, _) = conn.split_once(':')?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
}

fn scheme_error(field: &str, conn: &str, allowed: &[&str]) -> OrionError {
    let shown = match scheme_of(conn) {
        Some(s) => format!("'{s}'"),
        None => "no scheme".to_string(),
    };
    OrionError::validation(format!(
        "Connector {field} uses {shown}. Allowed: {}",
        allowed.join(", ")
    ))
}

/// When an endpoint is judged, which decides what may still be unresolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointPhase {
    /// Create, update, validate, `lint`: a reference, or a `${VAR}` this host
    /// cannot substitute, is deferred to load.
    Authoring,
    /// Registry load, after every reference resolved: the value is final.
    /// Errors name the scheme only, never the value, which may carry
    /// userinfo a reference was hiding.
    Load,
}

/// What load will see of `conn`, or `None` when this host cannot know: a
/// resolvable secret reference (`env://…`, `vault://…`) resolves at load, so
/// its scheme is the reference's, not the endpoint's; and `${VAR}`
/// placeholders are substituted by the load path before anything parses the
/// string. Refusing those at authoring would reject every connector authored
/// the documented way (`${ORDERS_DB_URL:-postgres://…}`); the load path judges
/// the resolved value, on the host that matters.
fn effective_endpoint(field: &str, conn: &str, phase: EndpointPhase) -> Option<String> {
    if phase == EndpointPhase::Load {
        return Some(conn.to_string());
    }
    if crate::connector::secrets::is_resolvable_reference(conn) {
        return None;
    }
    crate::config::env_substitute::substitute(conn, field).ok()
}

fn require_scheme(
    field: &str,
    conn: &str,
    allowed: &[&str],
    phase: EndpointPhase,
) -> Result<(), OrionError> {
    let Some(effective) = effective_endpoint(field, conn, phase) else {
        return Ok(());
    };
    let scheme = scheme_of(&effective).ok_or_else(|| scheme_error(field, &effective, allowed))?;
    if !allowed.contains(&scheme.as_str()) {
        return Err(scheme_error(field, &effective, allowed));
    }
    Ok(())
}

/// An HTTP endpoint (`http.url`, an OAuth2 `token_url`): the same deferral
/// as [`require_scheme`], then the stricter check the HTTP connector always
/// had — a URL that parses, with scheme `http` or `https`. `label` names the
/// field in the messages (`connector URL`, `OAuth2 token_url`), which read as
/// they did when each field had its own check. An empty `url` is allowed, as
/// it always was: a task may supply the whole URL.
fn require_http_url(label: &str, value: &str, phase: EndpointPhase) -> Result<(), OrionError> {
    if value.is_empty() {
        return Ok(());
    }
    let Some(effective) = effective_endpoint(label, value, phase) else {
        return Ok(());
    };
    let resolved = if phase == EndpointPhase::Load {
        " (resolved from a reference)"
    } else {
        ""
    };
    let parsed = url::Url::parse(&effective).map_err(|e| {
        OrionError::validation(match phase {
            EndpointPhase::Authoring => format!("Invalid {label} '{effective}': {e}"),
            EndpointPhase::Load => format!("Invalid {label}: {e}{resolved}"),
        })
    })?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        let mut subject = label.to_string();
        if let Some(first) = subject.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        return Err(OrionError::validation(format!(
            "{subject} must use http or https scheme, got '{scheme}'{resolved}"
        )));
    }
    Ok(())
}

/// Refuse a connector whose endpoint uses a scheme its backend cannot serve.
///
/// Called from `validate_connector_config` at [`EndpointPhase::Authoring`],
/// so it runs on create, update, validate and `lint`; and from the registry
/// load at [`EndpointPhase::Load`], on the value every reference resolved to.
/// Schemes only — no DNS, no sockets: the private-address check needs DNS
/// and must see every redirect, so it stays on the request and pool-open
/// paths.
pub fn validate_endpoint_schemes(
    parsed: &ConnectorConfig,
    phase: EndpointPhase,
) -> Result<(), OrionError> {
    match parsed {
        ConnectorConfig::Http(http) => {
            require_http_url("connector URL", &http.url, phase)?;
            if let Some(crate::connector::AuthConfig::OAuth2(o)) = &http.auth {
                require_http_url("OAuth2 token_url", &o.token_url, phase)?;
            }
            Ok(())
        }
        ConnectorConfig::Db(db) => {
            require_scheme(
                "connection_string",
                &db.connection_string,
                DB_SQL_SCHEMES,
                phase,
            )?;
            Ok(())
        }
        // A cache connector is in-process memory: it has no endpoint.
        ConnectorConfig::Cache(_) => Ok(()),
    }
}

/// Host and port to judge for a connection string, or `None` when there is no
/// network endpoint to check (`sqlite:`, which opens a file).
fn endpoint_of(conn: &str) -> Option<(String, u16)> {
    let scheme = scheme_of(conn)?;
    if scheme == "sqlite" {
        return None;
    }
    let parsed = url::Url::parse(conn).ok()?;
    let host = parsed.host_str()?.to_string();
    let port = parsed.port().or_else(|| default_port(&scheme))?;
    Some((host, port))
}

/// Shared body of the connection-string checks: honour the opt-out, skip
/// connection strings with no network endpoint to judge, and refuse the rest
/// when they target a private/internal address.
async fn check_conn_endpoint(
    kind: &str,
    connector_name: &str,
    conn: &str,
    allow_private: bool,
) -> Result<(), OrionError> {
    if allow_private {
        return Ok(());
    }
    let Some((host, port)) = endpoint_of(conn) else {
        return Ok(());
    };
    validate_hostport_not_private(&host, port)
        .await
        .map_err(|msg| refused(kind, connector_name, &msg))?;
    Ok(())
}

/// Private-address check for a `db` connector, on the pool-open path.
///
/// Skips `sqlite:` (a file, not a socket).
pub async fn check_db_endpoint(
    connector_name: &str,
    config: &DbConnectorConfig,
) -> Result<(), OrionError> {
    check_conn_endpoint(
        "db",
        connector_name,
        &config.connection_string,
        config.allow_private_urls,
    )
    .await
}

fn refused(kind: &str, connector_name: &str, msg: &str) -> OrionError {
    OrionError::validation(format!(
        "Refusing to connect {kind} connector '{connector_name}': {msg}. \
         Set \"allow_private_urls\": true on this connector if the target is \
         intentionally on a private network."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate_endpoint_schemes_at_authoring(parsed: &ConnectorConfig) -> Result<(), OrionError> {
        validate_endpoint_schemes(parsed, EndpointPhase::Authoring)
    }

    fn http(url: &str, token_url: Option<&str>) -> ConnectorConfig {
        let mut doc = serde_json::json!({"type": "http", "url": url});
        if let Some(token_url) = token_url {
            doc["auth"] = serde_json::json!({
                "type": "oauth2", "grant": "client_credentials", "token_url": token_url,
                "client_id": "id", "client_secret": "env://SECRET",
            });
        }
        serde_json::from_value(doc).expect("http config")
    }

    /// #338: an HTTP URL follows the rule every other endpoint does — a
    /// reference defers to load — and a literal keeps its old check and its
    /// old words.
    #[test]
    fn an_http_url_may_be_a_reference_and_a_literal_is_still_checked() {
        validate_endpoint_schemes_at_authoring(&http("env://PEER_API_URL", None))
            .expect("a reference defers");
        validate_endpoint_schemes_at_authoring(&http("", None)).expect("empty is allowed");
        let err = validate_endpoint_schemes_at_authoring(&http("ftp://example.com", None))
            .expect_err("ftp");
        assert!(
            err.to_string()
                .contains("Connector URL must use http or https scheme, got 'ftp'"),
            "{err}"
        );
        let err =
            validate_endpoint_schemes_at_authoring(&http("not a url", None)).expect_err("garbage");
        assert!(
            err.to_string()
                .contains("Invalid connector URL 'not a url'"),
            "{err}"
        );

        validate_endpoint_schemes_at_authoring(&http(
            "https://api.example.com",
            Some("env://TOKEN_URL"),
        ))
        .expect("a token_url reference defers");
        let err = validate_endpoint_schemes_at_authoring(&http(
            "https://api.example.com",
            Some("ftp://idp.example.com/token"),
        ))
        .expect_err("ftp token_url");
        assert!(
            err.to_string()
                .contains("OAuth2 token_url must use http or https scheme, got 'ftp'"),
            "{err}"
        );
    }

    /// At load the value is final: nothing defers, and the message names the
    /// scheme and never the value, which may carry userinfo.
    #[test]
    fn at_load_a_resolved_endpoint_is_judged_without_being_quoted() {
        let err = validate_endpoint_schemes(
            &http("ftp://user:hunter2@example.com", None),
            EndpointPhase::Load,
        )
        .expect_err("ftp at load");
        let message = err.to_string();
        assert!(
            message.contains("got 'ftp' (resolved from a reference)"),
            "{message}"
        );
        assert!(!message.contains("hunter2"), "{message}");
        let err =
            validate_endpoint_schemes(&http("env://NEVER_RESOLVED", None), EndpointPhase::Load)
                .expect_err("a reference left at load");
        assert!(!err.to_string().contains("NEVER_RESOLVED"), "{err}");
        let err = validate_endpoint_schemes(&db("mongo+x://u:hunter2@h/d"), EndpointPhase::Load)
            .expect_err("db at load");
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }

    fn db(conn: &str) -> ConnectorConfig {
        ConnectorConfig::Db(DbConnectorConfig {
            connection_string: conn.to_string(),
            driver: None,
            max_connections: None,
            connect_timeout_ms: None,
            query_timeout_ms: None,
            allow_private_urls: false,
            operations: Default::default(),
            dialect: Default::default(),
        })
    }

    #[test]
    fn db_accepts_every_supported_backend_scheme() {
        for conn in [
            "postgres://u:p@db.example.com/orion",
            "postgresql://u:p@db.example.com/orion",
            "mysql://u:p@db.example.com/orion",
            "mariadb://u:p@db.example.com/orion",
            "sqlite:/app/data/orion.db",
            "sqlite::memory:",
        ] {
            let result = validate_endpoint_schemes_at_authoring(&db(conn));
            assert!(result.is_ok(), "{conn} must be accepted: {result:?}");
        }
    }

    /// A stored endpoint is not always the endpoint itself: `${VAR}` text
    /// substitutes at load, and `env://` / reserved-scheme references resolve
    /// at load. The check judges the substituted value when this host can
    /// produce it, and defers to the load path when it cannot — refusing the
    /// raw text rejected every connector authored the documented way
    /// (`${ORDERS_DB_URL:-postgres://…}`, the postgres-orders example).
    #[test]
    fn db_placeholders_and_references_are_judged_after_resolution() {
        // A default makes the placeholder substitutable anywhere: the check
        // sees the substituted string, so a good default passes…
        validate_endpoint_schemes_at_authoring(&db(
            "${ORION_TEST_UNSET_DB_URL:-postgres://db.example.com/x}",
        ))
        .expect("placeholder with a valid default");
        // …and a bad one is still caught at the door.
        let err = validate_endpoint_schemes_at_authoring(&db(
            "${ORION_TEST_UNSET_DB_URL:-redis://not-a-db:6379}",
        ))
        .expect_err("placeholder with a foreign-scheme default");
        assert!(err.to_string().contains("Allowed:"), "{err}");

        // No default and unset here: only the load host can judge it.
        validate_endpoint_schemes_at_authoring(&db("${ORION_TEST_UNSET_DB_URL}"))
            .expect("unresolvable placeholder is the load path's to enforce");

        // Secret references resolve at load; their scheme is the reference's,
        // not the endpoint's.
        validate_endpoint_schemes_at_authoring(&db("env://ORDERS_DB_URL"))
            .expect("env:// reference");
        validate_endpoint_schemes_at_authoring(&db("vault://secret/data/db#url"))
            .expect("vault:// reference");
    }

    /// The S6 headline: a scheme the db pool would happily dial but that is
    /// not a database at all.
    #[test]
    fn db_rejects_foreign_schemes() {
        for conn in [
            "http://169.254.169.254/latest/meta-data",
            "file:///etc/passwd",
            "redis://cache.example.com:6379",
            "gopher://example.com:70/",
            "/app/data/orion.db",
            // MongoDB is not a Community backend: a `db` connector naming it
            // is an unsupported scheme like any other.
            "mongodb://m.example.com:27017/orion",
            "mongodb+srv://cluster.example.com/orion",
        ] {
            let err = validate_endpoint_schemes_at_authoring(&db(conn))
                .expect_err(&format!("{conn} must be refused"));
            assert!(
                err.to_string().contains("Allowed:"),
                "{conn}: unexpected error {err}"
            );
        }
    }

    #[test]
    fn endpoint_extraction_skips_file_backends() {
        assert_eq!(endpoint_of("sqlite:/app/data/orion.db"), None);
        assert_eq!(
            endpoint_of("postgres://u:p@db.example.com/orion"),
            Some(("db.example.com".to_string(), 5432))
        );
        assert_eq!(
            endpoint_of("mysql://u:p@db.example.com:3307/orion"),
            Some(("db.example.com".to_string(), 3307))
        );
    }

    /// The opt-out has to actually short-circuit, or every private-network
    /// deployment breaks.
    #[tokio::test]
    async fn allow_private_urls_short_circuits_every_check() {
        let mut cfg = DbConnectorConfig {
            connection_string: "postgres://u:p@127.0.0.1:5432/orion".to_string(),
            driver: None,
            max_connections: None,
            connect_timeout_ms: None,
            query_timeout_ms: None,
            allow_private_urls: false,
            operations: Default::default(),
            dialect: Default::default(),
        };
        assert!(check_db_endpoint("c", &cfg).await.is_err());
        cfg.allow_private_urls = true;
        assert!(check_db_endpoint("c", &cfg).await.is_ok());
    }

    /// The metadata endpoint the whole guard exists for.
    #[tokio::test]
    async fn link_local_metadata_endpoint_is_refused_on_every_backend() {
        let cfg = DbConnectorConfig {
            connection_string: "postgres://u:p@169.254.169.254:5432/orion".to_string(),
            driver: None,
            max_connections: None,
            connect_timeout_ms: None,
            query_timeout_ms: None,
            allow_private_urls: false,
            operations: Default::default(),
            dialect: Default::default(),
        };
        let err = check_db_endpoint("meta", &cfg).await.expect_err("test");
        assert!(err.to_string().contains("169.254.169.254"), "{err}");
        assert!(err.to_string().contains("allow_private_urls"), "{err}");
    }

    /// sqlite opens a file, so there is no address to judge — it must pass the
    /// runtime check rather than be refused for having no host.
    #[tokio::test]
    async fn sqlite_connection_strings_bypass_the_address_check() {
        let cfg = DbConnectorConfig {
            connection_string: "sqlite:/app/data/orion.db".to_string(),
            driver: None,
            max_connections: None,
            connect_timeout_ms: None,
            query_timeout_ms: None,
            allow_private_urls: false,
            operations: Default::default(),
            dialect: Default::default(),
        };
        check_db_endpoint("local", &cfg).await.expect("test");
    }
}

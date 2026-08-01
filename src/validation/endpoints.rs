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
//!      ([`check_db_endpoint`], [`check_cache_endpoint`],
//!      [`check_broker_endpoints`]), with the same `allow_private_urls`
//!      opt-out the HTTP and ES connectors already had.
//!
//! **Why not just call `validate_url_not_private`.** S7 made an
//! `{http, https}` gate the *first* statement of that function, so calling it
//! on a `postgres://` or `redis://` URL would refuse every database and cache
//! connector in existence. These paths go through
//! [`crate::validation::ssrf::validate_hostport_not_private`] instead, which
//! judges an already-split host and port and is scheme-agnostic by
//! construction.

use crate::connector::{CacheConnectorConfig, ConnectorConfig, DbConnectorConfig};
use crate::errors::OrionError;
use crate::validation::ssrf::validate_hostport_not_private;

/// Schemes a `db` connector may use for a SQL backend. `sqlite` opens a local
/// file rather than a socket — it is allowed because a single-node SQLite
/// connector is a supported shape, and it is skipped by the runtime address
/// check because there is no host to judge.
pub const DB_SQL_SCHEMES: &[&str] = &["postgres", "postgresql", "mysql", "mariadb", "sqlite"];

/// Schemes a `db` connector may use for MongoDB. Both land on the same
/// [`ConnectorConfig::Db`] variant — the backend is chosen by scheme, which is
/// why there is deliberately no `driver` field.
pub const DB_MONGO_SCHEMES: &[&str] = &["mongodb", "mongodb+srv"];

/// Schemes a `cache` connector may use when `backend = "redis"`.
pub const CACHE_SCHEMES: &[&str] = &["redis", "rediss"];

/// Schemes an `es` connector may use. The ES client is the shared reqwest
/// client, so this matches the HTTP connector exactly.
pub const ES_SCHEMES: &[&str] = &["http", "https"];

/// Default port per scheme, for connection strings that omit one. Used only to
/// build the address the private-IP check judges; the driver applies its own
/// default when it dials.
fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "postgres" | "postgresql" => Some(5432),
        "mysql" | "mariadb" => Some(3306),
        "redis" | "rediss" => Some(6379),
        "mongodb" => Some(27017),
        "http" => Some(80),
        "https" => Some(443),
        // mongodb+srv carries no port: the SRV record supplies it. Resolution
        // happens in the driver, and the resulting hosts are checked there.
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

fn require_scheme(field: &str, conn: &str, allowed: &[&str]) -> Result<String, OrionError> {
    let scheme = scheme_of(conn).ok_or_else(|| scheme_error(field, conn, allowed))?;
    if !allowed.contains(&scheme.as_str()) {
        return Err(scheme_error(field, conn, allowed));
    }
    Ok(scheme)
}

/// Refuse a connector whose endpoint uses a scheme its backend cannot serve.
///
/// Called from `validate_connector_config`, so it runs on both create and
/// update. Schemes only — no DNS, no sockets.
pub fn validate_endpoint_schemes(parsed: &ConnectorConfig) -> Result<(), OrionError> {
    match parsed {
        // The HTTP connector's URL is scheme-checked by its own branch in
        // `validate_connector_config`, which predates this module.
        ConnectorConfig::Http(_) => Ok(()),
        ConnectorConfig::Es(es) => {
            require_scheme("URL", &es.url, ES_SCHEMES)?;
            Ok(())
        }
        ConnectorConfig::Db(db) => {
            let allowed: Vec<&str> = DB_SQL_SCHEMES
                .iter()
                .chain(DB_MONGO_SCHEMES.iter())
                .copied()
                .collect();
            require_scheme("connection_string", &db.connection_string, &allowed)?;
            Ok(())
        }
        ConnectorConfig::Cache(cache) => {
            // `backend = "memory"` has no URL to check; the requirement that
            // redis carries one is enforced alongside the backend name.
            if cache.backend == "redis"
                && let Some(url) = cache.url.as_deref()
                && !url.trim().is_empty()
            {
                require_scheme("cache URL", url, CACHE_SCHEMES)?;
            }
            Ok(())
        }
        ConnectorConfig::Kafka(kafka) => {
            for broker in &kafka.brokers {
                parse_broker(broker)?;
            }
            Ok(())
        }
    }
}

/// Split a Kafka broker entry into host and port.
///
/// Brokers are bare `host:port`, not URLs, so `Url::parse` cannot be used and
/// a scheme allow-list has nothing to check. The shape is what gets validated:
/// a broker carrying a scheme (`http://b:9092`) is a configuration mistake
/// that librdkafka would report far from here.
fn parse_broker(broker: &str) -> Result<(String, u16), OrionError> {
    let broker = broker.trim();
    if broker.is_empty() {
        return Err(OrionError::validation(
            "Kafka broker entry is empty".to_string(),
        ));
    }
    if broker.contains("://") {
        return Err(OrionError::validation(format!(
            "Kafka broker '{broker}' must be host:port, not a URL"
        )));
    }
    // IPv6 literals are bracketed: [::1]:9092.
    let (host, port) = if let Some(rest) = broker.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(|| {
            OrionError::validation(format!(
                "Kafka broker '{broker}' has an unterminated IPv6 literal"
            ))
        })?;
        let port = tail.strip_prefix(':').unwrap_or("");
        (host.to_string(), port)
    } else {
        match broker.split_once(':') {
            Some((h, p)) => (h.to_string(), p),
            None => (broker.to_string(), ""),
        }
    };
    if host.is_empty() {
        return Err(OrionError::validation(format!(
            "Kafka broker '{broker}' has no host"
        )));
    }
    let port: u16 = if port.is_empty() {
        9092
    } else {
        port.parse().map_err(|_| {
            OrionError::validation(format!("Kafka broker '{broker}' has an invalid port"))
        })?
    };
    Ok((host, port))
}

/// Host and port to judge for a connection string, or `None` when there is no
/// network endpoint to check (`sqlite:`, or a `mongodb+srv:` URI whose hosts
/// only exist after the driver resolves the SRV record).
fn endpoint_of(conn: &str) -> Option<(String, u16)> {
    let scheme = scheme_of(conn)?;
    if scheme == "sqlite" || scheme == "mongodb+srv" {
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
/// Skips `sqlite:` (a file, not a socket) and `mongodb+srv:` (the hosts are
/// not known until the driver resolves the SRV record — `mongo_pool` checks
/// the resolved list instead, which is the only place they exist).
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

/// Private-address check for a `cache` connector, on the pool-open path.
///
/// A `backend = "memory"` connector carries no URL and has nothing to judge;
/// the redis pool refuses a missing one before it gets here.
pub async fn check_cache_endpoint(
    connector_name: &str,
    config: &CacheConnectorConfig,
) -> Result<(), OrionError> {
    let Some(url) = config.url.as_deref() else {
        return Ok(());
    };
    check_conn_endpoint("cache", connector_name, url, config.allow_private_urls).await
}

/// Private-address check for Kafka brokers.
pub async fn check_broker_endpoints(
    connector_name: &str,
    brokers: &[String],
    allow_private_urls: bool,
) -> Result<(), OrionError> {
    if allow_private_urls {
        return Ok(());
    }
    for broker in brokers {
        let (host, port) = parse_broker(broker)?;
        validate_hostport_not_private(&host, port)
            .await
            .map_err(|msg| refused("kafka", connector_name, &msg))?;
    }
    Ok(())
}

/// Private-address check for MongoDB hosts, after the driver has parsed the
/// URI (and, for `mongodb+srv`, resolved the SRV record). Replica-set URIs
/// name several hosts and are not parseable as a single URL, which is why
/// this takes the driver's own host list.
pub async fn check_mongo_hosts(
    connector_name: &str,
    hosts: &[(String, Option<u16>)],
    allow_private_urls: bool,
) -> Result<(), OrionError> {
    if allow_private_urls {
        return Ok(());
    }
    for (host, port) in hosts {
        validate_hostport_not_private(host, port.unwrap_or(27017))
            .await
            .map_err(|msg| refused("mongo", connector_name, &msg))?;
    }
    Ok(())
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
    use crate::connector::{CacheConnectorConfig, KafkaConnectorConfig, is_mongo_url};

    fn db(conn: &str) -> ConnectorConfig {
        ConnectorConfig::Db(DbConnectorConfig {
            connection_string: conn.to_string(),
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
            "mongodb://m.example.com:27017/orion",
            "mongodb+srv://cluster.example.com/orion",
        ] {
            let result = validate_endpoint_schemes(&db(conn));
            assert!(result.is_ok(), "{conn} must be accepted: {result:?}");
        }
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
        ] {
            let err =
                validate_endpoint_schemes(&db(conn)).expect_err(&format!("{conn} must be refused"));
            assert!(
                err.to_string().contains("Allowed:"),
                "{conn}: unexpected error {err}"
            );
        }
    }

    #[test]
    fn cache_rejects_non_redis_schemes_but_ignores_memory_backend() {
        let redis = |url: Option<&str>| {
            ConnectorConfig::Cache(CacheConnectorConfig {
                backend: "redis".to_string(),
                url: url.map(str::to_string),
                allow_private_urls: false,
                operations: Default::default(),
            })
        };
        validate_endpoint_schemes(&redis(Some("redis://cache.example.com:6379"))).expect("test");
        validate_endpoint_schemes(&redis(Some("rediss://cache.example.com:6379"))).expect("test");
        assert!(validate_endpoint_schemes(&redis(Some("http://cache.example.com"))).is_err());

        // memory backend: no URL to judge.
        validate_endpoint_schemes(&ConnectorConfig::Cache(CacheConnectorConfig {
            backend: "memory".to_string(),
            url: None,
            allow_private_urls: false,
            operations: Default::default(),
        }))
        .expect("test");
    }

    #[test]
    fn kafka_brokers_must_be_host_port() {
        let kafka = |brokers: Vec<&str>| {
            ConnectorConfig::Kafka(KafkaConnectorConfig {
                brokers: brokers.into_iter().map(str::to_string).collect(),
                topic: "t".to_string(),
                allow_private_urls: false,
                operations: Default::default(),
            })
        };
        validate_endpoint_schemes(&kafka(vec!["b1.example.com:9092", "b2.example.com:9092"]))
            .expect("test");
        validate_endpoint_schemes(&kafka(vec!["b.example.com"]))
            .expect("bare host defaults to 9092");
        validate_endpoint_schemes(&kafka(vec!["[2600::1]:9092"])).expect("bracketed ipv6");
        assert!(validate_endpoint_schemes(&kafka(vec!["http://b:9092"])).is_err());
        assert!(validate_endpoint_schemes(&kafka(vec!["b:not-a-port"])).is_err());
        assert!(validate_endpoint_schemes(&kafka(vec![""])).is_err());
    }

    #[test]
    fn broker_parsing_covers_ipv6_and_defaults() {
        assert_eq!(
            parse_broker("[::1]:9093").expect("test"),
            ("::1".to_string(), 9093)
        );
        assert_eq!(
            parse_broker("[2600::1]").expect("test"),
            ("2600::1".to_string(), 9092)
        );
        assert_eq!(
            parse_broker("host.example:1234").expect("test"),
            ("host.example".to_string(), 1234)
        );
    }

    #[test]
    fn endpoint_extraction_skips_file_and_srv_backends() {
        assert_eq!(endpoint_of("sqlite:/app/data/orion.db"), None);
        assert_eq!(endpoint_of("mongodb+srv://cluster.example.com/orion"), None);
        assert_eq!(
            endpoint_of("postgres://u:p@db.example.com/orion"),
            Some(("db.example.com".to_string(), 5432))
        );
        assert_eq!(
            endpoint_of("mysql://u:p@db.example.com:3307/orion"),
            Some(("db.example.com".to_string(), 3307))
        );
        assert_eq!(
            endpoint_of("redis://cache.example.com"),
            Some(("cache.example.com".to_string(), 6379))
        );
    }

    /// The opt-out has to actually short-circuit, or every private-network
    /// deployment breaks.
    #[tokio::test]
    async fn allow_private_urls_short_circuits_every_check() {
        let mut cfg = DbConnectorConfig {
            connection_string: "postgres://u:p@127.0.0.1:5432/orion".to_string(),
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

        assert!(
            check_broker_endpoints("c", &["127.0.0.1:9092".into()], false)
                .await
                .is_err()
        );
        assert!(
            check_broker_endpoints("c", &["127.0.0.1:9092".into()], true)
                .await
                .is_ok()
        );

        assert!(
            check_mongo_hosts("c", &[("10.0.0.5".to_string(), Some(27017))], false)
                .await
                .is_err()
        );
        assert!(
            check_mongo_hosts("c", &[("10.0.0.5".to_string(), Some(27017))], true)
                .await
                .is_ok()
        );
    }

    /// The metadata endpoint the whole guard exists for.
    #[tokio::test]
    async fn link_local_metadata_endpoint_is_refused_on_every_backend() {
        let cfg = DbConnectorConfig {
            connection_string: "postgres://u:p@169.254.169.254:5432/orion".to_string(),
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

        let cache = CacheConnectorConfig {
            backend: "redis".to_string(),
            url: Some("redis://169.254.169.254:6379".to_string()),
            allow_private_urls: false,
            operations: Default::default(),
        };
        assert!(check_cache_endpoint("meta", &cache).await.is_err());
    }

    /// sqlite opens a file, so there is no address to judge — it must pass the
    /// runtime check rather than be refused for having no host.
    #[tokio::test]
    async fn sqlite_connection_strings_bypass_the_address_check() {
        let cfg = DbConnectorConfig {
            connection_string: "sqlite:/app/data/orion.db".to_string(),
            max_connections: None,
            connect_timeout_ms: None,
            query_timeout_ms: None,
            allow_private_urls: false,
            operations: Default::default(),
            dialect: Default::default(),
        };
        check_db_endpoint("local", &cfg).await.expect("test");
    }

    #[test]
    fn mongo_urls_are_recognised_as_mongo() {
        assert!(is_mongo_url("mongodb://h/db"));
        assert!(is_mongo_url("mongodb+srv://h/db"));
        assert!(!is_mongo_url("postgres://h/db"));
    }
}

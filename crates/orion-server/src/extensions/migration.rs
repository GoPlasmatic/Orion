//! Legacy v1 → v2 catalog inventory and migration adapter (`V2-14.1`, `V2-14.5`, `DEC-005`).
//!
//! Maps legacy v1 `connector_type`, connection configs, channel protocols, and
//! workflow function names to their exact v2 provider/channel targets and
//! edition tier ([`EditionTier::Community`] vs [`EditionTier::Enterprise`]).
//! Rejects ambiguous or unsupported configurations with explicit findings rather
//! than silently choosing a default provider (`V2-14.5`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CompositionRoot;
use super::contract::ImplementationKind;
use super::foundation::{
    DRIVER_HTTP, DRIVER_MEMORY_CACHE, DRIVER_MYSQL, DRIVER_POSTGRESQL, DRIVER_SQLITE,
};

/// Enterprise connector provider IDs defined in `DEC-005`.
pub const ENTERPRISE_DRIVER_MONGODB: &str = "com.orion.enterprise.mongodb";
pub const ENTERPRISE_DRIVER_REDIS: &str = "com.orion.enterprise.redis";
pub const ENTERPRISE_DRIVER_ELASTICSEARCH: &str = "com.orion.enterprise.elasticsearch";
pub const ENTERPRISE_DRIVER_KAFKA: &str = "com.orion.enterprise.kafka";
pub const ENTERPRISE_DRIVER_SMTP: &str = "com.orion.enterprise.smtp";
pub const ENTERPRISE_DRIVER_STORAGE: &str = "com.orion.enterprise.storage";

pub use super::admission::EditionTier;

/// Resolved v2 provider target for a legacy v1 connector definition (`V2-14.1`, `V2-14.5`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedLegacyConnector {
    pub provider_id: &'static str,
    pub implementation: ImplementationKind,
    pub tier: EditionTier,
}

/// Resolved v2 provider operation for a legacy v1 workflow task (`V2-14.1`, `V2-14.5`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedLegacyFunction {
    pub provider_id: &'static str,
    pub operation: &'static str,
    pub tier: EditionTier,
}

/// Blocking migration finding emitted when a legacy connector, channel, or function
/// cannot be converted or requires an Enterprise provider not installed on the node (`V2-14.5`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationFinding {
    pub entity_kind: &'static str,
    pub entity_name: String,
    pub field: &'static str,
    pub code: &'static str,
    pub message: String,
}

/// Map a legacy v1 `(connector_type, config)` pair to its exact v2 provider (`V2-14.1`, `V2-14.5`).
///
/// Never silently defaults: if a `"db"` connector has an unrecognized or missing
/// URL scheme, returns a [`MigrationFinding`] instead of guessing a SQL dialect.
pub fn map_legacy_connector(
    connector_name: &str,
    connector_type: &str,
    config: &Value,
) -> Result<ResolvedLegacyConnector, MigrationFinding> {
    match connector_type.trim().to_ascii_lowercase().as_str() {
        "http" => Ok(ResolvedLegacyConnector {
            provider_id: DRIVER_HTTP,
            implementation: ImplementationKind::Rust,
            tier: EditionTier::Community,
        }),
        "db" => {
            let Some(conn_str) = config.get("connection_string").and_then(Value::as_str) else {
                return Err(MigrationFinding {
                    entity_kind: "connector",
                    entity_name: connector_name.to_string(),
                    field: "config.connection_string",
                    code: "MIGRATION_MISSING_CONNECTION_STRING",
                    message: "Legacy 'db' connector requires config.connection_string to determine v2 provider".to_string(),
                });
            };
            let lower = conn_str.trim().to_ascii_lowercase();
            if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
                Ok(ResolvedLegacyConnector {
                    provider_id: DRIVER_POSTGRESQL,
                    implementation: ImplementationKind::Rust,
                    tier: EditionTier::Community,
                })
            } else if lower.starts_with("sqlite:") {
                Ok(ResolvedLegacyConnector {
                    provider_id: DRIVER_SQLITE,
                    implementation: ImplementationKind::Rust,
                    tier: EditionTier::Community,
                })
            } else if lower.starts_with("mysql://") || lower.starts_with("mariadb://") {
                Ok(ResolvedLegacyConnector {
                    provider_id: DRIVER_MYSQL,
                    implementation: ImplementationKind::Rust,
                    tier: EditionTier::Community,
                })
            } else if lower.starts_with("mongodb://") || lower.starts_with("mongodb+srv://") {
                Ok(ResolvedLegacyConnector {
                    provider_id: ENTERPRISE_DRIVER_MONGODB,
                    implementation: ImplementationKind::Rust,
                    tier: EditionTier::Enterprise,
                })
            } else {
                Err(MigrationFinding {
                    entity_kind: "connector",
                    entity_name: connector_name.to_string(),
                    field: "config.connection_string",
                    code: "MIGRATION_UNSUPPORTED_DB_SCHEME",
                    message: format!(
                        "Cannot determine v2 provider for 'db' connector '{connector_name}' from connection_string scheme"
                    ),
                })
            }
        }
        "cache" => {
            let backend = config
                .get("backend")
                .and_then(Value::as_str)
                .unwrap_or("memory")
                .trim()
                .to_ascii_lowercase();
            match backend.as_str() {
                "memory" => Ok(ResolvedLegacyConnector {
                    provider_id: DRIVER_MEMORY_CACHE,
                    implementation: ImplementationKind::Rust,
                    tier: EditionTier::Community,
                }),
                "redis" => Ok(ResolvedLegacyConnector {
                    provider_id: ENTERPRISE_DRIVER_REDIS,
                    implementation: ImplementationKind::Rust,
                    tier: EditionTier::Enterprise,
                }),
                other => Err(MigrationFinding {
                    entity_kind: "connector",
                    entity_name: connector_name.to_string(),
                    field: "config.backend",
                    code: "MIGRATION_UNSUPPORTED_CACHE_BACKEND",
                    message: format!("Unsupported legacy cache backend '{other}'"),
                }),
            }
        }
        "es" => Ok(ResolvedLegacyConnector {
            provider_id: ENTERPRISE_DRIVER_ELASTICSEARCH,
            implementation: ImplementationKind::Rust,
            tier: EditionTier::Enterprise,
        }),
        "kafka" => Ok(ResolvedLegacyConnector {
            provider_id: ENTERPRISE_DRIVER_KAFKA,
            implementation: ImplementationKind::Rust,
            tier: EditionTier::Enterprise,
        }),
        "smtp" => Ok(ResolvedLegacyConnector {
            provider_id: ENTERPRISE_DRIVER_SMTP,
            implementation: ImplementationKind::Rust,
            tier: EditionTier::Enterprise,
        }),
        "storage" => Ok(ResolvedLegacyConnector {
            provider_id: ENTERPRISE_DRIVER_STORAGE,
            implementation: ImplementationKind::Rust,
            tier: EditionTier::Enterprise,
        }),
        other => Err(MigrationFinding {
            entity_kind: "connector",
            entity_name: connector_name.to_string(),
            field: "connector_type",
            code: "MIGRATION_UNKNOWN_CONNECTOR_TYPE",
            message: format!("Unknown legacy connector_type '{other}'"),
        }),
    }
}

/// Map a legacy v1 workflow connector function and its resolved connector target
/// to the v2 `(provider_id, operation, EditionTier)` (`V2-14.1`, `V2-14.5`).
pub fn map_legacy_function(
    workflow_name: &str,
    function_name: &str,
    connector: &ResolvedLegacyConnector,
) -> Result<ResolvedLegacyFunction, MigrationFinding> {
    let operation = match (function_name, connector.provider_id) {
        ("http_call", DRIVER_HTTP) => "request",
        ("db_read", DRIVER_POSTGRESQL | DRIVER_SQLITE | DRIVER_MYSQL) => "query",
        ("db_write", DRIVER_POSTGRESQL | DRIVER_SQLITE | DRIVER_MYSQL) => "execute",
        (
            "data_query",
            DRIVER_POSTGRESQL
            | DRIVER_SQLITE
            | DRIVER_MYSQL
            | ENTERPRISE_DRIVER_MONGODB
            | ENTERPRISE_DRIVER_ELASTICSEARCH,
        ) => "data_query",
        (
            "data_write",
            DRIVER_POSTGRESQL
            | DRIVER_SQLITE
            | DRIVER_MYSQL
            | ENTERPRISE_DRIVER_MONGODB
            | ENTERPRISE_DRIVER_ELASTICSEARCH,
        ) => "data_write",
        ("mongo_read", ENTERPRISE_DRIVER_MONGODB) => "mongo_read",
        ("mongo_write", ENTERPRISE_DRIVER_MONGODB) => "mongo_write",
        ("cache_read", DRIVER_MEMORY_CACHE | ENTERPRISE_DRIVER_REDIS) => "get",
        ("cache_write", DRIVER_MEMORY_CACHE | ENTERPRISE_DRIVER_REDIS) => "set",
        ("publish_kafka", ENTERPRISE_DRIVER_KAFKA) => "publish",
        ("send_email", ENTERPRISE_DRIVER_SMTP) => "send",
        ("storage_presign", ENTERPRISE_DRIVER_STORAGE) => "presign",
        ("storage_head", ENTERPRISE_DRIVER_STORAGE) => "head",
        _ => {
            return Err(MigrationFinding {
                entity_kind: "workflow",
                entity_name: workflow_name.to_string(),
                field: "tasks.function",
                code: "MIGRATION_INCOMPATIBLE_FUNCTION_CONNECTOR",
                message: format!(
                    "Legacy function '{function_name}' is incompatible with resolved provider '{}'",
                    connector.provider_id
                ),
            });
        }
    };

    Ok(ResolvedLegacyFunction {
        provider_id: connector.provider_id,
        operation,
        tier: connector.tier,
    })
}

/// Check whether a resolved legacy connector is supported by `root` (`V2-14.5`).
///
/// On a Community Edition node (`CompositionRoot::community()`), any connector
/// that resolves to an Enterprise provider produces an explicit
/// `ENTERPRISE_PROVIDER_REQUIRED` finding.
pub fn check_connector_availability(
    root: &CompositionRoot,
    connector_name: &str,
    resolved: &ResolvedLegacyConnector,
) -> Result<(), MigrationFinding> {
    if root
        .providers
        .get(resolved.implementation, resolved.provider_id)
        .is_some()
    {
        return Ok(());
    }

    let code = match resolved.tier {
        EditionTier::Enterprise => "ENTERPRISE_PROVIDER_REQUIRED",
        EditionTier::Community => "PROVIDER_NOT_REGISTERED",
    };
    Err(MigrationFinding {
        entity_kind: "connector",
        entity_name: connector_name.to_string(),
        field: "connector_type",
        code,
        message: format!(
            "Connector '{connector_name}' requires {} provider '{}' which is not registered in this assembly",
            resolved.tier.as_str(),
            resolved.provider_id
        ),
    })
}

/// Check whether a channel protocol is supported by `root` (`V2-14.5`, `DEC-005`).
pub fn check_channel_protocol_availability(
    root: &CompositionRoot,
    channel_name: &str,
    protocol: &str,
) -> Result<(), MigrationFinding> {
    if root.channels.get(protocol).is_some() {
        return Ok(());
    }
    let lower = protocol.trim().to_ascii_lowercase();
    let is_enterprise = matches!(lower.as_str(), "kafka" | "graphql" | "sse");
    Err(MigrationFinding {
        entity_kind: "channel",
        entity_name: channel_name.to_string(),
        field: "protocol",
        code: if is_enterprise {
            "ENTERPRISE_CHANNEL_DRIVER_REQUIRED"
        } else {
            "UNSUPPORTED_CHANNEL_PROTOCOL"
        },
        message: if is_enterprise {
            format!(
                "Channel '{channel_name}' uses protocol '{lower}', which is an Enterprise native Rust ChannelDriver"
            )
        } else {
            format!("Channel '{channel_name}' uses unknown protocol '{lower}'")
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn community_root_accepts_foundation_connectors_and_flags_enterprise_connectors() {
        let root = CompositionRoot::community();

        // 1. PostgreSQL -> Community org.orion.postgresql (available)
        let pg = map_legacy_connector(
            "orders-pg",
            "db",
            &json!({"connection_string": "postgres://localhost/orders"}),
        )
        .expect("maps pg");
        assert_eq!(pg.provider_id, DRIVER_POSTGRESQL);
        assert_eq!(pg.tier, EditionTier::Community);
        assert!(check_connector_availability(&root, "orders-pg", &pg).is_ok());

        // 2. SQLite -> Community org.orion.sqlite (available)
        let sqlite = map_legacy_connector(
            "local-sqlite",
            "db",
            &json!({"connection_string": "sqlite::memory:"}),
        )
        .expect("maps sqlite");
        assert_eq!(sqlite.provider_id, DRIVER_SQLITE);
        assert_eq!(sqlite.tier, EditionTier::Community);
        assert!(check_connector_availability(&root, "local-sqlite", &sqlite).is_ok());

        // 3. Memory Cache -> Community org.orion.memory-cache (available)
        let mem = map_legacy_connector("local-cache", "cache", &json!({"backend": "memory"}))
            .expect("maps memory cache");
        assert_eq!(mem.provider_id, DRIVER_MEMORY_CACHE);
        assert!(check_connector_availability(&root, "local-cache", &mem).is_ok());

        // 4. MySQL and MariaDB -> Community org.orion.mysql (available)
        for url in ["mysql://localhost/orders", "mariadb://localhost/orders"] {
            let mysql =
                map_legacy_connector("legacy-mysql", "db", &json!({"connection_string": url}))
                    .expect("maps mysql");
            assert_eq!(mysql.provider_id, DRIVER_MYSQL);
            assert_eq!(mysql.tier, EditionTier::Community);
            assert!(check_connector_availability(&root, "legacy-mysql", &mysql).is_ok());
        }

        // 5. Redis Cache -> Enterprise com.orion.enterprise.redis (flagged on Community root)
        let redis = map_legacy_connector("shared-redis", "cache", &json!({"backend": "redis"}))
            .expect("maps redis");
        assert_eq!(redis.provider_id, ENTERPRISE_DRIVER_REDIS);
        let finding = check_connector_availability(&root, "shared-redis", &redis)
            .expect_err("redis must require enterprise on community root");
        assert_eq!(finding.code, "ENTERPRISE_PROVIDER_REQUIRED");

        // 6. Ambiguous db scheme stops conversion rather than guessing a default provider.
        let bad_db = map_legacy_connector(
            "unknown-db",
            "db",
            &json!({"connection_string": "cockroach://localhost/db"}),
        )
        .expect_err("unknown scheme must fail");
        assert_eq!(bad_db.code, "MIGRATION_UNSUPPORTED_DB_SCHEME");

        // 7. Channel protocol check on Community root: rest/http/cron/source pass; kafka/graphql flag Enterprise.
        assert!(check_channel_protocol_availability(&root, "api", "rest").is_ok());
        assert!(check_channel_protocol_availability(&root, "poller", "source").is_ok());
        let kafka_ch = check_channel_protocol_availability(&root, "events", "kafka")
            .expect_err("kafka channel requires Enterprise driver");
        assert_eq!(kafka_ch.code, "ENTERPRISE_CHANNEL_DRIVER_REQUIRED");
    }
}

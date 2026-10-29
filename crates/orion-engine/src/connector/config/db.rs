use serde::{Deserialize, Serialize};

use super::{DialectGuards, OperationGates};

/// SQL connector. The backend is selected by the `connection_string` scheme
/// (`postgres://`, `mysql://`, `sqlite:`). Credentials belong in the
/// connection string, which is why there is no `auth`.
///
/// `driver` names the backend for a reader that cannot see the scheme — a
/// connection string that is a secret reference or a `${VAR}` placeholder —
/// which is what `compile --format deployment` needs to pick the provider.
/// It is not what selects the backend at run time; the scheme still is. So
/// it is held to the scheme instead: where the scheme is visible, a `driver`
/// that disagrees is refused at create, update and lint
/// ([`DbDriver::for_connection_string`]). Until 1.0 a free-form `driver`
/// existed that nothing read and nothing checked, which made `driver:
/// "mysql"` with a `postgres://` URL look meaningful (proposal F22); the
/// agreement check is what makes the field honest this time. A definition
/// set must name it on every `db` connector (`lint`'s `connector.db_driver`);
/// the single-entity admin API does not require it.
///
/// `retry` was accepted here until 1.0 and never read (proposal F7): the only
/// reader of a [`super::RetryConfig`] is `http_call`. Re-driving a database call is
/// not the same problem as re-driving an HTTP one — a statement that timed out
/// may already have been applied, exactly the hazard F8 closed for POST — and
/// the pool's own `connect_timeout_ms` already bounds connection setup, so
/// there was nothing safe left for the field to mean. Timeouts and pool sizing
/// are the knobs that exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbConnectorConfig {
    pub connection_string: String,
    /// The backend this connector reaches, stated. `None` when absent — and
    /// when a stored row carries a pre-1.0 free-form value (`"postgres"`,
    /// `"mongodb"`), which loads as it always did, ignored; the admin door
    /// refuses anything but the three names.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "DbDriver::deserialize_lenient"
    )]
    pub driver: Option<DbDriver>,
    #[serde(default)]
    pub max_connections: Option<u32>,
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
    #[serde(default)]
    pub query_timeout_ms: Option<u64>,
    /// Allow connecting to private/internal IP addresses. Default false
    /// (SSRF protection) — the same opt-out the HTTP connector carries. A database on a private network is the normal case, so this is
    /// the field most deployments will set; it exists so that reaching
    /// `169.254.169.254` or a neighbouring service is a deliberate act rather
    /// than the default. Ignored for `sqlite:`, which opens a file.
    #[serde(default)]
    pub allow_private_urls: bool,
    /// Which operations workflows may run through this connector.
    #[serde(default)]
    pub operations: OperationGates,
    /// Which tables the portable dialect may reach through this connector.
    #[serde(default)]
    pub dialect: DialectGuards,
}

/// The backend a `db` connector names in its `driver` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbDriver {
    Postgresql,
    Mysql,
    Sqlite,
}

impl DbDriver {
    /// Every value `driver` may take, as written.
    pub const NAMES: &'static [&'static str] = &["postgresql", "mysql", "sqlite"];

    /// The driver `name` spells, if it is one of [`Self::NAMES`].
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "postgresql" => Some(Self::Postgresql),
            "mysql" => Some(Self::Mysql),
            "sqlite" => Some(Self::Sqlite),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgresql => "postgresql",
            Self::Mysql => "mysql",
            Self::Sqlite => "sqlite",
        }
    }

    /// The driver a connection-string scheme selects: `postgres` and
    /// `postgresql`, `mysql` and `mariadb`, `sqlite`.
    pub fn from_scheme(scheme: &str) -> Option<Self> {
        match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Some(Self::Postgresql),
            "mysql" | "mariadb" => Some(Self::Mysql),
            "sqlite" => Some(Self::Sqlite),
            _ => None,
        }
    }

    /// What `connection_string`'s scheme says about the driver: `None` when
    /// the scheme is not visible — the string is a secret reference
    /// (`env://…`) or a `${VAR}` placeholder, which only the serving node
    /// resolves — else `Some` of the driver the scheme selects, itself
    /// `None` for a scheme no SQL backend serves (which the endpoint check
    /// refuses on its own).
    pub fn for_connection_string(connection_string: &str) -> Option<Option<Self>> {
        let trimmed = connection_string.trim_start();
        if trimmed.starts_with("${")
            || crate::connector::secrets::is_secret_reference(connection_string)
        {
            return None;
        }
        let scheme = trimmed.split_once(':').map(|(scheme, _)| scheme)?;
        Some(Self::from_scheme(scheme))
    }

    /// A stored row's `driver`, read as tolerantly as a removed field is:
    /// one of the three names, or `None` for anything else. Refusing an
    /// unknown value here would drop a pre-1.0 row that carried one from
    /// the registry; refusing it is the admin door's job.
    fn deserialize_lenient<'de, D>(deserializer: D) -> Result<Option<Self>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Option::<serde_json::Value>::deserialize(deserializer)?;
        Ok(value
            .as_ref()
            .and_then(|v| v.as_str())
            .and_then(Self::from_name))
    }
}

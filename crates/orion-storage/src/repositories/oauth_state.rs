//! Managed-OAuth2 runtime state (#268): the token state a connector's OAuth2
//! refresh keeps, one row per connector name.
//!
//! Its own table rather than part of a connector's configuration: the
//! configuration is declarative and deployed, and rotation state mutating it
//! would race its owner. `state_json` is encrypted at rest when
//! `storage.connector_encryption_key` is set.

use async_trait::async_trait;
use sea_query::{Expr, ExprTrait, Query};

use crate::models::ConnectorOauthStateRow;
use crate::schema::ConnectorOauthState as S;
use crate::{DbPool, build_sqlx};
use orion_common::config_encryption::ConfigCipher;
use orion_common::errors::OrionError;

#[async_trait]
pub trait OAuthStateRepository: Send + Sync {
    /// The stored OAuth2 token state for `connector_name`, decrypted.
    /// `None` when the connector has never refreshed on this estate.
    async fn get_oauth_state(
        &self,
        connector_name: &str,
    ) -> Result<Option<ConnectorOauthStateRow>, OrionError>;

    /// Upsert the OAuth2 token state for `connector_name`.
    async fn put_oauth_state(
        &self,
        connector_name: &str,
        fingerprint: &str,
        state_json: &str,
    ) -> Result<(), OrionError>;

    /// Drop the OAuth2 token state for `connector_name` (no-op when absent).
    async fn delete_oauth_state(&self, connector_name: &str) -> Result<(), OrionError>;
}

pub struct SqlOAuthStateRepository {
    pool: DbPool,
    /// Present when `storage.connector_encryption_key` is set. Writes
    /// encrypt `state_json`; reads decrypt, with plaintext pass-through for
    /// rows written before the key existed.
    cipher: Option<std::sync::Arc<ConfigCipher>>,
}

impl SqlOAuthStateRepository {
    pub fn new(pool: DbPool, cipher: Option<std::sync::Arc<ConfigCipher>>) -> Self {
        Self { pool, cipher }
    }

    /// The form `state_json` takes in the database.
    fn store_form(&self, state_json: &str) -> Result<String, OrionError> {
        match &self.cipher {
            Some(cipher) => cipher.encrypt(state_json),
            None => Ok(state_json.to_string()),
        }
    }

    /// Undo [`Self::store_form`]: decrypt when the key is set, pass plaintext
    /// through when it is not, and refuse loudly when a row is encrypted and
    /// no key is configured — serving the literal `enc:v1:…` envelope would
    /// send it upstream as a bearer credential.
    fn open(&self, value: String, connector_name: &str) -> Result<String, OrionError> {
        match &self.cipher {
            Some(cipher) => cipher.decrypt(&value),
            None if ConfigCipher::is_encrypted(&value) => Err(OrionError::internal(format!(
                "oauth state for connector '{connector_name}' is encrypted at rest but \
                 storage.connector_encryption_key is not set"
            ))),
            None => Ok(value),
        }
    }
}

#[async_trait]
impl OAuthStateRepository for SqlOAuthStateRepository {
    async fn get_oauth_state(
        &self,
        connector_name: &str,
    ) -> Result<Option<ConnectorOauthStateRow>, OrionError> {
        orion_common::metrics::timed_db_op("connectors.get_oauth_state", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::select()
                    .columns([S::Fingerprint, S::StateJson])
                    .from(S::Table)
                    .and_where(Expr::col(S::ConnectorName).eq(connector_name)),
            );
            let row = self
                .pool
                .fetch_optional_as::<ConnectorOauthStateRow>(&sql, values)
                .await?;
            match row {
                None => Ok(None),
                Some(mut row) => {
                    row.state_json = self.open(row.state_json, connector_name)?;
                    Ok(Some(row))
                }
            }
        })
        .await
    }

    async fn put_oauth_state(
        &self,
        connector_name: &str,
        fingerprint: &str,
        state_json: &str,
    ) -> Result<(), OrionError> {
        orion_common::metrics::timed_db_op("connectors.put_oauth_state", async {
            let stored = self.store_form(state_json)?;
            let now = Expr::cust(super::helpers::sql_now(self.pool.backend()));
            let mut insert = Query::insert()
                .into_table(S::Table)
                .columns([S::ConnectorName, S::Fingerprint, S::StateJson, S::UpdatedAt])
                .values_panic([
                    connector_name.into(),
                    fingerprint.into(),
                    stored.into(),
                    now,
                ])
                .to_owned();
            insert.on_conflict(
                sea_query::OnConflict::column(S::ConnectorName)
                    .update_columns([S::Fingerprint, S::StateJson, S::UpdatedAt])
                    .to_owned(),
            );
            let (sql, values) = build_sqlx(self.pool.backend(), &mut insert);
            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }

    async fn delete_oauth_state(&self, connector_name: &str) -> Result<(), OrionError> {
        orion_common::metrics::timed_db_op("connectors.delete_oauth_state", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::delete()
                    .from_table(S::Table)
                    .and_where(Expr::col(S::ConnectorName).eq(connector_name)),
            );
            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 32-byte key, hex-encoded — the shape `storage.connector_encryption_key`
    /// takes. Fixed rather than random so a failure is reproducible.
    const TEST_KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn encrypting(pool: &DbPool) -> SqlOAuthStateRepository {
        SqlOAuthStateRepository::new(
            pool.clone(),
            Some(std::sync::Arc::new(
                ConfigCipher::from_hex(TEST_KEY).expect("key"),
            )),
        )
    }

    /// With the key set, the state comes back as it went in — and it really
    /// is encrypted underneath: a repository with no cipher refuses it,
    /// without quoting the state it refused to open.
    #[tokio::test]
    async fn oauth_state_round_trips_through_the_cipher() {
        let pool = crate::test_sqlite_pool().await;
        let repo = encrypting(&pool);
        repo.put_oauth_state("oauth-rt", "fp1", r#"{"access_token":"abc"}"#)
            .await
            .expect("put");

        let opened = repo
            .get_oauth_state("oauth-rt")
            .await
            .expect("read")
            .expect("state is present");
        assert_eq!(opened.state_json, r#"{"access_token":"abc"}"#);
        assert_eq!(opened.fingerprint, "fp1");

        let plain = SqlOAuthStateRepository::new(pool.clone(), None);
        let err = plain
            .get_oauth_state("oauth-rt")
            .await
            .expect_err("encrypted oauth state with no key must not be served as-is");
        let message = err.to_string();
        assert!(message.contains("connector_encryption_key"), "{message}");
        assert!(!message.contains("abc"), "{message}");

        repo.delete_oauth_state("oauth-rt").await.expect("delete");
        assert!(
            repo.get_oauth_state("oauth-rt")
                .await
                .expect("read")
                .is_none()
        );
    }

    /// A row written before the key existed still reads once one is set.
    #[tokio::test]
    async fn plaintext_state_reads_once_a_key_is_configured() {
        let pool = crate::test_sqlite_pool().await;
        SqlOAuthStateRepository::new(pool.clone(), None)
            .put_oauth_state("legacy", "fp0", r#"{"access_token":"old"}"#)
            .await
            .expect("put without a key");
        let read = encrypting(&pool)
            .get_oauth_state("legacy")
            .await
            .expect("read")
            .expect("present");
        assert_eq!(read.state_json, r#"{"access_token":"old"}"#);
    }
}

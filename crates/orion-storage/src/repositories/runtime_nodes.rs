//! Each node's reported deployment status — desired and applied revision,
//! whether its configuration is degraded, the revisions it retains — one row
//! per node in `runtime_nodes`. A node upserts its own row; every node reads
//! all of them to see its peers' convergence and which revisions are still
//! in use. The security lease is judged on the database's clock, so the
//! upsert stamps it with the database's `now`, not the node's.

use async_trait::async_trait;
use sea_query::{Expr, Query};

use crate::models::RuntimeNodeRow;
use crate::schema::RuntimeNodes as N;
use crate::{DbPool, build_sqlx};
use orion_common::errors::OrionError;

#[async_trait]
pub trait RuntimeNodeRepository: Send + Sync {
    /// Insert or replace `row`'s node, renewing its security lease and
    /// `updated_at` to the database's now.
    async fn upsert_node(&self, row: &RuntimeNodeRow) -> Result<(), OrionError>;

    /// Every node's row. Empty when the table does not exist yet — a node
    /// reading before the migration ran sees no peers rather than an error.
    async fn list_nodes(&self) -> Result<Vec<RuntimeNodeRow>, OrionError>;
}

pub struct SqlRuntimeNodeRepository {
    pool: DbPool,
}

impl SqlRuntimeNodeRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl RuntimeNodeRepository for SqlRuntimeNodeRepository {
    async fn upsert_node(&self, row: &RuntimeNodeRow) -> Result<(), OrionError> {
        orion_common::metrics::timed_db_op("runtime_nodes.upsert", async {
            let now = || Expr::cust(super::helpers::sql_now(self.pool.backend()));
            let mut insert = Query::insert()
                .into_table(N::Table)
                .columns([
                    N::NodeId,
                    N::BootId,
                    N::DesiredRevision,
                    N::AppliedRevision,
                    N::AppliedManifestDigest,
                    N::ConfigurationDegraded,
                    N::RetainedRevisionsJson,
                    N::SecurityLeaseUntil,
                    N::UpdatedAt,
                ])
                .values_panic([
                    row.node_id.clone().into(),
                    row.boot_id.clone().into(),
                    row.desired_revision.into(),
                    row.applied_revision.into(),
                    row.applied_manifest_digest.clone().into(),
                    row.configuration_degraded.into(),
                    row.retained_revisions_json.clone().into(),
                    now(),
                    now(),
                ])
                .to_owned();
            insert.on_conflict(
                sea_query::OnConflict::column(N::NodeId)
                    .update_columns([
                        N::BootId,
                        N::DesiredRevision,
                        N::AppliedRevision,
                        N::AppliedManifestDigest,
                        N::ConfigurationDegraded,
                        N::RetainedRevisionsJson,
                        N::SecurityLeaseUntil,
                        N::UpdatedAt,
                    ])
                    .to_owned(),
            );
            let (sql, values) = build_sqlx(self.pool.backend(), &mut insert);
            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }

    async fn list_nodes(&self) -> Result<Vec<RuntimeNodeRow>, OrionError> {
        orion_common::metrics::timed_db_op("runtime_nodes.list", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::select()
                    .columns([
                        N::NodeId,
                        N::BootId,
                        N::DesiredRevision,
                        N::AppliedRevision,
                        N::AppliedManifestDigest,
                        N::ConfigurationDegraded,
                        N::RetainedRevisionsJson,
                    ])
                    .from(N::Table),
            );
            match self.pool.fetch_all_as::<RuntimeNodeRow>(&sql, values).await {
                Ok(rows) => Ok(rows),
                Err(e) if crate::is_undefined_table(&e) => Ok(Vec::new()),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn row(node: &str, applied: i64) -> RuntimeNodeRow {
        RuntimeNodeRow {
            node_id: node.into(),
            boot_id: "boot-1".into(),
            desired_revision: applied,
            applied_revision: applied,
            applied_manifest_digest: Some(format!("sha256:{applied}")),
            configuration_degraded: false,
            retained_revisions_json: format!("[{applied}]"),
        }
    }

    #[tokio::test]
    async fn an_upsert_replaces_the_nodes_row_and_leaves_its_peers() {
        let repo = SqlRuntimeNodeRepository::new(crate::test_sqlite_pool().await);
        repo.upsert_node(&row("node-a", 1)).await.unwrap();
        repo.upsert_node(&row("node-b", 1)).await.unwrap();
        let mut moved = row("node-a", 2);
        moved.configuration_degraded = true;
        moved.applied_manifest_digest = None;
        repo.upsert_node(&moved).await.unwrap();

        let mut nodes = repo.list_nodes().await.unwrap();
        nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        assert_eq!(nodes, vec![moved, row("node-b", 1)]);
    }
}

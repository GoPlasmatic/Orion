use sea_query::{Expr, ExprTrait, Order, Query, SelectStatement};

use super::super::{ArtifactState, DeploymentError};
use super::{
    ArtifactMetaRecord, ArtifactRecord, IN_CHUNK, execute, fetch_all, fetch_in_chunks,
    insert_if_absent, state_column, to_i64,
};
use crate::storage::schema::ArtifactObjects;
use crate::storage::{DbBackend, DbPool, DbTransaction, build_sqlx};

fn artifact_meta_columns() -> [ArtifactObjects; 4] {
    [
        ArtifactObjects::Digest,
        ArtifactObjects::SizeBytes,
        ArtifactObjects::State,
        ArtifactObjects::DrainEpoch,
    ]
}

fn select_artifacts_with_bytes(chunk: &[String]) -> SelectStatement {
    Query::select()
        .columns(artifact_meta_columns())
        .column(ArtifactObjects::PayloadBytes)
        .from(ArtifactObjects::Table)
        .and_where(Expr::col(ArtifactObjects::Digest).is_in(chunk.iter().cloned()))
        .to_owned()
}

/// The rows of `digests`, without their bytes.
pub(in super::super) async fn artifact_meta(
    tx: &mut DbTransaction,
    digests: &[String],
) -> Result<Vec<ArtifactMetaRecord>, DeploymentError> {
    fetch_in_chunks(tx, digests, |chunk| {
        Query::select()
            .columns(artifact_meta_columns())
            .from(ArtifactObjects::Table)
            .and_where(Expr::col(ArtifactObjects::Digest).is_in(chunk.iter().cloned()))
            .to_owned()
    })
    .await
}

/// The rows of `digests`, without their bytes, read through the pool.
pub(in super::super) async fn artifact_meta_from_pool(
    pool: &DbPool,
    digests: &[String],
) -> Result<Vec<ArtifactMetaRecord>, DeploymentError> {
    let mut out = Vec::new();
    for chunk in digests.chunks(IN_CHUNK) {
        let (sql, values) = build_sqlx(
            pool.backend(),
            Query::select()
                .columns(artifact_meta_columns())
                .from(ArtifactObjects::Table)
                .and_where(Expr::col(ArtifactObjects::Digest).is_in(chunk.iter().cloned())),
        );
        out.extend(
            pool.fetch_all_as::<ArtifactMetaRecord>(&sql, values)
                .await
                .map_err(DeploymentError::read)?,
        );
    }
    Ok(out)
}

/// Every artifact not yet deleted, without its bytes: what a collection
/// pass judges.
pub(in super::super) async fn live_artifacts(
    tx: &mut DbTransaction,
) -> Result<Vec<ArtifactMetaRecord>, DeploymentError> {
    fetch_all(
        tx,
        Query::select()
            .columns(artifact_meta_columns())
            .from(ArtifactObjects::Table)
            .and_where(Expr::col(ArtifactObjects::State).ne(state_column(ArtifactState::Deleted)))
            .order_by(ArtifactObjects::Digest, Order::Asc),
    )
    .await
}

/// The rows of `digests`, with their bytes, inside a transaction.
pub(in super::super) async fn artifacts(
    tx: &mut DbTransaction,
    digests: &[String],
) -> Result<Vec<ArtifactRecord>, DeploymentError> {
    fetch_in_chunks(tx, digests, select_artifacts_with_bytes).await
}

/// The rows of `digests`, with their bytes, read through the pool.
pub(in super::super) async fn artifacts_from_pool(
    pool: &DbPool,
    digests: &[String],
) -> Result<Vec<ArtifactRecord>, DeploymentError> {
    let mut out = Vec::new();
    for chunk in digests.chunks(IN_CHUNK) {
        let (sql, values) = build_sqlx(pool.backend(), &mut select_artifacts_with_bytes(chunk));
        out.extend(
            pool.fetch_all_as::<ArtifactRecord>(&sql, values)
                .await
                .map_err(DeploymentError::read)?,
        );
    }
    Ok(out)
}

/// The digests written or revived within the last `grace_secs` seconds,
/// judged on the database's clock.
pub(in super::super) async fn recent_artifacts(
    tx: &mut DbTransaction,
    grace_secs: u64,
) -> Result<std::collections::HashSet<String>, DeploymentError> {
    let secs = i64::try_from(grace_secs).unwrap_or(i64::MAX);
    let since = match tx.backend() {
        DbBackend::Sqlite => format!("datetime('now', '-{secs} seconds')"),
        DbBackend::Postgres => format!("LOCALTIMESTAMP - ({secs} * interval '1 second')"),
        DbBackend::Mysql => format!("CURRENT_TIMESTAMP - INTERVAL {secs} SECOND"),
    };
    let rows: Vec<(String,)> = fetch_all(
        tx,
        Query::select()
            .column(ArtifactObjects::Digest)
            .from(ArtifactObjects::Table)
            .and_where(Expr::col(ArtifactObjects::CreatedAt).gt(Expr::cust(since))),
    )
    .await?;
    Ok(rows.into_iter().map(|(d,)| d).collect())
}

/// Store `bytes` as the `ACTIVE` artifact `digest`. A row already active
/// with its bytes is left as it is (`false`); a row the collector drained or
/// deleted is revived with these bytes, its grace window restarted (`true`,
/// as for a new row).
pub(in super::super) async fn put_artifact(
    tx: &mut DbTransaction,
    digest: &str,
    bytes: &[u8],
) -> Result<bool, DeploymentError> {
    let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let insert = Query::insert()
        .into_table(ArtifactObjects::Table)
        .columns([
            ArtifactObjects::Digest,
            ArtifactObjects::SizeBytes,
            ArtifactObjects::State,
            ArtifactObjects::PayloadBytes,
        ])
        .values_panic([
            digest.into(),
            size.into(),
            state_column(ArtifactState::Active).into(),
            bytes.to_vec().into(),
        ])
        .to_owned();
    if insert_if_absent(tx, insert, [ArtifactObjects::Digest]).await? > 0 {
        return Ok(true);
    }
    // `created_at` is assigned first: MySQL evaluates a SET list left to
    // right, so a later assignment would read the new `state`.
    let (sql, values) = build_sqlx(
        tx.backend(),
        Query::update()
            .table(ArtifactObjects::Table)
            .value(
                ArtifactObjects::CreatedAt,
                Expr::case(
                    Expr::col(ArtifactObjects::State).ne(state_column(ArtifactState::Active)),
                    Expr::current_timestamp(),
                )
                .finally(Expr::col(ArtifactObjects::CreatedAt)),
            )
            .value(ArtifactObjects::State, state_column(ArtifactState::Active))
            .value(ArtifactObjects::DrainEpoch, Option::<i64>::None)
            .value(ArtifactObjects::PayloadBytes, bytes.to_vec())
            .value(ArtifactObjects::SizeBytes, size)
            .and_where(Expr::col(ArtifactObjects::Digest).eq(digest))
            .and_where(
                Expr::col(ArtifactObjects::State)
                    .ne(state_column(ArtifactState::Active))
                    .or(Expr::col(ArtifactObjects::PayloadBytes).is_null()),
            ),
    );
    Ok(execute(tx, &sql, values).await? > 0)
}

/// One collection pass's moves: `draining` marked at `epoch`, `deleted`
/// dropped to tombstones, `revived` put back to `ACTIVE` with a fresh grace
/// window.
pub(in super::super) async fn move_artifacts(
    tx: &mut DbTransaction,
    epoch: u64,
    draining: &[String],
    deleted: &[String],
    revived: &[String],
) -> Result<(), DeploymentError> {
    let backend = tx.backend();
    for chunk in draining.chunks(IN_CHUNK) {
        let (sql, values) = build_sqlx(
            backend,
            Query::update()
                .table(ArtifactObjects::Table)
                .value(
                    ArtifactObjects::State,
                    state_column(ArtifactState::Draining),
                )
                .value(ArtifactObjects::DrainEpoch, to_i64(epoch))
                .and_where(Expr::col(ArtifactObjects::Digest).is_in(chunk.iter().cloned())),
        );
        execute(tx, &sql, values).await?;
    }
    for chunk in deleted.chunks(IN_CHUNK) {
        let (sql, values) = build_sqlx(
            backend,
            Query::update()
                .table(ArtifactObjects::Table)
                .value(ArtifactObjects::State, state_column(ArtifactState::Deleted))
                .value(ArtifactObjects::PayloadBytes, Option::<Vec<u8>>::None)
                .and_where(Expr::col(ArtifactObjects::Digest).is_in(chunk.iter().cloned())),
        );
        execute(tx, &sql, values).await?;
    }
    for chunk in revived.chunks(IN_CHUNK) {
        let (sql, values) = build_sqlx(
            backend,
            Query::update()
                .table(ArtifactObjects::Table)
                .value(ArtifactObjects::CreatedAt, Expr::current_timestamp())
                .value(ArtifactObjects::State, state_column(ArtifactState::Active))
                .value(ArtifactObjects::DrainEpoch, Option::<i64>::None)
                .and_where(Expr::col(ArtifactObjects::Digest).is_in(chunk.iter().cloned())),
        );
        execute(tx, &sql, values).await?;
    }
    Ok(())
}

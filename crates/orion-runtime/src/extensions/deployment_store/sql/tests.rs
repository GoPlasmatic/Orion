use sea_query::{
    Expr, ExprTrait, LockType, MysqlQueryBuilder, PostgresQueryBuilder, Query, SqliteQueryBuilder,
};

use super::reads::select_head;
use super::*;
use crate::storage::schema::{ArtifactObjects, DeploymentHead};

/// The head lock renders on the backends that have one and nowhere else;
/// the CAS keeps its `WHERE head_revision = …` guard everywhere.
#[test]
fn the_head_lock_and_cas_render_per_backend() {
    let mut stmt = select_head();
    stmt.lock(LockType::Update);
    assert!(stmt.to_string(PostgresQueryBuilder).ends_with("FOR UPDATE"));
    assert!(stmt.to_string(MysqlQueryBuilder).ends_with("FOR UPDATE"));
    assert!(!stmt.to_string(SqliteQueryBuilder).contains("FOR UPDATE"));

    let cas = Query::update()
        .table(DeploymentHead::Table)
        .value(DeploymentHead::HeadRevision, 2i64)
        .and_where(Expr::col(DeploymentHead::SingletonId).eq(HEAD_ROW))
        .and_where(Expr::col(DeploymentHead::HeadRevision).eq(1i64))
        .to_owned();
    let sql = cas.to_string(PostgresQueryBuilder);
    assert!(sql.contains(r#""head_revision" = 1"#), "{sql}");
}

/// A revival restarts the grace window only for a row that was not
/// active, and MySQL reads that before `state` is reassigned.
#[test]
fn a_revival_assigns_created_at_before_state() {
    let stmt = Query::update()
        .table(ArtifactObjects::Table)
        .value(
            ArtifactObjects::CreatedAt,
            Expr::case(
                Expr::col(ArtifactObjects::State).ne("active"),
                Expr::current_timestamp(),
            )
            .finally(Expr::col(ArtifactObjects::CreatedAt)),
        )
        .value(ArtifactObjects::State, "active")
        .to_owned();
    let sql = stmt.to_string(MysqlQueryBuilder);
    let created = sql.find("`created_at` =").expect("created_at set");
    let state = sql.find("`state` = 'active'").expect("state set");
    assert!(created < state, "{sql}");
}

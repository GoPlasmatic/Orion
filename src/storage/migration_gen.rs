//! Migration SQL generator for multiple database backends.
//!
//! Uses sea-query's DDL API to define tables once in Rust, then generates
//! backend-specific SQL for SQLite, PostgreSQL, and MySQL.
//!
//! Run the tests with `cargo test migration_gen` to regenerate migration files.

use sea_query::{
    ColumnDef, Expr, Index, IndexCreateStatement, MysqlQueryBuilder, PostgresQueryBuilder,
    SqliteQueryBuilder, Table, TableCreateStatement,
};

use super::schema::*;

// ============================================================
// Table definitions
// ============================================================

fn workflows_table() -> TableCreateStatement {
    Table::create()
        .table(Workflows::Table)
        .if_not_exists()
        .col(ColumnDef::new(Workflows::WorkflowId).text().not_null())
        .col(ColumnDef::new(Workflows::Version).integer().not_null())
        .col(ColumnDef::new(Workflows::Name).text().not_null())
        .col(ColumnDef::new(Workflows::Description).text())
        .col(
            ColumnDef::new(Workflows::Priority)
                .integer()
                .not_null()
                .default(0),
        )
        .col(
            ColumnDef::new(Workflows::Status)
                .text()
                .not_null()
                .default("draft"),
        )
        .col(
            ColumnDef::new(Workflows::RolloutPercentage)
                .integer()
                .not_null()
                .default(100),
        )
        .col(
            ColumnDef::new(Workflows::ConditionJson)
                .text()
                .not_null()
                .default("true"),
        )
        .col(
            ColumnDef::new(Workflows::TasksJson)
                .text()
                .not_null()
                .default("[]"),
        )
        .col(
            ColumnDef::new(Workflows::Tags)
                .text()
                .not_null()
                .default("[]"),
        )
        .col(
            ColumnDef::new(Workflows::ContinueOnError)
                .boolean()
                .not_null()
                .default(false),
        )
        .col(
            ColumnDef::new(Workflows::CreatedAt)
                .timestamp()
                .not_null()
                .default(Expr::current_timestamp()),
        )
        .col(
            ColumnDef::new(Workflows::UpdatedAt)
                .timestamp()
                .not_null()
                .default(Expr::current_timestamp()),
        )
        .primary_key(
            Index::create()
                .col(Workflows::WorkflowId)
                .col(Workflows::Version),
        )
        .to_owned()
}

fn channels_table() -> TableCreateStatement {
    Table::create()
        .table(Channels::Table)
        .if_not_exists()
        .col(ColumnDef::new(Channels::ChannelId).text().not_null())
        .col(ColumnDef::new(Channels::Version).integer().not_null())
        .col(ColumnDef::new(Channels::Name).text().not_null())
        .col(ColumnDef::new(Channels::Description).text())
        .col(ColumnDef::new(Channels::ChannelType).text().not_null())
        .col(ColumnDef::new(Channels::Protocol).text().not_null())
        .col(ColumnDef::new(Channels::Methods).text())
        .col(ColumnDef::new(Channels::RoutePattern).text())
        .col(ColumnDef::new(Channels::Topic).text())
        .col(ColumnDef::new(Channels::ConsumerGroup).text())
        .col(
            ColumnDef::new(Channels::TransportConfigJson)
                .text()
                .not_null()
                .default("{}"),
        )
        .col(ColumnDef::new(Channels::WorkflowId).text())
        .col(
            ColumnDef::new(Channels::ConfigJson)
                .text()
                .not_null()
                .default("{}"),
        )
        .col(
            ColumnDef::new(Channels::Status)
                .text()
                .not_null()
                .default("draft"),
        )
        .col(
            ColumnDef::new(Channels::Priority)
                .integer()
                .not_null()
                .default(0),
        )
        .col(
            ColumnDef::new(Channels::CreatedAt)
                .timestamp()
                .not_null()
                .default(Expr::current_timestamp()),
        )
        .col(
            ColumnDef::new(Channels::UpdatedAt)
                .timestamp()
                .not_null()
                .default(Expr::current_timestamp()),
        )
        .primary_key(
            Index::create()
                .col(Channels::ChannelId)
                .col(Channels::Version),
        )
        .to_owned()
}

fn connectors_table() -> TableCreateStatement {
    Table::create()
        .table(Connectors::Table)
        .if_not_exists()
        .col(
            ColumnDef::new(Connectors::Id)
                .text()
                .not_null()
                .primary_key(),
        )
        .col(
            ColumnDef::new(Connectors::Name)
                .text()
                .not_null()
                .unique_key(),
        )
        .col(ColumnDef::new(Connectors::ConnectorType).text().not_null())
        .col(
            ColumnDef::new(Connectors::ConfigJson)
                .text()
                .not_null()
                .default("{}"),
        )
        .col(
            ColumnDef::new(Connectors::Enabled)
                .boolean()
                .not_null()
                .default(true),
        )
        .col(
            ColumnDef::new(Connectors::CreatedAt)
                .timestamp()
                .not_null()
                .default(Expr::current_timestamp()),
        )
        .col(
            ColumnDef::new(Connectors::UpdatedAt)
                .timestamp()
                .not_null()
                .default(Expr::current_timestamp()),
        )
        .to_owned()
}

fn traces_table() -> TableCreateStatement {
    Table::create()
        .table(Traces::Table)
        .if_not_exists()
        .col(
            ColumnDef::new(Traces::Id)
                .text()
                .not_null()
                .primary_key(),
        )
        .col(
            ColumnDef::new(Traces::Channel)
                .text()
                .not_null()
                .default("default"),
        )
        .col(ColumnDef::new(Traces::ChannelId).text())
        .col(
            ColumnDef::new(Traces::Mode)
                .text()
                .not_null()
                .default("sync"),
        )
        .col(
            ColumnDef::new(Traces::Status)
                .text()
                .not_null()
                .default("pending"),
        )
        .col(ColumnDef::new(Traces::InputJson).text())
        .col(ColumnDef::new(Traces::ResultJson).text())
        .col(ColumnDef::new(Traces::ErrorMessage).text())
        .col(ColumnDef::new(Traces::DurationMs).double())
        .col(ColumnDef::new(Traces::StartedAt).timestamp())
        .col(ColumnDef::new(Traces::CompletedAt).timestamp())
        .col(
            ColumnDef::new(Traces::CreatedAt)
                .timestamp()
                .not_null()
                .default(Expr::current_timestamp()),
        )
        .col(
            ColumnDef::new(Traces::UpdatedAt)
                .timestamp()
                .not_null()
                .default(Expr::current_timestamp()),
        )
        .to_owned()
}

// ============================================================
// Index definitions
// ============================================================

fn workflow_indexes() -> Vec<IndexCreateStatement> {
    vec![
        Index::create()
            .name("idx_workflows_status")
            .table(Workflows::Table)
            .col(Workflows::Status)
            .to_owned(),
        Index::create()
            .name("idx_workflows_workflow_id")
            .table(Workflows::Table)
            .col(Workflows::WorkflowId)
            .to_owned(),
        Index::create()
            .name("idx_workflows_priority_name")
            .table(Workflows::Table)
            .col((Workflows::Priority, sea_query::IndexOrder::Desc))
            .col((Workflows::Name, sea_query::IndexOrder::Asc))
            .to_owned(),
    ]
}

fn channel_indexes() -> Vec<IndexCreateStatement> {
    vec![
        Index::create()
            .name("idx_channels_status")
            .table(Channels::Table)
            .col(Channels::Status)
            .to_owned(),
        Index::create()
            .name("idx_channels_name")
            .table(Channels::Table)
            .col(Channels::Name)
            .to_owned(),
        Index::create()
            .name("idx_channels_type_status")
            .table(Channels::Table)
            .col(Channels::ChannelType)
            .col(Channels::Status)
            .to_owned(),
        Index::create()
            .name("idx_channels_channel_id")
            .table(Channels::Table)
            .col(Channels::ChannelId)
            .to_owned(),
        Index::create()
            .name("idx_channels_route")
            .table(Channels::Table)
            .col(Channels::RoutePattern)
            .to_owned(),
        Index::create()
            .name("idx_channels_topic")
            .table(Channels::Table)
            .col(Channels::Topic)
            .to_owned(),
        Index::create()
            .name("idx_channels_workflow")
            .table(Channels::Table)
            .col(Channels::WorkflowId)
            .to_owned(),
    ]
}

fn trace_indexes() -> Vec<IndexCreateStatement> {
    let mut indexes = vec![
        Index::create()
            .name("idx_traces_status")
            .table(Traces::Table)
            .col(Traces::Status)
            .to_owned(),
        Index::create()
            .name("idx_traces_status_channel")
            .table(Traces::Table)
            .col(Traces::Status)
            .col(Traces::Channel)
            .to_owned(),
        Index::create()
            .name("idx_traces_channel")
            .table(Traces::Table)
            .col(Traces::Channel)
            .to_owned(),
        Index::create()
            .name("idx_traces_mode")
            .table(Traces::Table)
            .col(Traces::Mode)
            .to_owned(),
        Index::create()
            .name("idx_traces_created_at")
            .table(Traces::Table)
            .col(Traces::CreatedAt)
            .to_owned(),
    ];

    // channel_id partial index for non-MySQL
    indexes.push(
        Index::create()
            .name("idx_traces_channel_id")
            .table(Traces::Table)
            .col(Traces::ChannelId)
            .to_owned(),
    );

    indexes
}

// ============================================================
// Backend-specific raw SQL (CHECK constraints, triggers, views)
// ============================================================

fn views_sql(_backend: &str) -> String {
    // Views use standard SQL that works on all backends
    let mut sql = String::new();

    sql += "\n-- Latest version per workflow_id\n";
    sql += "CREATE VIEW current_workflows AS\n";
    sql += "SELECT w.*\n";
    sql += "FROM workflows w\n";
    sql += "INNER JOIN (\n";
    sql += "  SELECT workflow_id, MAX(version) AS max_version\n";
    sql += "  FROM workflows\n";
    sql += "  GROUP BY workflow_id\n";
    sql += ") latest ON w.workflow_id = latest.workflow_id AND w.version = latest.max_version;\n";

    sql += "\n-- Latest version per channel_id\n";
    sql += "CREATE VIEW current_channels AS\n";
    sql += "SELECT c.*\n";
    sql += "FROM channels c\n";
    sql += "INNER JOIN (\n";
    sql += "  SELECT channel_id, MAX(version) AS max_version\n";
    sql += "  FROM channels\n";
    sql += "  GROUP BY channel_id\n";
    sql += ") latest ON c.channel_id = latest.channel_id AND c.version = latest.max_version;\n";

    sql
}

fn triggers_sql(backend: &str) -> String {
    match backend {
        "sqlite" => sqlite_triggers(),
        "postgres" => postgres_triggers(),
        "mysql" => mysql_triggers(),
        _ => String::new(),
    }
}

fn sqlite_triggers() -> String {
    r#"
-- Auto-update updated_at
CREATE TRIGGER trg_workflows_updated_at AFTER UPDATE ON workflows
BEGIN
  UPDATE workflows SET updated_at = datetime('now')
  WHERE workflow_id = NEW.workflow_id AND version = NEW.version;
END;

CREATE TRIGGER trg_channels_updated_at AFTER UPDATE ON channels
BEGIN
  UPDATE channels SET updated_at = datetime('now')
  WHERE channel_id = NEW.channel_id AND version = NEW.version;
END;

CREATE TRIGGER trg_connectors_updated_at AFTER UPDATE ON connectors
BEGIN
  UPDATE connectors SET updated_at = datetime('now') WHERE id = NEW.id;
END;

CREATE TRIGGER trg_traces_updated_at AFTER UPDATE ON traces
BEGIN
  UPDATE traces SET updated_at = datetime('now') WHERE id = NEW.id;
END;

-- Only one draft per workflow_id
CREATE TRIGGER trg_workflows_single_draft
BEFORE INSERT ON workflows
WHEN NEW.status = 'draft'
BEGIN
  SELECT RAISE(ABORT, 'Only one draft version allowed per workflow')
  WHERE EXISTS (
    SELECT 1 FROM workflows
    WHERE workflow_id = NEW.workflow_id AND status = 'draft'
  );
END;

-- Only one draft per channel_id
CREATE TRIGGER trg_channels_single_draft
BEFORE INSERT ON channels
WHEN NEW.status = 'draft'
BEGIN
  SELECT RAISE(ABORT, 'Only one draft version allowed per channel')
  WHERE EXISTS (
    SELECT 1 FROM channels
    WHERE channel_id = NEW.channel_id AND status = 'draft'
  );
END;

-- Prevent content changes on active workflows
CREATE TRIGGER trg_workflows_active_immutable
BEFORE UPDATE ON workflows
WHEN OLD.status = 'active'
  AND NEW.status = 'active'
  AND (OLD.name != NEW.name
    OR OLD.description IS NOT NEW.description
    OR OLD.priority != NEW.priority
    OR OLD.condition_json != NEW.condition_json
    OR OLD.tasks_json != NEW.tasks_json
    OR OLD.tags != NEW.tags
    OR OLD.continue_on_error != NEW.continue_on_error)
BEGIN
  SELECT RAISE(ABORT, 'Cannot modify content of active workflows');
END;

-- Prevent content changes on active channels
CREATE TRIGGER trg_channels_active_immutable
BEFORE UPDATE ON channels
WHEN OLD.status = 'active'
  AND NEW.status = 'active'
  AND (OLD.name != NEW.name
    OR OLD.description IS NOT NEW.description
    OR OLD.channel_type != NEW.channel_type
    OR OLD.protocol != NEW.protocol
    OR OLD.methods IS NOT NEW.methods
    OR OLD.route_pattern IS NOT NEW.route_pattern
    OR OLD.topic IS NOT NEW.topic
    OR OLD.consumer_group IS NOT NEW.consumer_group
    OR OLD.workflow_id IS NOT NEW.workflow_id
    OR OLD.config_json != NEW.config_json)
BEGIN
  SELECT RAISE(ABORT, 'Cannot modify content of active channels');
END;
"#
    .to_string()
}

fn postgres_triggers() -> String {
    r#"
-- Generic updated_at trigger function
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = CURRENT_TIMESTAMP;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_workflows_updated_at
    BEFORE UPDATE ON workflows
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER trg_channels_updated_at
    BEFORE UPDATE ON channels
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER trg_connectors_updated_at
    BEFORE UPDATE ON connectors
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER trg_traces_updated_at
    BEFORE UPDATE ON traces
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- Single-draft enforcement via partial unique indexes
CREATE UNIQUE INDEX idx_workflows_single_draft
    ON workflows (workflow_id) WHERE status = 'draft';

CREATE UNIQUE INDEX idx_channels_single_draft
    ON channels (channel_id) WHERE status = 'draft';
"#
    .to_string()
}

fn mysql_triggers() -> String {
    r#"
-- Auto-update updated_at
DELIMITER //
CREATE TRIGGER trg_workflows_updated_at
    BEFORE UPDATE ON workflows
    FOR EACH ROW
BEGIN
    SET NEW.updated_at = CURRENT_TIMESTAMP;
END//

CREATE TRIGGER trg_channels_updated_at
    BEFORE UPDATE ON channels
    FOR EACH ROW
BEGIN
    SET NEW.updated_at = CURRENT_TIMESTAMP;
END//

CREATE TRIGGER trg_connectors_updated_at
    BEFORE UPDATE ON connectors
    FOR EACH ROW
BEGIN
    SET NEW.updated_at = CURRENT_TIMESTAMP;
END//

CREATE TRIGGER trg_traces_updated_at
    BEFORE UPDATE ON traces
    FOR EACH ROW
BEGIN
    SET NEW.updated_at = CURRENT_TIMESTAMP;
END//

-- Single-draft enforcement
CREATE TRIGGER trg_workflows_single_draft
    BEFORE INSERT ON workflows
    FOR EACH ROW
BEGIN
    IF NEW.status = 'draft' THEN
        IF EXISTS (SELECT 1 FROM workflows WHERE workflow_id = NEW.workflow_id AND status = 'draft') THEN
            SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Only one draft version allowed per workflow';
        END IF;
    END IF;
END//

CREATE TRIGGER trg_channels_single_draft
    BEFORE INSERT ON channels
    FOR EACH ROW
BEGIN
    IF NEW.status = 'draft' THEN
        IF EXISTS (SELECT 1 FROM channels WHERE channel_id = NEW.channel_id AND status = 'draft') THEN
            SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Only one draft version allowed per channel';
        END IF;
    END IF;
END//
DELIMITER ;
"#
    .to_string()
}

fn partial_index_sql(backend: &str) -> String {
    match backend {
        "sqlite" | "postgres" => {
            r#"
CREATE INDEX idx_channels_route_partial ON channels(route_pattern) WHERE route_pattern IS NOT NULL;
CREATE INDEX idx_channels_topic_partial ON channels(topic) WHERE topic IS NOT NULL;
CREATE INDEX idx_channels_workflow_partial ON channels(workflow_id) WHERE workflow_id IS NOT NULL;
CREATE INDEX idx_traces_channel_id_partial ON traces(channel_id) WHERE channel_id IS NOT NULL;
"#
            .to_string()
        }
        // MySQL doesn't support partial indexes
        _ => String::new(),
    }
}

// ============================================================
// Public generator function
// ============================================================

/// Generate the complete migration SQL for a given backend.
pub fn generate_migration(backend: &str) -> String {
    let tables = [
        workflows_table(),
        channels_table(),
        connectors_table(),
        traces_table(),
    ];

    let mut sql = format!(
        "-- Auto-generated migration for {} backend\n-- Generated by Orion migration_gen\n\n",
        backend
    );

    // Generate CREATE TABLE statements
    for table in &tables {
        let ddl = match backend {
            "sqlite" => table.build(SqliteQueryBuilder),
            "postgres" => table.build(PostgresQueryBuilder),
            "mysql" => table.build(MysqlQueryBuilder),
            _ => panic!("Unsupported backend: {backend}"),
        };
        sql += &ddl;
        sql += ";\n\n";
    }

    // Generate indexes
    let all_indexes: Vec<IndexCreateStatement> = [
        workflow_indexes(),
        channel_indexes(),
        trace_indexes(),
    ]
    .into_iter()
    .flatten()
    .collect();

    for idx in &all_indexes {
        let ddl = match backend {
            "sqlite" => idx.build(SqliteQueryBuilder),
            "postgres" => idx.build(PostgresQueryBuilder),
            "mysql" => idx.build(MysqlQueryBuilder),
            _ => panic!("Unsupported backend: {backend}"),
        };
        sql += &ddl;
        sql += ";\n";
    }

    // Partial indexes (backend-specific raw SQL)
    sql += &partial_index_sql(backend);

    // Views (standard SQL)
    sql += &views_sql(backend);

    // Triggers (backend-specific raw SQL)
    sql += &triggers_sql(backend);

    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_sqlite_migration() {
        let sql = generate_migration("sqlite");
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(sql.contains("workflows"));
        assert!(sql.contains("channels"));
        assert!(sql.contains("connectors"));
        assert!(sql.contains("traces"));
        assert!(sql.contains("CREATE VIEW current_workflows"));
        assert!(sql.contains("CREATE TRIGGER trg_workflows_updated_at"));
        assert!(sql.contains("CREATE TRIGGER trg_workflows_single_draft"));
    }

    #[test]
    fn test_generate_postgres_migration() {
        let sql = generate_migration("postgres");
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(sql.contains("update_updated_at_column"));
        assert!(sql.contains("idx_workflows_single_draft"));
        // Postgres uses BOOLEAN not INTEGER for booleans
        assert!(!sql.contains("INTEGER NOT NULL DEFAULT FALSE"));
    }

    #[test]
    fn test_generate_mysql_migration() {
        let sql = generate_migration("mysql");
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(sql.contains("DELIMITER //"));
        assert!(sql.contains("SIGNAL SQLSTATE"));
    }

    #[test]
    #[ignore] // Run manually with: cargo test test_write_migration -- --ignored
    fn test_write_migration_files() {
        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        for backend in &["sqlite", "postgres", "mysql"] {
            let sql = generate_migration(backend);
            let dir = format!("{manifest_dir}/migrations/{backend}");
            std::fs::create_dir_all(&dir).unwrap();
            let path = format!("{dir}/001_initial.sql");
            std::fs::write(&path, &sql).unwrap();
            eprintln!("Wrote {path} ({} bytes)", sql.len());
        }
    }

    #[test]
    fn test_all_backends_have_views() {
        for backend in &["sqlite", "postgres", "mysql"] {
            let sql = generate_migration(backend);
            assert!(
                sql.contains("CREATE VIEW current_workflows"),
                "{backend} missing current_workflows view"
            );
            assert!(
                sql.contains("CREATE VIEW current_channels"),
                "{backend} missing current_channels view"
            );
        }
    }
}

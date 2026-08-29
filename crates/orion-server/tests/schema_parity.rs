//! Cross-backend schema parity (proposal D10).
//!
//! Orion ships three hand-written migration sets. Nothing compared them, which
//! is how a migration added to two of three backends stayed invisible until a
//! container test happened to touch the column, and how the 0.3.0 `INT4`
//! incident (Postgres `integer` where the row struct decoded `i64`) shipped.
//!
//! This binary migrates each backend from scratch, introspects it and asserts
//! the three agree on:
//!
//! - the set of base tables, and per table `{column → logical type, nullable}`;
//! - the `idx_*` indexes **and the ordered column list of each one** — an index
//!   that exists everywhere but covers `(created_at)` on one backend and
//!   `(created_at, id)` on another is exactly as broken as a missing one, and
//!   is the failure D8's migration could have introduced;
//! - the column names of each view.
//!
//! ## What "agree" means, and what it deliberately ignores
//!
//! The normaliser is intentionally loose about *spelling*: it is here to catch
//! *a column missing on one backend*, *a width mismatch* and *an index that
//! covers different columns*, not to relitigate dialect syntax. A normaliser
//! that fails on every legitimate difference gets deleted the first week, so
//! these are folded away on purpose:
//!
//! - **String widths.** MySQL cannot index `TEXT` without a prefix length, so
//!   identifier columns are `varchar(n)` there and `text` elsewhere. Both are
//!   `text`.
//! - **Integer widths where a backend does not declare one.** SQLite integers
//!   are always 64-bit whatever the column says, so its width is `None` and
//!   compares equal to anything. Postgres and MySQL both declare widths, so
//!   `bigint` vs `int` between *those two* still fails — which is the shape
//!   the 0.3.0 incident had.
//! - **Index column *direction* and partiality.** `("priority" DESC, "name")`
//!   compares as `[priority, name]`, and the `WHERE` clause of a partial index
//!   is not read: MySQL has neither concept, and the partial indexes are
//!   already allow-listed as Postgres/SQLite-only. One index — the DLQ claim
//!   index — exists everywhere with deliberately different key columns for the
//!   same reason; it is allow-listed by name in `DIVERGENT_INDEX_COLUMNS`.
//! - **Defaults, collation, storage parameters.** Not compared.
//! - **Triggers.** Compared nowhere: SQLite uses `RAISE(ABORT)`, Postgres
//!   `plpgsql` functions and MySQL `SIGNAL`, so even the names diverge for
//!   good reason. The single-draft and active-immutability rules are covered
//!   behaviourally by `storage_postgres.rs` / `storage_mysql.rs`.
//!
//! Nullability *is* compared. A column that is `NOT NULL` on two backends and
//! nullable on the third is the same silent-until-fatal drift as a width
//! mismatch: the Rust model declares one or the other, and the disagreement
//! surfaces as a decode error or a failed insert on one backend only.
//!
//! ## Running it
//!
//! Only the SQLite half runs without Docker. `schema_is_identical_across_backends`
//! is `#[ignore]`d and starts Postgres and MySQL containers; it also runs in
//! the `Schema parity across backends` CI step. Every failure message names
//! the table, the column and both types, so a CI-only failure is diagnosable
//! from the log without a local container.
//!
//! ```text
//! cargo test --test schema_parity                # SQLite only
//! cargo test --test schema_parity -- --ignored   # all three, needs Docker
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use orion::storage::DbBackend;

// ---------------------------------------------------------------------------
// The normalised shape
// ---------------------------------------------------------------------------

/// A column type reduced to what is worth comparing across dialects.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LogicalType {
    /// `text` | `integer` | `boolean` | `real` | `timestamp` | `blob`, or the
    /// raw declared type when nothing matched (so an unknown type is loud
    /// rather than silently equal to everything).
    kind: String,
    /// Declared integer width in bits, when the backend declares one. `None`
    /// for SQLite, whose integers are 64-bit regardless of the declared type.
    width: Option<u32>,
}

impl LogicalType {
    /// Widths are compared only when both sides declare one — see the module
    /// docs for why SQLite abstains.
    fn agrees_with(&self, other: &Self) -> bool {
        if self.kind != other.kind {
            return false;
        }
        match (self.width, other.width) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }
}

impl std::fmt::Display for LogicalType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.width {
            Some(bits) => write!(f, "{}({bits})", self.kind),
            None => write!(f, "{}", self.kind),
        }
    }
}

/// A column, reduced to the two properties worth comparing across dialects.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Column {
    ty: LogicalType,
    /// `false` when the column is declared `NOT NULL`.
    nullable: bool,
}

impl Column {
    fn new(ty: LogicalType, nullable: bool) -> Self {
        Self { ty, nullable }
    }
}

#[derive(Debug, Default)]
struct Schema {
    /// Base tables only: `table → {column → column}`.
    tables: BTreeMap<String, BTreeMap<String, Column>>,
    /// `(table, index name) → ordered column list`, for migration-created
    /// indexes (the `idx_` prefix every `CREATE INDEX` in `migrations/` uses).
    /// Implicit primary-key and unique-constraint indexes are named by the
    /// server, differently on each backend, so they are out of scope.
    indexes: BTreeMap<(String, String), Vec<String>>,
    /// `view → {column names}`. Types are not compared for views: they are
    /// derived by each planner. The names are what matters — a view left
    /// stale by an `ALTER TABLE` is the failure this catches (the scar is
    /// `migrations/postgres/008_recreate_current_views.sql`).
    views: BTreeMap<String, BTreeSet<String>>,
}

/// Objects that legitimately exist on some backends only, with the reason.
/// Anything not listed here must exist everywhere.
const BACKEND_SPECIFIC_INDEXES: &[(&str, &str)] = &[
    (
        "idx_workflows_single_draft",
        "Postgres enforces one-draft-per-id with a partial unique index; \
         SQLite and MySQL use BEFORE INSERT/UPDATE triggers",
    ),
    (
        "idx_channels_single_draft",
        "same as idx_workflows_single_draft",
    ),
    (
        "idx_channels_route_partial",
        "partial index — MySQL has no equivalent",
    ),
    (
        "idx_channels_topic_partial",
        "partial index — MySQL has no equivalent",
    ),
    (
        "idx_channels_workflow_partial",
        "partial index — MySQL has no equivalent",
    ),
    (
        "idx_traces_channel_id_partial",
        "partial index — MySQL has no equivalent",
    ),
];

/// Indexes that exist on all three backends but deliberately cover different
/// columns, with the reason. Presence is still required; only the column list
/// is exempt.
const DIVERGENT_INDEX_COLUMNS: &[(&str, &str)] = &[(
    "idx_trace_dlq_next_retry",
    "SQLite and Postgres put the `retry_count < max_retries` half of the DLQ \
     claim predicate in a partial index's WHERE clause, so the key is \
     (next_retry_at). MySQL has no partial indexes and carries retry_count as \
     a second key column instead. Forcing the column lists to match would mean \
     dropping retry_count from the MySQL index — a regression, not parity.",
)];

fn is_backend_specific(index: &str) -> bool {
    BACKEND_SPECIFIC_INDEXES
        .iter()
        .any(|(name, _)| *name == index)
}

fn columns_may_diverge(index: &str) -> bool {
    DIVERGENT_INDEX_COLUMNS
        .iter()
        .any(|(name, _)| *name == index)
}

/// Reduce a backend's declared type to a [`LogicalType`].
///
/// `column_type` is MySQL's fuller spelling (`tinyint(1)`, `varchar(255)`);
/// pass the same string as `data_type` where a backend has only one.
///
/// The backend matters for exactly one thing: `integer` means int4 on
/// Postgres but "whatever fits, up to 64 bits" on SQLite, so only the former
/// gets a comparable width.
fn normalise(backend: DbBackend, data_type: &str, column_type: &str) -> LogicalType {
    let data = data_type.trim().to_ascii_lowercase();
    // Strip any declared length: `varchar(255)` and `text` are one thing here.
    // D29: this folding is a deliberate blind spot — MySQL's varchar(255)
    // columns (`route_pattern`, `topic`, `consumer_group`) genuinely diverge
    // from the unbounded text SQLite/Postgres use, and this test cannot see
    // it. The divergence is closed at the validation boundary instead:
    // `MAX_VARCHAR_FIELD_LEN` caps those fields at create/update, so a value
    // that stores on two backends and fails on the third cannot be written.
    let bare = data.split('(').next().unwrap_or(&data).trim().to_string();
    let full = column_type.trim().to_ascii_lowercase();

    // MySQL renders `bool` as `tinyint(1)`; a wider tinyint is a number.
    if matches!(bare.as_str(), "bool" | "boolean") || full.starts_with("tinyint(1)") {
        return LogicalType {
            kind: "boolean".to_string(),
            width: None,
        };
    }

    let text = [
        "text",
        "varchar",
        "character varying",
        "char",
        "character",
        "tinytext",
        "mediumtext",
        "longtext",
        "clob",
        "json",
        "jsonb",
    ];
    let real = [
        "double",
        "double precision",
        "float",
        "real",
        "numeric",
        "decimal",
    ];
    let timestamp = [
        "timestamp",
        "timestamp_text",
        "timestamp without time zone",
        "timestamptz",
        "timestamp with time zone",
        "datetime",
    ];
    let blob = ["blob", "bytea", "longblob", "varbinary", "binary"];

    // The declared integer width in bits. SQLite abstains entirely: its
    // storage class is dynamic and every `integer` column holds 64 bits, so
    // it has no width to disagree about.
    let integer_width = |name: &str| -> Option<u32> {
        if backend == DbBackend::Sqlite {
            return None;
        }
        match name {
            "bigint" | "int8" => Some(64),
            "mediumint" => Some(24),
            "integer" | "int" | "int4" => Some(32),
            "smallint" | "int2" => Some(16),
            "tinyint" => Some(8),
            _ => None,
        }
    };

    let kind = if text.contains(&bare.as_str()) {
        "text"
    } else if real.contains(&bare.as_str()) {
        "real"
    } else if timestamp.contains(&bare.as_str()) {
        "timestamp"
    } else if blob.contains(&bare.as_str()) {
        "blob"
    } else if matches!(
        bare.as_str(),
        "integer"
            | "int"
            | "int4"
            | "int8"
            | "bigint"
            | "smallint"
            | "int2"
            | "mediumint"
            | "tinyint"
    ) {
        return LogicalType {
            kind: "integer".to_string(),
            width: integer_width(&bare),
        };
    } else {
        // Unknown: keep the raw spelling so the mismatch is visible rather
        // than being folded into some catch-all bucket.
        return LogicalType {
            kind: bare,
            width: None,
        };
    };

    LogicalType {
        kind: kind.to_string(),
        width: None,
    }
}

// ---------------------------------------------------------------------------
// Introspection, per backend
// ---------------------------------------------------------------------------

/// Migrations bookkeeping, not part of Orion's schema.
const IGNORED_TABLES: &[&str] = &["_sqlx_migrations"];

async fn sqlite_schema() -> Schema {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    // One connection so the in-memory database survives between statements.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::from_str("sqlite::memory:").expect("sqlite url"))
        .await
        .expect("open in-memory sqlite");
    orion::storage::migrator_for(DbBackend::Sqlite)
        .run(&pool)
        .await
        .expect("sqlite migrations must apply");

    let mut schema = Schema::default();

    let objects: Vec<(String, String)> =
        sqlx::query_as("SELECT type, name FROM sqlite_master WHERE type IN ('table', 'view')")
            .fetch_all(&pool)
            .await
            .expect("list sqlite objects");

    for (kind, name) in objects {
        if name.starts_with("sqlite_") || IGNORED_TABLES.contains(&name.as_str()) {
            continue;
        }
        let columns: Vec<(String, String, i64)> =
            sqlx::query_as("SELECT name, type, \"notnull\" FROM pragma_table_info(?)")
                .bind(&name)
                .fetch_all(&pool)
                .await
                .unwrap_or_else(|e| panic!("introspect sqlite {name}: {e}"));
        if kind == "view" {
            schema
                .views
                .insert(name, columns.into_iter().map(|(c, _, _)| c).collect());
        } else {
            schema.tables.insert(
                name,
                columns
                    .into_iter()
                    .map(|(column, declared, notnull)| {
                        let logical = normalise(DbBackend::Sqlite, &declared, &declared);
                        (column, Column::new(logical, notnull == 0))
                    })
                    .collect(),
            );
        }
    }

    // `_` is a LIKE wildcard, hence the ESCAPE — `idx_%` would also match an
    // index called `idxfoo`.
    let indexes: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT m.tbl_name, m.name, ii.name, ii.seqno \
         FROM sqlite_master m JOIN pragma_index_info(m.name) ii \
         WHERE m.type = 'index' AND m.name LIKE 'idx\\_%' ESCAPE '\\' \
         ORDER BY m.name, ii.seqno",
    )
    .fetch_all(&pool)
    .await
    .expect("list sqlite indexes");
    schema.indexes = collect_index_columns(indexes);

    schema
}

/// Fold `(table, index, column, position)` rows into `(table, index) → ordered
/// columns`. Every backend's catalog hands them over in that shape.
fn collect_index_columns(
    rows: Vec<(String, String, String, i64)>,
) -> BTreeMap<(String, String), Vec<String>> {
    let mut by_index: BTreeMap<(String, String), Vec<(i64, String)>> = BTreeMap::new();
    for (table, index, column, position) in rows {
        by_index
            .entry((table, index))
            .or_default()
            .push((position, column));
    }
    by_index
        .into_iter()
        .map(|(key, mut columns)| {
            columns.sort();
            (key, columns.into_iter().map(|(_, c)| c).collect())
        })
        .collect()
}

async fn postgres_schema(pool: &sqlx::PgPool) -> Schema {
    orion::storage::migrator_for(DbBackend::Postgres)
        .run(pool)
        .await
        .expect("postgres migrations must apply");

    let mut schema = Schema::default();

    let columns: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT c.table_name::text, c.column_name::text, c.data_type::text, \
                c.is_nullable::text, t.table_type::text \
         FROM information_schema.columns c \
         JOIN information_schema.tables t \
           ON t.table_schema = c.table_schema AND t.table_name = c.table_name \
         WHERE c.table_schema = 'public'",
    )
    .fetch_all(pool)
    .await
    .expect("introspect postgres columns");

    for (table, column, data_type, is_nullable, table_type) in columns {
        if IGNORED_TABLES.contains(&table.as_str()) {
            continue;
        }
        if table_type == "VIEW" {
            schema.views.entry(table).or_default().insert(column);
        } else {
            schema.tables.entry(table).or_default().insert(
                column,
                Column::new(
                    normalise(DbBackend::Postgres, &data_type, &data_type),
                    is_nullable == "YES",
                ),
            );
        }
    }

    // `pg_indexes.indexdef` would have to be parsed to recover the column
    // list, so go through the catalog: `indkey` is the ordered attribute
    // vector, and `WITH ORDINALITY` preserves that order through the unnest.
    let indexes: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT t.relname::text, ic.relname::text, a.attname::text, k.ord::bigint \
         FROM pg_index i \
         JOIN pg_class ic ON ic.oid = i.indexrelid \
         JOIN pg_class t ON t.oid = i.indrelid \
         JOIN pg_namespace n ON n.oid = ic.relnamespace \
         JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord) ON true \
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
         WHERE n.nspname = 'public' AND ic.relname LIKE 'idx\\_%' \
         ORDER BY ic.relname, k.ord",
    )
    .fetch_all(pool)
    .await
    .expect("introspect postgres indexes");
    schema.indexes = collect_index_columns(indexes);

    schema
}

async fn mysql_schema(pool: &sqlx::MySqlPool) -> Schema {
    orion::storage::migrator_for(DbBackend::Mysql)
        .run(pool)
        .await
        .expect("mysql migrations must apply");

    let mut schema = Schema::default();

    // MySQL 8's information_schema is a view over the data dictionary and
    // returns these columns as VARBINARY, which sqlx refuses to decode into
    // `String` — hence the CAST on every one of them. Without it this whole
    // function panics before it can compare anything.
    let columns: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT CAST(c.TABLE_NAME AS CHAR), CAST(c.COLUMN_NAME AS CHAR), \
                CAST(c.DATA_TYPE AS CHAR), CAST(c.COLUMN_TYPE AS CHAR), \
                CAST(c.IS_NULLABLE AS CHAR), CAST(t.TABLE_TYPE AS CHAR) \
         FROM information_schema.COLUMNS c \
         JOIN information_schema.TABLES t \
           ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME \
         WHERE c.TABLE_SCHEMA = DATABASE()",
    )
    .fetch_all(pool)
    .await
    .expect("introspect mysql columns");

    for (table, column, data_type, column_type, is_nullable, table_type) in columns {
        if IGNORED_TABLES.contains(&table.as_str()) {
            continue;
        }
        if table_type == "VIEW" {
            schema.views.entry(table).or_default().insert(column);
        } else {
            schema.tables.entry(table).or_default().insert(
                column,
                Column::new(
                    normalise(DbBackend::Mysql, &data_type, &column_type),
                    is_nullable == "YES",
                ),
            );
        }
    }

    let indexes: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT CAST(TABLE_NAME AS CHAR), CAST(INDEX_NAME AS CHAR), \
                CAST(COLUMN_NAME AS CHAR), CAST(SEQ_IN_INDEX AS SIGNED) \
         FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = DATABASE() AND INDEX_NAME LIKE 'idx\\_%' \
         ORDER BY INDEX_NAME, SEQ_IN_INDEX",
    )
    .fetch_all(pool)
    .await
    .expect("introspect mysql indexes");
    schema.indexes = collect_index_columns(indexes);

    schema
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

fn nullability(nullable: bool) -> &'static str {
    if nullable { "nullable" } else { "NOT NULL" }
}

/// Compare two backends, collecting every disagreement. Each message names the
/// table, the column and both types, so a CI-only failure is actionable from
/// the log alone.
fn differences(left_name: &str, left: &Schema, right_name: &str, right: &Schema) -> Vec<String> {
    let mut problems = Vec::new();

    let left_tables: BTreeSet<&String> = left.tables.keys().collect();
    let right_tables: BTreeSet<&String> = right.tables.keys().collect();
    for table in left_tables.difference(&right_tables) {
        problems.push(format!(
            "table `{table}` exists on {left_name} but not on {right_name}"
        ));
    }
    for table in right_tables.difference(&left_tables) {
        problems.push(format!(
            "table `{table}` exists on {right_name} but not on {left_name}"
        ));
    }

    for (table, left_columns) in &left.tables {
        let Some(right_columns) = right.tables.get(table) else {
            continue;
        };
        for (column, left_column) in left_columns {
            let left_type = &left_column.ty;
            let Some(right_column) = right_columns.get(column) else {
                problems.push(format!(
                    "{table}.{column} exists on {left_name} ({left_type}) but not on {right_name}"
                ));
                continue;
            };
            let right_type = &right_column.ty;
            if !left_type.agrees_with(right_type) {
                problems.push(format!(
                    "{table}.{column}: {left_name} has {left_type}, {right_name} has {right_type}"
                ));
            }
            if left_column.nullable != right_column.nullable {
                problems.push(format!(
                    "{table}.{column}: {left_name} is {}, {right_name} is {} — the Rust model \
                     declares one of the two, so this fails on one backend only",
                    nullability(left_column.nullable),
                    nullability(right_column.nullable),
                ));
            }
        }
        for column in right_columns.keys() {
            if !left_columns.contains_key(column) {
                problems.push(format!(
                    "{table}.{column} exists on {right_name} but not on {left_name}"
                ));
            }
        }
    }

    for (key, left_columns) in &left.indexes {
        let (table, index) = key;
        if is_backend_specific(index) {
            continue;
        }
        match right.indexes.get(key) {
            None => problems.push(format!(
                "index `{index}` on `{table}` exists on {left_name} but not on {right_name} \
                 — add it to the {right_name} migration set, or to \
                 BACKEND_SPECIFIC_INDEXES with a reason"
            )),
            Some(right_columns) if right_columns != left_columns && !columns_may_diverge(index) => {
                problems.push(format!(
                    "index `{index}` on `{table}` covers {left_columns:?} on {left_name} but \
                     {right_columns:?} on {right_name} — an index with the same name and \
                     different columns serves a different query. Fix the migration set that \
                     is behind, or add it to DIVERGENT_INDEX_COLUMNS with a reason"
                ))
            }
            Some(_) => {}
        }
    }
    for key in right.indexes.keys() {
        let (table, index) = key;
        if !is_backend_specific(index) && !left.indexes.contains_key(key) {
            problems.push(format!(
                "index `{index}` on `{table}` exists on {right_name} but not on {left_name} \
                 — add it to the {left_name} migration set, or to \
                 BACKEND_SPECIFIC_INDEXES with a reason"
            ));
        }
    }

    for (view, left_columns) in &left.views {
        match right.views.get(view) {
            None => problems.push(format!(
                "view `{view}` exists on {left_name} but not on {right_name}"
            )),
            Some(right_columns) if right_columns != left_columns => {
                let missing: Vec<&String> = left_columns.difference(right_columns).collect();
                let extra: Vec<&String> = right_columns.difference(left_columns).collect();
                problems.push(format!(
                    "view `{view}` differs: missing on {right_name} {missing:?}, \
                     extra on {right_name} {extra:?} (a view is not rebuilt by an \
                     ALTER TABLE on Postgres or MySQL — see \
                     migrations/postgres/008_recreate_current_views.sql)"
                ));
            }
            Some(_) => {}
        }
    }
    for view in right.views.keys() {
        if !left.views.contains_key(view) {
            problems.push(format!(
                "view `{view}` exists on {right_name} but not on {left_name}"
            ));
        }
    }

    problems
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Shorthands for "as SQLite / Postgres / MySQL declares it".
fn lite(declared: &str) -> LogicalType {
    normalise(DbBackend::Sqlite, declared, declared)
}
fn pg(declared: &str) -> LogicalType {
    normalise(DbBackend::Postgres, declared, declared)
}
fn my(data_type: &str, column_type: &str) -> LogicalType {
    normalise(DbBackend::Mysql, data_type, column_type)
}

#[test]
fn normaliser_folds_dialect_spellings_together() {
    // The same column, as each backend declares it.
    assert!(lite("text").agrees_with(&my("varchar", "varchar(255)")));
    assert!(lite("timestamp_text").agrees_with(&pg("timestamp without time zone")));
    assert!(lite("timestamp_text").agrees_with(&my("datetime", "datetime")));
    assert!(lite("double").agrees_with(&pg("double precision")));
    assert!(lite("boolean").agrees_with(&my("tinyint", "tinyint(1)")));
    // SQLite's width-free `integer` matches a declared width either way.
    assert!(lite("integer").agrees_with(&pg("bigint")));
    assert!(lite("integer").agrees_with(&my("int", "int")));
}

#[test]
fn normaliser_still_catches_a_width_mismatch_between_declared_backends() {
    // The 0.3.0 incident: Postgres `integer` (int4) where MySQL had `bigint`.
    let narrow = pg("integer");
    let wide = my("bigint", "bigint");
    assert!(
        !narrow.agrees_with(&wide),
        "int4 vs int8 between two backends that both declare a width must fail"
    );
    assert_eq!(narrow.to_string(), "integer(32)");
    assert_eq!(wide.to_string(), "integer(64)");
    // And a genuinely different kind is never folded away.
    assert!(!lite("text").agrees_with(&pg("bigint")));
    assert!(!lite("boolean").agrees_with(&pg("text")));
    // A tinyint that is not a bool stays a number.
    assert!(!my("tinyint", "tinyint(4)").agrees_with(&lite("boolean")));
}

/// The cross-backend run needs Docker, so its failure messages have to be
/// readable from a CI log with no database in front of you. This pins them,
/// locally, against a schema mutated the five ways drift actually happens.
#[tokio::test]
async fn a_difference_names_the_table_the_column_and_both_types() {
    let real = sqlite_schema().await;

    // A column that never made it onto the second backend.
    let mut missing_column = sqlite_schema().await;
    missing_column
        .tables
        .get_mut("traces")
        .expect("traces")
        .remove("channel_id");
    let problems = differences("sqlite", &real, "postgres", &missing_column);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("traces.channel_id")
            && problems[0].contains("sqlite")
            && problems[0].contains("postgres"),
        "message must name the table, the column and both backends: {}",
        problems[0]
    );

    // The 0.3.0 shape: same column, different width.
    let integer = |bits| {
        Column::new(
            LogicalType {
                kind: "integer".to_string(),
                width: Some(bits),
            },
            false,
        )
    };
    let mut narrowed = sqlite_schema().await;
    narrowed
        .tables
        .get_mut("workflows")
        .expect("workflows")
        .insert("version".to_string(), integer(32));
    let mut widened = sqlite_schema().await;
    widened
        .tables
        .get_mut("workflows")
        .expect("workflows")
        .insert("version".to_string(), integer(64));
    let problems = differences("postgres", &narrowed, "mysql", &widened);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("workflows.version")
            && problems[0].contains("integer(32)")
            && problems[0].contains("integer(64)"),
        "message must name both types: {}",
        problems[0]
    );

    // A column that lost its NOT NULL on one backend only.
    let mut relaxed = sqlite_schema().await;
    relaxed
        .tables
        .get_mut("traces")
        .expect("traces")
        .get_mut("created_at")
        .expect("created_at")
        .nullable = true;
    let problems = differences("postgres", &real, "mysql", &relaxed);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("traces.created_at")
            && problems[0].contains("NOT NULL")
            && problems[0].contains("nullable"),
        "message must name the column and both nullabilities: {}",
        problems[0]
    );

    // An index added to one migration set and forgotten in another.
    let mut unindexed = sqlite_schema().await;
    unindexed
        .indexes
        .remove(&("traces".to_string(), "idx_traces_updated_at".to_string()));
    let problems = differences("sqlite", &real, "mysql", &unindexed);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("idx_traces_updated_at") && problems[0].contains("BACKEND_SPECIFIC"),
        "message must name the index and the escape hatch: {}",
        problems[0]
    );

    // An index that exists everywhere but covers different columns — the
    // failure D8's `(created_at, id)` replacement could have introduced by
    // landing on only two of the three backends.
    let mut half_index = sqlite_schema().await;
    half_index.indexes.insert(
        ("traces".to_string(), "idx_traces_created_at_id".to_string()),
        vec!["created_at".to_string()],
    );
    let problems = differences("sqlite", &real, "mysql", &half_index);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("idx_traces_created_at_id")
            && problems[0].contains("\"created_at\", \"id\"")
            && problems[0].contains("different columns"),
        "message must name the index and both column lists: {}",
        problems[0]
    );

    // A backend-specific index is not a difference.
    let mut without_partial = sqlite_schema().await;
    without_partial.indexes.remove(&(
        "traces".to_string(),
        "idx_traces_channel_id_partial".to_string(),
    ));
    assert!(
        differences("sqlite", &real, "mysql", &without_partial).is_empty(),
        "MySQL has no partial indexes; that is allow-listed, not drift"
    );

    // Nor is the one index whose columns are allow-listed to diverge: MySQL
    // carries the partial predicate's column in the key instead.
    let mut mysql_shaped = sqlite_schema().await;
    mysql_shaped.indexes.insert(
        (
            "trace_dlq".to_string(),
            "idx_trace_dlq_next_retry".to_string(),
        ),
        vec!["next_retry_at".to_string(), "retry_count".to_string()],
    );
    assert!(
        differences("sqlite", &real, "mysql", &mysql_shaped).is_empty(),
        "idx_trace_dlq_next_retry is in DIVERGENT_INDEX_COLUMNS with a reason"
    );
}

/// Runs everywhere: proves the introspection actually works, and pins the
/// tables, the sortable-column indexes (D8) and the views it must find.
#[tokio::test]
async fn sqlite_schema_is_introspectable() {
    let schema = sqlite_schema().await;

    for table in [
        "workflows",
        "channels",
        "connectors",
        "traces",
        "trace_dlq",
        "audit_logs",
        "config_epoch",
        "job_leases",
    ] {
        assert!(
            schema.tables.contains_key(table),
            "introspection missed `{table}`; found {:?}",
            schema.tables.keys().collect::<Vec<_>>()
        );
    }

    let traces = &schema.tables["traces"];
    assert_eq!(traces["id"].ty.kind, "text");
    assert!(!traces["id"].nullable);
    assert_eq!(traces["duration_ms"].ty.kind, "real");
    assert!(traces["duration_ms"].nullable);
    assert_eq!(traces["created_at"].ty.kind, "timestamp");
    assert_eq!(
        schema.tables["workflows"]["continue_on_error"].ty.kind,
        "boolean"
    );
    assert_eq!(schema.tables["workflows"]["version"].ty.kind, "integer");

    // Every whitelisted sort column needs an index, and the keyset index has
    // to carry the tie-break column (D8) — not just the name.
    assert_eq!(
        schema
            .indexes
            .get(&("traces".to_string(), "idx_traces_updated_at".to_string())),
        Some(&vec!["updated_at".to_string()]),
        "found {:?}",
        schema.indexes
    );
    assert_eq!(
        schema
            .indexes
            .get(&("traces".to_string(), "idx_traces_created_at_id".to_string())),
        Some(&vec!["created_at".to_string(), "id".to_string()]),
        "the keyset cursor orders by (created_at, id); found {:?}",
        schema.indexes
    );
    assert!(
        !schema
            .indexes
            .contains_key(&("traces".to_string(), "idx_traces_created_at".to_string())),
        "the single-column created_at index is a prefix of the new one and \
         must have been dropped; found {:?}",
        schema.indexes
    );

    assert!(schema.views.contains_key("current_workflows"));
    assert!(schema.views.contains_key("current_channels"));
    assert_eq!(
        schema.views["current_workflows"],
        schema.tables["workflows"].keys().cloned().collect(),
        "current_workflows is SELECT w.* — it must expose exactly the table's columns"
    );

    // A schema compared with itself is the identity case for the comparator.
    let again = sqlite_schema().await;
    assert!(
        differences("sqlite", &schema, "sqlite-again", &again).is_empty(),
        "the comparator must not report a schema as differing from itself"
    );
}

/// Every backend must have been introspected into something non-trivial.
///
/// Without this, a catalog query that quietly returns no rows on one backend
/// reads as "no disagreements" for everything it should have populated —
/// which is how a parity test passes while comparing nothing.
fn assert_non_trivial(name: &str, schema: &Schema) {
    for table in [
        "workflows",
        "channels",
        "connectors",
        "traces",
        "audit_logs",
    ] {
        assert!(
            schema.tables.contains_key(table),
            "{name}: introspection found no `{table}` table — the catalog query \
             is wrong, not the schema. Found {:?}",
            schema.tables.keys().collect::<Vec<_>>()
        );
    }
    assert!(
        schema.indexes.len() >= 20,
        "{name}: introspection found only {} idx_* indexes; the migration sets \
         create ~30",
        schema.indexes.len()
    );
    assert!(
        schema.indexes.values().all(|columns| !columns.is_empty()),
        "{name}: an index came back with no columns, so the column comparison \
         is vacuous: {:?}",
        schema.indexes
    );
    assert_eq!(
        schema.views.keys().collect::<Vec<_>>(),
        vec!["current_channels", "current_workflows"],
        "{name}: both `current_*` views must be introspected"
    );
}

/// The real thing: all three migration sets applied to a real database and
/// compared. Container-gated because Docker is not available on every dev
/// machine; only the SQLite half of this file runs without it.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test schema_parity -- --ignored"]
async fn schema_is_identical_across_backends() {
    use testcontainers::runners::AsyncRunner;

    let pg_container = testcontainers_modules::postgres::Postgres::default()
        .start()
        .await
        .expect("start postgres");
    let pg_port = pg_container
        .get_host_port_ipv4(5432)
        .await
        .expect("postgres port");
    let pg_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&format!(
            "postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres"
        ))
        .await
        .expect("connect postgres");

    let my_container = testcontainers_modules::mysql::Mysql::default()
        .start()
        .await
        .expect("start mysql");
    let my_port = my_container
        .get_host_port_ipv4(3306)
        .await
        .expect("mysql port");
    let my_pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&format!("mysql://root@127.0.0.1:{my_port}/test"))
        .await
        .expect("connect mysql");

    let sqlite = sqlite_schema().await;
    let postgres = postgres_schema(&pg_pool).await;
    let mysql = mysql_schema(&my_pool).await;
    assert_non_trivial("sqlite", &sqlite);
    assert_non_trivial("postgres", &postgres);
    assert_non_trivial("mysql", &mysql);

    let mut problems = differences("sqlite", &sqlite, "postgres", &postgres);
    problems.extend(differences("sqlite", &sqlite, "mysql", &mysql));
    problems.extend(differences("postgres", &postgres, "mysql", &mysql));
    problems.sort();
    problems.dedup();

    assert!(
        problems.is_empty(),
        "the three migration sets have drifted apart ({} difference(s)). \
         Fix the backend that is missing the change — every table, column, \
         `idx_*` index and view must exist on all three:\n  {}",
        problems.len(),
        problems.join("\n  ")
    );
}

// ============================================================
// The trigger message `map_duplicate` matches
// ============================================================

/// `helpers::map_duplicate` turns a single-draft trigger violation into a
/// `409 Conflict`, and on SQLite and MySQL it can only recognise one by its
/// **message text**: `RAISE(ABORT, …)` and `SIGNAL SQLSTATE '45000'` carry no
/// constraint kind sqlx can classify. That makes a string literal in a
/// migration file part of the crate's behaviour, and nothing checked the two
/// agreed.
///
/// The stakes are quiet: a trigger written with different wording still
/// enforces the rule, so no data is at risk — the API just answers `500`
/// instead of `409` for a duplicate draft, on one backend, and the client that
/// would have retried sensibly gives up instead.
///
/// The existing spellings cannot drift (migrations are checksum-frozen), so
/// what this guards is the *next* one.
#[test]
fn single_draft_trigger_message_matches_the_code() {
    use orion::storage::repositories::helpers::SINGLE_DRAFT_TRIGGER_MSG;

    let migrations = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut raising = Vec::new();

    for backend in ["sqlite", "postgres", "mysql"] {
        let dir = migrations.join(backend);
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "sql"))
            .collect();
        files.sort();

        for file in files {
            let sql = std::fs::read_to_string(&file).expect("read migration");
            for (n, line) in sql.lines().enumerate() {
                // Every way the three backends raise from a trigger.
                let raises = line.contains("RAISE(ABORT")
                    || line.contains("RAISE EXCEPTION")
                    || line.contains("MESSAGE_TEXT");
                if !raises {
                    continue;
                }
                // Only the single-draft rule is matched by text; the
                // active-immutability triggers surface as plain 500s by design.
                if !line.to_lowercase().contains("draft") {
                    continue;
                }
                let where_ = format!(
                    "{backend}/{}:{}",
                    file.file_name().unwrap().display(),
                    n + 1
                );
                raising.push((where_, line.trim().to_string()));
            }
        }
    }

    assert!(
        !raising.is_empty(),
        "found no single-draft trigger messages at all — this test has stopped \
         looking at anything, which is worse than a mismatch"
    );

    let mismatched: Vec<&(String, String)> = raising
        .iter()
        .filter(|(_, line)| !line.contains(SINGLE_DRAFT_TRIGGER_MSG))
        .collect();

    assert!(
        mismatched.is_empty(),
        "these single-draft triggers do not say `{SINGLE_DRAFT_TRIGGER_MSG}`, so \
         `map_duplicate` will not recognise them and a duplicate draft will \
         answer 500 instead of 409:\n  {}",
        mismatched
            .iter()
            .map(|(w, l)| format!("{w}: {l}"))
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

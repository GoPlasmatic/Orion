//! Turning a SQL row into JSON, once per backend (#309).
//!
//! Connector queries used to run on an `sqlx::AnyPool`, and `Any` maps a
//! driver's type info through a closed whitelist: `AnyTypeInfoKind` has nine
//! variants, and `sqlx-postgres`'s conversion errors on anything it cannot
//! spell. That is nine decodable Postgres types — `bool`, the three integer
//! widths, both floats, `bytea`, `text`/`varchar` — and a task failure for
//! `uuid`, `numeric`, `timestamptz`, `date`, `json`, `jsonb`, arrays and enums.
//!
//! Two things made that worse than a missing feature. It is **data-dependent**:
//! the conversion happens per row, so `SELECT '{"a":1}'::json … WHERE false`
//! returned `200 []` and the same query with one row returned `500` — a query
//! could pass every test against an empty table and fail the first time
//! production had data. And it was **not just `db_read`**: `data_query` and
//! `data_write` share this decoder, so the portable dialect inherited the same
//! nine types.
//!
//! The fix is to stop routing connectors through `Any` at all. That is not new
//! architecture: `storage::DbPool` has held `Sqlite | Postgres | Mysql` behind
//! a dispatch macro for the server's own database since 1.0. The connector path
//! is the half that never got it.
//!
//! ## Shape
//!
//! One `rows_to_json` per backend, each matching its own type info
//! exhaustively rather than probing candidate Rust types in turn. The
//! probe-cascade this pattern replaced (in the original `db_read`) fell through
//! to `Value::Null` for anything it did not recognise, so a column read back as
//! null even though the query succeeded — a wrong answer, not an error. A type
//! this module does not handle is a **named 400**, because a decode failure is
//! an authoring problem the author can act on, not an engine fault.

use serde_json::{Map, Value};
use sqlx::{Column, Row, TypeInfo, ValueRef};

/// How to render a value JSON cannot hold exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NumericAs {
    /// A JSON number. Convenient, computable in JSONLogic, and lossy beyond
    /// 2^53 or on most decimal fractions — which for a money column is a
    /// correctness bug the caller cannot see. The default because it is what an
    /// author reaching for `SELECT price` expects, with [`NumericAs::String`]
    /// as the deliberate opt-out.
    #[default]
    Number,
    /// The exact decimal, as a string. Every digit survives; arithmetic on it
    /// needs a cast in the workflow, which is the point — the loss becomes
    /// visible instead of silent.
    String,
}

impl NumericAs {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "number" => Some(Self::Number),
            "string" => Some(Self::String),
            _ => None,
        }
    }
    pub const VALUES: &'static str = "number/string";
}

/// A column that could not be decoded, named so the author can act.
///
/// Carries the remedy as well as the cause: casting in SQL and parsing back is
/// what an author actually does about an exotic type, and saying so turns a
/// `500` and an "unhandled variant" log line into something self-service.
pub struct DecodeError {
    pub column: String,
    pub sql_type: String,
    pub detail: String,
}

impl DecodeError {
    pub fn message(&self, function: &str) -> String {
        format!(
            "{function}: column '{}' has SQL type {}, which cannot be represented as \
             JSON here ({}). Cast it in the query — `SELECT {}::text` — and use \
             `parse_json` if it holds a document, or select the columns you need.",
            self.column, self.sql_type, self.detail, self.column
        )
    }
}

type Decoded<T> = Result<T, DecodeError>;

fn column_names<R: Row>(row: &R) -> Vec<String> {
    row.columns().iter().map(|c| c.name().to_string()).collect()
}

/// `numeric` as JSON, per the caller's choice. The exact decimal is produced
/// first either way, so `Number` rounds a known-good value rather than
/// inheriting whatever the driver's float conversion did.
fn decimal_to_json(exact: String, mode: NumericAs, column: &str, sql_type: &str) -> Decoded<Value> {
    match mode {
        NumericAs::String => Ok(Value::String(exact)),
        NumericAs::Number => exact
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| DecodeError {
                column: column.to_string(),
                sql_type: sql_type.to_string(),
                detail: format!("'{exact}' is not a finite JSON number"),
            }),
    }
}

/// JSON has no NaN or infinity. Saying so beats emitting null, which is
/// indistinguishable from a SQL NULL.
fn float_to_json(v: f64, column: &str, sql_type: &str) -> Decoded<Value> {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .ok_or_else(|| DecodeError {
            column: column.to_string(),
            sql_type: sql_type.to_string(),
            detail: format!("the value is {v}, which JSON cannot represent"),
        })
}

/// Binary columns become a string: the UTF-8 text when the bytes are valid
/// UTF-8 (MySQL reports `TEXT`/`JSON` columns as `BLOB`, so this is the common
/// case), otherwise lowercase hex.
fn blob_to_json(bytes: Vec<u8>) -> Value {
    match String::from_utf8(bytes) {
        Ok(s) => Value::String(s),
        Err(e) => Value::String(crate::crypto::encode_bytes(
            crate::crypto::Codec::Hex,
            &e.into_bytes(),
        )),
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL
// ---------------------------------------------------------------------------

/// Decode a Postgres result set.
///
/// Dispatch is on `PgTypeInfo` — `kind()` for the structural cases (an array,
/// an enum, a domain over another type) and `name()` for the rest. Matching the
/// type the server declared, rather than probing Rust types until one sticks,
/// is what makes an unhandled type a named error instead of a silent null.
pub fn pg_rows_to_json(rows: &[sqlx::postgres::PgRow], numeric: NumericAs) -> Decoded<Vec<Value>> {
    rows.iter()
        .map(|row| {
            let mut obj = Map::new();
            for (i, name) in column_names(row).into_iter().enumerate() {
                obj.insert(name.clone(), pg_column(row, i, &name, numeric)?);
            }
            Ok(Value::Object(obj))
        })
        .collect()
}

fn pg_column(
    row: &sqlx::postgres::PgRow,
    i: usize,
    name: &str,
    numeric: NumericAs,
) -> Decoded<Value> {
    use sqlx::postgres::{PgTypeInfo, PgTypeKind};

    let raw = row.try_get_raw(i).map_err(|e| DecodeError {
        column: name.to_string(),
        sql_type: "?".to_string(),
        detail: e.to_string(),
    })?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let info: PgTypeInfo = raw.type_info().into_owned();
    let sql_type = info.name().to_string();

    let fail = |detail: String| DecodeError {
        column: name.to_string(),
        sql_type: sql_type.clone(),
        detail,
    };
    // A domain is a constrained alias — `CREATE DOMAIN email AS text` — and
    // decodes exactly as the type it wraps. Unwrapped first so every rule
    // below applies through it.
    let kind = info.kind().clone();
    if let PgTypeKind::Domain(inner) = &kind {
        return pg_by_name(row, i, name, inner.name(), numeric, &fail);
    }
    // An enum arrives as its label, in the wire's binary format, which for an
    // enum *is* the text. sqlx has no Rust type for a database-defined enum, so
    // this is the one place raw bytes are read directly.
    if let PgTypeKind::Enum(_) = &kind {
        let bytes = raw.as_bytes().map_err(|e| fail(e.to_string()))?;
        return std::str::from_utf8(bytes)
            .map(|s| Value::String(s.to_string()))
            .map_err(|e| fail(format!("enum label is not UTF-8: {e}")));
    }
    if let PgTypeKind::Array(elem) = &kind {
        return pg_array(row, i, name, elem.name(), numeric, &fail);
    }

    pg_by_name(row, i, name, &sql_type, numeric, &fail)
}

/// The scalar table. Names are `PgTypeInfo::name()` — sqlx's display names,
/// which are the Postgres internal names (`INT4`, not `integer`).
fn pg_by_name(
    row: &sqlx::postgres::PgRow,
    i: usize,
    column: &str,
    sql_type: &str,
    numeric: NumericAs,
    fail: &dyn Fn(String) -> DecodeError,
) -> Decoded<Value> {
    macro_rules! get {
        ($t:ty) => {
            row.try_get::<$t, _>(i).map_err(|e| fail(e.to_string()))?
        };
    }
    let value = match sql_type {
        "BOOL" => Value::Bool(get!(bool)),
        "INT2" => Value::Number(i64::from(get!(i16)).into()),
        "INT4" => Value::Number(i64::from(get!(i32)).into()),
        // `i64` is exact in serde_json, so a bigint never loses precision the
        // way `numeric` can.
        "INT8" => Value::Number(get!(i64).into()),
        "OID" => Value::Number(u64::from(get!(sqlx::postgres::types::Oid).0).into()),
        "FLOAT4" => float_to_json(f64::from(get!(f32)), column, sql_type)?,
        "FLOAT8" => float_to_json(get!(f64), column, sql_type)?,
        "NUMERIC" => decimal_to_json(
            get!(bigdecimal::BigDecimal).to_string(),
            numeric,
            column,
            sql_type,
        )?,
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "\"CHAR\"" | "CITEXT" | "UNKNOWN" => {
            Value::String(get!(String))
        }
        "UUID" => Value::String(get!(uuid::Uuid).to_string()),
        // The value itself, not a re-parsed string: the whole reason a workflow
        // stores a document is to read it back as one.
        "JSON" | "JSONB" => get!(Value),
        "BYTEA" => blob_to_json(get!(Vec<u8>)),
        "TIMESTAMPTZ" => Value::String(
            get!(chrono::DateTime<chrono::Utc>)
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        ),
        "TIMESTAMP" => Value::String(get!(chrono::NaiveDateTime).to_string()),
        "DATE" => Value::String(get!(chrono::NaiveDate).to_string()),
        "TIME" => Value::String(get!(chrono::NaiveTime).to_string()),
        other => {
            return Err(fail(format!(
                "no JSON representation for {other} is defined"
            )));
        }
    };
    Ok(value)
}

/// `Vec<T>` for the element types sqlx can decode as a slice. Arrays of the
/// exotic types fall through to the same named error a scalar would give.
fn pg_array(
    row: &sqlx::postgres::PgRow,
    i: usize,
    column: &str,
    elem: &str,
    numeric: NumericAs,
    fail: &dyn Fn(String) -> DecodeError,
) -> Decoded<Value> {
    macro_rules! arr {
        ($t:ty, $f:expr) => {{
            let items: Vec<Option<$t>> = row.try_get(i).map_err(|e| fail(e.to_string()))?;
            let f = $f;
            items
                .into_iter()
                .map(|v| match v {
                    Some(v) => f(v),
                    None => Ok(Value::Null),
                })
                .collect::<Decoded<Vec<Value>>>()?
        }};
    }
    let items = match elem {
        "BOOL" => arr!(bool, |v: bool| Ok(Value::Bool(v))),
        "INT2" => arr!(i16, |v: i16| Ok(Value::Number(i64::from(v).into()))),
        "INT4" => arr!(i32, |v: i32| Ok(Value::Number(i64::from(v).into()))),
        "INT8" => arr!(i64, |v: i64| Ok(Value::Number(v.into()))),
        "FLOAT4" => arr!(f32, |v: f32| float_to_json(f64::from(v), column, elem)),
        "FLOAT8" => arr!(f64, |v: f64| float_to_json(v, column, elem)),
        "NUMERIC" => arr!(bigdecimal::BigDecimal, |v: bigdecimal::BigDecimal| {
            decimal_to_json(v.to_string(), numeric, column, elem)
        }),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CITEXT" => {
            arr!(String, |v: String| Ok(Value::String(v)))
        }
        "UUID" => arr!(uuid::Uuid, |v: uuid::Uuid| Ok(Value::String(v.to_string()))),
        "JSON" | "JSONB" => arr!(Value, Ok::<Value, DecodeError>),
        "BYTEA" => arr!(Vec<u8>, |v: Vec<u8>| Ok(blob_to_json(v))),
        "TIMESTAMPTZ" => arr!(chrono::DateTime<chrono::Utc>, |v: chrono::DateTime<
            chrono::Utc,
        >| {
            Ok(Value::String(
                v.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
            ))
        }),
        "TIMESTAMP" => arr!(chrono::NaiveDateTime, |v: chrono::NaiveDateTime| Ok(
            Value::String(v.to_string())
        )),
        "DATE" => arr!(chrono::NaiveDate, |v: chrono::NaiveDate| Ok(Value::String(
            v.to_string()
        ))),
        other => {
            return Err(fail(format!(
                "no JSON representation for an array of {other} is defined"
            )));
        }
    };
    Ok(Value::Array(items))
}

// ---------------------------------------------------------------------------
// MySQL
// ---------------------------------------------------------------------------

/// Decode a MySQL result set.
///
/// A far smaller vocabulary than Postgres — no arrays, no user-defined types —
/// so this sits close to what `Any` already managed, plus the date/time family,
/// `DECIMAL` and `JSON`.
pub fn mysql_rows_to_json(
    rows: &[sqlx::mysql::MySqlRow],
    numeric: NumericAs,
) -> Decoded<Vec<Value>> {
    rows.iter()
        .map(|row| {
            let mut obj = Map::new();
            for (i, name) in column_names(row).into_iter().enumerate() {
                obj.insert(name.clone(), mysql_column(row, i, &name, numeric)?);
            }
            Ok(Value::Object(obj))
        })
        .collect()
}

fn mysql_column(
    row: &sqlx::mysql::MySqlRow,
    i: usize,
    name: &str,
    numeric: NumericAs,
) -> Decoded<Value> {
    let raw = row.try_get_raw(i).map_err(|e| DecodeError {
        column: name.to_string(),
        sql_type: "?".to_string(),
        detail: e.to_string(),
    })?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let sql_type = raw.type_info().name().to_string();
    let fail = |detail: String| DecodeError {
        column: name.to_string(),
        sql_type: sql_type.clone(),
        detail,
    };
    macro_rules! get {
        ($t:ty) => {
            row.try_get::<$t, _>(i).map_err(|e| fail(e.to_string()))?
        };
    }

    let value = match sql_type.as_str() {
        // MySQL has no boolean: `BOOLEAN` is `TINYINT(1)`, and sqlx reports the
        // storage type. Reading it as an integer is therefore the honest
        // answer — a workflow comparing to `1` works on every MySQL, while a
        // guess at `true` would depend on a column width sqlx does not expose.
        "TINYINT" => Value::Number(i64::from(get!(i8)).into()),
        "SMALLINT" | "YEAR" => Value::Number(i64::from(get!(i16)).into()),
        "INT" | "MEDIUMINT" => Value::Number(i64::from(get!(i32)).into()),
        "BIGINT" => Value::Number(get!(i64).into()),
        "TINYINT UNSIGNED" => Value::Number(u64::from(get!(u8)).into()),
        "SMALLINT UNSIGNED" => Value::Number(u64::from(get!(u16)).into()),
        "INT UNSIGNED" | "MEDIUMINT UNSIGNED" => Value::Number(u64::from(get!(u32)).into()),
        "BIGINT UNSIGNED" => Value::Number(get!(u64).into()),
        "FLOAT" => float_to_json(f64::from(get!(f32)), name, &sql_type)?,
        "DOUBLE" => float_to_json(get!(f64), name, &sql_type)?,
        "DECIMAL" => decimal_to_json(
            get!(bigdecimal::BigDecimal).to_string(),
            numeric,
            name,
            &sql_type,
        )?,
        // `ENUM` and `SET` are string types on the wire.
        "VARCHAR" | "CHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET" => {
            Value::String(get!(String))
        }
        "JSON" => get!(Value),
        "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => {
            blob_to_json(get!(Vec<u8>))
        }
        "TIMESTAMP" => Value::String(
            get!(chrono::DateTime<chrono::Utc>)
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        ),
        "DATETIME" => Value::String(get!(chrono::NaiveDateTime).to_string()),
        "DATE" => Value::String(get!(chrono::NaiveDate).to_string()),
        "TIME" => Value::String(get!(chrono::NaiveTime).to_string()),
        other => {
            return Err(fail(format!(
                "no JSON representation for {other} is defined"
            )));
        }
    };
    Ok(value)
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

/// Decode a SQLite result set.
///
/// SQLite has five storage classes and no static column types, so the type info
/// here describes the *value*, not the schema. That is why a `numeric_as`
/// setting has nothing to act on: a SQLite `NUMERIC` column stores whichever
/// class the value fits, and what comes back is already an integer, a float or
/// text.
pub fn sqlite_rows_to_json(
    rows: &[sqlx::sqlite::SqliteRow],
    _numeric: NumericAs,
) -> Decoded<Vec<Value>> {
    rows.iter()
        .map(|row| {
            let mut obj = Map::new();
            for (i, name) in column_names(row).into_iter().enumerate() {
                obj.insert(name.clone(), sqlite_column(row, i, &name)?);
            }
            Ok(Value::Object(obj))
        })
        .collect()
}

fn sqlite_column(row: &sqlx::sqlite::SqliteRow, i: usize, name: &str) -> Decoded<Value> {
    let raw = row.try_get_raw(i).map_err(|e| DecodeError {
        column: name.to_string(),
        sql_type: "?".to_string(),
        detail: e.to_string(),
    })?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let sql_type = raw.type_info().name().to_string();
    let fail = |detail: String| DecodeError {
        column: name.to_string(),
        sql_type: sql_type.clone(),
        detail,
    };
    macro_rules! get {
        ($t:ty) => {
            row.try_get::<$t, _>(i).map_err(|e| fail(e.to_string()))?
        };
    }

    let value = match sql_type.as_str() {
        "BOOLEAN" => Value::Bool(get!(bool)),
        "INTEGER" | "INT8" | "BIGINT" => Value::Number(get!(i64).into()),
        "REAL" | "DOUBLE" | "FLOAT" => float_to_json(get!(f64), name, &sql_type)?,
        "TEXT" | "VARCHAR" | "DATETIME" | "DATE" | "TIME" => Value::String(get!(String)),
        "BLOB" => blob_to_json(get!(Vec<u8>)),
        "NULL" => Value::Null,
        other => {
            return Err(fail(format!(
                "no JSON representation for {other} is defined"
            )));
        }
    };
    Ok(value)
}

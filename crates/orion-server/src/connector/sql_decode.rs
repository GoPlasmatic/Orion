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

/// How to render a binary column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BinaryAs {
    /// The bytes as text when they are valid UTF-8, lowercase hex when they are
    /// not.
    ///
    /// The default, and the one setting whose *output shape depends on the
    /// data*: two rows of one column can come back as text and as hex, with
    /// nothing distinguishing them, so a workflow that decodes the hex breaks
    /// the first time a value happens to be valid UTF-8. It is the default
    /// because MySQL reports `TEXT` and `JSON` columns as `BLOB`, which makes
    /// text the right answer far more often than not — and because it is what
    /// every existing task already reads.
    ///
    /// For a column that is genuinely binary, name the encoding instead.
    #[default]
    Auto,
    /// Lowercase hex, whatever the bytes are.
    Hex,
    /// Standard base64 (padded), whatever the bytes are.
    Base64,
    /// The bytes as UTF-8 text, or a named decode error when they are not —
    /// the strict reading of "this column holds text".
    Text,
}

impl BinaryAs {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "hex" => Some(Self::Hex),
            "base64" => Some(Self::Base64),
            "text" => Some(Self::Text),
            _ => None,
        }
    }
    pub const VALUES: &'static str = "auto/hex/base64/text";
}

/// How one result set renders the values JSON cannot hold exactly.
///
/// One struct rather than a growing parameter list: every decoder threads it
/// from the handler to the column, and a third question about rendering should
/// not mean touching all eight signatures again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RowFormat {
    pub numeric: NumericAs,
    pub binary: BinaryAs,
}

impl RowFormat {
    /// The rendering an unconfigured task gets.
    pub fn new(numeric: NumericAs, binary: BinaryAs) -> Self {
        Self { numeric, binary }
    }
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

/// Binary columns become a string, in the spelling `binary_as` asks for.
///
/// [`BinaryAs::Auto`] is the historical rule — UTF-8 text when the bytes are
/// valid UTF-8 (MySQL reports `TEXT`/`JSON` columns as `BLOB`, so this is the
/// common case), lowercase hex when they are not. It is also the one mode whose
/// output shape is decided by the value rather than by the task, which is the
/// reason the other three exist.
fn blob_to_json(bytes: Vec<u8>, mode: BinaryAs, column: &str, sql_type: &str) -> Decoded<Value> {
    let hex = |b: &[u8]| Value::String(crate::crypto::encode_bytes(crate::crypto::Codec::Hex, b));
    Ok(match mode {
        BinaryAs::Auto => match String::from_utf8(bytes) {
            Ok(s) => Value::String(s),
            Err(e) => hex(&e.into_bytes()),
        },
        BinaryAs::Hex => hex(&bytes),
        BinaryAs::Base64 => Value::String(crate::crypto::encode_bytes(
            crate::crypto::Codec::Base64,
            &bytes,
        )),
        BinaryAs::Text => match String::from_utf8(bytes) {
            Ok(s) => Value::String(s),
            Err(e) => {
                return Err(DecodeError {
                    column: column.to_string(),
                    sql_type: sql_type.to_string(),
                    detail: format!("binary_as is \"text\" but the bytes are not valid UTF-8: {e}"),
                });
            }
        },
    })
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
pub fn pg_rows_to_json(rows: &[sqlx::postgres::PgRow], format: RowFormat) -> Decoded<Vec<Value>> {
    rows.iter()
        .map(|row| {
            let mut obj = Map::new();
            for (i, name) in column_names(row).into_iter().enumerate() {
                obj.insert(name.clone(), pg_column(row, i, &name, format)?);
            }
            Ok(Value::Object(obj))
        })
        .collect()
}

fn pg_column(
    row: &sqlx::postgres::PgRow,
    i: usize,
    name: &str,
    format: RowFormat,
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
    //
    // In practice a domain column never reaches here: PostgreSQL reports the
    // *base* type's OID in the row description, so `CREATE DOMAIN email AS
    // text` arrives already spelled `TEXT`. This branch is the belt to that
    // brace, and it has to stay a branch rather than become an assumption —
    // but note it cannot be reached through `try_get` either, because that
    // re-reads the value's own type info and compares by OID, and no Rust type
    // can claim an OID the database invented. If a path ever does arrive here,
    // it needs `try_get_unchecked`, not a new arm.
    let kind = info.kind().clone();
    if let PgTypeKind::Domain(inner) = &kind {
        return pg_by_name(row, i, name, inner.name(), format, &fail);
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
        return pg_array(row, i, name, elem.name(), format, &fail);
    }

    pg_by_name(row, i, name, &sql_type, format, &fail)
}

/// The scalar table. Names are `PgTypeInfo::name()` — sqlx's display names,
/// which are the Postgres internal names (`INT4`, not `integer`).
fn pg_by_name(
    row: &sqlx::postgres::PgRow,
    i: usize,
    column: &str,
    sql_type: &str,
    format: RowFormat,
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
            format.numeric,
            column,
            sql_type,
        )?,
        // `CHAR` here is `bpchar` — `char(n)` — because that is the display
        // name sqlx reports for it. Postgres' internal one-byte type is a
        // different type whose display name carries its own quotes, and `str`
        // is not compatible with it, so it belongs in the catch-all's named
        // error rather than in this arm. `citext` is lowercase because an
        // extension type reports `oid::regtype::text` verbatim; that is also
        // how sqlx spells it in its own compatibility check, so matching any
        // other case would claim more than the driver will decode.
        "TEXT" | "VARCHAR" | "CHAR" | "NAME" | "citext" | "UNKNOWN" => Value::String(get!(String)),
        "UUID" => Value::String(get!(uuid::Uuid).to_string()),
        // The value itself, not a re-parsed string: the whole reason a workflow
        // stores a document is to read it back as one.
        "JSON" | "JSONB" => get!(Value),
        "BYTEA" => blob_to_json(get!(Vec<u8>), format.binary, column, sql_type)?,
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
    format: RowFormat,
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
            decimal_to_json(v.to_string(), format.numeric, column, elem)
        }),
        // The same spellings as the scalar table above: `pg_column` hands this
        // function `elem.name()`, so `char(n)[]` arrives as `CHAR` and
        // `citext[]` as `citext`. Keeping the two lists in step is the point —
        // an element type the scalar table decodes and this one does not is an
        // asymmetry no author can predict from the docs.
        "TEXT" | "VARCHAR" | "CHAR" | "NAME" | "citext" => {
            arr!(String, |v: String| Ok(Value::String(v)))
        }
        "OID" => arr!(
            sqlx::postgres::types::Oid,
            |v: sqlx::postgres::types::Oid| Ok(Value::Number(u64::from(v.0).into()))
        ),
        "UUID" => arr!(uuid::Uuid, |v: uuid::Uuid| Ok(Value::String(v.to_string()))),
        "JSON" | "JSONB" => arr!(Value, Ok::<Value, DecodeError>),
        "BYTEA" => arr!(Vec<u8>, |v: Vec<u8>| blob_to_json(
            v,
            format.binary,
            column,
            elem
        )),
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
        "TIME" => arr!(chrono::NaiveTime, |v: chrono::NaiveTime| Ok(Value::String(
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
    format: RowFormat,
) -> Decoded<Vec<Value>> {
    rows.iter()
        .map(|row| {
            let mut obj = Map::new();
            for (i, name) in column_names(row).into_iter().enumerate() {
                obj.insert(name.clone(), mysql_column(row, i, &name, format)?);
            }
            Ok(Value::Object(obj))
        })
        .collect()
}

fn mysql_column(
    row: &sqlx::mysql::MySqlRow,
    i: usize,
    name: &str,
    format: RowFormat,
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
        // MySQL has no boolean type: `BOOLEAN` and `BOOL` are aliases for
        // `TINYINT(1)`. sqlx does not report the storage type for one — it
        // names any `Tiny` column whose display width is 1 `BOOLEAN`, and that
        // width is the one MySQL 8 still preserves after deprecating the rest,
        // precisely because it is the boolean convention. So a `BOOLEAN` column
        // reads back as a JSON boolean, agreeing with Postgres `bool`.
        //
        // `bool` rather than `i8` is also the only choice that decodes at all
        // for `TINYINT(1) UNSIGNED`: sqlx reports that as `BOOLEAN` too (the
        // width guard precedes the unsigned guard) but refuses `i8` for any
        // unsigned column, and the flag that would let us tell the two apart is
        // private. A `TINYINT(1)` genuinely holding a small integer therefore
        // reads back as `true`; `SELECT flags + 0` returns `BIGINT` and a
        // number.
        "BOOLEAN" => Value::Bool(get!(bool)),
        "TINYINT" => Value::Number(i64::from(get!(i8)).into()),
        "SMALLINT" => Value::Number(i64::from(get!(i16)).into()),
        // `YEAR` is unsigned, and sqlx keeps it out of the *signed* integer
        // compatibility set entirely — `i16` is refused on both counts, so this
        // shared an arm with `SMALLINT` and decoded nothing. `u16` is the type
        // sqlx accepts for it.
        "YEAR" => Value::Number(u64::from(get!(u16)).into()),
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
            format.numeric,
            name,
            &sql_type,
        )?,
        // `ENUM` and `SET` are string types on the wire. There is no `SET` arm
        // because there is no `SET` name to match: MySQL sends a `SET` column
        // as a `String` carrying the SET flag, and sqlx's naming never consults
        // that flag — so such a column arrives here spelled `CHAR`.
        "VARCHAR" | "CHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" => {
            Value::String(get!(String))
        }
        "JSON" => get!(Value),
        "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => {
            blob_to_json(get!(Vec<u8>), format.binary, name, &sql_type)?
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
/// text. `binary_as` *is* honoured — `BLOB` is a storage class, so it survives
/// the same round trip a declared type does not.
pub fn sqlite_rows_to_json(
    rows: &[sqlx::sqlite::SqliteRow],
    format: RowFormat,
) -> Decoded<Vec<Value>> {
    rows.iter()
        .map(|row| {
            let mut obj = Map::new();
            for (i, name) in column_names(row).into_iter().enumerate() {
                obj.insert(name.clone(), sqlite_column(row, i, &name, format)?);
            }
            Ok(Value::Object(obj))
        })
        .collect()
}

fn sqlite_column(
    row: &sqlx::sqlite::SqliteRow,
    i: usize,
    name: &str,
    format: RowFormat,
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

    // These four are the whole vocabulary — SQLite's fifth storage class is
    // NULL, which the early return above already took. The list is exhaustive
    // rather than optimistic: for a non-null value sqlx reports the *storage
    // class*, never the declared column type (the declared type is only its
    // fallback for a NULL). So a column declared `BOOLEAN` or `DATETIME`
    // arrives here as `INTEGER` or `TEXT`, an arm spelled for a declared type
    // would be unreachable, and the catch-all below cannot fire at all.
    let value = match sql_type.as_str() {
        "INTEGER" => Value::Number(get!(i64).into()),
        "REAL" => float_to_json(get!(f64), name, &sql_type)?,
        "TEXT" => Value::String(get!(String)),
        "BLOB" => blob_to_json(get!(Vec<u8>), format.binary, name, &sql_type)?,
        other => {
            return Err(fail(format!(
                "no JSON representation for {other} is defined"
            )));
        }
    };
    Ok(value)
}

#[cfg(test)]
mod binary_tests {
    // The crate warns on `panic!` because production code should not have any.
    // `DecodeError` carries no `Debug`, so the two uses below are how a test
    // unwraps one through the message an author would actually read.
    #![allow(clippy::panic)]

    use super::*;

    /// `DecodeError` carries no `Debug`, and this is a test — so unwrap it
    /// through the message an author would actually see.
    fn blob(bytes: Vec<u8>, mode: BinaryAs) -> Value {
        match blob_to_json(bytes, mode, "c", "BLOB") {
            Ok(v) => v,
            Err(e) => panic!("{}", e.message("test")),
        }
    }

    /// The historical rule, and the only mode whose result shape is decided by
    /// the bytes rather than by the task.
    #[test]
    fn auto_reads_utf8_as_text_and_the_rest_as_hex() {
        assert_eq!(
            blob(b"hello".to_vec(), BinaryAs::Auto),
            Value::String("hello".to_string())
        );
        assert_eq!(
            blob(vec![0xff, 0x00], BinaryAs::Auto),
            Value::String("ff00".to_string())
        );
    }

    /// The point of naming an encoding: one shape whatever the bytes are, so a
    /// workflow that decodes the column cannot be broken by a row that happens
    /// to be valid UTF-8.
    #[test]
    fn a_named_encoding_does_not_depend_on_the_bytes() {
        for (mode, utf8, binary) in [
            (BinaryAs::Hex, "68690a", "ff00"),
            (BinaryAs::Base64, "aGkK", "/wA="),
        ] {
            assert_eq!(
                blob(b"hi\n".to_vec(), mode),
                Value::String(utf8.to_string()),
                "{mode:?} on text-shaped bytes"
            );
            assert_eq!(
                blob(vec![0xff, 0x00], mode),
                Value::String(binary.to_string()),
                "{mode:?} on binary bytes"
            );
        }
    }

    /// `text` is the strict reading: bytes that are not UTF-8 are a named
    /// decode error, not a silent fallback to another shape.
    #[test]
    fn text_refuses_bytes_that_are_not_utf8() {
        let Err(err) = blob_to_json(vec![0xff], BinaryAs::Text, "payload", "BYTEA") else {
            panic!("bytes that are not UTF-8 must not decode as text");
        };
        let msg = err.message("db_read");
        assert!(msg.contains("payload"), "{msg}");
        assert!(msg.contains("binary_as"), "{msg}");
    }

    #[test]
    fn the_default_is_the_historical_rule() {
        assert_eq!(BinaryAs::default(), BinaryAs::Auto);
        assert_eq!(RowFormat::default().binary, BinaryAs::Auto);
        assert_eq!(BinaryAs::parse("base64"), Some(BinaryAs::Base64));
        assert_eq!(BinaryAs::parse("utf8"), None);
    }
}

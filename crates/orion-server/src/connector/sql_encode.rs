//! Turning a JSON value into a PostgreSQL parameter, typed by what the server
//! declares (N6).
//!
//! The mirror of [`super::sql_decode`], and it is meant to read like it.
//! `sql_decode` renders a value as JSON using the type the server *reports* for
//! the column; this module renders JSON as a parameter using the type the
//! server *declares* for the placeholder.
//!
//! ## The defect this closes
//!
//! `pool_cache::bind_params` picks a parameter's SQL type from the
//! shape of its JSON value — a string binds as `text`, a number as `int8`.
//! PostgreSQL types its parameters at `Parse`, and sqlx caches the prepared
//! statement keyed by the SQL text alone, so the first call through a query
//! freezes that statement's parameter types and every later call sends bytes
//! encoded for *its* value's type into a slot the server still reads with the
//! *first* call's type. The `($1)::int` cast never gets a say — a cast applies
//! to the parameter after it is received, not to how it is received.
//!
//! It is order-dependent, so a query passes every test that happens to send one
//! shape. And it is not always an error: `int8recv` accepts any eight bytes, so
//! an eight-character string bound where `int8` was declared decodes to a
//! plausible, wrong number with no failure anywhere.
//!
//! ## The shape
//!
//! Ask PostgreSQL. Preparing a statement with *no* declared parameter types
//! makes the server infer every one of them from the query text, which it can
//! already do — `WHERE id = $1` against a `uuid` column infers `uuid`, and an
//! otherwise-unconstrained parameter resolves to `text` rather than failing.
//! Coercing each value to the inferred type is what makes the binding a
//! property of the query instead of the message.
//!
//! That also makes the coercion **idempotent with respect to `Parse`**: because
//! a value is coerced to the type the server inferred, binding it re-declares
//! the same type. A statement-cache miss, an eviction, or a different pooled
//! connection cannot change the outcome, so the cache is an optimisation rather
//! than something correctness leans on.
//!
//! ## Two outcomes, and only one of them is an error
//!
//! A declared type this table does not carry is **not** an error — it is
//! [`Binding::Unsupported`], and the caller falls back to the value-shaped
//! binding with the statement cache disabled.
//!
//! Refusing instead would give a better message, and for the built-in types it
//! would cost nothing: PostgreSQL ships no assignment cast from `text` to
//! `inet`, `bytea`, `interval`, `money`, a range or a composite, so those
//! queries already fail whatever is bound. The reason to fall back anyway is
//! the case that cannot be enumerated — `CREATE CAST (text AS mytype) AS
//! ASSIGNMENT` is a thing an author can write, and so is a domain over one.
//! Degrading leaves those working; refusing would decide, on their behalf, that
//! they do not.
//!
//! A value that will not convert *is* an error: the type is supported and the
//! value is wrong, which is an authoring problem the author can act on.
//!
//! **The choice between the two must depend only on the declared types, never
//! on the values.** Mixing the paths for one SQL text would be unsound —
//! sqlx's cache lookup happens before its `persistent` check, so a fallback
//! call would still find, and misread, a statement a typed call had cached.

use serde_json::Value;
use sqlx::postgres::{PgArguments, PgTypeInfo, PgTypeKind};
use sqlx::{Arguments, TypeInfo};

/// How one statement's parameters are going to be bound.
///
/// Generic over the driver's own arguments type, so the three backends share
/// one call site while only PostgreSQL does any of the work.
#[derive(Debug)]
pub enum Bound<A> {
    /// Typed against what the server declared for each placeholder.
    Typed(A),
    /// Bound by the shape of each JSON value, as Orion has always done.
    ///
    /// `cache` is whether the prepared statement may be kept. It is false only
    /// on PostgreSQL, and for the reason this module exists: a value-shaped
    /// bind is what freezes one call's parameter types into a statement every
    /// later call then reuses. Left cached, the fallback would carry the very
    /// defect the typed path removes. MySQL re-sends its parameter types on
    /// every execute and SQLite has no static parameter types, so neither has
    /// anything to gain by giving up its cache.
    Fallback { cache: bool },
}

/// A parameter value, before it knows what SQL type it is going to be.
///
/// One vocabulary so the two parameter sources share the coercion: `db_read` /
/// `db_write` arrive as [`serde_json::Value`], `data_query` / `data_write` as
/// `sea_query::Value` through the portable dialect.
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// An object or array. Also how a JSON document reaches a `jsonb` column.
    Json(Value),
}

impl Scalar {
    /// The JSON type name, for an error that says what arrived without
    /// quoting the value itself.
    fn kind_name(&self) -> &'static str {
        match self {
            Scalar::Null => "null",
            Scalar::Bool(_) => "a boolean",
            Scalar::Int(_) | Scalar::Float(_) => "a number",
            Scalar::Str(_) => "a string",
            Scalar::Json(Value::Array(_)) => "an array",
            Scalar::Json(_) => "an object",
        }
    }
}

impl From<&Value> for Scalar {
    fn from(v: &Value) -> Self {
        match v {
            Value::Null => Scalar::Null,
            Value::Bool(b) => Scalar::Bool(*b),
            Value::Number(n) => n
                .as_i64()
                .map(Scalar::Int)
                // A `u64` above `i64::MAX`. Its digits are what matter, so it
                // travels as text and the numeric arms parse it back. This has
                // to be asked *before* `as_f64`, which answers `Some` for one
                // of these and rounds it — the reason the equivalent arm in
                // `bind_params` has never been reachable.
                .or_else(|| n.as_u64().map(|u| Scalar::Str(u.to_string())))
                .or_else(|| n.as_f64().map(Scalar::Float))
                .unwrap_or_else(|| Scalar::Str(n.to_string())),
            Value::String(s) => Scalar::Str(s.clone()),
            other => Scalar::Json(other.clone()),
        }
    }
}

/// The portable dialect's values as [`Scalar`]s.
///
/// `None` for a `sea_query::Value` variant Orion's renderer does not produce —
/// `query::backend::sql::to_sea_value` emits `Bool`, `BigInt`, `Double` and
/// `String` and nothing else. A variant that turns up anyway is a shape this
/// module has never seen, and the honest answer to that is the fallback rather
/// than a guess at what it meant.
pub fn scalars_from_sea(values: &sea_query::Values) -> Option<Vec<Scalar>> {
    use sea_query::Value as V;

    values
        .0
        .iter()
        .map(|v| {
            Some(match v {
                V::Bool(o) => o.map_or(Scalar::Null, Scalar::Bool),
                V::TinyInt(o) => o.map_or(Scalar::Null, |i| Scalar::Int(i.into())),
                V::SmallInt(o) => o.map_or(Scalar::Null, |i| Scalar::Int(i.into())),
                V::Int(o) => o.map_or(Scalar::Null, |i| Scalar::Int(i.into())),
                V::BigInt(o) => o.map_or(Scalar::Null, Scalar::Int),
                V::TinyUnsigned(o) => o.map_or(Scalar::Null, |i| Scalar::Int(i.into())),
                V::SmallUnsigned(o) => o.map_or(Scalar::Null, |i| Scalar::Int(i.into())),
                V::Unsigned(o) => o.map_or(Scalar::Null, |i| Scalar::Int(i.into())),
                // Above `i64::MAX` the digits are what matter, exactly as for a
                // JSON number of the same size.
                V::BigUnsigned(o) => o.map_or(Scalar::Null, |u| {
                    i64::try_from(u).map_or_else(|_| Scalar::Str(u.to_string()), Scalar::Int)
                }),
                V::Float(o) => o.map_or(Scalar::Null, |f| Scalar::Float(f.into())),
                V::Double(o) => o.map_or(Scalar::Null, Scalar::Float),
                V::String(o) => o.as_ref().map_or(Scalar::Null, |s| Scalar::Str(s.clone())),
                V::Char(o) => o.map_or(Scalar::Null, |c| Scalar::Str(c.to_string())),
                // `Value::Json` and the temporal variants sit behind sea-query
                // features Orion does not enable, so they cannot appear and
                // there is no arm to write for them.
                _ => return None,
            })
        })
        .collect()
}

/// A parameter that could not be encoded, named so the author can act.
///
/// The same three-field shape as [`super::sql_decode::DecodeError`], for the
/// same reason: the variation belongs in `detail`, and the message carries the
/// remedy rather than only the cause.
///
/// It names the placeholder, the declared type and the JSON type that arrived.
/// It never names the **value** — a parameter is the part of a query most
/// likely to hold something private. That is also what makes `Debug` safe to
/// derive here where `sql_decode::DecodeError` does without one.
#[derive(Debug)]
pub struct EncodeError {
    /// Zero-based; rendered as the `$n` the author wrote.
    pub param: usize,
    pub sql_type: String,
    pub detail: String,
}

impl EncodeError {
    pub fn message(&self, function: &str) -> String {
        format!(
            "{function}: parameter ${n} is declared {} by the query, and the \
             value given cannot be converted to it ({}). Pass a value of the \
             right shape, or cast the placeholder in the query — \
             `(${n})::text` — to bind it as text instead.",
            self.sql_type,
            self.detail,
            n = self.param + 1
        )
    }
}

type Encoded<T> = Result<T, EncodeError>;

/// What [`pg_arguments`] decided.
#[derive(Debug)]
pub enum Binding {
    /// Every declared type is in the table; the arguments are typed to them.
    Typed(PgArguments),
    /// A declared type has no mapping here. Carries its name for the log —
    /// the caller falls back to the value-shaped binding.
    Unsupported(String),
}

/// How a declared type is bound, once domains are unwrapped.
enum Declared<'a> {
    Scalar(&'a str),
    Array(&'a str),
    Enum,
}

/// A domain chain longer than this is pathological; stop rather than spin.
const MAX_DOMAIN_DEPTH: usize = 8;

/// Unwrap domains, then classify.
///
/// A domain is a constrained alias — `CREATE DOMAIN email AS text` — and binds
/// exactly as the type it wraps, so it is unwrapped first and every rule below
/// applies through it. This mirrors `sql_decode`'s handling, including its
/// blind spot: an array's element type is taken by name without consulting its
/// kind, so an array *of* a domain or an enum lands in the catch-all. The two
/// sides must have the same gaps — an element type one direction handles and
/// the other does not is an asymmetry no author can predict from the docs.
fn resolve(info: &PgTypeInfo) -> Option<Declared<'_>> {
    // `PgTypeInfo::kind()` is `unreachable!()` for a type declaration sqlx has
    // not resolved against the catalogue, and the three shapes that can be in
    // that state are exactly the ones with no OID or no name. `Describe`
    // always hands back resolved types, so this is belt to that brace — but a
    // panic on a request path is not the way to discover we were wrong, and
    // an unclassifiable type has a perfectly good answer already: fall back.
    if info.oid().is_none() || info.name() == "?" {
        return None;
    }
    let mut cur = info;
    for _ in 0..MAX_DOMAIN_DEPTH {
        match cur.kind() {
            PgTypeKind::Domain(inner) => cur = inner,
            PgTypeKind::Enum(_) => return Some(Declared::Enum),
            PgTypeKind::Array(elem) => return Some(Declared::Array(elem.name())),
            _ => return Some(Declared::Scalar(cur.name())),
        }
    }
    Some(Declared::Scalar(cur.name()))
}

/// The scalar types this module binds.
///
/// One list, read by both the scalar and the array binder, so the two cannot
/// drift apart. Names are `PgTypeInfo::name()` — sqlx's display names, which
/// are the PostgreSQL internal ones (`INT4`, not `integer`). `CHAR` is
/// `bpchar` (`char(n)`) and `citext` is lowercase, both for the reasons
/// `sql_decode` spells out.
///
/// `BYTEA` is deliberately absent. `crypto::decode_bytes` would make hex or
/// base64 input mechanically easy, but the decode side lets the author *name*
/// the encoding per task (`binary_as`) and this side has no such knob —
/// sniffing which one a string is would be exactly the data-dependent
/// behaviour #309 was filed about. It falls back, and `decode($1, 'base64')`
/// (whose `$1` infers `TEXT`) is the way to write it.
const BINDABLE: &[&str] = &[
    "BOOL",
    "INT2",
    "INT4",
    "INT8",
    "OID",
    "FLOAT4",
    "FLOAT8",
    "NUMERIC",
    "TEXT",
    "VARCHAR",
    "CHAR",
    "NAME",
    "citext",
    "UNKNOWN",
    "UUID",
    "JSON",
    "JSONB",
    "TIMESTAMPTZ",
    "TIMESTAMP",
    "DATE",
    "TIME",
];

/// Build the arguments for one statement, or say the table cannot.
///
/// `declared` is what the server inferred, in placeholder order.
pub fn pg_arguments(declared: &[PgTypeInfo], values: &[Scalar]) -> Encoded<Binding> {
    // Decided over the declared types alone, and before any value is touched,
    // so the choice of path can never depend on what a caller sent.
    for info in declared {
        let unsupported = match resolve(info) {
            None => Some(info.name().to_string()),
            Some(Declared::Enum) => None,
            Some(Declared::Scalar(n) | Declared::Array(n)) => {
                (!BINDABLE.contains(&n)).then(|| n.to_string())
            }
        };
        if let Some(name) = unsupported {
            return Ok(Binding::Unsupported(name));
        }
    }

    if declared.len() != values.len() {
        return Err(EncodeError {
            param: values.len().min(declared.len()),
            sql_type: "?".to_string(),
            detail: format!(
                "the query has {} placeholder(s) but {} parameter(s) were given",
                declared.len(),
                values.len()
            ),
        });
    }

    let mut args = PgArguments::default();
    args.reserve(values.len(), 0);
    for (i, (info, value)) in declared.iter().zip(values).enumerate() {
        let sql_type = info.name().to_string();
        let fail = |detail: String| EncodeError {
            param: i,
            sql_type: sql_type.clone(),
            detail,
        };
        match resolve(info) {
            // An enum's binary wire form *is* its label, so the text encoding
            // is already the right bytes. sqlx has no Rust type for a
            // database-defined enum, which is why this is text rather than
            // something enum-shaped.
            Some(Declared::Enum) => add(&mut args, opt(value, |v| as_str(v, &fail))?, &fail)?,
            Some(Declared::Scalar(name)) => add_scalar(&mut args, name, value, &fail)?,
            Some(Declared::Array(elem)) => add_array(&mut args, elem, value, &fail)?,
            // Screened out above; an error rather than a panic so a future
            // divergence between the screen and this match degrades.
            None => return Err(fail("the declared type could not be read".to_string())),
        }
    }
    Ok(Binding::Typed(args))
}

/// `Null` binds as a typed NULL, so the declared type still matches.
fn opt<T>(v: &Scalar, f: impl FnOnce(&Scalar) -> Encoded<T>) -> Encoded<Option<T>> {
    match v {
        Scalar::Null => Ok(None),
        other => f(other).map(Some),
    }
}

fn add<'q, T>(args: &mut PgArguments, value: T, fail: &dyn Fn(String) -> EncodeError) -> Encoded<()>
where
    T: 'q + sqlx::Encode<'q, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    args.add(value).map_err(|e| fail(e.to_string()))
}

fn add_scalar(
    args: &mut PgArguments,
    name: &str,
    v: &Scalar,
    fail: &dyn Fn(String) -> EncodeError,
) -> Encoded<()> {
    match name {
        "BOOL" => add(args, opt(v, |v| as_bool(v, fail))?, fail),
        // The width is not optional: an `INT4` slot is four bytes on the wire,
        // and binding an `i64` into it is the same defect this module exists to
        // close, wearing a different hat.
        "INT2" => add(args, opt(v, |v| as_int::<i16>(v, fail))?, fail),
        "INT4" => add(args, opt(v, |v| as_int::<i32>(v, fail))?, fail),
        "INT8" => add(args, opt(v, |v| as_i64(v, fail))?, fail),
        "OID" => add(
            args,
            opt(v, |v| {
                as_int::<u32>(v, fail).map(sqlx::postgres::types::Oid)
            })?,
            fail,
        ),
        "FLOAT4" => add(args, opt(v, |v| as_f64(v, fail).map(|f| f as f32))?, fail),
        "FLOAT8" => add(args, opt(v, |v| as_f64(v, fail))?, fail),
        "NUMERIC" => add(args, opt(v, |v| as_decimal(v, fail))?, fail),
        "TEXT" | "VARCHAR" | "CHAR" | "NAME" | "citext" | "UNKNOWN" => {
            add(args, opt(v, |v| as_str(v, fail))?, fail)
        }
        "UUID" => add(args, opt(v, |v| as_uuid(v, fail))?, fail),
        "JSON" | "JSONB" => add(args, opt(v, |v| as_json(v, fail))?, fail),
        "TIMESTAMPTZ" => add(args, opt(v, |v| as_timestamptz(v, fail))?, fail),
        "TIMESTAMP" => add(args, opt(v, |v| as_timestamp(v, fail))?, fail),
        "DATE" => add(args, opt(v, |v| as_date(v, fail))?, fail),
        "TIME" => add(args, opt(v, |v| as_time(v, fail))?, fail),
        // Unreachable: `pg_arguments` screens every declared type against
        // `BINDABLE` before it binds anything. Kept as an error rather than a
        // panic so a future name added to one list and not the other degrades
        // instead of aborting a request.
        other => Err(fail(format!("no binding for {other} is defined"))),
    }
}

/// The array table. Element names are the scalar table's, which is the
/// invariant `BINDABLE` exists to hold.
fn add_array(
    args: &mut PgArguments,
    elem: &str,
    v: &Scalar,
    fail: &dyn Fn(String) -> EncodeError,
) -> Encoded<()> {
    let items: Option<Vec<Scalar>> = match v {
        Scalar::Null => None,
        Scalar::Json(Value::Array(a)) => Some(a.iter().map(Scalar::from).collect()),
        other => {
            return Err(fail(format!(
                "expected an array, got {}",
                other.kind_name()
            )));
        }
    };

    /// Coerce every element through one scalar conversion.
    macro_rules! each {
        ($f:expr) => {
            match &items {
                None => None,
                Some(list) => Some(
                    list.iter()
                        .map(|s| opt(s, $f))
                        .collect::<Encoded<Vec<_>>>()?,
                ),
            }
        };
    }

    match elem {
        "BOOL" => add(args, each!(|v| as_bool(v, fail)), fail),
        "INT2" => add(args, each!(|v| as_int::<i16>(v, fail)), fail),
        "INT4" => add(args, each!(|v| as_int::<i32>(v, fail)), fail),
        "INT8" => add(args, each!(|v| as_i64(v, fail)), fail),
        "OID" => add(
            args,
            each!(|v| as_int::<u32>(v, fail).map(sqlx::postgres::types::Oid)),
            fail,
        ),
        "FLOAT4" => add(args, each!(|v| as_f64(v, fail).map(|f| f as f32)), fail),
        "FLOAT8" => add(args, each!(|v| as_f64(v, fail)), fail),
        "NUMERIC" => add(args, each!(|v| as_decimal(v, fail)), fail),
        "TEXT" | "VARCHAR" | "CHAR" | "NAME" | "citext" | "UNKNOWN" => {
            add(args, each!(|v| as_str(v, fail)), fail)
        }
        "UUID" => add(args, each!(|v| as_uuid(v, fail)), fail),
        "JSON" | "JSONB" => add(args, each!(|v| as_json(v, fail)), fail),
        "TIMESTAMPTZ" => add(args, each!(|v| as_timestamptz(v, fail)), fail),
        "TIMESTAMP" => add(args, each!(|v| as_timestamp(v, fail)), fail),
        "DATE" => add(args, each!(|v| as_date(v, fail)), fail),
        "TIME" => add(args, each!(|v| as_time(v, fail)), fail),
        other => Err(fail(format!(
            "no binding for an array of {other} is defined"
        ))),
    }
}

// ---- The conversions ----
//
// Each accepts the JSON spelling `sql_decode` *emits* for that type, so a value
// read out of one query binds into the next unchanged. That round trip is the
// property these are written against.

fn as_i64(v: &Scalar, fail: &dyn Fn(String) -> EncodeError) -> Encoded<i64> {
    match v {
        Scalar::Int(i) => Ok(*i),
        // A JSON number that happens to be whole. `1.0` is an integer; `1.5` is
        // a different value and saying so is better than truncating it.
        Scalar::Float(f) if f.fract() == 0.0 && *f >= -(2f64.powi(63)) && *f < 2f64.powi(63) => {
            Ok(*f as i64)
        }
        Scalar::Str(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|e| fail(format!("expected an integer, got a string ({e})"))),
        other => Err(fail(format!(
            "expected an integer, got {}",
            other.kind_name()
        ))),
    }
}

fn as_int<T>(v: &Scalar, fail: &dyn Fn(String) -> EncodeError) -> Encoded<T>
where
    T: TryFrom<i64>,
{
    let n = as_i64(v, fail)?;
    T::try_from(n).map_err(|_| {
        fail(format!(
            "{n} is outside the range of this column's integer type"
        ))
    })
}

fn as_f64(v: &Scalar, fail: &dyn Fn(String) -> EncodeError) -> Encoded<f64> {
    let f = match v {
        Scalar::Int(i) => *i as f64,
        Scalar::Float(f) => *f,
        Scalar::Str(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|e| fail(format!("expected a number, got a string ({e})")))?,
        other => {
            return Err(fail(format!(
                "expected a number, got {}",
                other.kind_name()
            )));
        }
    };
    // The mirror of `sql_decode::float_to_json`, which refuses to render one.
    if f.is_finite() {
        Ok(f)
    } else {
        Err(fail("the value is not a finite number".to_string()))
    }
}

fn as_decimal(v: &Scalar, fail: &dyn Fn(String) -> EncodeError) -> Encoded<bigdecimal::BigDecimal> {
    use std::str::FromStr;
    // The string form is the lossless one, and the bind-side twin of
    // `numeric_as: "string"`: a JSON number has already been through `f64` by
    // the time it arrives here, so its digits beyond 2^53 are gone whatever
    // this does with them.
    let text = match v {
        Scalar::Str(s) => s.trim().to_string(),
        Scalar::Int(i) => i.to_string(),
        Scalar::Float(f) => f.to_string(),
        other => {
            return Err(fail(format!(
                "expected a decimal, got {}",
                other.kind_name()
            )));
        }
    };
    bigdecimal::BigDecimal::from_str(&text)
        .map_err(|e| fail(format!("expected a decimal number ({e})")))
}

fn as_str(v: &Scalar, _fail: &dyn Fn(String) -> EncodeError) -> Encoded<String> {
    Ok(match v {
        Scalar::Str(s) => s.clone(),
        Scalar::Int(i) => i.to_string(),
        Scalar::Float(f) => f.to_string(),
        Scalar::Bool(b) => b.to_string(),
        Scalar::Json(j) => j.to_string(),
        Scalar::Null => String::new(),
    })
}

fn as_bool(v: &Scalar, fail: &dyn Fn(String) -> EncodeError) -> Encoded<bool> {
    match v {
        Scalar::Bool(b) => Ok(*b),
        Scalar::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(fail(
                "expected a boolean, or the string \"true\" or \"false\"".to_string(),
            )),
        },
        other => Err(fail(format!(
            "expected a boolean, got {}",
            other.kind_name()
        ))),
    }
}

fn as_uuid(v: &Scalar, fail: &dyn Fn(String) -> EncodeError) -> Encoded<uuid::Uuid> {
    match v {
        Scalar::Str(s) => uuid::Uuid::parse_str(s.trim())
            .map_err(|e| fail(format!("expected a UUID string ({e})"))),
        other => Err(fail(format!(
            "expected a UUID string, got {}",
            other.kind_name()
        ))),
    }
}

fn as_json(v: &Scalar, fail: &dyn Fn(String) -> EncodeError) -> Encoded<Value> {
    Ok(match v {
        Scalar::Json(j) => j.clone(),
        Scalar::Bool(b) => Value::Bool(*b),
        Scalar::Int(i) => Value::Number((*i).into()),
        Scalar::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .ok_or_else(|| fail("the value is not a finite number".to_string()))?,
        // Parsed, not wrapped. PostgreSQL's own `text` -> `json` cast parses,
        // so an author who has been passing a serialised document into a
        // `jsonb` column — which works today — keeps storing a document rather
        // than silently starting to store a string containing one.
        Scalar::Str(s) => {
            serde_json::from_str(s).map_err(|e| fail(format!("expected JSON text ({e})")))?
        }
        Scalar::Null => Value::Null,
    })
}

fn as_timestamptz(
    v: &Scalar,
    fail: &dyn Fn(String) -> EncodeError,
) -> Encoded<chrono::DateTime<chrono::Utc>> {
    let s = match v {
        Scalar::Str(s) => s.trim().to_string(),
        other => {
            return Err(fail(format!(
                "expected a timestamp string, got {}",
                other.kind_name()
            )));
        }
    };
    // RFC 3339 is what `sql_decode` emits, and the space-separated variant is
    // what PostgreSQL prints. Both carry an offset, which is the part that
    // matters: a timestamp with no offset means whatever the session's
    // TimeZone says, and quietly choosing UTC on the author's behalf would be
    // the silent reinterpretation this module exists to prevent. The remedy is
    // in the error — cast the placeholder and let the server read it.
    chrono::DateTime::parse_from_rfc3339(&s)
        .or_else(|_| chrono::DateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f%#z"))
        .or_else(|_| chrono::DateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f%:z"))
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            fail(format!(
                "expected a timestamp with a UTC offset, such as \
                 \"2026-09-02T05:00:00Z\" ({e})"
            ))
        })
}

fn as_timestamp(
    v: &Scalar,
    fail: &dyn Fn(String) -> EncodeError,
) -> Encoded<chrono::NaiveDateTime> {
    let s = match v {
        Scalar::Str(s) => s.trim().to_string(),
        other => {
            return Err(fail(format!(
                "expected a timestamp string, got {}",
                other.kind_name()
            )));
        }
    };
    // The space-separated form first: that is what `sql_decode` emits for this
    // type — chrono's `Display` for `NaiveDateTime`, not RFC 3339 — so it is
    // the spelling a value read out of one query arrives in.
    chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f"))
        .or_else(|_| {
            chrono::DateTime::parse_from_rfc3339(&s)
                .map(|dt| dt.with_timezone(&chrono::Utc).naive_utc())
        })
        .map_err(|e| {
            fail(format!(
                "expected a timestamp such as \"2026-09-02 05:00:00\" ({e})"
            ))
        })
}

fn as_date(v: &Scalar, fail: &dyn Fn(String) -> EncodeError) -> Encoded<chrono::NaiveDate> {
    match v {
        Scalar::Str(s) => chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
            .map_err(|e| fail(format!("expected a date such as \"2026-09-02\" ({e})"))),
        other => Err(fail(format!(
            "expected a date string, got {}",
            other.kind_name()
        ))),
    }
}

fn as_time(v: &Scalar, fail: &dyn Fn(String) -> EncodeError) -> Encoded<chrono::NaiveTime> {
    match v {
        Scalar::Str(s) => chrono::NaiveTime::parse_from_str(s.trim(), "%H:%M:%S%.f")
            .map_err(|e| fail(format!("expected a time such as \"05:00:00\" ({e})"))),
        other => Err(fail(format!(
            "expected a time string, got {}",
            other.kind_name()
        ))),
    }
}

/// Ask PostgreSQL what it declared for each placeholder, and bind to that.
///
/// Preparing with no declared types is what makes the server infer them, and
/// preparing on *this* connection is what makes the answer apply to the execute
/// that follows — the statement cache is per connection.
///
/// Falls back rather than failing in the two cases where the table has no
/// opinion: a declared type it does not carry, and a statement the server will
/// not describe. The second covers a genuine SQL error too, which is deliberate
/// — the execute reports it exactly as it does today, rather than this leg
/// inventing a second, worse spelling of the same failure.
pub async fn pg_typed_args(
    conn: &mut sqlx::PgConnection,
    sql: &str,
    params: Option<&[Scalar]>,
) -> Result<Bound<PgArguments>, EncodeError> {
    use sqlx::{Executor, Statement};

    // The portable dialect hands back `None` for a value shape this module does
    // not model. Nothing is prepared in that case, so nothing needs clearing.
    let Some(params) = params else {
        return Ok(Bound::Fallback { cache: false });
    };
    let Ok(statement) = conn.prepare(sql).await else {
        // Nothing was cached — the prepare is what would have cached it.
        return Ok(Bound::Fallback { cache: false });
    };
    let declared = match statement.parameters() {
        Some(sqlx::Either::Left(declared)) => declared.to_vec(),
        _ => return fall_back(conn, None).await,
    };
    match pg_arguments(&declared, params)? {
        Binding::Typed(args) => Ok(Bound::Typed(args)),
        Binding::Unsupported(sql_type) => fall_back(conn, Some(sql_type)).await,
    }
}

/// Give up on the typed path, and undo the prepare that got us here.
///
/// This is the part that is easy to miss. `prepare` caches the statement under
/// its SQL text with the types the *server* inferred, and sqlx looks that cache
/// up before it consults `persistent` — so a value-shaped bind issued next on
/// this connection would find the inferred statement and pour text bytes into,
/// say, an `inet` slot. Worse than the defect being fixed, not better.
///
/// Clearing costs the connection its other cached statements, which is why it
/// is on this path only: a query reaches it when its declared types have no
/// binding here, which is rare and already the degraded case.
async fn fall_back(
    conn: &mut sqlx::PgConnection,
    sql_type: Option<String>,
) -> Result<Bound<PgArguments>, EncodeError> {
    use sqlx::Connection;

    if let Some(sql_type) = sql_type {
        // Worth saying out loud: the query still runs, but it keeps the old
        // order-dependent binding, and nothing else would say so.
        tracing::debug!(
            sql_type = %sql_type,
            "parameter type has no typed binding; falling back to value-shaped binding"
        );
    }
    if let Err(e) = conn.clear_cached_statements().await {
        tracing::debug!(error = %e, "could not clear the statement cache after a fallback");
    }
    Ok(Bound::Fallback { cache: false })
}

/// The portable dialect's own values, as the driver's arguments.
///
/// `SqlxValues` implements `IntoArguments` for all four sqlx databases, so at a
/// call site inside `dispatch_sql_pool!` there is nothing to tell the compiler
/// which one is meant — and `DB::Arguments` is an associated type, so naming one
/// does not work backwards to its database either. The pool does: `Pool<DB>` is
/// an ordinary generic struct, and every arm already has one in scope. It is
/// borrowed only to pin `DB`, which is why it goes unread.
pub fn sea_args_for<'q, DB>(
    _pool: &sqlx::Pool<DB>,
    values: sea_query_sqlx::SqlxValues,
) -> DB::Arguments<'q>
where
    DB: sqlx::Database,
    sea_query_sqlx::SqlxValues: sqlx::IntoArguments<'q, DB>,
{
    sqlx::IntoArguments::into_arguments(values)
}

/// MySQL sends its parameter types with every `COM_STMT_EXECUTE`, so a cached
/// statement cannot carry a stale one and there is nothing here to fix.
pub async fn mysql_typed_args(
    _conn: &mut sqlx::MySqlConnection,
    _sql: &str,
    _params: Option<&[Scalar]>,
) -> Result<Bound<sqlx::mysql::MySqlArguments>, EncodeError> {
    Ok(Bound::Fallback { cache: true })
}

/// SQLite has no static parameter types at all — a value carries a storage
/// class, not a declared one — so the same holds for the same reason.
pub async fn sqlite_typed_args<'q>(
    _conn: &mut sqlx::SqliteConnection,
    _sql: &str,
    _params: Option<&[Scalar]>,
) -> Result<Bound<sqlx::sqlite::SqliteArguments<'q>>, EncodeError> {
    Ok(Bound::Fallback { cache: true })
}

#[cfg(test)]
mod tests {
    // The crate warns on `panic!` because production code should not have any.
    // A test that has to say *which* case failed is the exception the other
    // test modules in this crate already make.
    #![allow(clippy::panic)]

    use super::*;
    use serde_json::json;

    fn info<T: sqlx::Type<sqlx::Postgres>>() -> PgTypeInfo {
        T::type_info()
    }

    fn fail_for(sql_type: &str) -> impl Fn(String) -> EncodeError + '_ {
        move |detail| EncodeError {
            param: 0,
            sql_type: sql_type.to_string(),
            detail,
        }
    }

    /// Every name the screen admits must have an arm in *both* tables.
    ///
    /// This is the invariant `sql_decode` states for its own pair and the
    /// reason `BINDABLE` is one list: an element type the scalar table binds
    /// and the array table does not is an asymmetry no author can predict from
    /// the docs. A typed NULL is enough to reach the arm without needing a
    /// plausible value for every type.
    #[test]
    fn every_bindable_name_has_a_scalar_and_an_array_arm() {
        for name in BINDABLE {
            let f = fail_for(name);
            let mut args = PgArguments::default();
            assert!(
                add_scalar(&mut args, name, &Scalar::Null, &f).is_ok(),
                "{name} has no scalar arm"
            );
            let mut args = PgArguments::default();
            assert!(
                add_array(&mut args, name, &Scalar::Null, &f).is_ok(),
                "{name} has no array arm"
            );
        }
    }

    /// The defect, stated as a test: one placeholder, both JSON shapes.
    ///
    /// Neither call may depend on which came first, because neither call gets
    /// to decide the type any more.
    #[test]
    fn one_placeholder_takes_a_number_and_a_string() {
        let declared = vec![info::<i64>()];
        for value in [Scalar::Int(5), Scalar::Str("5".to_string())] {
            match pg_arguments(&declared, std::slice::from_ref(&value)) {
                Ok(Binding::Typed(_)) => {}
                _ => panic!("{value:?} did not bind against INT8"),
            }
        }

        let declared = vec![info::<String>()];
        for value in [Scalar::Int(5), Scalar::Str("5".to_string())] {
            match pg_arguments(&declared, std::slice::from_ref(&value)) {
                Ok(Binding::Typed(_)) => {}
                _ => panic!("{value:?} did not bind against TEXT"),
            }
        }
    }

    /// The width trap. An `INT4` slot is four bytes on the wire, so a value
    /// that only fits an `i64` has to be refused rather than truncated.
    #[test]
    fn an_int4_slot_refuses_a_value_that_does_not_fit() {
        let f = fail_for("INT4");
        assert!(as_int::<i32>(&Scalar::Int(i64::from(i32::MAX)), &f).is_ok());
        let Err(err) = as_int::<i32>(&Scalar::Int(i64::from(i32::MAX) + 1), &f) else {
            panic!("an out-of-range value must be refused");
        };
        assert!(err.detail.contains("outside the range"), "{}", err.detail);
    }

    /// A whole number is an integer whatever JSON called it; a fractional one
    /// is a different value, and truncating it silently is the class of bug
    /// this module exists to remove.
    #[test]
    fn a_fractional_number_is_not_an_integer() {
        let f = fail_for("INT8");
        assert_eq!(as_i64(&Scalar::Float(3.0), &f).ok(), Some(3));
        assert!(as_i64(&Scalar::Float(3.5), &f).is_err());
    }

    /// `sql_decode` renders a `TIMESTAMP` with chrono's `Display` — a space,
    /// not a `T` — so that is the spelling a value read out of one query
    /// arrives in, and it has to bind back into the next one.
    #[test]
    fn a_timestamp_accepts_the_spelling_sql_decode_emits() {
        let f = fail_for("TIMESTAMP");
        for s in [
            "2026-09-02 05:00:00",
            "2026-09-02 05:00:00.123",
            "2026-09-02T05:00:00",
            "2026-09-02T05:00:00Z",
        ] {
            assert!(
                as_timestamp(&Scalar::Str(s.to_string()), &f).is_ok(),
                "{s} was refused"
            );
        }
    }

    /// A `timestamptz` with no offset means whatever the session's TimeZone
    /// says. Choosing UTC on the author's behalf would be a silent
    /// reinterpretation, so it is refused and the message says what to do.
    #[test]
    fn a_timestamptz_requires_an_offset() {
        let f = fail_for("TIMESTAMPTZ");
        for s in [
            "2026-09-02T05:00:00Z",
            "2026-09-02T05:00:00+05:30",
            "2026-09-02 05:00:00+00",
        ] {
            assert!(
                as_timestamptz(&Scalar::Str(s.to_string()), &f).is_ok(),
                "{s} was refused"
            );
        }
        let Err(err) = as_timestamptz(&Scalar::Str("2026-09-02 05:00:00".to_string()), &f) else {
            panic!("a naive timestamp must be refused for timestamptz");
        };
        assert!(err.detail.contains("UTC offset"), "{}", err.detail);
    }

    /// The exact decimal survives, which is the bind-side twin of
    /// `numeric_as: "string"`.
    #[test]
    fn numeric_keeps_every_digit_of_a_string() {
        let f = fail_for("NUMERIC");
        let exact = "1234567890123456789.0123456";
        let d = as_decimal(&Scalar::Str(exact.to_string()), &f).expect("decimal");
        assert_eq!(d.to_string(), exact);
    }

    /// PostgreSQL's own `text` -> `json` cast parses, and passing a serialised
    /// document into a `jsonb` column works today because of it. Wrapping the
    /// string instead would keep the query working while quietly storing
    /// something else.
    #[test]
    fn json_text_is_parsed_rather_than_wrapped() {
        let f = fail_for("JSONB");
        assert_eq!(
            as_json(&Scalar::Str(r#"{"a":1}"#.to_string()), &f).expect("json"),
            json!({"a": 1})
        );
        assert_eq!(
            as_json(&Scalar::Json(json!([1, 2])), &f).expect("json"),
            json!([1, 2])
        );
        assert!(as_json(&Scalar::Str("not json".to_string()), &f).is_err());
    }

    /// A uuid parameter no longer needs `($1)::uuid` around it.
    #[test]
    fn a_uuid_binds_from_the_string_sql_decode_emits() {
        let declared = vec![info::<uuid::Uuid>()];
        let id = "11111111-2222-3333-4444-555555555555";
        assert!(matches!(
            pg_arguments(&declared, &[Scalar::Str(id.to_string())]),
            Ok(Binding::Typed(_))
        ));
        assert!(pg_arguments(&declared, &[Scalar::Str("nope".to_string())]).is_err());
    }

    /// An array binds element-wise, and a null element stays a null rather
    /// than becoming a string that says so.
    #[test]
    fn an_array_binds_element_wise_including_nulls() {
        let declared = vec![info::<Vec<i64>>()];
        assert!(matches!(
            pg_arguments(&declared, &[Scalar::Json(json!([1, "2", null]))]),
            Ok(Binding::Typed(_))
        ));
        assert!(pg_arguments(&declared, &[Scalar::Json(json!(["x"]))]).is_err());
        assert!(pg_arguments(&declared, &[Scalar::Int(1)]).is_err());
    }

    /// A declared type outside the table is not a failure — it is the signal
    /// to fall back to the value-shaped binding, which is what Orion does
    /// today and which keeps `INSERT INTO t (addr) VALUES ($1)` working.
    #[test]
    fn an_unmapped_declared_type_falls_back_rather_than_failing() {
        match pg_arguments(&[info::<Vec<u8>>()], &[Scalar::Str("aGk=".to_string())]) {
            Ok(Binding::Unsupported(name)) => assert_eq!(name, "BYTEA"),
            other => panic!("BYTEA should fall back, got {other:?}"),
        }
    }

    /// The path is chosen over the declared types alone. Whatever the values
    /// are, one SQL text cannot take the typed path on one call and the
    /// fallback on the next — sqlx looks the statement up before it consults
    /// `persistent`, so a mixed pair would misread a cached statement.
    #[test]
    fn the_path_does_not_depend_on_the_values() {
        let unmapped = [info::<Vec<u8>>()];
        for v in [
            Scalar::Null,
            Scalar::Int(1),
            Scalar::Str("x".to_string()),
            Scalar::Json(json!({"a": 1})),
        ] {
            assert!(
                matches!(pg_arguments(&unmapped, &[v]), Ok(Binding::Unsupported(_))),
                "the fallback decision moved with the value"
            );
        }
    }

    #[test]
    fn a_parameter_count_mismatch_is_named() {
        let Err(err) = pg_arguments(&[info::<i64>(), info::<i64>()], &[Scalar::Int(1)]) else {
            panic!("a mismatched parameter count must be refused");
        };
        assert!(err.detail.contains("placeholder(s)"), "{}", err.detail);
    }

    /// The message names the placeholder and the declared type, and carries
    /// the remedy — but never the value.
    #[test]
    fn the_message_names_the_placeholder_and_the_remedy() {
        let e = EncodeError {
            param: 2,
            sql_type: "INT8".to_string(),
            detail: "expected an integer, got a string".to_string(),
        };
        let m = e.message("db_read");
        assert!(m.contains("parameter $3"), "{m}");
        assert!(m.contains("INT8"), "{m}");
        assert!(m.contains("($3)::text"), "{m}");
    }

    /// A `u64` above `i64::MAX` keeps its digits rather than being rounded
    /// through `f64` — the same trick `bind_params` uses, preserved.
    #[test]
    fn a_huge_unsigned_number_travels_as_text() {
        let v = json!(u64::MAX);
        assert_eq!(Scalar::from(&v), Scalar::Str(u64::MAX.to_string()));
    }
}

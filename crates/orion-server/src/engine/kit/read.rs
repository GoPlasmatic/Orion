//! Orion's field readers, over the value a dataflow-rs 4 call evaluated
//! (I183).
//!
//! A function's expression fields are `Expr<OwnedDataValue>`, not typed
//! expressions, because the readers here coerce where a typed `Expr<String>`
//! or `Expr<i64>` would refuse: `required_str` spells a number or a boolean
//! out as a key, `optional_i64` reads a `null` result as absent. Running
//! workflows rely on both, so each reader is its dataflow-rs 3 twin in
//! `connector_helpers.rs` or `mongo_common.rs`, moved onto the value
//! `Ctx::eval` lends: the same coercions, the same `null` handling, the same
//! message and the same class. The twins stay for the dataflow-rs 3
//! handlers until those go, and the tests below hold the two to one
//! behaviour.
//!
//! Each reader takes the field's value as `Option<&OwnedDataValue>`: `None`
//! when the task leaves the field unset. A value that is not a string is
//! judged as dataflow-rs 3 judged it, through the JSON it bridged the
//! value to — a datetime as its text, a non-finite number as `null` — so a
//! datetime still reads as a key and a message still names the JSON type.
//! A string is lent, never copied.

use std::borrow::Cow;

use dataflow_rs::Expr;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::FromValue;
use serde_json::{Map, Value};

use crate::connector::sql_decode::{BinaryAs, NumericAs, RowFormat};
use crate::engine::functions::connector_helpers::{json_type_name, parse_duration_secs};
use crate::engine::{ErrorClass, HandlerError};

/// A reader's refusal: every one is the caller's to fix, a `400` with the
/// message kept, as dataflow-rs 3's `Validation` was.
fn refused(message: String) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, message)
}

/// `v` as the JSON value dataflow-rs 3 handed its reader for it.
fn json(v: &OwnedDataValue) -> Value {
    Value::from(v)
}

/// The "requires" refusal of a required field the task leaves unset.
fn requires(field: &str, handler: &str) -> HandlerError {
    refused(format!("{handler} requires '{field}' field"))
}

/// One key from a value: a string as itself, a number or a boolean spelled
/// out. Anything else is handed back, as JSON, for the caller's error.
fn scalar_to_key(v: &OwnedDataValue) -> Result<Cow<'_, str>, Value> {
    match v {
        OwnedDataValue::String(s) => Ok(Cow::Borrowed(s)),
        other => match json(other) {
            Value::String(s) => Ok(Cow::Owned(s)),
            Value::Number(n) => Ok(Cow::Owned(n.to_string())),
            Value::Bool(b) => Ok(Cow::Owned(b.to_string())),
            other => Err(other),
        },
    }
}

/// A `params` object, for the dialects that fold it into a filter's
/// `{"param": …}` nodes: the object's entries, or none for anything else.
/// `resolve_params`'s reading.
///
/// It takes the JSON a rendered `params` document serializes into
/// (`kit::rendered`), the value `resolve_params` was handed, rather than an
/// evaluated value: the field is a document, never an expression.
pub fn params(v: Option<Value>) -> Map<String, Value> {
    match v {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// A required field as a string. Scalars stringify; `null`, objects and
/// arrays are refused, so a path that does not resolve is an error rather
/// than the literal key `"null"`. `resolve_required_str`'s reading.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`] error when the field is unset or not a
/// scalar.
pub fn required_str<'a>(
    v: Option<&'a OwnedDataValue>,
    field: &str,
    handler: &str,
) -> Result<Cow<'a, str>, HandlerError> {
    let v = v.ok_or_else(|| requires(field, handler))?;
    scalar_to_key(v).map_err(|other| {
        refused(format!(
            "{handler} '{field}' must resolve to a string or number, got {}",
            json_type_name(&other)
        ))
    })
}

/// A field naming several keys (`cache_read.keys`, `cache_delete.keys`) as
/// strings, in order, at most `max` of them. Each element converts as
/// [`required_str`] converts one key. `resolve_required_str_list`'s reading.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`] error when the field is unset, not an
/// array, longer than `max`, or holds an element that is not a scalar.
pub fn required_str_list(
    v: Option<&OwnedDataValue>,
    field: &str,
    handler: &str,
    max: usize,
) -> Result<Vec<String>, HandlerError> {
    let v = v.ok_or_else(|| requires(field, handler))?;
    let not_keys = || {
        refused(format!(
            "{handler} '{field}' must resolve to an array of keys"
        ))
    };
    let items = match v {
        OwnedDataValue::Array(items) => items,
        _ => return Err(not_keys()),
    };
    if items.len() > max {
        return Err(refused(format!(
            "{handler} '{field}' resolved to {} keys; at most {max} are allowed in one call",
            items.len()
        )));
    }
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            scalar_to_key(item).map(Cow::into_owned).map_err(|other| {
                refused(format!(
                    "{handler} '{field}[{i}]' must resolve to a string or number, got {}",
                    json_type_name(&other)
                ))
            })
        })
        .collect()
}

/// Seconds from a duration field: integer seconds or a
/// [`parse_duration_secs`] string. `None` when the task leaves the field
/// unset or it resolves to `null`, so a caller with a default applies it.
/// Bounds stay with the caller. `resolve_duration_secs`'s reading.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`] error for a negative or fractional number,
/// a string that is not a duration, or any other type.
pub fn duration_secs(
    v: Option<&OwnedDataValue>,
    field: &str,
    handler: &str,
) -> Result<Option<u64>, HandlerError> {
    let Some(v) = v else {
        return Ok(None);
    };
    let from_text = |text: &str| {
        parse_duration_secs(text)
            .map(Some)
            .map_err(|e| refused(format!("{handler}: '{field}': {e}")))
    };
    if let OwnedDataValue::String(text) = v {
        return from_text(text);
    }
    match json(v) {
        // An expression whose path is absent resolves to null, which is the
        // same statement as "the task did not set this".
        Value::Null => Ok(None),
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| refused(format!("{handler}: '{field}' must be a positive integer")))
            .map(Some),
        Value::String(text) => from_text(&text),
        _ => Err(refused(format!(
            "{handler}: '{field}' must be seconds (integer) or a duration like \"24h\""
        ))),
    }
}

/// An optional field as a string: unset or `null` is `None`; anything but a
/// string is refused. `resolve_optional_str`'s reading.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`] error for a value that is not a string.
pub fn optional_str<'a>(
    v: Option<&'a OwnedDataValue>,
    field: &str,
    handler: &str,
) -> Result<Option<Cow<'a, str>>, HandlerError> {
    match v {
        None => Ok(None),
        Some(OwnedDataValue::String(s)) => Ok(Some(Cow::Borrowed(s))),
        Some(other) => match json(other) {
            Value::String(s) => Ok(Some(Cow::Owned(s))),
            Value::Null => Ok(None),
            _ => Err(refused(format!(
                "{handler}: '{field}' must resolve to a string"
            ))),
        },
    }
}

/// A boolean field, `default` when the task leaves it unset or it resolves
/// to `null`. A switch that would silently read `false` because its
/// expression produced a string is refused. `resolve_bool_or`'s reading.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`] error for a value that is not a boolean.
pub fn bool_or(
    v: Option<&OwnedDataValue>,
    field: &str,
    handler: &str,
    default: bool,
) -> Result<bool, HandlerError> {
    let Some(v) = v else {
        return Ok(default);
    };
    match json(v) {
        Value::Null => Ok(default),
        Value::Bool(b) => Ok(b),
        other => Err(refused(format!(
            "{handler} '{field}' must resolve to a boolean, got {}",
            json_type_name(&other)
        ))),
    }
}

/// [`bool_or`] with `false` as the default. `resolve_bool`'s reading.
///
/// # Errors
///
/// As [`bool_or`].
pub fn boolean(
    v: Option<&OwnedDataValue>,
    field: &str,
    handler: &str,
) -> Result<bool, HandlerError> {
    bool_or(v, field, handler, false)
}

/// An optional unsigned integer: unset or `null` is `None`; anything but a
/// non-negative integer is refused. `resolve_optional_u64`'s reading.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`] error for a negative or fractional number,
/// or a value that is not a number.
pub fn optional_u64(
    v: Option<&OwnedDataValue>,
    field: &str,
    handler: &str,
) -> Result<Option<u64>, HandlerError> {
    let Some(v) = v else {
        return Ok(None);
    };
    match json(v) {
        Value::Null => Ok(None),
        Value::Number(n) => n.as_u64().map(Some).ok_or_else(|| {
            refused(format!(
                "{handler} '{field}' must resolve to a non-negative integer"
            ))
        }),
        other => Err(refused(format!(
            "{handler} '{field}' must resolve to a number, got {}",
            json_type_name(&other)
        ))),
    }
}

/// [`optional_u64`]'s twin for a value that may be negative
/// (`cache_incr.by`). `resolve_optional_i64`'s reading.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`] error for a fractional number, one outside
/// 64 bits, or a value that is not a number.
pub fn optional_i64(
    v: Option<&OwnedDataValue>,
    field: &str,
    handler: &str,
) -> Result<Option<i64>, HandlerError> {
    let Some(v) = v else {
        return Ok(None);
    };
    match json(v) {
        Value::Null => Ok(None),
        Value::Number(n) => n.as_i64().map(Some).ok_or_else(|| {
            refused(format!(
                "{handler} '{field}' must resolve to a whole number that fits in 64 bits"
            ))
        }),
        other => Err(refused(format!(
            "{handler} '{field}' must resolve to a number, got {}",
            json_type_name(&other)
        ))),
    }
}

/// The positional `params` bound to a raw-SQL statement: unset or `null`
/// binds nothing; anything but an array is refused rather than dropped,
/// which would leave the statement's placeholders unbound.
/// `resolve_bind_params`'s reading.
///
/// It takes the JSON a rendered `params` document serializes into, as
/// [`params`] does.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`] error for a value that is not an array.
pub fn bind_params(v: Option<Value>, handler: &str) -> Result<Vec<Value>, HandlerError> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => Ok(items),
        other => Err(refused(format!(
            "{handler} 'params' must resolve to an array of bind values, got {}",
            json_type_name(&other)
        ))),
    }
}

/// `numeric_as`: how an arbitrary-precision decimal is rendered, a number
/// when unset. `resolve_numeric_as`'s reading.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`] error for a value that is not a string, or
/// a string that names no rendering.
pub fn numeric_as(v: Option<&OwnedDataValue>, handler: &str) -> Result<NumericAs, HandlerError> {
    match optional_str(v, "numeric_as", handler)? {
        None => Ok(NumericAs::default()),
        Some(s) => NumericAs::parse(&s).ok_or_else(|| {
            refused(format!(
                "{handler}: 'numeric_as' must be one of {} (got '{s}')",
                NumericAs::VALUES
            ))
        }),
    }
}

/// `binary_as`: how a binary column is rendered, `auto` when unset.
/// `resolve_binary_as`'s reading.
///
/// # Errors
///
/// As [`numeric_as`].
pub fn binary_as(v: Option<&OwnedDataValue>, handler: &str) -> Result<BinaryAs, HandlerError> {
    match optional_str(v, "binary_as", handler)? {
        None => Ok(BinaryAs::default()),
        Some(s) => BinaryAs::parse(&s).ok_or_else(|| {
            refused(format!(
                "{handler}: 'binary_as' must be one of {} (got '{s}')",
                BinaryAs::VALUES
            ))
        }),
    }
}

/// Both rendering choices a result set carries, `numeric_as` read first.
/// `resolve_row_format`'s reading.
///
/// # Errors
///
/// As [`numeric_as`] and [`binary_as`].
pub fn row_format(
    numeric: Option<&OwnedDataValue>,
    binary: Option<&OwnedDataValue>,
    handler: &str,
) -> Result<RowFormat, HandlerError> {
    Ok(RowFormat::new(
        numeric_as(numeric, handler)?,
        binary_as(binary, handler)?,
    ))
}

/// A MongoDB `limit` or `skip`: `mongo_common::resolve_u64`'s reading.
///
/// Its `null` rule is not [`optional_u64`]'s: a *literal* `null` is unset,
/// but an expression that resolves to `null` is refused. So `v` is `None`
/// when the task leaves the field unset or writes it as `null`, and the
/// rendered value otherwise.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`] error for anything but a non-negative
/// integer, a resolved `null` included.
pub fn mongo_u64(
    v: Option<&OwnedDataValue>,
    field: &str,
    handler: &str,
) -> Result<Option<u64>, HandlerError> {
    let Some(v) = v else {
        return Ok(None);
    };
    match json(v) {
        Value::Number(n) => n.as_u64().map(Some).ok_or_else(|| {
            refused(format!(
                "{handler} '{field}' must be a non-negative integer"
            ))
        }),
        other => Err(refused(format!(
            "{handler} '{field}' must resolve to a non-negative integer, got {}",
            json_type_name(&other)
        ))),
    }
}

/// dataflow-rs 3's `Template::resolve_string`, the reading of every string
/// parameter of its typed configs (`http_call`'s path, headers and formats,
/// `publish_kafka`'s topic and key): a string as itself, anything else its
/// compact JSON, so `7` is `"7"` and `null` is `"null"`. Never refused, as it
/// never was; a string is lent.
pub fn plain_str(v: &OwnedDataValue) -> Cow<'_, str> {
    match v {
        OwnedDataValue::String(s) => Cow::Borrowed(s),
        other => Cow::Owned(other.to_string()),
    }
}

/// dataflow-rs 3's `Template::resolve_u64` (`http_call.timeout_ms`): any
/// finite number from `0` to `u64::MAX`, a fraction cut to its whole part,
/// so `2.5` is `2`. `label` opens the refusal, which quotes the value.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`] error for a negative or non-finite number,
/// one past `u64::MAX`, or a value that is not a number.
pub fn template_u64(v: &OwnedDataValue, label: &str) -> Result<u64, HandlerError> {
    let read = match v {
        OwnedDataValue::Number(n) => {
            let f = n.as_f64();
            // Judged before the cast, which would saturate or give 0.
            (f.is_finite() && f >= 0.0 && f <= u64::MAX as f64).then_some(f as u64)
        }
        _ => None,
    };
    read.ok_or_else(|| {
        refused(format!(
            "{label} must evaluate to a non-negative number, got {v}"
        ))
    })
}

/// A parameter dataflow-rs 3's typed configs declared `Option<Template>`
/// (`http_call`'s `path`, `body`, `body_format` and `response_format`,
/// `publish_kafka`'s `key` and `value`), as serde read it: an explicit
/// `null` is the parameter left unset. dataflow-rs 4 compiles a `null` as a
/// constant expression, so the caller asks here first.
pub fn present<T: FromValue>(field: &Option<Expr<T>>) -> Option<&Expr<T>> {
    field.as_ref().filter(|expr| !expr.raw().is_null())
}

/// Each reader against its dataflow-rs 3 twin over the same JSON: the value
/// on success, and the class and the message on a refusal — computed from
/// the twin, never written out.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::functions::connector_helpers as v3;
    use crate::engine::functions::templated_input::TemplatedInput;
    use dataflow_rs_v3::engine::task_context::TaskContext;
    use serde_json::json;
    use std::sync::Arc;

    /// A field's value as a reader sees it, or its refusal's class and
    /// message.
    type Seen<T> = Result<T, (ErrorClass, String)>;

    fn seen_v3<T>(result: Result<T, dataflow_rs_v3::DataflowError>) -> Seen<T> {
        result.map_err(|e| {
            let e = HandlerError::from(e);
            (e.class, e.msg)
        })
    }

    fn seen_v4<T>(result: Result<T, HandlerError>) -> Seen<T> {
        result.map_err(|e| (e.class, e.msg))
    }

    /// The values every reader is held to: the coercions, the `null`s, each
    /// JSON type, and the numbers on the edges of 64 bits. `None` is the
    /// field left unset.
    fn values() -> Vec<Option<Value>> {
        vec![
            None,
            Some(Value::Null),
            Some(json!("orders")),
            Some(json!("")),
            Some(json!(7)),
            Some(json!(0)),
            Some(json!(-3)),
            Some(json!(2.5)),
            Some(json!(1e20)),
            Some(json!(u64::MAX)),
            Some(json!(i64::MIN)),
            Some(json!(true)),
            Some(json!(false)),
            Some(json!("5m")),
            Some(json!("24h")),
            Some(json!("90")),
            Some(json!("5x")),
            Some(json!("18446744073709551615d")),
            Some(json!("number")),
            Some(json!("string")),
            Some(json!("hex")),
            Some(json!("base64")),
            Some(json!([])),
            Some(json!(["a", 1, true])),
            Some(json!(["a", null])),
            Some(json!(["a", {"k": 1}])),
            Some(json!({"k": 1})),
        ]
    }

    /// Runs `f` with a dataflow-rs 3 task context over an empty message: the
    /// readers' templates are never compiled here, so each field reads as the
    /// literal it was written as, which is the value a compiled one resolves
    /// to.
    fn with_v3<R>(f: impl FnOnce(&TaskContext<'_>) -> R) -> R {
        let datalogic = Arc::new(dataflow_rs_v3::datalogic_rs::Engine::new());
        let mut message = dataflow_rs_v3::Message::from_value(&json!({}));
        let ctx = TaskContext::new(&mut message, &datalogic);
        f(&ctx)
    }

    /// The input dataflow-rs 3 reads `field` from, and the value dataflow-rs
    /// 4 hands the reader. dataflow-rs 3 saw a template's result through
    /// the JSON bridge, so its value is the bridged one: `u64::MAX` is a
    /// float to both.
    fn inputs(field: &str, value: Option<&Value>) -> (TemplatedInput, Option<OwnedDataValue>) {
        let owned = value.map(OwnedDataValue::from);
        let mut raw = Map::new();
        if let Some(owned) = &owned {
            raw.insert(field.to_string(), Value::from(owned));
        }
        (TemplatedInput::from(Value::Object(raw)), owned)
    }

    /// Holds one reader to its twin over every value.
    fn twin<T: PartialEq + std::fmt::Debug>(
        field: &str,
        v3_read: impl Fn(&TemplatedInput, &TaskContext<'_>) -> Result<T, dataflow_rs_v3::DataflowError>,
        v4_read: impl Fn(Option<&OwnedDataValue>) -> Result<T, HandlerError>,
    ) {
        for value in values() {
            let (input, owned) = inputs(field, value.as_ref());
            let v3 = with_v3(|ctx| seen_v3(v3_read(&input, ctx)));
            let v4 = seen_v4(v4_read(owned.as_ref()));
            assert_eq!(v4, v3, "{field} = {value:?}");
        }
    }

    const H: &str = "probe";

    #[test]
    fn required_str_is_resolve_required_str() {
        twin(
            "key",
            |input, ctx| v3::resolve_required_str(input, "key", H, ctx),
            |v| required_str(v, "key", H).map(Cow::into_owned),
        );
    }

    /// The coercions a typed `Expr<String>` would refuse, and every running
    /// `cache_*` caller can send.
    #[test]
    fn required_str_spells_out_a_number_and_a_boolean() {
        let seven = OwnedDataValue::from(&json!(7));
        let yes = OwnedDataValue::from(&json!(true));
        assert_eq!(
            required_str(Some(&seven), "key", H).ok().as_deref(),
            Some("7")
        );
        assert_eq!(
            required_str(Some(&yes), "key", H).ok().as_deref(),
            Some("true")
        );
        let text = OwnedDataValue::from(&json!("orders"));
        assert!(matches!(
            required_str(Some(&text), "key", H),
            Ok(Cow::Borrowed("orders"))
        ));
    }

    #[test]
    fn required_str_list_is_resolve_required_str_list() {
        for max in [0, 1, 1000] {
            twin(
                "keys",
                |input, ctx| v3::resolve_required_str_list(input, "keys", H, ctx, max),
                |v| required_str_list(v, "keys", H, max),
            );
        }
    }

    #[test]
    fn duration_secs_is_resolve_duration_secs() {
        twin(
            "ttl_secs",
            |input, ctx| v3::resolve_duration_secs(input, ctx, H, "ttl_secs"),
            |v| duration_secs(v, "ttl_secs", H),
        );
        let five_minutes = OwnedDataValue::from(&json!("5m"));
        assert_eq!(
            duration_secs(Some(&five_minutes), "ttl_secs", H).ok(),
            Some(Some(300))
        );
    }

    #[test]
    fn optional_str_is_resolve_optional_str() {
        twin(
            "from",
            |input, ctx| v3::resolve_optional_str(input, "from", H, ctx),
            |v| optional_str(v, "from", H).map(|s| s.map(Cow::into_owned)),
        );
    }

    #[test]
    fn bool_or_is_resolve_bool_or() {
        for default in [true, false] {
            twin(
                "upsert",
                |input, ctx| v3::resolve_bool_or(input, "upsert", H, ctx, default),
                |v| bool_or(v, "upsert", H, default),
            );
        }
        twin(
            "raw",
            |input, ctx| v3::resolve_bool(input, "raw", H, ctx),
            |v| boolean(v, "raw", H),
        );
    }

    #[test]
    fn optional_u64_is_resolve_optional_u64() {
        twin(
            "leeway_secs",
            |input, ctx| v3::resolve_optional_u64(input, "leeway_secs", H, ctx),
            |v| optional_u64(v, "leeway_secs", H),
        );
    }

    #[test]
    fn optional_i64_is_resolve_optional_i64() {
        twin(
            "by",
            |input, ctx| v3::resolve_optional_i64(input, "by", H, ctx),
            |v| optional_i64(v, "by", H),
        );
        assert_eq!(
            optional_i64(Some(&OwnedDataValue::Null), "by", H).ok(),
            Some(None)
        );
    }

    #[test]
    fn bind_params_is_resolve_bind_params() {
        twin(
            "params",
            |input, ctx| v3::resolve_bind_params(input, H, ctx),
            |v| bind_params(v.map(Value::from), H),
        );
    }

    #[test]
    fn params_is_resolve_params() {
        twin(
            "params",
            |input, ctx| Ok(v3::resolve_params(input, H, ctx)),
            |v| Ok(params(v.map(Value::from))),
        );
    }

    #[test]
    fn numeric_and_binary_as_are_their_twins() {
        twin(
            "numeric_as",
            |input, ctx| v3::resolve_numeric_as(input, H, ctx),
            |v| numeric_as(v, H),
        );
        twin(
            "binary_as",
            |input, ctx| v3::resolve_binary_as(input, H, ctx),
            |v| binary_as(v, H),
        );
        // Read together, `numeric_as` first.
        for (numeric, binary) in [
            (json!("string"), json!("hex")),
            (json!("money"), json!("hex")),
            (json!("string"), json!(7)),
        ] {
            let input = TemplatedInput::from(json!({"numeric_as": numeric, "binary_as": binary}));
            let v3 = with_v3(|ctx| seen_v3(v3::resolve_row_format(&input, H, ctx)));
            let (numeric, binary) = (
                OwnedDataValue::from(&numeric),
                OwnedDataValue::from(&binary),
            );
            let v4 = seen_v4(row_format(Some(&numeric), Some(&binary), H));
            assert_eq!(v4, v3);
        }
    }

    /// `plain_str` and `template_u64` against dataflow-rs 3's `Template`
    /// over every scalar, which an uncompiled template reads as itself. A
    /// compiled one, over every kind of value, is held to them by
    /// `http_call`'s twin over its typed config.
    #[test]
    fn plain_str_and_template_u64_are_templates_readers() {
        let label = "http_call timeout_ms";
        for value in values().into_iter().flatten() {
            if !(value.is_string() || value.is_number() || value.is_boolean()) {
                continue;
            }
            let template = dataflow_rs_v3::Template::from(value.clone());
            let owned = OwnedDataValue::from(&value);
            let v3 = with_v3(|ctx| template.resolve_string(ctx)).expect("a scalar resolves");
            assert_eq!(plain_str(&owned), v3, "{value}");
            let v3 = with_v3(|ctx| seen_v3(template.resolve_u64(ctx, label)));
            assert_eq!(seen_v4(template_u64(&owned, label)), v3, "{value}");
        }
        assert_eq!(plain_str(&OwnedDataValue::Null), "null");
        assert_eq!(
            plain_str(&OwnedDataValue::from(&json!({"a": [1]}))),
            r#"{"a":[1]}"#
        );
        assert!(matches!(
            plain_str(&OwnedDataValue::from(&json!("x"))),
            Cow::Borrowed("x")
        ));
        assert_eq!(
            template_u64(&OwnedDataValue::from(&json!(2.5)), label).ok(),
            Some(2)
        );
    }

    /// An explicit `null` is unset; anything else, a `null` an expression
    /// computes included, is the parameter.
    #[test]
    fn present_reads_an_authored_null_as_unset() {
        let exprs = crate::engine::kit::test_exprs();
        let compiled = |raw: Value| Some(exprs.compile::<OwnedDataValue>(&raw).expect("compiles"));
        assert!(present(&compiled(Value::Null)).is_none());
        assert!(present::<OwnedDataValue>(&None).is_none());
        assert!(present(&compiled(json!("/orders"))).is_some());
        assert!(present(&compiled(json!({"var": "data.missing"}))).is_some());
    }

    /// `mongo_common::resolve_u64` reads the authored field: a literal `null`
    /// is unset, which the caller says by passing `None`.
    #[test]
    fn mongo_u64_is_mongo_commons_resolve_u64() {
        for value in values() {
            let (input, owned) = inputs("limit", value.as_ref());
            let v3 = with_v3(|ctx| {
                seen_v3(crate::engine::functions::mongo_common::resolve_u64(
                    &input, "limit", H, ctx,
                ))
            });
            let authored_null = matches!(value, Some(Value::Null));
            let v4 = seen_v4(mongo_u64(
                owned.as_ref().filter(|_| !authored_null),
                "limit",
                H,
            ));
            assert_eq!(v4, v3, "limit = {value:?}");
        }
        // A path that resolves to `null` is not unset: dataflow-rs 3 folds
        // the hole and refuses what it finds.
        let input = TemplatedInput::from(json!({"limit": {"var": "data.missing"}}));
        let v3 = with_v3(|ctx| {
            seen_v3(crate::engine::functions::mongo_common::resolve_u64(
                &input, "limit", H, ctx,
            ))
        });
        let v4 = seen_v4(mongo_u64(Some(&OwnedDataValue::Null), "limit", H));
        assert_eq!(v4, v3);
        assert!(v4.is_err());
    }
}

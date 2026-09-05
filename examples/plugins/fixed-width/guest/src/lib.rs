//! `acme.fixedwidth`: a fixed-width record codec.
//!
//! Bank statements, mainframe extracts and clearing files still arrive as
//! lines where each field is a byte range — the account in columns 1–10, the
//! amount in 11–21, and so on. Parsing one in JSONLogic is a `substr` per
//! field with the offsets repeated for each; a codec is what a plugin is for.
//!
//! Two functions, both driven by a `spec`: an array of `{name, width, type}`
//! entries in column order, `type` one of `string` (trimmed), `number`
//! (digits, an optional leading `-`, parsed as an integer or a decimal) and
//! `date` (`YYYYMMDD`, emitted as `YYYY-MM-DD`).
//!
//! - `acme.fixedwidth.parse`: `record` + `spec` → an object keyed by field.
//! - `acme.fixedwidth.format`: `fields` + `spec` → the record. Strings are
//!   right-padded with spaces and truncated to the width; numbers are
//!   left-padded with zeros; dates are written back as eight digits.
//!
//! Every refusal is `caller-input`: the record or the fields were wrong for
//! this spec, and the same message cannot succeed on a retry.

use orion_plugin_sdk::{Plugin, PluginError, Value, export_plugin, json, serde_json};

struct FixedWidth;

struct Field {
    name: String,
    width: usize,
    kind: Kind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    String,
    Number,
    Date,
}

fn spec(input: &Value) -> Result<Vec<Field>, PluginError> {
    let entries = input["spec"].as_array().ok_or_else(|| {
        PluginError::caller_input("BAD_SPEC", "'spec' must be an array of {name, width, type}")
    })?;
    if entries.is_empty() {
        return Err(PluginError::caller_input("BAD_SPEC", "'spec' declares no fields"));
    }
    entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let name = entry["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .ok_or_else(|| PluginError::caller_input("BAD_SPEC", format!("spec[{i}].name must be a non-empty string")))?;
            let width = entry["width"]
                .as_u64()
                .filter(|w| *w > 0)
                .ok_or_else(|| PluginError::caller_input("BAD_SPEC", format!("spec[{i}].width must be a positive integer")))?;
            let kind = match entry["type"].as_str().unwrap_or("string") {
                "string" => Kind::String,
                "number" => Kind::Number,
                "date" => Kind::Date,
                other => {
                    return Err(PluginError::caller_input(
                        "BAD_SPEC",
                        format!("spec[{i}].type '{other}' is not one of string, number, date"),
                    ));
                }
            };
            if kind == Kind::Date && width != 8 {
                return Err(PluginError::caller_input(
                    "BAD_SPEC",
                    format!("spec[{i}]: a date field is 8 wide (YYYYMMDD), not {width}"),
                ));
            }
            Ok(Field {
                name: name.to_string(),
                width: width as usize,
                kind,
            })
        })
        .collect()
}

fn parse(input: &Value) -> Result<Value, PluginError> {
    let fields = spec(input)?;
    let record = input["record"]
        .as_str()
        .ok_or_else(|| PluginError::caller_input("BAD_RECORD", "'record' must be a string"))?;
    let chars: Vec<char> = record.chars().collect();
    let total: usize = fields.iter().map(|f| f.width).sum();
    if chars.len() < total {
        return Err(PluginError::caller_input(
            "RECORD_TOO_SHORT",
            format!("the spec needs {total} characters, the record has {}", chars.len()),
        ));
    }
    let mut out = serde_map();
    let mut at = 0;
    for field in &fields {
        let raw: String = chars[at..at + field.width].iter().collect();
        at += field.width;
        let value = match field.kind {
            Kind::String => Value::String(raw.trim().to_string()),
            Kind::Number => {
                let text = raw.trim();
                if text.is_empty() {
                    Value::Null
                } else {
                    // Leading zeros are the fixed-width convention, and
                    // `i64::from_str` accepts them; a decimal point falls
                    // through to the float parse.
                    let number = text
                        .parse::<i64>()
                        .map(serde_json::Number::from)
                        .ok()
                        .or_else(|| text.parse::<f64>().ok().and_then(serde_json::Number::from_f64))
                        .ok_or_else(|| {
                            PluginError::caller_input(
                                "BAD_NUMBER",
                                format!("'{}' is not a number: '{text}'", field.name),
                            )
                        })?;
                    Value::Number(number)
                }
            }
            Kind::Date => {
                let text = raw.trim();
                if text.is_empty() {
                    Value::Null
                } else if text.len() == 8 && text.chars().all(|c| c.is_ascii_digit()) {
                    let (y, m, d) = (&text[0..4], &text[4..6], &text[6..8]);
                    let (mm, dd): (u32, u32) = (m.parse().unwrap_or(0), d.parse().unwrap_or(0));
                    if !(1..=12).contains(&mm) || !(1..=31).contains(&dd) {
                        return Err(PluginError::caller_input(
                            "BAD_DATE",
                            format!("'{}' is not a calendar date: {text}", field.name),
                        ));
                    }
                    Value::String(format!("{y}-{m}-{d}"))
                } else {
                    return Err(PluginError::caller_input(
                        "BAD_DATE",
                        format!("'{}' is not YYYYMMDD: '{text}'", field.name),
                    ));
                }
            }
        };
        out.insert(field.name.clone(), value);
    }
    Ok(Value::Object(out))
}

fn format(input: &Value) -> Result<Value, PluginError> {
    let fields = spec(input)?;
    let values = input["fields"]
        .as_object()
        .ok_or_else(|| PluginError::caller_input("BAD_FIELDS", "'fields' must be an object"))?;
    let mut record = String::new();
    for field in &fields {
        let value = values.get(&field.name).unwrap_or(&Value::Null);
        let text = match (field.kind, value) {
            (_, Value::Null) => String::new(),
            (Kind::String, Value::String(s)) => s.clone(),
            (Kind::String, other) => other.to_string(),
            (Kind::Number, Value::Number(n)) => n.to_string(),
            (Kind::Number, other) => {
                return Err(PluginError::caller_input(
                    "BAD_NUMBER",
                    format!("'{}' must be a number, got {other}", field.name),
                ));
            }
            (Kind::Date, Value::String(s)) => s.chars().filter(|c| c.is_ascii_digit()).collect(),
            (Kind::Date, other) => {
                return Err(PluginError::caller_input(
                    "BAD_DATE",
                    format!("'{}' must be a YYYY-MM-DD string, got {other}", field.name),
                ));
            }
        };
        let count = text.chars().count();
        if count > field.width {
            if field.kind == Kind::String {
                record.extend(text.chars().take(field.width));
            } else {
                return Err(PluginError::caller_input(
                    "FIELD_TOO_WIDE",
                    format!("'{}' is {count} characters, the field is {}", field.name, field.width),
                ));
            }
            continue;
        }
        let pad = field.width - count;
        match field.kind {
            Kind::String => {
                record.push_str(&text);
                record.extend(std::iter::repeat_n(' ', pad));
            }
            Kind::Number | Kind::Date => {
                record.extend(std::iter::repeat_n(if text.is_empty() { ' ' } else { '0' }, pad));
                record.push_str(&text);
            }
        }
    }
    Ok(json!({ "record": record }))
}

fn serde_map() -> serde_json::Map<String, Value> {
    serde_json::Map::new()
}

impl Plugin for FixedWidth {
    fn invoke(function: &str, input: Value) -> Result<Value, PluginError> {
        match function {
            "acme.fixedwidth.parse" => parse(&input),
            "acme.fixedwidth.format" => format(&input),
            other => Err(PluginError::caller_input(
                "UNKNOWN_FUNCTION",
                format!("this component exports no '{other}'"),
            )),
        }
    }
}

export_plugin!(FixedWidth);

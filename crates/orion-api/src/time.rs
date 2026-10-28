//! The one timestamp format on the wire: RFC 3339 in UTC with a `Z`
//! (`2026-01-01T00:00:00Z`, fractional seconds only when present).
//!
//! The DTOs hold `NaiveDateTime` — every stored instant is UTC — and these
//! `serde(with)` modules put the zone on the wire so a client never has to guess
//! it. Reading is tolerant: a value with an offset is converted to UTC, and one
//! with no zone designator is taken as UTC, which is what older servers sent.

use chrono::{DateTime, NaiveDateTime, SecondsFormat};
use serde::{Deserialize, Deserializer, Serializer};

fn format(value: &NaiveDateTime) -> String {
    value.and_utc().to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn parse(text: &str) -> Result<NaiveDateTime, String> {
    if let Ok(with_zone) = DateTime::parse_from_rfc3339(text) {
        return Ok(with_zone.naive_utc());
    }
    NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
        .map_err(|e| format!("invalid timestamp '{text}': {e}"))
}

/// `serde(with = "crate::time::rfc3339")` for a `NaiveDateTime`.
pub mod rfc3339 {
    use super::*;

    pub fn serialize<S: Serializer>(value: &NaiveDateTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format(value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<NaiveDateTime, D::Error> {
        let text = String::deserialize(d)?;
        parse(&text).map_err(serde::de::Error::custom)
    }
}

/// `serde(with = "crate::time::rfc3339_opt")` for an `Option<NaiveDateTime>`.
pub mod rfc3339_opt {
    use super::*;

    pub fn serialize<S: Serializer>(
        value: &Option<NaiveDateTime>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => s.serialize_str(&format(v)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NaiveDateTime>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|text| parse(&text).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Row {
        #[serde(with = "rfc3339")]
        at: NaiveDateTime,
        #[serde(default, with = "rfc3339_opt")]
        maybe: Option<NaiveDateTime>,
    }

    #[test]
    fn writes_utc_with_a_z() {
        let at = NaiveDateTime::parse_from_str("2026-01-02T03:04:05", "%Y-%m-%dT%H:%M:%S")
            .expect("test");
        let json = serde_json::to_value(Row {
            at,
            maybe: Some(at),
        })
        .expect("test");
        assert_eq!(json["at"], "2026-01-02T03:04:05Z");
        assert_eq!(json["maybe"], "2026-01-02T03:04:05Z");
    }

    #[test]
    fn keeps_fractional_seconds_and_reads_every_form() {
        let a: Row = serde_json::from_str(r#"{"at":"2026-01-02T03:04:05.250Z"}"#).expect("test");
        assert_eq!(
            serde_json::to_value(&a).expect("test")["at"],
            "2026-01-02T03:04:05.250Z"
        );
        let zoneless: Row = serde_json::from_str(r#"{"at":"2026-01-02T03:04:05"}"#).expect("test");
        let offset: Row =
            serde_json::from_str(r#"{"at":"2026-01-02T05:04:05+02:00"}"#).expect("test");
        assert_eq!(zoneless.at, offset.at);
        assert!(zoneless.maybe.is_none());
    }

    #[test]
    fn rejects_a_non_timestamp() {
        assert!(serde_json::from_str::<Row>(r#"{"at":"yesterday"}"#).is_err());
    }
}

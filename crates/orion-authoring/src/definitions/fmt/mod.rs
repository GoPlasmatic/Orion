//! `orion-server fmt`: one canonical layout for every definition file.
//!
//! The formatter has one style and no configuration — see [`style`] for the
//! numbers and the key tables, [`roles`] for how a node is recognised, and
//! [`printer`] for the layout itself. This module is the entry point and,
//! more importantly, the **guards**: a formatter that runs unattended in a
//! pre-commit hook must never change what a document means, so
//! [`format_str`] re-parses its own output and compares it with the input as
//! `serde_json::Value` — the runtime's view — before anything is returned.
//! A mismatch is a formatter bug, and it is reported as one rather than
//! written.
//!
//! What the formatter changes: whitespace, string escapes (re-emitted
//! canonically), and the order of *known* keys in *known* object shapes.
//! What it never changes: values, number spellings, array order, or the
//! order of keys it does not recognise.

pub mod printer;
pub mod roles;
pub mod style;

use crate::definitions::json::{Document, ParseError};

/// What formatting one document produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Already in the house style, byte for byte with what was read — a file
    /// that differs only in a BOM or its line endings is `Changed`.
    Unchanged,
    Changed(String),
}

#[derive(Debug)]
pub enum FmtError {
    /// Not a JSON document. Reported with `line, column`.
    Parse(ParseError),
    /// The formatted text does not convert to the same value as the input.
    /// A bug in this module; the file must not be written.
    RoundTrip { origin: String },
    /// Formatting the output again changed it. Also a bug; checked in debug
    /// builds and tests, where the cost of a second pass is irrelevant.
    NotIdempotent { origin: String },
}

impl std::fmt::Display for FmtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FmtError::Parse(e) => write!(f, "{e}"),
            FmtError::RoundTrip { origin } => write!(
                f,
                "formatter bug: the formatted output of '{origin}' does not parse to the same \
                 document — nothing was written; please report this with the file"
            ),
            FmtError::NotIdempotent { origin } => write!(
                f,
                "formatter bug: formatting '{origin}' twice gives two different results — \
                 nothing was written; please report this with the file"
            ),
        }
    }
}

impl std::error::Error for FmtError {}

impl From<ParseError> for FmtError {
    fn from(e: ParseError) -> Self {
        FmtError::Parse(e)
    }
}

/// Format one document. `origin` names it in an error.
pub fn format_str(text: &str, origin: &str) -> Result<Outcome, FmtError> {
    let doc = Document::parse(text)?;
    let formatted = format_document(&doc);

    // The guard. One extra parse of a small file, every time, in every
    // build: this is the line between "the formatter has a bug" and "the
    // formatter corrupted a workflow".
    let reparsed = Document::parse(&formatted).map_err(|_| FmtError::RoundTrip {
        origin: origin.to_string(),
    })?;
    if reparsed.to_value() != doc.to_value() {
        return Err(FmtError::RoundTrip {
            origin: origin.to_string(),
        });
    }
    if cfg!(any(test, debug_assertions)) && format_document(&reparsed) != formatted {
        return Err(FmtError::NotIdempotent {
            origin: origin.to_string(),
        });
    }

    // Byte-for-byte against what was read, so `--check` is exact about BOMs
    // and line endings — they are part of what `fmt` fixes.
    if text == formatted {
        Ok(Outcome::Unchanged)
    } else {
        Ok(Outcome::Changed(formatted))
    }
}

/// Lay out an already-parsed document. No guard: [`format_str`] is the
/// entry point that promises correctness.
pub fn format_document(doc: &Document) -> String {
    printer::print(&doc.root.node)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(text: &str) -> String {
        match format_str(text, "test").expect("test input is valid") {
            Outcome::Unchanged => text.to_string(),
            Outcome::Changed(s) => s,
        }
    }

    #[test]
    fn a_minified_workflow_takes_the_house_layout() {
        let input = r#"{"tasks":[{"function":{"input":{"target":"order","source":"payload"},"name":"parse_json"},"name":"Parse","id":"parse"},{"id":"flag","name":"Flag","condition":{">":[{"var":"data.order.total"},10000]},"function":{"name":"map","input":{"mappings":[{"logic":true,"path":"data.order.flagged"},{"path":"data.order.alert","logic":{"cat":["High-value order: $",{"var":"data.order.total"}]}}]}}}],"name":"High-Value Order","condition":true,"workflow_id":"high-value-order","tags":["pkg:high-value-order"]}"#;
        let expected = r#"{
  "workflow_id": "high-value-order",
  "name": "High-Value Order",
  "tags": ["pkg:high-value-order"],
  "condition": true,
  "tasks": [
    {
      "id": "parse",
      "name": "Parse",
      "function": { "name": "parse_json", "input": { "source": "payload", "target": "order" } }
    },
    {
      "id": "flag",
      "name": "Flag",
      "condition": { ">": [{ "var": "data.order.total" }, 10000] },
      "function": {
        "name": "map",
        "input": {
          "mappings": [
            { "path": "data.order.flagged", "logic": true },
            {
              "path": "data.order.alert",
              "logic": { "cat": ["High-value order: $", { "var": "data.order.total" }] }
            }
          ]
        }
      }
    }
  ]
}
"#;
        assert_eq!(fmt(input), expected);
    }

    #[test]
    fn compound_logic_breaks_one_argument_per_line_and_leaves_inline() {
        let input = r#"{"condition":{"and":[{">=":[{"var":"data.order.amount"},100]},{"<":[{"var":"data.order.amount"},500]}]}}"#;
        let expected = r#"{
  "condition": {
    "and": [
      { ">=": [{ "var": "data.order.amount" }, 100] },
      { "<": [{ "var": "data.order.amount" }, 500] }
    ]
  }
}
"#;
        assert_eq!(fmt(input), expected);
    }

    #[test]
    fn a_unary_node_stays_on_one_line_past_the_width() {
        // The generic root does not fit, so it breaks; the unary read inside
        // it is printed flat regardless of its length.
        let long = "x".repeat(120);
        let input = format!(r#"{{"a":{{"var":"{long}"}}}}"#);
        let out = fmt(&input);
        assert_eq!(out, format!("{{\n  \"a\": {{ \"var\": \"{long}\" }}\n}}\n"));
    }

    #[test]
    fn a_leaf_node_breaks_when_it_does_not_fit() {
        let long = "y".repeat(90);
        let input = format!(r#"{{">=":[{{"var":"{long}"}},500]}}"#);
        let out = fmt(&input);
        assert_eq!(
            out,
            format!("{{\n  \">=\": [\n    {{ \"var\": \"{long}\" }},\n    500\n  ]\n}}\n")
        );
    }

    #[test]
    fn scalar_arrays_inline_up_to_the_cap() {
        assert_eq!(
            fmt(r#"{"methods":["POST"]}"#),
            "{ \"methods\": [\"POST\"] }\n"
        );
        let nine = fmt(r#"{"n":[1,2,3,4,5,6,7,8,9]}"#);
        assert!(nine.starts_with("{\n  \"n\": [\n    1,\n"), "{nine}");
        let eight = fmt(r#"{"n":[1,2,3,4,5,6,7,8]}"#);
        assert_eq!(eight, "{ \"n\": [1, 2, 3, 4, 5, 6, 7, 8] }\n");
    }

    #[test]
    fn empty_containers_and_scalars_at_the_root() {
        assert_eq!(fmt("{}"), "{}\n");
        assert_eq!(fmt("[]"), "[]\n");
        assert_eq!(fmt(r#"{"a":{},"b":[]}"#), "{ \"a\": {}, \"b\": [] }\n");
        assert_eq!(fmt("42"), "42\n");
        assert_eq!(fmt("\"s\""), "\"s\"\n");
    }

    #[test]
    fn unchanged_is_exact_and_normalises_bom_and_crlf() {
        let canonical = "{ \"a\": 1 }\n";
        assert_eq!(
            format_str(canonical, "t").expect("test input is valid"),
            Outcome::Unchanged
        );
        assert_eq!(
            format_str("\u{feff}{ \"a\": 1 }\r\n", "t").expect("test input is valid"),
            Outcome::Changed(canonical.to_string())
        );
        assert_eq!(
            format_str("{ \"a\": 1 }", "t").expect("test input is valid"),
            Outcome::Changed(canonical.to_string()),
            "a missing trailing newline is a change"
        );
    }

    #[test]
    fn numbers_and_unknown_keys_are_preserved() {
        let out = fmt(r#"{"zeta":1.0,"alpha":1e3,"neg":-0,"big":123456789012345678901234567890}"#);
        assert_eq!(
            out,
            "{ \"zeta\": 1.0, \"alpha\": 1e3, \"neg\": -0, \"big\": 123456789012345678901234567890 }\n"
        );
    }

    #[test]
    fn from_sorts_first_everywhere() {
        let out = fmt(r#"{"collection":"users","$from":"constants.db"}"#);
        assert_eq!(
            out,
            "{ \"$from\": \"constants.db\", \"collection\": \"users\" }\n"
        );
    }

    #[test]
    fn a_parse_error_is_reported_with_its_position() {
        let err =
            format_str("{\n  \"a\": 1,\n  \"a\": 2\n}", "dup.json").expect_err("must be refused");
        assert!(matches!(err, FmtError::Parse(_)));
        assert!(err.to_string().starts_with("line 3, column 3"), "{err}");
    }
}

//! The runtime vocabulary supplied to the lightweight definition formatter.

use std::sync::LazyLock;

use orion_definitions::fmt::Formatter;
use orion_definitions::json::Document;

pub use orion_definitions::fmt::{FmtError, Outcome, style};

/// The house formatter with this build's operator names and Orion input order.
/// Plugin inputs retain author order, independent of what a node has loaded.
pub fn formatter() -> &'static Formatter {
    static FORMATTER: LazyLock<Formatter> = LazyLock::new(|| {
        Formatter::new(
            crate::engine::operators::operator_names().iter().cloned(),
            crate::engine::functions::schema::registry()
                .iter()
                .map(|schema| {
                    (
                        schema.name.to_string(),
                        schema
                            .input_fields
                            .iter()
                            .map(|field| field.name.to_string())
                            .collect(),
                    )
                }),
        )
    });
    &FORMATTER
}

pub fn format_str(text: &str, origin: &str) -> Result<Outcome, FmtError> {
    formatter().format_str(text, origin)
}

pub fn format_document(doc: &Document) -> String {
    formatter().format_document(doc)
}

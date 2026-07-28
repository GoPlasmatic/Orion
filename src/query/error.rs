//! Located errors for query translation.
//!
//! Every failure names the offending construct and where it occurred (`at` is a
//! best-effort dotted location inside `filter`), so a query-builder UI or a
//! workflow author can see precisely why a query is not runnable. All variants
//! map to `DataflowError::Validation` (a 4xx-style error) at the handler edge.

use dataflow_rs::engine::error::DataflowError;

/// A translation-time error, produced during envelope parse, lowering, or render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// An operator outside the portable vocabulary was used.
    UnsupportedInQuery { op: String, at: String },
    /// A construct is recognised but has no portable form in v1
    /// (e.g. column-to-column comparison).
    NotRepresentable { what: String, at: String },
    /// The envelope (`source`/`fields`/`sort`/…) is malformed.
    InvalidEnvelope(String),
    /// A field reference is invalid for identity mode (e.g. a dotted JSON path).
    InvalidField { field: String, at: String },
    /// A `{"param": name}` referenced a name absent from the params map.
    MissingParam { name: String, at: String },
    /// A `some`/`all`/`none` referenced a relation not declared in the schema.
    UnknownRelation { relation: String, at: String },
    /// The requested page size exceeds the configured hard maximum.
    LimitExceeded { requested: u64, max: u64 },
    /// The requested `skip` offset exceeds the configured hard maximum.
    SkipExceeded { requested: u64, max: u64 },
    /// The query uses a feature the chosen backend cannot express.
    FeatureUnsupportedByTarget { feature: String, target: String },
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::UnsupportedInQuery { op, at } => {
                write!(f, "operator '{op}' is not supported in a query (at {at})")
            }
            QueryError::NotRepresentable { what, at } => {
                write!(f, "{what} has no portable form in v1 (at {at})")
            }
            QueryError::InvalidEnvelope(msg) => write!(f, "invalid query envelope: {msg}"),
            QueryError::InvalidField { field, at } => {
                write!(f, "invalid field reference '{field}' (at {at})")
            }
            QueryError::MissingParam { name, at } => {
                write!(f, "query references undefined param '{name}' (at {at})")
            }
            QueryError::UnknownRelation { relation, at } => {
                write!(f, "unknown relation '{relation}' (at {at})")
            }
            QueryError::FeatureUnsupportedByTarget { feature, target } => {
                write!(f, "{feature} is not supported by the {target} backend")
            }
            QueryError::LimitExceeded { requested, max } => {
                write!(
                    f,
                    "requested limit {requested} exceeds the configured maximum {max}"
                )
            }
            QueryError::SkipExceeded { requested, max } => {
                write!(
                    f,
                    "requested skip {requested} exceeds the configured maximum {max}"
                )
            }
        }
    }
}

impl std::error::Error for QueryError {}

impl From<QueryError> for DataflowError {
    fn from(e: QueryError) -> Self {
        DataflowError::Validation(e.to_string())
    }
}

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
    /// (e.g. column-to-column comparison, an array/object column value).
    NotRepresentable { what: String, at: String },
    /// The envelope (`source`/`fields`/`sort`/`op`/`values`/…) is malformed.
    /// Shared by the read and write envelopes (W20).
    InvalidEnvelope(String),
    /// A field reference is invalid for identity mode (e.g. a dotted JSON path).
    InvalidField { field: String, at: String },
    /// A `{"param": name}` referenced a name absent from the params map.
    MissingParam { name: String, at: String },
    /// A `some`/`all`/`none` referenced a relation not declared in the schema.
    UnknownRelation { relation: String, at: String },
    /// The envelope named an entity the schema does not declare, under the
    /// `reject` unmapped policy — the 1.0 default (F24).
    UndeclaredEntity { entity: String },
    /// An entity declares columns but marks every one `queryable: false`, so a
    /// query that names no `fields` has nothing it may return (F24). Refused
    /// rather than widened back to `SELECT *`.
    NoQueryableColumns { entity: String },
    /// The entity resolves to a physical table/collection/index outside the
    /// connector's `allowed_entities` list (F24).
    EntityNotAllowed { entity: String, physical: String },
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
            QueryError::InvalidEnvelope(msg) => write!(f, "invalid envelope: {msg}"),
            QueryError::InvalidField { field, at } => {
                write!(f, "invalid field reference '{field}' (at {at})")
            }
            QueryError::MissingParam { name, at } => {
                write!(f, "query references undefined param '{name}' (at {at})")
            }
            QueryError::UnknownRelation { relation, at } => {
                write!(f, "unknown relation '{relation}' (at {at})")
            }
            // F24: the one error a 0.x workflow hits after the unmapped default
            // flipped to `reject`, so it spells out both ways forward rather
            // than just stating the refusal.
            QueryError::UndeclaredEntity { entity } => write!(
                f,
                "entity '{entity}' is not declared in the task's schema: add \
                 \"schema\": {{\"entities\": {{\"{entity}\": {{\"columns\": \
                 {{\"<column>\": {{}}}}}}}}}} naming the columns this task uses, \
                 or add \"unmapped\": \"identity\" to that schema to accept \
                 undeclared names as physical ones (pre-1.0 behaviour)"
            ),
            QueryError::NoQueryableColumns { entity } => write!(
                f,
                "entity '{entity}' declares columns but none of them are queryable, \
                 so a query naming no \"fields\" has nothing it may return: mark a \
                 column \"queryable\": true, or read it through a different entity"
            ),
            QueryError::EntityNotAllowed { entity, physical } => write!(
                f,
                "entity '{entity}' resolves to '{physical}', which the connector's \
                 allowed_entities list does not permit"
            ),
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

impl QueryError {
    /// Whether the message names connector topology and must therefore be
    /// redacted before it reaches the anonymous data plane (G3).
    ///
    /// This used to be a prefix written by the `Display` impl itself, which
    /// meant every `to_string()` — including the ones that only ever reach a
    /// log line — carried `orion.connector_detail: `. Classifying the variant
    /// at the boundary keeps `Display` a plain sentence and puts the decision
    /// where the redaction actually happens.
    pub fn is_connector_detail(&self) -> bool {
        // The physical name and the existence of an operator allowlist are
        // both connector topology. Every other variant is workflow-structural
        // and safe to return verbatim — that is what makes a misconfigured
        // task diagnosable from the response.
        matches!(self, QueryError::EntityNotAllowed { .. })
    }
}

impl From<QueryError> for DataflowError {
    fn from(e: QueryError) -> Self {
        if e.is_connector_detail() {
            return crate::errors::connector_detail_error(e);
        }
        DataflowError::Validation(e.to_string())
    }
}

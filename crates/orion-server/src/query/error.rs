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
    ///
    /// `did_you_mean` carries the nearest declared column when the name looks
    /// like a typo of one — see [`nearest`]. It is `None` wherever the refusal
    /// is not about a misspelling: a dotted path, an empty name, a `field`
    /// value that is not a string, or a column that exists and is simply not
    /// readable.
    InvalidField {
        field: String,
        at: String,
        did_you_mean: Option<String>,
    },
    /// A `{"param": name}` referenced a name absent from the params map.
    MissingParam { name: String, at: String },
    /// A `some`/`all`/`none` referenced a relation not declared in the schema.
    UnknownRelation { relation: String, at: String },
    /// The envelope named an entity the schema does not declare, under the
    /// `reject` unmapped policy — the 1.0 default (F24).
    UndeclaredEntity {
        entity: String,
        did_you_mean: Option<String>,
    },
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
            QueryError::InvalidField {
                field,
                at,
                did_you_mean,
            } => {
                write!(f, "invalid field reference '{field}' (at {at})")?;
                write_suggestion(f, did_you_mean)
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
            QueryError::UndeclaredEntity {
                entity,
                did_you_mean,
            } => {
                write!(
                    f,
                    "entity '{entity}' is not declared in the task's schema: add \
                     \"schema\": {{\"entities\": {{\"{entity}\": {{\"columns\": \
                     {{\"<column>\": {{}}}}}}}}}} naming the columns this task uses, \
                     or add \"unmapped\": \"identity\" to that schema to accept \
                     undeclared names as physical ones (pre-1.0 behaviour)"
                )?;
                write_suggestion(f, did_you_mean)
            }
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

/// Append `— did you mean "x"?` when there is a candidate.
fn write_suggestion(
    f: &mut std::fmt::Formatter<'_>,
    did_you_mean: &Option<String>,
) -> std::fmt::Result {
    match did_you_mean {
        Some(name) => write!(f, " — did you mean \"{name}\"?"),
        None => Ok(()),
    }
}

/// How far a name may be from a candidate before a typo stops being the likely
/// explanation.
///
/// Scaled to the candidate, which the config suggester does not have to do:
/// its candidates are long (`ORION_SERVER__PORT`), while an envelope key or a
/// column can be three characters, and a fixed budget of 3 would offer "age"
/// for "id". One edit for a short name, two from six characters, three from
/// nine — never more than a third of the candidate.
fn max_distance(candidate: &str) -> usize {
    (candidate.chars().count() / 3).clamp(1, 3)
}

/// The nearest candidate to `name`, when one is close enough that a typo is the
/// likely explanation.
///
/// Ties break on the candidate's own order so the suggestion is stable rather
/// than dependent on a hash map's iteration. Case-insensitive, because a
/// mis-cased name is a typo the author cannot see.
pub(crate) fn nearest<'a, I>(name: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let needle: Vec<char> = name.to_lowercase().chars().collect();
    candidates
        .into_iter()
        .filter(|c| *c != name)
        .map(|candidate| {
            let lowered: Vec<char> = candidate.to_lowercase().chars().collect();
            (
                crate::text::edit_distance_chars(&needle, &lowered),
                candidate,
            )
        })
        .filter(|(distance, candidate)| *distance <= max_distance(candidate))
        .min_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, candidate)| candidate.to_string())
}

#[cfg(test)]
mod suggestion_tests {
    use super::*;

    #[test]
    fn a_near_miss_is_offered() {
        assert_eq!(
            nearest("fileds", ["source", "filter", "fields", "sort"]),
            Some("fields".to_string())
        );
        assert_eq!(
            nearest("Name", ["id", "name", "email"]),
            Some("name".to_string())
        );
    }

    /// The budget scales with the candidate, so a short name does not collect
    /// an unrelated neighbour.
    #[test]
    fn an_unrelated_name_is_not_offered() {
        assert_eq!(nearest("id", ["age", "name", "email"]), None);
        assert_eq!(nearest("customer_reference", ["id", "name"]), None);
        assert_eq!(nearest("", ["id"]), None);
    }

    /// The name itself is never the suggestion — offering a name back to the
    /// author who just typed it says nothing. A *different* near neighbour
    /// still is, which is the whole point.
    #[test]
    fn the_name_itself_is_never_offered() {
        assert_eq!(nearest("secret", ["secret"]), None);
        assert_eq!(
            nearest("secret", ["secret", "secrets"]),
            Some("secrets".to_string())
        );
    }
}

//! The rules, one file per group. `ALL` is the registry, in the order rules
//! run and are listed; a rule not in it does not exist.

pub mod correctness;
pub mod duplication;
pub mod perf;
pub mod style;

use super::Rule;

pub static ALL: &[&dyn Rule] = &[
    &correctness::WorkflowNeverMatches,
    &correctness::TaskNeverRuns,
    &correctness::UnreachableStep,
    &correctness::UnconditionalCallCycle,
    &correctness::PayloadVar,
    &correctness::MappingOverwritten,
    &correctness::MetadataVarUndeclared,
    &correctness::SecretUndeclared,
    &correctness::ResponseCookieType,
    &perf::ParseResultOverwritten,
    &perf::RedundantStepCondition,
    &perf::GroupConditionRepeated,
    &duplication::FragmentAvailable,
    &duplication::RepeatedTaskSequence,
    &duplication::RepeatedValue,
    &style::TerminalOnLastStep,
];

/// Walk a JSON value with the path of every node, in the `a.b[2].c`
/// notation every finding uses.
pub(super) fn walk_values<'a>(
    value: &'a serde_json::Value,
    path: &str,
    f: &mut impl FnMut(&str, &'a serde_json::Value),
) {
    f(path, value);
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let at = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                walk_values(v, &at, f);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                walk_values(v, &format!("{path}[{i}]"), f);
            }
        }
        _ => {}
    }
}

/// `a`, `b` and `c` → "`a`, `b` and `c`".
pub(super) fn list_ids(ids: &[&str]) -> String {
    match ids {
        [] => String::new(),
        [one] => format!("`{one}`"),
        [head @ .., last] => format!(
            "{} and `{last}`",
            head.iter()
                .map(|i| format!("`{i}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

//! Structural identity of steps and values: canonical JSON with the parts
//! that name a thing removed, so two steps that *do* the same thing hash
//! the same however they are labelled.

use serde_json::Value;

/// A step with every `id` and `name` removed, recursively through a group's
/// members, rendered as canonical JSON. Two steps with equal keys are
/// interchangeable to the engine up to their ids.
pub fn step_key(step: &Value) -> String {
    let mut stripped = step.clone();
    strip_labels(&mut stripped);
    crate::storage::content::canonical_json(&stripped)
}

/// A step with only `id` removed, for matching against a fragment whose
/// task names are part of what it says.
pub fn step_key_keeping_names(step: &Value) -> String {
    let mut stripped = step.clone();
    strip_ids(&mut stripped);
    crate::storage::content::canonical_json(&stripped)
}

fn strip_labels(step: &mut Value) {
    if let Some(obj) = step.as_object_mut() {
        obj.remove("id");
        obj.remove("name");
        if let Some(members) = obj.get_mut("tasks").and_then(Value::as_array_mut) {
            members.iter_mut().for_each(strip_labels);
        }
    }
}

fn strip_ids(step: &mut Value) {
    if let Some(obj) = step.as_object_mut() {
        obj.remove("id");
        if let Some(members) = obj.get_mut("tasks").and_then(Value::as_array_mut) {
            members.iter_mut().for_each(strip_ids);
        }
    }
}

/// Canonical JSON of any value, for identity of repeated literals.
pub fn value_key(value: &Value) -> String {
    crate::storage::content::canonical_json(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn labels_do_not_change_a_step_key() {
        let a =
            json!({"id": "a", "name": "A", "function": {"name": "log", "input": {"message": "x"}}});
        let b =
            json!({"name": "B", "id": "b", "function": {"name": "log", "input": {"message": "x"}}});
        assert_eq!(step_key(&a), step_key(&b));
        let c =
            json!({"id": "c", "name": "C", "function": {"name": "log", "input": {"message": "y"}}});
        assert_ne!(step_key(&a), step_key(&c));
    }

    #[test]
    fn group_members_are_stripped_too() {
        let g1 = json!({"id": "g", "tasks": [{"id": "x", "name": "X", "function": {"name": "log", "input": {}}}]});
        let g2 = json!({"id": "h", "tasks": [{"id": "y", "name": "Y", "function": {"name": "log", "input": {}}}]});
        assert_eq!(step_key(&g1), step_key(&g2));
        assert_ne!(step_key_keeping_names(&g1), step_key_keeping_names(&g2));
    }
}

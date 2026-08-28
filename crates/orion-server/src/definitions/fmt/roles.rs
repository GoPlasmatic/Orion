//! What each node *is*, so the printer knows how it wants to be laid out.
//!
//! Classification is by shape, top-down: a document is a workflow because it
//! has `tasks`, a step is a group because it has `tasks`, a node is an
//! operator because it is a single-key object whose key the engine evaluates.
//! Nothing here consults a file name or a field-schema registry, and that is
//! what lets `--stdin` format a bare `tasks` array the way it formats one
//! inside a workflow.
//!
//! The one decision that is purely structural and worth stating: **an
//! operator node is recognised wherever it appears** — in a `condition`, a
//! mapping's `logic`, a connector payload, a case file's `expect`. Layout does
//! not depend on whether the engine will evaluate the node, only on what it
//! looks like, and for a single-key object the operator layout and the
//! generic layout never disagree except that a compound operator always
//! breaks — which is the readable choice for a data object of that shape too.

use super::style;
use crate::definitions::json::{Member, Node};

/// The role of a node, which decides its key order and its layout mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    // ---- documents and their parts: canonical key order, always broken ----
    Workflow,
    Channel,
    Connector,
    SharedDoc,
    /// A `constants`/`errors`/… namespace: one named value per line.
    NamedValues,
    /// The `fragments` map: one fragment per line.
    FragmentMap,
    Fragment,
    CaseFile,
    Artifact,
    ArtifactMeta,
    TaskList,
    Task,
    Group,
    UseStep,
    // ---- inline when they fit ----
    FunctionHeader,
    Input(InputKind),
    /// The `mappings` / `rules` array: one entry per line once there is more
    /// than one.
    EntryList(EntryKind),
    Mapping,
    ValidationRule,
    LoopObject,
    Operator(OperatorShape),
    /// The argument array of an operator node.
    OperatorArgs(OperatorShape),
    /// An array of scalars.
    ScalarArray,
    /// An object keyed entirely by dotted paths — a case file's `expect`, a
    /// use-case's assertions. A checklist reads one entry per line.
    PathMap,
    /// Anything unrecognised: author order, fits-or-break.
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Map,
    Validation,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Mapping,
    ValidationRule,
}

/// How an operator node is shaped, which is the whole of the JSONLogic
/// layout rule: unary always inlines, leaf inlines when it fits, compound
/// always breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorShape {
    /// The argument is a scalar, a read node, or a one-element array of
    /// either: `{ "var": "x" }`, `{ "!": { "var": "x" } }`,
    /// `{ "length": [{ "var": "x" }] }`.
    Unary,
    /// Every argument is a scalar or a unary node:
    /// `{ ">=": [{ "var": "x" }, 500] }`.
    Leaf,
    /// Anything deeper.
    Compound,
}

/// Depth at or below which an object under a generic parent is still
/// classified as an entity by shape. Covers a bulk-import file (a root array
/// of entities, depth 1) and a promotion artifact (`workflows[]`, depth 2)
/// without reaching a task payload that happens to carry a `tasks` key.
const ENTITY_SHAPE_DEPTH: usize = 2;

/// Operators whose argument names a path rather than computing one. They are
/// unary whatever the argument's *shape* — `{ "var": ["data.x", 0] }` is a
/// read with a default, not a two-argument expression — as long as the
/// argument is scalar or an array of scalars. This is a layout list, not an
/// arity claim.
const READ_OPERATORS: &[&str] = &["var", "val", "secret", "missing", "missing_some"];

/// The role of a document root.
///
/// A bare array of steps is a task list: an editor formatting a selected
/// `tasks` array through `--stdin` gets the layout it would have inside its
/// workflow, rather than a generic array whose first group is mistaken for
/// a workflow of its own.
pub fn root_role(node: &Node) -> Role {
    if let Some(shape) = operator_shape(node) {
        return Role::Operator(shape);
    }
    if let Some(items) = node.as_array()
        && !items.is_empty()
        && items.iter().all(|i| is_step_like(&i.node))
    {
        return Role::TaskList;
    }
    entity_role(node).unwrap_or(Role::Generic)
}

/// An object with an `id` and one of the three step discriminators.
fn is_step_like(node: &Node) -> bool {
    let Some(members) = node.as_object() else {
        return false;
    };
    let has = |k: &str| members.iter().any(|m| m.key.node == k);
    has("id") && (has("function") || has("tasks") || has("use"))
}

/// The role of `child`, found under `parent` at `key` (an object member) or
/// at no key (an array element). `depth` is the child's container depth from
/// the root, for the entity-shape cutoff.
pub fn child_role(parent: Role, key: Option<&str>, child: &Node, depth: usize) -> Role {
    if let Some(shape) = operator_shape(child) {
        return Role::Operator(shape);
    }
    match parent {
        Role::Workflow => match key {
            Some("tasks") => Role::TaskList,
            Some("loop") if child.as_object().is_some() => Role::LoopObject,
            _ => generic_or_scalar_array(child),
        },
        Role::Fragment | Role::Group => match key {
            Some("tasks") => Role::TaskList,
            _ => generic_or_scalar_array(child),
        },
        Role::TaskList => step_role(child),
        Role::Task => match key {
            Some("function") if child.as_object().is_some() => Role::FunctionHeader,
            _ => generic_or_scalar_array(child),
        },
        Role::FunctionHeader => match key {
            Some("input") if child.as_object().is_some() => {
                // The kind is decided by the sibling `name`, which the
                // header's own layout looks up and passes down through
                // `input_role`; here we only know it is an input.
                Role::Input(InputKind::Other)
            }
            _ => generic_or_scalar_array(child),
        },
        Role::Input(InputKind::Map) => match key {
            Some("mappings") if child.as_array().is_some() => Role::EntryList(EntryKind::Mapping),
            _ => generic_or_scalar_array(child),
        },
        Role::Input(InputKind::Validation) => match key {
            Some("rules") if child.as_array().is_some() => {
                Role::EntryList(EntryKind::ValidationRule)
            }
            _ => generic_or_scalar_array(child),
        },
        Role::EntryList(EntryKind::Mapping) if child.as_object().is_some() => Role::Mapping,
        Role::EntryList(EntryKind::ValidationRule) if child.as_object().is_some() => {
            Role::ValidationRule
        }
        Role::SharedDoc => match key {
            Some("fragments") if child.as_object().is_some() => Role::FragmentMap,
            Some(_) if child.as_object().is_some() => Role::NamedValues,
            _ => generic_or_scalar_array(child),
        },
        Role::FragmentMap if child.as_object().is_some() => Role::Fragment,
        Role::Artifact => match key {
            Some("package") if child.as_object().is_some() => Role::ArtifactMeta,
            Some("connectors" | "workflows" | "channels") if child.as_array().is_some() => {
                Role::Generic
            }
            _ => generic_or_scalar_array(child),
        },
        Role::Generic if depth <= ENTITY_SHAPE_DEPTH => {
            entity_role(child).unwrap_or_else(|| generic_or_scalar_array(child))
        }
        _ => generic_or_scalar_array(child),
    }
}

/// The role of a `function.input` object once its function name is known.
pub fn input_role(function_name: Option<&str>) -> Role {
    Role::Input(match function_name {
        Some("map") => InputKind::Map,
        Some("validation" | "validate") => InputKind::Validation,
        _ => InputKind::Other,
    })
}

/// The canonical key order of a `function.input`: the registry's field
/// table for an Orion function, the documented order for an engine built-in,
/// author order for a name neither knows.
pub fn input_key_order(function_name: Option<&str>) -> Option<Vec<&'static str>> {
    let name = function_name?;
    if let Some(schema) = crate::engine::functions::schema::registry()
        .iter()
        .find(|s| s.name == name)
    {
        return Some(schema.input_fields.iter().map(|f| f.name).collect());
    }
    style::BUILTIN_INPUT_KEYS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, keys)| keys.to_vec())
}

/// The canonical key order for a role, or `None` for author order.
pub fn key_order(role: Role) -> Option<&'static [&'static str]> {
    Some(match role {
        Role::Workflow => style::WORKFLOW_KEYS,
        Role::Task => style::TASK_KEYS,
        Role::Group => style::GROUP_KEYS,
        Role::UseStep => style::USE_STEP_KEYS,
        Role::FunctionHeader => style::FUNCTION_KEYS,
        Role::Mapping => style::MAPPING_KEYS,
        Role::ValidationRule => style::VALIDATION_RULE_KEYS,
        Role::LoopObject => style::LOOP_KEYS,
        Role::Channel => style::CHANNEL_KEYS,
        Role::Connector => style::CONNECTOR_KEYS,
        Role::SharedDoc => style::SHARED_DOC_KEYS,
        Role::Fragment => style::FRAGMENT_KEYS,
        Role::CaseFile => style::CASE_KEYS,
        Role::Artifact => style::ARTIFACT_KEYS,
        Role::ArtifactMeta => style::ARTIFACT_META_KEYS,
        _ => return None,
    })
}

/// An entity, a shared document, a case file or an artifact, told apart by
/// the same keys `Entity::classify` and `is_shared_document` use.
fn entity_role(node: &Node) -> Option<Role> {
    let members = node.as_object()?;
    let has = |k: &str| members.iter().any(|m| m.key.node == k);
    if has("tasks") {
        return Some(Role::Workflow);
    }
    if has("connector_type") {
        return Some(Role::Connector);
    }
    if has("channel_type") || has("protocol") {
        return Some(Role::Channel);
    }
    if has("constants") || has("errors") || has("fragments") {
        return Some(Role::SharedDoc);
    }
    if has("workflow") && has("input") && has("expect") {
        return Some(Role::CaseFile);
    }
    if has("package") && has("workflows") {
        return Some(Role::Artifact);
    }
    None
}

/// The three-way split every walk over authored steps makes: a group carries
/// `tasks` (the engine's own `is_group` test — presence of the key, whatever
/// its type), a fragment call site carries `use`, everything else is a task.
fn step_role(step: &Node) -> Role {
    let Some(members) = step.as_object() else {
        return generic_or_scalar_array(step);
    };
    let has = |k: &str| members.iter().any(|m| m.key.node == k);
    if has("tasks") {
        Role::Group
    } else if has("use") {
        Role::UseStep
    } else {
        Role::Task
    }
}

fn generic_or_scalar_array(node: &Node) -> Role {
    match node {
        Node::Array(items) if items.iter().all(|i| i.node.is_scalar()) => Role::ScalarArray,
        Node::Object(members)
            if members.len() > 1 && members.iter().all(|m| m.key.node.contains('.')) =>
        {
            Role::PathMap
        }
        _ => Role::Generic,
    }
}

/// `Some(shape)` when `node` is an operator node: a single-member object
/// whose key the engine evaluates.
pub fn operator_shape(node: &Node) -> Option<OperatorShape> {
    let [member] = node.as_object()? else {
        return None;
    };
    let op = member.key.node.as_str();
    if !crate::engine::operators::is_operator(op) {
        return None;
    }
    let arg = &member.value.node;
    if READ_OPERATORS.contains(&op) && is_scalar_or_scalar_array(arg) {
        return Some(OperatorShape::Unary);
    }
    Some(match arg {
        Node::Array(items) => match items.as_slice() {
            [single] if is_leafish(&single.node) => OperatorShape::Unary,
            _ if items.iter().all(|i| is_leafish(&i.node)) => OperatorShape::Leaf,
            _ => OperatorShape::Compound,
        },
        // A single object argument: `{ "!": { "var": "x" } }` is a read and
        // stays unary; `{ "!": { "in": […] } }` wraps a leaf and is one —
        // it inlines when it fits rather than unconditionally; anything
        // deeper is compound.
        Node::Object(_) if is_read_node(arg) => OperatorShape::Unary,
        Node::Object(_) => match operator_shape(arg) {
            Some(OperatorShape::Unary | OperatorShape::Leaf) => OperatorShape::Leaf,
            Some(OperatorShape::Compound) => OperatorShape::Compound,
            None if is_atom(arg) => OperatorShape::Unary,
            None => OperatorShape::Compound,
        },
        _ => OperatorShape::Unary,
    })
}

/// What an expression is built from: a literal, a short array of literals,
/// or a single-member object holding one — `{ "field": "id" }`,
/// `{ "param": "customer_id" }` in the query dialect, `{ "var": "x" }`. An
/// argument list of atoms is a leaf whatever the operator.
fn is_atom(node: &Node) -> bool {
    match node {
        Node::Object(members) => {
            matches!(members.as_slice(), [m] if is_scalar_or_scalar_array(&m.value.node))
        }
        other => is_scalar_or_scalar_array(other),
    }
}

/// A scalar, or an array of scalars short enough to inline.
fn is_scalar_or_scalar_array(node: &Node) -> bool {
    match node {
        Node::Array(items) => {
            items.len() <= style::STYLE.max_scalar_inline
                && items.iter().all(|i| i.node.is_scalar())
        }
        other => other.is_scalar(),
    }
}

/// An argument that keeps its operator a leaf: an atom, or a unary node.
fn is_leafish(node: &Node) -> bool {
    is_atom(node) || operator_shape(node) == Some(OperatorShape::Unary)
}

/// A `{ "var": … }`-family node: single member, read operator.
fn is_read_node(node: &Node) -> bool {
    matches!(node.as_object(), Some([m]) if READ_OPERATORS.contains(&m.key.node.as_str()))
}

/// Reorder `members` so `$from` comes first, then `order`'s keys in table
/// order, then everything else in author order. Author order is the only
/// thing that changes; nothing is added or dropped.
pub fn order_members<'a>(members: &'a [Member], order: Option<&[&str]>) -> Vec<&'a Member> {
    let mut out: Vec<&Member> = Vec::with_capacity(members.len());
    let mut taken = vec![false; members.len()];
    let mut take = |pred: &dyn Fn(&str) -> bool, out: &mut Vec<&'a Member>| {
        for (i, m) in members.iter().enumerate() {
            if !taken[i] && pred(&m.key.node) {
                taken[i] = true;
                out.push(m);
            }
        }
    };
    take(&|k| k == style::FROM_KEY, &mut out);
    if let Some(order) = order {
        for key in order {
            take(&|k| k == *key, &mut out);
        }
    }
    take(&|_| true, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definitions::json::Document;

    fn node(text: &str) -> Node {
        Document::parse(text)
            .expect("test input is valid")
            .root
            .node
    }

    #[test]
    fn operator_shapes() {
        let shape = |t: &str| operator_shape(&node(t));
        assert_eq!(shape(r#"{"var": "data.x"}"#), Some(OperatorShape::Unary));
        assert_eq!(
            shape(r#"{"var": ["data.x", 0]}"#),
            Some(OperatorShape::Unary)
        );
        assert_eq!(shape(r#"{"secret": "hmac"}"#), Some(OperatorShape::Unary));
        assert_eq!(
            shape(r#"{"!": {"var": "data.ok"}}"#),
            Some(OperatorShape::Unary)
        );
        assert_eq!(
            shape(r#"{"length": [{"var": "data.items"}]}"#),
            Some(OperatorShape::Unary)
        );
        assert_eq!(shape(r#"{"now": []}"#), Some(OperatorShape::Leaf));
        assert_eq!(
            shape(r#"{">=": [{"var": "data.amount"}, 500]}"#),
            Some(OperatorShape::Leaf)
        );
        assert_eq!(
            shape(r#"{"!": {"!!": {"var": "x"}}}"#),
            Some(OperatorShape::Leaf)
        );
        assert_eq!(
            shape(r#"{"and": [{">=": [{"var": "a"}, 1]}, true]}"#),
            Some(OperatorShape::Compound)
        );
        assert_eq!(
            shape(r#"{"var": {"cat": ["a", "b"]}}"#),
            Some(OperatorShape::Leaf),
            "a read whose path is computed is laid out by its argument"
        );
        assert_eq!(
            shape(r#"{"in": [{"var": "data.tier"}, ["vip", "premium"]]}"#),
            Some(OperatorShape::Leaf),
            "a short array literal is an atom"
        );
        assert_eq!(
            shape(r#"{"==": [{"field": "id"}, {"param": "customer_id"}]}"#),
            Some(OperatorShape::Leaf),
            "query-dialect atoms are atoms"
        );
        assert_eq!(
            shape(r#"{"!": {"in": [{"var": "x"}, [1, 7, 42]]}}"#),
            Some(OperatorShape::Leaf),
            "a unary wrapper around a leaf is a leaf"
        );
        assert_eq!(
            shape(r#"{"!": {"and": [{"var": "a"}, {"in": [{"var": "b"}, [1]]}]}}"#),
            Some(OperatorShape::Compound),
            "a unary wrapper around a compound is compound"
        );
        assert_eq!(
            shape(r#"{"in": [{"var": "x"}, [1, 2, 3, 4, 5, 6, 7, 8, 9]]}"#),
            Some(OperatorShape::Compound),
            "an array literal past the scalar cap is not an atom"
        );
        assert_eq!(shape(r#"{"path": "x"}"#), None, "not an operator");
        assert_eq!(shape(r#"{"var": "x", "extra": 1}"#), None, "two keys");
        assert_eq!(shape(r#"{}"#), None);
    }

    #[test]
    fn documents_are_classified_by_shape() {
        assert_eq!(
            root_role(&node(r#"{"name": "w", "tasks": []}"#)),
            Role::Workflow
        );
        assert_eq!(
            root_role(&node(r#"{"name": "c", "connector_type": "http"}"#)),
            Role::Connector
        );
        assert_eq!(
            root_role(&node(r#"{"name": "c", "protocol": "http"}"#)),
            Role::Channel
        );
        assert_eq!(root_role(&node(r#"{"constants": {}}"#)), Role::SharedDoc);
        assert_eq!(
            root_role(&node(
                r#"{"workflow": "w.json", "input": {}, "expect": {}}"#
            )),
            Role::CaseFile
        );
        assert_eq!(
            root_role(&node(r#"{"package": {}, "workflows": []}"#)),
            Role::Artifact
        );
        assert_eq!(root_role(&node(r#"{"hello": 1}"#)), Role::Generic);
        assert_eq!(
            root_role(&node(
                r#"[{"id": "t", "function": {}}, {"id": "g", "tasks": []}]"#
            )),
            Role::TaskList,
            "a bare array of steps, as an editor sends one"
        );
        assert_eq!(root_role(&node(r#"[{"id": "t"}]"#)), Role::Generic);
        assert_eq!(root_role(&node("[]")), Role::Generic);
        assert_eq!(
            child_role(
                Role::Generic,
                Some("expect"),
                &node(r#"{"data.a": 1, "data.b": 2}"#),
                1
            ),
            Role::PathMap
        );
        assert_eq!(
            child_role(Role::Generic, Some("expect"), &node(r#"{"data.a": 1}"#), 1),
            Role::Generic,
            "one entry is not a checklist"
        );
        assert_eq!(
            root_role(&node(r#"{"var": "x"}"#)),
            Role::Operator(OperatorShape::Unary)
        );
    }

    #[test]
    fn steps_split_three_ways() {
        let list = Role::TaskList;
        assert_eq!(
            child_role(list, None, &node(r#"{"id": "g", "tasks": []}"#), 2),
            Role::Group
        );
        assert_eq!(
            child_role(list, None, &node(r#"{"id": "u", "use": "guard"}"#), 2),
            Role::UseStep
        );
        assert_eq!(
            child_role(list, None, &node(r#"{"id": "t"}"#), 2),
            Role::Task
        );
    }

    #[test]
    fn entities_inside_bulk_and_artifact_files_are_recognised_but_payloads_are_not() {
        let wf = node(r#"{"name": "w", "tasks": []}"#);
        assert_eq!(
            child_role(Role::Generic, None, &wf, 1),
            Role::Workflow,
            "bulk file"
        );
        assert_eq!(
            child_role(Role::Generic, None, &wf, 2),
            Role::Workflow,
            "artifact"
        );
        assert_eq!(
            child_role(Role::Generic, Some("body"), &wf, 6),
            Role::Generic,
            "a payload with a `tasks` key is data"
        );
    }

    #[test]
    fn members_reorder_with_from_first_and_unknown_keys_last_in_author_order() {
        let n = node(r#"{"zeta": 1, "name": "n", "$from": "c.x", "id": "i", "alpha": 2}"#);
        let ordered: Vec<&str> = order_members(
            n.as_object().expect("test input is valid"),
            Some(style::TASK_KEYS),
        )
        .iter()
        .map(|m| m.key.node.as_str())
        .collect();
        assert_eq!(ordered, ["$from", "id", "name", "zeta", "alpha"]);
    }
}

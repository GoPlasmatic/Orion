//! The printer: a role-annotated tree in, canonical text out.
//!
//! Two passes. **Measure** builds a `Doc` bottom-up, computing for every
//! container the width of its one-line rendering and whether its role even
//! permits one — once per node, so the whole thing is linear. **Emit** walks
//! top-down with the current column and prints a container flat when it may
//! and it fits, broken otherwise. The measure pass is what keeps a naive
//! "try flat, fall back" printer from going quadratic on wide trees: nothing
//! is rendered twice.

use super::roles::{self, EntryKind, OperatorShape, Role};
use super::style::STYLE;
use crate::definitions::json::{Member, Node, Spanned};

/// How a container may be laid out. Decided by role, refined by content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// One line, whatever the width — a unary operator node.
    AlwaysInline,
    /// One line when every descendant permits it and it fits.
    InlineIfFits,
    /// One member per line, always.
    AlwaysBreak,
}

/// A laid-out node.
enum Doc {
    Scalar(String),
    Container {
        open: char,
        close: char,
        /// `(rendered key, value)`; the key is `None` for array elements.
        items: Vec<(Option<String>, Doc)>,
        mode: Mode,
        /// Width of the flat rendering, valid whether or not it may be used.
        flat_len: usize,
        /// Whether a flat rendering is permitted at all — `mode` allows it
        /// and every descendant's does too.
        can_inline: bool,
    },
}

impl Doc {
    fn flat_len(&self) -> usize {
        match self {
            Doc::Scalar(s) => s.len(),
            Doc::Container { flat_len, .. } => *flat_len,
        }
    }

    fn can_inline(&self) -> bool {
        match self {
            Doc::Scalar(_) => true,
            Doc::Container { can_inline, .. } => *can_inline,
        }
    }
}

/// Format a root node.
pub fn print(root: &Node) -> String {
    let doc = layout(root, roles::root_role(root), 0);
    let mut out = String::new();
    emit(&doc, 0, 0, 0, &mut out);
    out.push('\n');
    out
}

// ============================================================
// Measure
// ============================================================

fn layout(node: &Node, role: Role, depth: usize) -> Doc {
    match node {
        Node::Array(items) => layout_array(items, role, depth),
        Node::Object(members) => layout_object(
            members,
            role,
            depth,
            roles::key_order(role).map(<[_]>::to_vec),
        ),
        scalar => Doc::Scalar(render_scalar(scalar)),
    }
}

fn layout_array(items: &[Spanned<Node>], role: Role, depth: usize) -> Doc {
    let children: Vec<(Option<String>, Doc)> = items
        .iter()
        .map(|item| {
            let child_role = roles::child_role(role, None, &item.node, depth + 1);
            (None, layout(&item.node, child_role, depth + 1))
        })
        .collect();
    let mode = match role {
        Role::TaskList => Mode::AlwaysBreak,
        Role::EntryList(EntryKind::Mapping | EntryKind::ValidationRule) if items.len() > 1 => {
            Mode::AlwaysBreak
        }
        Role::ScalarArray if items.len() > STYLE.max_scalar_inline => Mode::AlwaysBreak,
        Role::OperatorArgs(OperatorShape::Compound) => Mode::AlwaysBreak,
        Role::OperatorArgs(OperatorShape::Unary) => Mode::AlwaysInline,
        _ => Mode::InlineIfFits,
    };
    container('[', ']', children, mode)
}

/// `order` is the canonical key order to apply — the role's own table, or
/// for a function input the table its function name selects.
fn layout_object(
    members: &[Member],
    role: Role,
    depth: usize,
    order: Option<Vec<&'static str>>,
) -> Doc {
    // An operator node's single argument takes the operator's shape with it,
    // so a compound argument array breaks one argument per line and a unary
    // one never breaks.
    if let Role::Operator(shape) = role {
        let member = &members[0];
        let arg_role = match &member.value.node {
            Node::Array(_) => Role::OperatorArgs(shape),
            other => roles::child_role(role, Some(&member.key.node), other, depth + 1),
        };
        let child = layout(&member.value.node, arg_role, depth + 1);
        let mode = match shape {
            OperatorShape::Unary => Mode::AlwaysInline,
            OperatorShape::Leaf => Mode::InlineIfFits,
            OperatorShape::Compound => Mode::AlwaysBreak,
        };
        return container(
            '{',
            '}',
            vec![(Some(render_string(&member.key.node)), child)],
            mode,
        );
    }

    // A function header decides what its `input` is — and how its keys are
    // ordered — from its own `name`.
    let function_name = match role {
        Role::FunctionHeader => members
            .iter()
            .find(|m| m.key.node == "name")
            .and_then(|m| m.value.node.as_str()),
        _ => None,
    };

    let ordered = roles::order_members(members, order.as_deref());
    let children: Vec<(Option<String>, Doc)> = ordered
        .into_iter()
        .map(|m| {
            let key = m.key.node.as_str();
            let value = &m.value.node;
            let child = match (role, key, value) {
                (Role::FunctionHeader, "input", Node::Object(input)) => layout_object(
                    input,
                    roles::input_role(function_name),
                    depth + 1,
                    roles::input_key_order(function_name),
                ),
                _ => layout(
                    value,
                    roles::child_role(role, Some(key), value, depth + 1),
                    depth + 1,
                ),
            };
            (Some(render_string(key)), child)
        })
        .collect();

    let mode = match role {
        Role::Workflow
        | Role::Channel
        | Role::Connector
        | Role::SharedDoc
        | Role::NamedValues
        | Role::FragmentMap
        | Role::Fragment
        | Role::CaseFile
        | Role::Artifact
        | Role::ArtifactMeta
        | Role::Task
        | Role::Group
        | Role::UseStep
        | Role::PathMap => Mode::AlwaysBreak,
        _ => Mode::InlineIfFits,
    };
    container('{', '}', children, mode)
}

fn container(open: char, close: char, items: Vec<(Option<String>, Doc)>, mode: Mode) -> Doc {
    let flat_len = if items.is_empty() {
        2
    } else {
        let pad = if open == '{' { 2 } else { 0 };
        let separators = 2 * (items.len() - 1);
        let body: usize = items
            .iter()
            .map(|(key, value)| key.as_ref().map_or(0, |k| k.len() + 2) + value.flat_len())
            .sum();
        2 + pad + separators + body
    };
    let can_inline = match mode {
        Mode::AlwaysBreak => items.is_empty(),
        _ => items.iter().all(|(_, v)| v.can_inline()),
    };
    Doc::Container {
        open,
        close,
        items,
        mode,
        flat_len,
        can_inline,
    }
}

// ============================================================
// Emit
// ============================================================

/// Print `doc` at `indent` (the column its closing bracket returns to),
/// starting at `column`, with `suffix` columns reserved after it on the same
/// line — the trailing comma of a member that is not the last.
fn emit(doc: &Doc, indent: usize, column: usize, suffix: usize, out: &mut String) {
    match doc {
        Doc::Scalar(s) => out.push_str(s),
        Doc::Container {
            open,
            close,
            items,
            mode,
            flat_len,
            can_inline,
        } => {
            let fits = column + flat_len + suffix <= STYLE.width;
            if items.is_empty() || (*can_inline && (*mode == Mode::AlwaysInline || fits)) {
                emit_flat(doc, out);
                return;
            }
            out.push(*open);
            let inner = indent + STYLE.indent;
            let last = items.len() - 1;
            for (i, (key, value)) in items.iter().enumerate() {
                out.push('\n');
                push_spaces(out, inner);
                let mut col = inner;
                if let Some(key) = key {
                    out.push_str(key);
                    out.push_str(": ");
                    col += key.len() + 2;
                }
                let comma = usize::from(i != last);
                emit(value, inner, col, comma, out);
                if comma == 1 {
                    out.push(',');
                }
            }
            out.push('\n');
            push_spaces(out, indent);
            out.push(*close);
        }
    }
}

fn emit_flat(doc: &Doc, out: &mut String) {
    match doc {
        Doc::Scalar(s) => out.push_str(s),
        Doc::Container {
            open, close, items, ..
        } => {
            out.push(*open);
            if !items.is_empty() {
                let pad = *open == '{';
                if pad {
                    out.push(' ');
                }
                for (i, (key, value)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    if let Some(key) = key {
                        out.push_str(key);
                        out.push_str(": ");
                    }
                    emit_flat(value, out);
                }
                if pad {
                    out.push(' ');
                }
            }
            out.push(*close);
        }
    }
}

fn push_spaces(out: &mut String, n: usize) {
    out.extend(std::iter::repeat_n(' ', n));
}

// ============================================================
// Scalars
// ============================================================

fn render_scalar(node: &Node) -> String {
    match node {
        Node::Null => "null".to_string(),
        Node::Bool(true) => "true".to_string(),
        Node::Bool(false) => "false".to_string(),
        Node::Number(lexeme) => lexeme.to_string(),
        Node::String(s) => render_string(s),
        Node::Array(_) | Node::Object(_) => unreachable!("containers are laid out, not rendered"),
    }
}

/// Canonical JSON string escaping: `"`, `\` and the C0 controls, nothing
/// else — every other character, ASCII or not, is emitted raw. This is
/// `serde_json`'s own policy, so the round-trip guard's two sides agree on
/// every string.
pub fn render_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_escape_like_serde_json() {
        for s in [
            "plain",
            "quote\"back\\slash/",
            "tab\tnewline\ncr\r",
            "\u{1}\u{8}\u{c}\u{1f}",
            "é😀 raw",
            "\u{7f}",
        ] {
            assert_eq!(
                render_string(s),
                serde_json::to_string(s).expect("test input is valid"),
                "{s:?}"
            );
        }
    }
}

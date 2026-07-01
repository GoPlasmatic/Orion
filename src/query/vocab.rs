//! The allowed operator vocabulary, and the mapping from JSONLogic operator keys
//! to their dialect meaning. Anything not classified here is rejected by
//! [`crate::query::lower`] with a located `UnsupportedInQuery` error.
//!
//! This deliberately mirrors the operator set datalogic-rs evaluates (proposal
//! §3.5), so a filter reads the same whether it is translated here or evaluated
//! elsewhere in Orion. `some`/`all`/`none` (relations) and `include` are Phase 2+.

use crate::query::ir::CmpOp;

/// What a recognised operator key means to the lowerer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    And,
    Or,
    Not,
    /// A scalar comparison (`==` `!=` `<` `<=` `>` `>=`, with `===`/`!==` aliased).
    Cmp(CmpOp),
    /// Membership (list haystack) or substring (string/field haystack).
    In,
    StartsWith,
    EndsWith,
    /// `missing` — one or more fields have no meaningful value.
    Missing,
    /// Relation quantifiers over a declared relation (Phase 2+).
    Some,
    All,
    None,
}

/// Classify a JSONLogic operator key, or `None` if it is outside the vocabulary.
pub fn classify(op: &str) -> Option<OpKind> {
    Some(match op {
        "and" => OpKind::And,
        "or" => OpKind::Or,
        "!" => OpKind::Not,
        "==" | "===" => OpKind::Cmp(CmpOp::Eq),
        "!=" | "!==" => OpKind::Cmp(CmpOp::Ne),
        "<" => OpKind::Cmp(CmpOp::Lt),
        "<=" => OpKind::Cmp(CmpOp::Le),
        ">" => OpKind::Cmp(CmpOp::Gt),
        ">=" => OpKind::Cmp(CmpOp::Ge),
        "in" => OpKind::In,
        "starts_with" => OpKind::StartsWith,
        "ends_with" => OpKind::EndsWith,
        "missing" => OpKind::Missing,
        "some" => OpKind::Some,
        "all" => OpKind::All,
        "none" => OpKind::None,
        _ => return None,
    })
}

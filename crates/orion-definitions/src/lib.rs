//! Static definition primitives: the order-preserving JSON AST ([`json`]),
//! source provenance ([`provenance`]), and the cross-dialect SQL lexer ([`sql_lex`]).

#![warn(clippy::unwrap_used, clippy::panic)]

pub mod json;
pub mod provenance;
pub mod sql_lex;

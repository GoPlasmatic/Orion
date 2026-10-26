//! Static definition primitives: the order-preserving JSON AST ([`json`]),
//! source provenance ([`provenance`]), the cross-dialect SQL lexer ([`sql_lex`]),
//! and the house formatter ([`fmt`]) over an injected language vocabulary.

pub mod fmt;
pub mod json;
pub mod provenance;
pub mod sql_lex;

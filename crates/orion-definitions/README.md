# orion-definitions

Static definition primitives for [Orion](https://github.com/GoPlasmatic/Orion):

- `json`: order-preserving, span-carrying RFC 8259 JSON parser and AST (`Document`, `Node`, `Span`)
- `provenance`: source-map provenance tracking (`SourceMap`, `Provenance`) across `$from`, `use`, and `$sql` expansions
- `sql_lex`: cross-dialect SQL lexer, statement normalizer, placeholder extractor, and read-only statement verifier

## Licence

Apache-2.0. See [LICENSE](https://github.com/GoPlasmatic/Orion/blob/main/LICENSE).

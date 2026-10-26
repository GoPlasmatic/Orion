# orion-definitions

Static definition primitives for [Orion](https://github.com/GoPlasmatic/Orion):

- `json`: order-preserving, span-carrying RFC 8259 JSON parser and AST (`Document`, `Node`, `Span`)
- `provenance`: source-map provenance tracking (`SourceMap`, `Provenance`) across `$from`, `use`, and `$sql` expansions
- `sql_lex`: cross-dialect SQL lexer, statement normalizer, placeholder extractor, and read-only statement verifier
- `fmt`: the house formatter, with round-trip and idempotence guards. It takes operator names and function-field order as data, so editors can format definitions without linking the engine or runtime.

```rust
use orion_definitions::fmt::Formatter;

let formatter = Formatter::new(
    ["var", "and", ">="].map(str::to_string),
    [("fetch".to_string(), vec!["url".to_string(), "output".to_string()])],
);
let result = formatter
    .format_str(r#"{"condition":{"var":"data.ready"}}"#, "workflow.json")
    .expect("valid JSON");
```

The vocabulary describes the language being edited; it does not configure the
style. Unknown input keys and unknown functions keep their authored order.
Orion's authoring crate supplies the vocabulary from its operator and function
catalogues, and the CLI uses that adapter.

## Licence

Apache-2.0. See [LICENSE](https://github.com/GoPlasmatic/Orion/blob/main/LICENSE).

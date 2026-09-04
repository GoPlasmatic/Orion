# Expressions

Read this reference when authoring JSONLogic conditions, mappings, channel
guards, or expression-capable function inputs.

## Evaluation model

A scalar is a literal. An object with one recognized operator key is an
expression. Arrays evaluate their elements. Multi-key objects are templates on
template-capable surfaces, with expression values evaluated recursively.

```json
{ ">": [{ "var": "data.order.total" }, 10000] }
```

Use `{"var":"data.x"}` for message data, `{"var":"metadata.x"}` for ingress
metadata, and `{"var":"temp_data.x"}` for scratch state.

Two failure modes matter:

- Strict surfaces such as workflow/task conditions reject unknown operators or
  malformed expressions.
- Template surfaces such as `map.logic` may interpret an unknown one-key
  object as ordinary data. A typo can therefore be written through literally
  and still return success.

Run `lint` and `clippy`, and inspect any object produced where a scalar was
expected.

## Operator discovery and families

The exact vocabulary comes from the engine linked into the installed Orion
release. The main families are:

- core comparison, boolean, arithmetic, collection, `var`, `missing`, and
  `missing_some`;
- date/time;
- extended string, array, math, object, and control/error handling;
- Orion encoding helpers (base64, base64url, hex), URL encoding, joining, and
  randomness;
- `secret` for declared secrets.

Consult the matching Orion expression reference for exact names and arity:
<https://docs.goplasmatic.io/reference/expressions.html>. Do not substitute
JavaScript or jq syntax.

Use explicit coercion sparingly. JSONLogic truthiness and equality are not
Rust, SQL, or JavaScript semantics. Test null, missing, empty array/object,
numeric strings, and zero whenever they are plausible inputs.

## Templates versus operator objects

A multi-key object is normally a template:

```json
{
  "Authorization": { "cat": ["Bearer ", { "secret": "partner_token" }] },
  "X-Order-ID": { "var": "data.order.id" }
}
```

A single-key object whose key is a registered operator is evaluated. To emit
literal data that resembles an operator expression, use the documented literal
mechanism for the target field or restructure the template. Never assume an
operator-looking object will survive unchanged.

Channel admission guards require a predicate. Orion explicitly rejects
ambiguous multi-key guard objects that could otherwise be truthy templates.

## Secrets and variables

Operators declare non-secret values in `[vars]` and credentials in
`[secrets]`:

```toml
[vars]
topic_prefix = "dev"

[secrets]
partner_token = "env://PARTNER_TOKEN"
```

Read them as:

```json
{ "var": "metadata.vars.topic_prefix" }
```

```json
{ "secret": "partner_token" }
```

Variables are stamped into message metadata and therefore appear in traces.
Secrets live in the engine store and are not message fields. Orion rejects a
secret read whose result would be recorded, including map output and structured
log fields. Secret reads are valid in transient expressions such as request
headers, cryptographic inputs, conditions, and channel guards when the schema
permits them.

Do not confuse these with `var://name`. That syntax substitutes a declared
non-secret variable into stored connector/channel configuration at load time;
it is not JSONLogic.

## Connector documents

Modern connector function parameters are expression-capable according to their
runtime schemas. This includes nested HTTP headers and Kafka topics. Literals
still fold to themselves.

MongoDB query and pipeline documents often contain one-key objects such as
`{"$oid":"..."}` or database operators. These are data documents, not
automatically JSONLogic. Follow the function schema's document/expression
boundary; do not rewrite native database syntax into JSONLogic.

## Defensive authoring

- Prefer direct paths over elaborate coercion chains.
- Guard optional values with `missing`, `missing_some`, or an explicit
  default before arithmetic or string operations.
- Test expressions using representative bad inputs, not only the happy path.
- Keep rate-limit/cache keys stable and scalar.
- Do not log secrets or authentication headers.
- For randomness, time, and external state, assert invariants rather than exact
  values in tests.

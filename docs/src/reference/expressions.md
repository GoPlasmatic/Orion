<!-- description: The complete JSONLogic operator vocabulary Orion compiles — core, dates, strings, arrays, encoding and randomness — plus the operators that fail silently. -->
# Expression Language (JSONLogic)

[JSONLogic](https://jsonlogic.com) is Orion's expression language. Expressions
decide whether a workflow matches, whether a task runs, and what a `map`
mapping writes. See the [Workflow Reference](./workflows.md). Channels use the
same language for `validation_logic` and rate-limit `key_logic`. See
[Channel Configuration](./channel-config.md). Every expression is evaluated by
[datalogic-rs](https://github.com/GoPlasmatic/datalogic-rs) and compiled once,
at engine build time.

> [!WARNING]
> **A misspelled operator inside a `map` mapping is not an error.** JSONLogic
> cannot distinguish `{ "upper": [...] }` used as an operator from a data
> object that happens to have one key. Mappings render through a templating
> path: inner expressions resolve, and the unknown object is written through as
> a *literal*. So `{ "uppr": [...] }` puts `{"uppr": "widget"}` at the target
> path — no error, no failed task, `200` to the caller. When a mapping yields a
> JSON object where you expected a scalar, check the operator name against the
> tables below first.
>
> Conditions behave differently. They are compiled and evaluated strictly, so
> the same misspelling there is a hard error rather than a silent literal.

> [!NOTE]
> What an expression can read depends on where it appears. Workflow and task
> expressions read the [data context](./workflows.md); `validation_logic` and
> `key_logic` each see a smaller, dedicated context —
> [Channel Configuration](./channel-config.md) documents both. `key_logic`'s
> header set is closed by default and extended per channel with
> [`rate_limit.key_headers`](./channel-config.md#rate-limiting); a path outside
> it resolves to `null`, which is refused rather than used as a key.

## Available operators

The tables below are the **complete** set Orion compiles. They are not the
whole JSONLogic spec. See [Feature boundary](#feature-boundary).
`crates/orion-server/tests/integration/jsonlogic_operators_test.rs` asserts
this table against the engine, so it cannot drift from what actually runs.

### Core

| Operator | Example | Meaning |
|----------|---------|---------|
| `var` / `val` | `{ "var": "data.order.total" }` | Read a value from the context (dotted path) |
| `val` (path chain) | `{ "val": ["data", "items", { "val": ["temp_data", "i"] }] }` | Read with **computed** segments — the only way to index an array by a value |
| `==` / `!=` | `{ "==": [{ "var": "data.type" }, "order"] }` | Loose equality |
| `===` / `!==` | `{ "===": [{ "var": "data.qty" }, 1] }` | Strict equality (no type coercion) |
| `>` `>=` `<` `<=` | `{ ">": [{ "var": "data.order.total" }, 10000] }` | Comparison |
| `and` / `or` / `!` | `{ "and": [a, b] }` | Boolean logic |
| `!!` | `{ "!!": [{ "var": "data.order.id" }] }` | Truthiness (e.g. "is present") |
| `if` / `?:` | `{ "if": [cond, then, else] }` | Conditional value |
| `+` `-` `*` `/` `%` | `{ "*": [{ "var": "data.qty" }, 1.1] }` | Arithmetic |
| `max` / `min` | `{ "max": [1, 2, 3] }` | Largest / smallest |
| `cat` | `{ "cat": ["Order #", { "var": "data.order.id" }] }` | String concatenation |
| `substr` | `{ "substr": [{ "var": "data.code" }, 0, 3] }` | Substring |
| `in` | `{ "in": [{ "var": "data.tier" }, ["vip", "premium"]] }` | Membership (array or substring) |
| `merge` | `{ "merge": [[1, 2], [3]] }` | Flatten arrays into one |
| `map` / `filter` / `reduce` | `{ "map": [{ "var": "data.items" }, { "var": "price" }] }` | Array transforms |
| `all` / `some` / `none` | `{ "some": [{ "var": "data.items" }, { ">": [{ "var": "qty" }, 0] }] }` | Array predicates |
| `missing` / `missing_some` | `{ "missing": ["data.order.id"] }` | Report absent paths |

### Dates (`datetime`)

| Operator | Example | Meaning |
|----------|---------|---------|
| `now` | `{ "now": [] }` | Current instant, as an RFC 3339 string |
| `datetime` | `{ "datetime": ["2026-07-31T00:00:00Z"] }` | Build a datetime from an RFC 3339 string |
| `parse_date` | `{ "parse_date": [{ "var": "data.when" }, "yyyy-MM-dd"] }` | Parse with an explicit format |
| `format_date` | `{ "format_date": [{ "now": [] }, "yyyy-MM-dd", "Asia/Kolkata"] }` | Format a datetime, optionally in an IANA timezone |
| `date_diff` | `{ "date_diff": [a, b, "days"] }` | Whole units between two datetimes |
| `timestamp` | `{ "timestamp": ["1d"] }` | Build a **duration** from a duration string |

Format strings use the JSONLogic vocabulary: `yyyy`, `MM`, `dd`, `HH`, `mm`,
`ss`. Orion translates it to the underlying `strftime` spec, so raw
`%Y`-style patterns also work. Prefer the `yyyy` form; it is the documented
one.

`format_date` and `parse_date` accept an optional trailing IANA zone name
(`"Asia/Kolkata"`, `"America/New_York"`): `format_date` renders the instant as
that zone's wall-clock (DST-correct), and `parse_date` reads a naive input as
wall-clock time *in* that zone. A misspelled literal zone fails when the
expression is compiled, not per request. Do not add fixed offsets by hand —
`{ "+": [ts, { "timestamp": "5h30m" }] }` is wrong in any zone with DST.

### Strings (`ext-string`)

| Operator | Example | Meaning |
|----------|---------|---------|
| `length` | `{ "length": [{ "var": "data.items" }] }` | Length of a string **or** array |
| `upper` / `lower` | `{ "upper": [{ "var": "data.code" }] }` | Change case |
| `trim` | `{ "trim": [{ "var": "data.name" }] }` | Strip surrounding whitespace |
| `split` | `{ "split": [{ "var": "data.csv" }, ","] }` | Split into an array |
| `starts_with` / `ends_with` | `{ "starts_with": [{ "var": "data.sku" }, "AB-"] }` | Prefix / suffix test |

### Arrays (`ext-array`) and math (`ext-math`)

| Operator | Example | Meaning |
|----------|---------|---------|
| `sort` | `{ "sort": [{ "var": "data.scores" }] }` | Sort ascending |
| `slice` | `{ "slice": [{ "var": "data.items" }, 0, 2] }` | Sub-array by start/end |
| `abs` | `{ "abs": [{ "var": "data.delta" }] }` | Absolute value |
| `ceil` / `floor` | `{ "ceil": [{ "var": "data.price" }] }` | Round up / down |
| `group_by` | `{ "group_by": [{ "var": "data.meetings" }, { "format_date": [{ "var": "start" }, "dd MMM yyyy", "Asia/Kolkata" ] }] }` | Collapse on a computed key → array of `{key, items}` rows, insertion-ordered |
| `distinct` | `{ "distinct": [{ "var": "data.tags" }] }` | Deduplicate, first occurrence wins; add a key expression to dedupe by computed key |

`group_by`'s second argument is evaluated **per element**: inside it, `var`
paths are element-relative and `{ "var": "" }` is the element itself. The
result is an *array* of `{key, items}` groups (not an object), so it composes
directly with `map` / `filter` / `sort`.

### Objects (`ext-object`)

| Operator | Example | Meaning |
|----------|---------|---------|
| `keys` / `values` | `{ "keys": [{ "var": "data.totals" }] }` | An object's keys / values as arrays |
| `entries` | `{ "entries": [{ "var": "data.totals" }] }` | `[{key, value}]` rows — iterate any object with `map` |

### Control (`ext-control`, `error-handling`)

| Operator | Example | Meaning |
|----------|---------|---------|
| `??` | `{ "??": [{ "var": "data.nickname" }, "anonymous"] }` | Coalesce — first non-null |
| `type` | `{ "type": [{ "var": "data.price" }] }` | Type name as a string |
| `exists` | `{ "exists": ["data", "order", "id"] }` | Path presence — see [Sharp edges](#sharp-edges) |
| `switch` / `match` | see [Sharp edges](#sharp-edges) | Multi-way branch |
| `try` / `throw` | `{ "try": [expr, fallback] }` | Catch / raise an evaluation error |

### Encoding (Orion operators)

Registered by Orion itself rather than gated by a cargo feature — available on
every expression surface (conditions, `map` logic, `body_logic`, channel
guards). A string encodes as its UTF-8 bytes; any other value encodes as its
compact-JSON text (key order preserved); `null` is an error. Decoders are
strict: the input must be valid for the alphabet — base64 accepts padded and
unpadded input, and the decoded bytes must be valid UTF-8. Binary payloads
belong to the `crypto` function's `input_encoding` instead.

| Operator | Example | Meaning |
|----------|---------|---------|
| `base64_encode` / `base64_decode` | `{ "base64_encode": [{ "var": "data.raw" }] }` | Standard base64 (RFC 4648 §4, padded; decode tolerates unpadded) |
| `base64url_encode` / `base64url_decode` | `{ "base64url_encode": [{ "var": "data.claims" }] }` | URL-safe base64, unpadded — the JWS form |
| `hex_encode` / `hex_decode` | `{ "hex_encode": [{ "var": "data.token" }] }` | Lowercase hex |

### Randomness (Orion operators)

CSPRNG-backed value generation — never constant-folded, so every evaluation
draws fresh; there is deliberately no seed parameter, and range/alphabet
sampling is uniform (no modulo bias). The first argument selects the
generator kind; a new kind is a table row, never a new operator. Bad
arguments (unknown kind, inverted bounds, out-of-range length, an alphabet
with duplicate characters) are evaluation-time errors naming what is
allowed. Values are drawn live in dry-run and `orion-server test` too — like
`now`, avoid asserting exact outputs in regression cases.

| Operator | Example | Meaning |
|----------|---------|---------|
| `random` | `{ "random": ["digits", 6] }` | Kind-selected generation: `["uuid"]` / `["uuid", "v7"]` (canonical UUID; v7 is time-sortable), `["digits", n]` (exactly-n digits, leading zeros kept — the OTP shape, n ≤ 64), `["int", min, max]` (inclusive, within ±2⁵³−1), `["string", len, alphabet?]` (len ≤ 1024; named sets `alphanumeric` (default) / `hex` / `numeric` / `url-safe`, or a custom string of 2–256 distinct characters), `["bytes", n, encoding?]` (n ≤ 1024; `hex` default / `base64` / `base64url` per the encoding table above) |

### Strings (Orion operators)

Registered by Orion, so — like the encoding and randomness operators above and
unlike [`ext-string`](#strings-ext-string): they are not gated by a cargo
feature and are available on every expression surface.

`url_encode` follows the same text model as the encoders: a string encodes as
its UTF-8 bytes, any other value as its compact-JSON text, and `null` is an
error rather than an empty value. `url_decode` is strict — a malformed `%XX`
sequence or a non-UTF-8 result is an evaluation error, never a lossy guess.

| Operator | Example | Meaning |
|----------|---------|---------|
| `url_encode` | `{ "url_encode": [{ "var": "data.email" }] }` | Percent-encode per RFC 3986: unreserved `A–Z a–z 0–9 - _ . ~` survive, everything else becomes uppercase `%XX` |
| `url_decode` | `{ "url_decode": [{ "var": "data.state" }] }` | The exact inverse of `url_encode` |
| `join` | `{ "join": [{ "var": "data.tags" }, ", "] }` | Join an array's elements with a separator |

**`url_encode` is RFC 3986, not form-encoding.** A space becomes `%20`, never
`+`, and a literal `+` becomes `%2B`. It is also *stricter* than JavaScript's
`encodeURIComponent`, which leaves `!'()*` literal — that is the comparison
most authors will make. `url_decode` correspondingly does **not** treat `+` as
a space.

Reach for `url_encode` whenever a value is interpolated into an outbound query
string, which today means building `path` or `path_logic` with `cat`:

```json
{ "cat": ["/search?q=", { "url_encode": [{ "var": "data.term" }] }] }
```

Without it, a value containing `&` or `#` silently restructures or truncates
the query — a correctness and injection hole with no other mitigation, since
`http_call` has no `query` field.

**`join` takes both arguments.** A missing separator is an error, not an
implicit `""`, and a non-array first argument is an error too (`cat` already
handles scalars). Elements render exactly as `cat` renders them, so
`{"join": [arr, ""]}` is precisely `{"cat": [arr]}`.

Two idioms follow from `join` and are worth knowing instead of asking for more
operators:

- **`replace(s, from, to)` is `{"join": [{"split": [s, from]}, to]}`**: literal
  substring replacement, not a regex. (`{"cat": [{"split": [s, from]}]}` is the
  delete-all form and worked before `join` existed.) Splitting on an empty
  delimiter explodes into characters, so guard against that.
- **`{"cat": [arr]}`** remains the fastest spelling of `join(arr, "")`.

> [!WARNING]
> `{"cat": [arr, "|"]}` does **not** join with `|` — `cat` flattens the array
> and then appends the separator once, at the end. The `reduce`-with-sentinel
> workaround is also wrong: it cannot distinguish "first element" from "first
> element is empty", so `["", "b"]` joins to `"b"` rather than `", b"`. Use
> `join`.

### Secrets

| Operator | Example | Meaning |
|----------|---------|---------|
| `secret` | `{ "secret": "partner_hmac" }` | Read a value the operator declared in the `[secrets]` config section |

`secret` is the only operator that reads from outside the message. Its argument
is a name, not a path into the data context, and the value it returns is never
recorded: the store is held by the engine rather than by the message, so a
secret cannot reach a trace, a `map` mapping clone or a response body — there
is nothing to strip.

That guarantee is enforced, not advised. A workflow that reads a secret
anywhere the engine would record the result — a `map` mapping, a `log` message
or field — is refused when the engine is built, and so is a workflow naming a
secret the instance does not declare. Both surface as a quarantined channel
with the reason named, not as a value that silently goes missing. So read a
secret in a **condition**, or in one of the five function fields that take key
material — `crypto.key`, `jwt_sign.key`, and `jwt_verify`'s `keys[].key`,
`issuer` and `audience`, and nowhere else. A value *derived* from a secret
belongs inside a function, not in a mapping.

Three limits worth knowing:

- **`env://` and `vault://` resolve in five function fields, not every one.**
  Those five read a *reference string* themselves, because their handlers do —
  and inside `jwt_verify.keys` it is each entry's `key`, not the entry: a
  reference in a sibling `kid` or `key_encoding` is read verbatim and is refused
  like any other stray one. The `{"secret": …}` **operator** is separate and
  wider: it resolves wherever the engine evaluates JSONLogic, which now includes
  a connector task's expression fields, so
  `{"cat": ["Bearer ", {"secret": "partner_token"}]}` works in an `http_call`
  header. It still resolves nothing in a document-shaped field, which folds
  `{"var": …}` only. A credential a remote system needs generally belongs on the
  **connector**, which is what connectors are for.
- **Channel guards read them too.** `validation_logic`, `authorization_logic`
  and the rate-limit and cache `key_logic` compile on an engine built over the
  same store the workflow engines get, so `{"secret": …}` resolves there as
  well. Secrets are start-time config, resolved before any channel loads, so a
  guard sees exactly what a workflow sees.
- **Deployment values are not secrets.** A topic prefix or a partner's base URL
  belongs in `[vars]`, read as `{"var": "metadata.vars.name"}`, which *is*
  recorded, on purpose, because an operator reading a trace needs to see it.
  See [Environment Variables](./environment-variables.md).

## Sharp edges

Five operators take a shape or a value that is easy to get wrong. Each bullet
states the failure mode; most fail quietly, with a plausible answer rather
than an error.

- **`exists` takes path segments, not a dotted path.** Unlike `var`, it does
  not split on `.`, and it evaluates its arguments as literals rather than as
  expressions. `{ "exists": ["data.order.id"] }` looks for a single top-level
  key literally named `data.order.id` and returns `false`. Wrapping the
  argument in a `var` returns `false` too, because the *value* is not a path.
  Spell it out:

  ```json
  { "exists": ["data", "order", "id"] }
  ```

- **`switch` takes an array of `[case, result]` pairs**, not a flat
  alternating list. A flat list is not rejected: the second element is read as
  the case array, fails to match, and the third element is returned as the
  default arm. You silently get one fixed branch for every input.

  ```json
  { "switch": [
      { "var": "data.tier" },
      [ ["gold", 20], ["silver", 10] ],
      0
  ] }
  ```

- **`date_diff` units are plural.** `"days"`, `"hours"`, `"minutes"`,
  `"seconds"`. An unrecognized unit — including the singular `"day"` — returns
  `0` rather than an error, which reads exactly like "the dates are the same".

- **`timestamp` is not a datetime-to-epoch conversion.** It parses a
  *duration* (`"1d"` → `"1d:0h:0m:0s"`) for use in date arithmetic. Passing it
  a datetime is an `Invalid duration format` error.

- **`now` is evaluated per call**, so two `now` mappings in one workflow can
  land on different instants. Compute it once into a field and read that field
  if you need a single consistent stamp.

## Connector fields: expressions, and documents

Most of a connector task's fields are JSONLogic, evaluated against the message
like anything else on this page — a cache key, a storage key, an email
subject or recipient, a TTL, a JWT audience, the data going into `crypto`:

```json
{ "connector": "redis",
  "key": { "cat": ["tenant:", { "var": "data.tenant" }, ":order:", { "var": "data.id" }] } }
```

A field written as a plain literal is JSONLogic for itself, so the static
spelling is unchanged and costs nothing: it folds once when the engine is built,
and only a field that actually reads the message is evaluated per request.

**The document-shaped fields are the exception, and deliberately so.** A MongoDB
`document`, `documents`, `update`, `filter`, `array_filters`, `projection` or
`sort`; an aggregation `pipeline`; `cache_write`'s `value`; `jwt_sign`'s
`claims`; the `params` of the SQL and dialect functions — these fold
`{"var": "some.path"}` nodes against the message, at any depth, and treat
**every other node as a literal**.

```json
{ "op": "insert_one",
  "document": {
    "userId":    { "var": "data.user_id" },
    "expiresAt": { "cat": ["2026-", { "var": "data.month" }] }
  } }
```

`userId` gets the value. `expiresAt` gets the *object* `{"cat": [...]}`, stored
in MongoDB verbatim, and in a `filter`, a node like that matches nothing.
There is no error at write time. Note that `{"val": …}` is folded no more than
`cat` is, despite being a documented operator: only `var` is.

The reason is `$`. These are exactly the fields that carry MongoDB operators and
extended-JSON wrappers — `$set`, `$push`, `$oid`, `$date`, and one `$` is
stripped from every key in a position the engine evaluates. Making them
expressions would turn `{"$set": …}` into `{"set": …}` in every stored
definition that was not hand-corrected, silently. The scalar fields carry no
such keys, so they carry the capability instead.

Compute the value in a `map` task first and reference the result:

```json
{ "path": "temp_data.expires_at",
  "logic": { "cat": ["2026-", { "var": "data.month" }] } }
```

```json
{ "expiresAt": { "var": "temp_data.expires_at" } }
```

`orion-server lint` warns when it finds one of these, and
`orion-server test` can assert on the resolved payload with `expect_calls` —
see [Test Workflows Offline](../build/testing.md#assert-on-what-a-workflow-writes).

## Feature boundary

The extension categories — `datetime`, `ext-string`, `ext-array`, `ext-math`,
`ext-control`, `error-handling` — are Cargo features of datalogic-rs.

> [!NOTE]
> Orion reaches datalogic-rs through dataflow-rs and cannot enable a datalogic
> feature on its own. The extension operators are available only as dataflow-rs
> enables them; Orion turns them all on through dataflow-rs's `all-operators`
> feature. A build without that feature compiles only the [Core](#core) set.

## Related

- [Workflow Reference](./workflows.md): the workflow object and the data
  context that conditions and mappings read.
- [Channel Configuration](./channel-config.md): `validation_logic` and
  `key_logic`, and the dedicated context each one sees.
- [Function Reference](./functions.md): every function's `input` schema,
  including the fields that accept logic values.

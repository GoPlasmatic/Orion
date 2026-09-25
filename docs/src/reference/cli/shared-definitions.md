<!-- description: How a definition set says a thing once: $from splices a constant, use and $use expand fragments, $each repeats, and compile resolves them all. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# Shared definitions

A definition set can say a thing once: a shared value, a task sequence, a parameterised value, or one element repeated. All of it is expanded **before** validation. `lint`, `dry-run` and `test` check and run the expanded form, and the server, the admin API and traces never see a reference.

## Synopsis

```json
{ "constants": { "db": { "connector": "sias-mongo", "database": "app" } },
  "errors":    { "USER_NOT_FOUND": { "status": 400, "body": "User Not Found !" } } }
```

## Description

**`$from` splices a named value** into the object it sits in:

```json
{ "input": { "$from": "constants.db", "collection": "users" } }
```

resolves to `{"connector": "sias-mongo", "database": "app", "collection": "users"}`. It is a **merge, not a substitution**, and **siblings win**, so a call site overrides one field without copying the rest. A `$from` alone in its object, naming a scalar or array, replaces the whole node.

**Fragments are named task sequences**, parameterised:

```json
{ "fragments": { "require-session": {
    "params": { "deny_message": { "default": "Session expired." } },
    "tasks": [ { "id": "check", "name": "Check",
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.msg", "logic": { "$param": "deny_message" } } ] } } } ] } } }
```

```json
{ "id": "_session", "use": "require-session", "with": { "deny_message": "Please sign in." } }
```

A `use` step goes wherever a step goes: in a workflow's `tasks`, inside a task group, and in a loop's `setup` list.

Expanded task ids are namespaced by the call-site id (`_session.check`). A fragment therefore cannot collide with the including workflow or with a second instance of itself. **Every** id the fragment contributes is prefixed, including those inside a task group, a group's own id and its members' alike. The prefix is flat rather than one segment per enclosing group, so `refused`/`deny` become `_session.refused` and `_session.deny`. A parameter with no `default` is required at every call site. A fragment may use another fragment, at any depth, and the inner ids carry both call sites: `_session.inner.check`.

A shared document is one carrying `constants`, `errors`, `fragments` or a `package` declaration, and no entity field. A `.sql` file is not a shared document. It is read only when a `$sql` names it. It is found by shape, like entities, and split across as many files as you like. A name defined twice is an error rather than a silent last-write-wins.

Every unresolved reference is a lint error, which is why set mode resolves the catalog with no flag; the single-file commands take `--definitions <dir>`.

## Value fragments (`$use`)

A fragment may hold one `value` instead of `tasks`. It is spliced by `$use` wherever a value goes, under the same rule as `$from`:

```json
{ "fragments": { "wrote": {
    "params": { "slot": {} },
    "value": { ">": [{ "var": "{{slot}}.rows_affected" }, 0] } } } }
```

```json
{ "condition": { "$use": "wrote", "with": { "slot": "temp_data.fold" } } }
```

An object value merges into the call site, and the call site's other keys win. Any other value replaces a call site that has no other keys. A fragment declares exactly one of `tasks` and `value`. `use` of a value fragment and `$use` of a task fragment are both errors naming the kind. In a `tasks` array, a `$use` whose value is a step is a step.

## Parameters: `$param` and `{{name}}`

Inside a fragment, `{"$param": "name"}` is replaced by the argument itself, keeping its type. `"{{name}}"` inside a string is replaced by its text: a string as it is, a number or boolean as JSON. Interpolating an object, an array or `null` is an error; use `$param` for those. A `{{…}}` naming nothing in scope stays text, so a PostgreSQL array literal `'{{1,2}}'` is safe. Object keys are never interpolated.

Scope is closed. A fragment sees its own parameters and nothing from the call site, unless the call site passes it through `with`. An argument written as `{"$from": "constants.x"}` is the constant itself, so it can be interpolated or repeated over. A `$param` naming nothing in scope inside a fragment is a warning, since it would write that object into the data.

## Repetition (`$each`)

An `$each` element of any array is replaced by one copy of its `do` per value:

```json
{ "$each": { "p": [0, 1, 2, 3] },
  "do": { "id": "infer{{p}}", "name": "Infer {{p}}", "function": { "name": "map", "input": {
    "mappings": [{ "path": "data.out{{p}}", "logic": { "val": ["seats", { "$param": "p" }] } }] } } } }
```

The list is a literal array, a `$from` constant or a `$param`. An empty list produces nothing. The element holds exactly `$each`, with one name, and `do`. For a product, nest one `$each` in another's `do`: object keys have no order to repeat by. A `do` is one element. In a `tasks` array it is a step, so it may be a `use` step or a group. Duplicate ids are left to the validator, which is why ids interpolate a binding. An `$each` outside an array is an error, and so is rebinding a name already in scope.

## Composition and limits

A constant may reference another constant, use a value fragment or hold an `$each`. Every constant is resolved once, when the set is loaded, so one `compile` resolves it fully. A cycle among constants or fragments is reported once, with its chain, and every reference into it is dropped. Fragments and `$each` nest at most 16 deep, and one document makes at most 4096 `$each` copies.

| Check | When |
|---|---|
| `shared.fragment` | A fragment declares both or neither of `tasks` and `value` |
| `shared.fragment_kind` | `use` names a value fragment, or `$use` a task fragment |
| `shared.fragment_param` | An unknown argument, or a required parameter left unbound |
| `shared.param_unbound` | A `$param` naming nothing in scope, inside a fragment or an `$each` (a warning) |
| `shared.interpolate_non_scalar` | `{{name}}` bound to an object, an array or `null` |
| `shared.each_shape`, `shared.each_list`, `shared.each_position` | A malformed `$each`, a list that is not an array, or an `$each` outside an array |
| `shared.each_limit`, `shared.depth` | More than 4096 copies, or nesting past 16 levels |
| `shared.binding_shadowed` | An `$each` rebinding a name already in scope |
| `shared.cycle` | A fragment or constant that reaches itself |

A finding inside an expansion names the trail that produced it, such as `(fragment 'guard' › $each p = 3)`.

> [!NOTE]
> Expansion is an **authoring and deploy** mechanism. The admin API takes one JSON body with no set to resolve against, so `POST /api/v1/admin/workflows` does not accept `$from`, `use`, `$use` or `$each`; it refuses them with [`UNCOMPILED_SOURCE`](../errors.md#field-error-codes), naming the reference and its coordinate. [`orion-server compile`](./orion-server/compile.md) is the step that produces what it does accept. `package export` needs no inlining step for the same reason: it exports what a server stored, which was already compiled.

## Statements in `.sql` files

A `db_read` or `db_write` statement can live in a file of its own:

```json
{ "query": { "$sql": "../sql/settle.sql" } }
```

`$sql` must be the only key in its object. The path is relative to the file the reference sits in, must end in `.sql`, and may not leave the definition set. A `$sql` inside a fragment or a shared constant is written relative to the shared document. It is re-read relative to each workflow it lands in. A file is at most 1 MiB.

`compile` replaces the reference with the statement in **normal form**. Comments and every run of whitespace become one space, and none is left at either end. Strings, quoted identifiers, dollar-quoted bodies and optimizer hints (`/*+ … */`, `/*! … */`) are copied byte for byte. One trailing `;` is dropped. So a comment or an indentation change moves neither the statement nor the package's content hash. Nothing is interpolated into the file: values still travel in `params`.

The lexer refuses what PostgreSQL and MySQL read differently, rather than guessing, and names the line in the `.sql` file:

- **A backslash before a closing quote**, as in `'it\'s'`. PostgreSQL ends the string at that quote, and MySQL escapes it. Write `''` for a quote, or `E'…'` on PostgreSQL.
- **`--` glued to the next character**, as in `--x`. PostgreSQL and SQLite start a comment, and MySQL reads two minus signs. Write `-- ` with a space.

MySQL's `#` comments are not recognized, because `#` is an operator in PostgreSQL. A CRLF inside a string literal is data and is kept.

`lint`, `dry-run` and `test` resolve `$sql` for a single workflow file with no `--definitions`, against the file's own directory. A finding about the statement names the `.sql` file it came from.

## The `package` document

A set may declare what it is and which Orion it needs, in one shared document:

```json
{ "package": { "name": "orders", "requires": { "orion": ">=1.8.2, <2" } } }
```

`requires.orion` is a version range in the usual comparator syntax: `>=1.8.2, <2`, `^1.8`, `1.8.*`. Every offline command checks the running binary against it **first**. `lint`, `clippy`, `compile`, `fmt`, and `dry-run` or `test` given `--definitions` stop with one line when the binary is outside the range:

```
Error: the definition set (definitions/package.json) requires Orion >=1.9.0, <2; this is orion-server 1.8.2
```

That line replaces the errors a set would otherwise draw from features the binary predates, such as an unknown channel protocol or function. `fmt` formats nothing in that case, because the style tables differ between versions. A pre-release binary is judged as its release, so `1.9.0-rc.1` satisfies `>=1.9.0`.

`compile` carries the range into the artifact's `requires.orion`, where `package plan` and `package apply` check the target. It also uses `name` as the package name when `--name` is not given. The range is not content: changing it moves no hash.

- **One per set.** A second `package` document is an error naming the first.
- **A closed shape.** `name` and `requires` are the only fields, and `orion` the only requirement, so a typo such as `require` is an error rather than a gate silently switched off.
- **Not a `$from` namespace.** `{"$from": "package.name"}` resolves nothing.
- **Told apart from an artifact.** A promotion artifact also has a top-level `package`, with a `content_hash` and `workflows` beside it. An artifact inside a definitions tree is still not part of the set.

A binary that predates the document skips the file with a note, so the protection starts with the first release that reads it.

These mechanisms are passes in one pipeline, and the pipeline is the place a future authoring convenience is added. Fragments, `$use` and `$each` expand first, in one walk, then `$from` splices, then `$sql` inlines. A new pass is compiled by `compile`, reported in its per-pass summary, and named by the admin API's refusal. None of those three has to learn about it.

## Related

- [`orion-server compile`](./orion-server/compile.md): the step that resolves both forms into what the admin API accepts.
- [`orion-server lint`](./orion-server/lint.md): set mode, where every reference must resolve.
- [Author a workflow](../../guides/author/workflows.md): `$from` and `use` in a real set.
- [Errors and response envelopes](../errors.md): `UNCOMPILED_SOURCE`, the refusal a source form meets at the API.

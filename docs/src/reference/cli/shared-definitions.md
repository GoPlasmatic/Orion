<!-- description: How a definition set says a thing once: $from splices a shared constant or error, use expands a parameterised task fragment, and compile resolves both. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Shared definitions

A definition set can say a thing once. Two mechanisms, one resolution pass, both expanded **before** validation. `lint`, `dry-run` and `test` all check and run the expanded form, and the server, the admin API, traces and the UI never see a reference.

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

Expanded task ids are namespaced by the call-site id (`_session.check`). A fragment therefore cannot collide with the including workflow or with a second instance of itself. **Every** id the fragment contributes is prefixed, including those inside a task group, a group's own id and its members' alike. The prefix is flat rather than one segment per enclosing group, so `refused`/`deny` become `_session.refused` and `_session.deny`. A parameter with no `default` is required at every call site. A fragment cannot include another fragment, at any depth.

A shared document is one carrying `constants`, `errors` or `fragments` and no entity field. It is found by shape, like entities, and split across as many files as you like. A name defined twice is an error rather than a silent last-write-wins.

Every unresolved reference is a lint error, which is why set mode resolves the catalog with no flag; the single-file commands take `--definitions <dir>`.

> [!NOTE]
> Expansion is an **authoring and deploy** mechanism. The admin API takes one JSON body with no set to resolve against, so `POST /api/v1/admin/workflows` does not accept `$from` or `use`; it refuses them with [`UNCOMPILED_SOURCE`](../errors.md#field-error-codes), naming the reference and its coordinate. [`orion-server compile`](./orion-server/compile.md) is the step that produces what it does accept. `package export` needs no inlining step for the same reason: it exports what a server stored, which was already compiled.

Both mechanisms are passes in one pipeline, and the pipeline is the place a future authoring convenience is added. A new pass is compiled by `compile`, reported in its per-pass summary, and named by the admin API's refusal. None of those three has to learn about it.

## Related

- [`orion-server compile`](./orion-server/compile.md): the step that resolves both forms into what the admin API accepts.
- [`orion-server lint`](./orion-server/lint.md): set mode, where every reference must resolve.
- [Author a workflow](../../guides/author/workflows.md): `$from` and `use` in a real set.
- [Errors and response envelopes](../errors.md): `UNCOMPILED_SOURCE`, the refusal a source form meets at the API.

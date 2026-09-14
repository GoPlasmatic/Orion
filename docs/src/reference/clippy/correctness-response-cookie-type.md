<!-- description: The `correctness.response_cookie_type` advisory rule, level `warn`, scope workflow: a response cookie attribute is a literal of the wrong type, so the cookie. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `correctness.response_cookie_type`

A `warn` rule, scope workflow. A response cookie attribute is a literal of the wrong type, so the cookie is always dropped.

## Synopsis

```console
$ orion-server clippy ./definitions
warn[correctness.response_cookie_type] a response cookie attribute is a literal of the wrong type, so the cookie is always dropped
```

## Description

A `secure` or `http_only` that is a literal non-boolean, or a `max_age` that is a literal non-integer, in a mapping to `data._orion.response.cookies`. The response builder refuses the value and drops the cookie, while the request still answers with its declared status. Coercing the string `"false"` to `true` would be worse. A dropped session cookie therefore presents to a browser exactly like the browser having refused it.

## Caveats

Silent when the value is an expression, which can be either type per request. Those are reported at runtime in the response envelope's `errors` and in `orion_response_drops_total`.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.

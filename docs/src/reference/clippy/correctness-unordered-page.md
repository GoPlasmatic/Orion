<!-- description: The `correctness.unordered_page` advisory rule, level `deny`, scope workflow: a read that skips rows without ordering them, so the page it skips is undefined. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `correctness.unordered_page`

A `deny` rule, scope workflow. A read that skips rows without ordering them, so the page it skips is undefined.

## Synopsis

```console
$ orion-server clippy ./definitions
deny[correctness.unordered_page] a read that skips rows without ordering them, so the page it skips is undefined
```

## Description

A `data_query` or `mongo_read` whose `skip` is not accompanied by a `sort`. `skip` names a position in an order. Without one the rows come back in whatever the query plan emitted, so "skip the first 20" names no particular set. Two calls need not agree, with no writes in between and no error anywhere. The dialect already refuses this one level in: an `include` must state a `sort`. The per-parent page is cut in the database, and "the first 10 orders" otherwise has no defined answer. The root page has the same problem and keeps accepting it, so the finding is here.

## Caveats

Silent when `skip` is the literal `0`, which skips nothing. Silent when a `sort` is given, whether or not its keys are unique. Silent for a `limit` with no `sort`, which is a legitimate "any n". See [Paging](../data-dialect.md#paging) for the cursor that pages without an offset at all.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.

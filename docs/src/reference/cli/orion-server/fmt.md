<!-- description: orion-server fmt rewrites definition files to the one house style, with --check for CI and --stdin for editors; the exit codes and what it refuses. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `orion-server fmt`

Formats definition files to the house style, the way `cargo fmt` formats Rust. One style, nothing to configure; the style itself is documented on [Definition Style](../../fmt.md). Needs no config, database, or server.

## Synopsis

```bash
orion-server fmt [PATH]... [--check] [--stdin]
```

## Description

Files are rewritten atomically: a sibling temp file is renamed over the original, permissions preserved. The write happens only after the formatted output has been parsed again and compared with the input as the runtime sees it. A formatter that could change what a workflow means would not be safe to run from a pre-commit hook. A file that is not strict JSON, has a duplicate key, or nests too deep for the parser is reported with its line and column. It is left untouched.

## Options

| Flag | Description |
|------|-------------|
| `PATH` | Files or directories (default: `.`). Every `.json` under a directory is formatted — entities, shared documents, `*.case.json` files and fixtures alike. Hidden entries, `target/` and `node_modules/` are skipped; symlinked directories are not followed. |
| `--check` | Write nothing. Print a unified diff for every file that is not in the house style and exit 1 if there is one — the CI form. |
| `--stdin` | Format one document from stdin to stdout, for editor integration. On a parse error nothing reaches stdout. |

## Returns

| Exit code | Meaning |
|---|---|
| `0` | Every file is formatted, or has been written by this run. |
| `1` | `--check` found at least one file it would rewrite. |
| `2` | A file could not be read, parsed or written. The other files are still processed. Also: a directory holds a [`package` document](../shared-definitions.md#the-package-document) whose `requires.orion` excludes this binary, and nothing was formatted. |

## Examples

```
$ orion-server fmt --check ./definitions
--- a/definitions/orders/channel.json
+++ b/definitions/orders/channel.json
@@ -3,9 +3,7 @@
   "name": "orders",
   "channel_type": "sync",
   "protocol": "rest",
-  "methods": [
-    "POST"
-  ],
+  "methods": ["POST"],
   "route_pattern": "/orders",
1 file(s) would be reformatted, 61 unchanged
```

```bash
orion-server fmt ./definitions && orion-server lint ./definitions
```

## Related

- [Definition style (`fmt`)](../../fmt.md): the one layout this command writes.
- [Test a workflow offline](../../../guides/author/testing.md): `fmt --check` beside `lint` in a CI gate.
- [`orion-server lint`](./lint.md): the check that usually follows a format.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.

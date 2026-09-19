# SQL in files

A `db_read` whose statement lives in a `.sql` file instead of a JSON string.

A real statement runs to dozens of lines, and as one JSON string it is
unreviewable: no line breaks, no comments, and every quote escaped. The
workflow here names the file instead:

```json
"query": { "$sql": "sql/describe.sql" }
```

`sql/describe.sql` is ordinary SQL, with comments and indentation. The path is
relative to the workflow's own file, and may not leave the set.

**The server never sees the reference.** `$sql` is an authoring convenience,
like `$from`: `orion-server compile` reads the file, puts the statement in
normal form, and writes the string the admin API accepts. Normal form means
comments and runs of whitespace become one space, while strings and quoted
identifiers stay byte for byte. Rewording a comment therefore changes neither
the compiled statement nor the package's content hash.

`lint` checks the file the same way `compile` does. A statement the lexer
cannot read the same way on PostgreSQL and MySQL is refused, naming the line
in the `.sql` file. So is a `db_read` statement that is not a read.

## Deploy it

The example deploy script posts each file as it is, and the admin API refuses
an uncompiled `$sql`. Compile the package and apply the artifact instead:

```bash
orion-server compile examples/packages/sql-files --name sql-files --version content -o sql-files.json
orion-server package apply -s http://localhost:8080 -f sql-files.json
```

## Try it

```bash
curl -s -X POST http://localhost:8080/api/v1/data/describe-word \
  -H 'content-type: application/json' -d '{"word": "orion"}'
```

The answer carries `data.result`: `[{"shout": "ORION", "letters": 5}]`.

The connector is an in-memory SQLite database, so the statement needs no
tables. `examples/workflow-tests/sql-files-describe.case.json` runs the same
workflow offline and checks the statement the task sends:

```bash
orion-server test examples/workflow-tests/sql-files-describe.case.json
```

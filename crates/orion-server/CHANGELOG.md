# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`orion-server compile <dir>` — a definition set in, files the admin API
  accepts out.** The authoring conveniences a set may use, `$from` for a shared
  value and `use` for a task fragment, resolve when a *set* is loaded, and the
  admin API loads no set: it takes one document with nothing to resolve names
  against. Nothing in the product performed that step for a deploy tool —
  `package export` reads a live instance, which only ever stored compiled
  documents — so the only path from `definitions/` to a running instance was a
  tool that reimplemented the expander.

  `compile` runs every gate `lint <dir>` runs and then emits: a promotion
  artifact by default, hashed exactly as `package export` hashes one so
  `package plan|apply|diff` consume it unchanged; `--format dir` mirrors the
  input tree, one compiled file per entity; `--format bulk` writes the three
  bulk-import arrays. `--requires-channel` / `--requires-connector` fill the
  artifact's `requires`, and `--no-activate` emits drafts.

- **An authoring layer the next simplification plugs into**
  (`definitions/compile.rs`). `$from` and `use` are now two `Pass`es in an
  ordered pipeline rather than a hard-coded pair of rewrites. A pass declares
  its **residue** — where its own syntax still appears in a document — and that
  one method is read three times: the pipeline test asserts residue is empty
  after compiling (which is what "canonical" means and what the runtime relies
  on), `compile` reports which passes fired, and the admin API turns leftover
  residue into an error that names it. Adding a pass gets all three.

### Fixed

- **The admin API refused `$from` / `use` with the symptom, not the cause**
  (#295). An uncompiled reference reached the function-input validator as
  literal JSON and was refused for the fields the reference would have
  supplied — `tasks[1].function.input` *requires 'connector'* — so an author
  went looking for a typo that was not there. A `use` step arrived as a task
  missing its `name` and `function`; a connector config as `missing field
  connection_string`; a channel config as `unknown field '$from'`, which reads
  as a misspelling. All four now return `UNCOMPILED_SOURCE` with the
  reference, its authored coordinate, and the command that resolves it.

  This also closes a hole rather than only rewording one: a `$from` deep enough
  in a task payload — inside a `map` mapping's `logic`, say — satisfied every
  schema and was **stored with 201**, and the workflow then wrote the literal
  `{"$from": …}` object into its response at runtime. It is refused now. The
  detection comes from the compiler's own passes, so what `compile` consumes
  and what the API refuses cannot drift apart.

## [1.2.0] - 2026-08-26

### Changed

- **Docs: the "Build with AI" path is the agent skill, not an MCP server.**
  `orion-cli`'s built-in MCP server was removed (see the CLI changelog — its
  HTTP transport served the admin API with no authentication of its own), so
  `ai/mcp-setup.md` is replaced by `ai/skills.md`, `ai/claude-code.md` is a
  CLI-driven session, and the skill itself ships in `skills/orion/`.

### Fixed

- **Two AI-facing pages described a rollback that does not exist.**
  `ai/claude-code.md` and the prompt pack both said rollback was "re-activating
  a previous version". Nothing reactivates an archived version in place —
  status addresses a workflow id, not a version, and activating always promotes
  the current draft. Both now carry the real procedure (cut a new version, put
  the known-good content in it, activate), which is what
  `build/versioning.md#roll-back` has always specified.


### Added

- **`GET /admin/functions` serves every valid function name** ([#288]) — it
  served the schema registry, which is 18 of the 27 names a workflow may use.
  The nine it omitted (`map`, `filter`, `log`, `parse_json`, `parse_xml`,
  `validation`/`validate`, `publish_json`, `publish_xml`) are the most-used
  functions there are: 425 of 631 tasks in the deployment that reported it,
  `map` alone 310. Anything completing from this endpoint offered the connector
  functions and none of the ones people type.

  Engine built-ins now appear with `source: "engine"` and **no** `input_fields`
  — omitted rather than nulled, because absence is the honest encoding and what
  a consumer branches on. Orion handlers carry `source: "orion"` and their
  schema as before. `validation` carries `validate` in `aliases` rather than
  appearing twice, so a completion tool is not told there are two functions.
  `category` gains a fourth value, `data`, matching the grouping the reference
  page already gives these.

  The schema registry itself is untouched, and so are `validate_input` and the
  create-time field errors — the catalogue is a second view over it plus the
  built-ins, not a widening of it. Two lists rather than one overloaded one.

  This also closes a hole rather than only filling a gap:
  `functions_docs_drift_test` asserted the summary table against the *registry*,
  so the eight dataflow-rs rows in `reference/functions.md` were checked by
  nothing and could drift freely. The table is now held to all 27.

  Additive — rows and one field. A consumer that assumed every row carries
  `input_fields` must tolerate its absence, which the `source` discriminator
  exists to explain.

- **`UNKNOWN_FUNCTION` names the nearest valid function** ([#289], [#288]) —
  a typo now gets a suggestion instead of a bare refusal:

  ```
  Unknown function 'mongo_writes' — did you mean 'mongo_write'? — this workflow
  would be accepted and then fail at its first request
  ```

  Candidates come from `known_functions()`, the set the validation gate itself
  consults, so a suggestion is always a name the engine can run — including the
  `RequiresHandler` subtlety that keeps `enrich` out of it. The edit-distance
  window scales with the shorter name (a third of it, clamped 1–3) rather than
  being fixed, so `http_request` gets no guess instead of a wrong one. Message
  text only; the `field`, the `code` and the error shape are unchanged.

  Item 2 of #288 — the catalogue endpoint — is the entry above.

- **dataflow-rs 3.6: task groups and `terminal`.** A `tasks` element carrying
  its own `tasks` key is a **task group** — one condition guarding a contiguous
  run of tasks — and any step may set `terminal: true` to end the workflow after
  it runs. Together they are the guard clause (*if this, answer and stop*),
  which removes the hand-written negation every later task otherwise has to
  restate. The condition is evaluated once on entry, groups nest 8 deep, and
  group ids share the task id namespace.

  **Orion needed real work to accept them, not just the version bump.** The
  engine flattens the step tree at parse, so the executor was fine — but every
  Orion check that asks "what does this workflow reference" reads the *authored*
  JSON, and each of those iterated the array looking for `function`. Before this,
  a grouped workflow was **rejected outright** by `validate_workflow_tasks_schema`
  (the group looked like a task missing its `name` and `function`), and had the
  validator been lenient the group's members would instead have gone unchecked:
  a connector referenced only from inside a guard clause would have passed
  closure checking, and its tasks would have shipped unvalidated.

  So there is now one flattener, `engine::walk_steps` (`engine/steps.rs`), and
  every walk uses it: the connector and `channel_call` closure walks, the task
  schema validator (with nested paths — `tasks[1].tasks[0].function.name`), the
  unresolvable-JSONLogic advisory, the offline call-log correlation, fragment
  expansion, `POST /workflows/validate` (which otherwise reported a group as a
  task missing its `name` — `valid: false` for a workflow `POST /workflows`
  accepts and the engine runs) and `preflight`'s dialect-schema scan (which
  otherwise could not see a schema-less `data_query` inside a guard clause —
  the one 1.0 break it exists to catch in advance). The shape catch-all now
  parses through the engine's own step parser rather than `Vec<Task>`, which
  does not flatten and would reject every grouped workflow the engine accepts.

  Groups are validated in their own right: id required and unique across tasks
  *and* groups, non-empty `tasks`, boolean `terminal`, and a depth cap mirroring
  the engine's so an author gets a field error rather than a failed reload.

  Additive on the wire — every existing workflow parses and behaves identically.

- **Shared definition sources: fragments and a value catalog** ([#285]). A
  definition set can say a thing once. A shared document — any JSON carrying
  `constants`, `errors` or `fragments` and no entity field, split across as
  many files as you like — declares named values and named task sequences that
  workflows reference.

  `{"$from": "constants.db", "collection": "users"}` **splices** the named
  value's fields into the object it sits in, and **siblings win**, so a call
  site overrides one field without copying the rest. A `$from` alone in its
  object naming a scalar replaces the node. One operator over **open
  namespaces** rather than the proposed `$const` and `$error` pair: those are
  the same operation, and a future `timeouts` catalog now costs no code.

  `{"id": "_session", "use": "require-session", "with": {…}}` expands a
  parameterised task sequence in place, with ids namespaced by the call-site id
  (`_session.check`) so a fragment cannot collide with the including workflow
  or with a second instance of itself. A parameter with no `default` is
  required. A fragment cannot include another fragment — refused with a message
  rather than looping on a cycle.

  Expansion runs on the raw JSON **before** `CreateWorkflowRequest` parses, so
  `lint`, `dry-run` and `test` all check and run the expanded form. `lint <dir>`
  finds the catalog with no flag; the single-file commands take
  `--definitions <dir>`, and a reference without one now names the reference
  and the missing flag rather than surfacing as "task 0 has no name".

  Deliberately authoring-and-deploy only: the admin API receives one body with
  no set to resolve against, so it still takes expanded JSON, and the engine,
  traces and the UI never meet a reference. `package export` needs no inlining
  step for the same reason — it exports what a server stored.

- **`orion-server lint <dir>` — validate a definition set** ([#286]). Every
  channel, workflow and connector under a directory, plus the references
  *between* them: a `channel_call` target that exists nowhere, a task naming a
  connector that is absent or of the wrong type, a channel whose `workflow_id`
  resolves to nothing, duplicate ids, names and routes. Route collisions are
  judged exactly as the runtime route table judges them — `/o/{id}` and
  `/o/{orderId}` are one route, `methods: []` means every method, a Kafka
  channel's stray `route_pattern` serves nothing, and a deliberate
  higher-`priority` override is not a collision. A set could be green on
  every per-file gate and still be missing a channel at runtime, because the
  file that would disprove it is one `lint <file>` never opens.

  Entities are discovered by **shape**, recursively — `tasks` is a workflow,
  `connector_type` a connector, `channel_type`/`protocol` a channel. Anything
  else is reported as skipped rather than silently ignored, and a directory
  yielding no definitions is an error: a set lint that quietly stops reading is
  worse than no set lint.

  References must resolve in-set by default. `--requires-channel` and
  `--requires-connector` widen that for a set that depends on something
  deployed elsewhere — the directory equivalent of a package artifact's
  `requires`. `--deny-warnings` behaves exactly as it does for a single file.
  `lint <file>` is byte-for-byte unchanged.

  Findings carry a third, exit-neutral severity beside `error` and `warning`:
  a `note` is an inventory line the report is expected to hold — today the
  environment variables a set references via `env://` (`[env.reference]`) —
  a fact about a correct set, never counted by the exit code or by
  `--deny-warnings`. Counting it as a warning would have made the flag fail
  on every set that authors a secret the documented way.

- **`metadata` in a `*.case.json`** ([#283]) — a case can supply the request
  metadata the HTTP ingress would have built (`headers`, `params`, `query`,
  `cookies`, `auth.claims`, `channel`, `http_method`, plus any caller keys), so
  a workflow that branches on `metadata.headers` is testable offline instead of
  only against a running server. `dry-run --metadata <file>` takes the same
  object.

  Normalized the way the ingress builds one, so an offline pass means a
  production pass: header keys lowercased, credential headers
  (`authorization`, `cookie`, `proxy-authorization`, `x-api-key`) masked, and
  the engine-owned `_orion_errors` cleared. The credential list is now shared
  with the ingress rather than duplicated.

- **A recorded connector-call log** ([#283]) — a stub answers a call and
  nothing sees what the task tried to send, so a stubbed `mongo_write` was
  unobservable. Every connector-backed call is now recorded with its payload
  resolved the way the real handler resolves it, driven off the schema
  registry's `resolvable` flags, so a new function is covered as soon as it
  fills in its field table. `crypto`, `jwt_sign` and `jwt_verify` are excluded:
  they run for real offline and their inputs can carry key material.

  Two ways to assert on it. `expect_calls` matches per function, positionally,
  as a deep subset, with the call count checked — so an unexpected extra write
  fails and `"publish_kafka": []` asserts nothing was published. Presence is
  strict there, unlike `expect`: `"revokedAt": null` asserts *written as null*.
  `calls.<function>[i]…` paths in `expect` reach anything `expect_calls`
  cannot say.

  This is what makes the verbatim-JSONLogic bug visible: a connector field
  folds `{"var": …}` and nothing else, so `{"if": […]}` in a `document` is
  stored as a literal BSON object. The recorded call shows the object.

- **`expect_tasks`** ([#283]) — the ids of the tasks that ran, in order,
  matched exactly. Sourced from the execution trace rather than the audit
  trail, which cannot distinguish a condition-skipped task from an absent one.
  Unchecked when omitted.

- **`metadata.`, `temp_data.` and `audit_trail.` roots in `expect`** ([#283]),
  alongside `data.` and the new `calls.`, with array indexing
  (`calls.mongo_write[0]` or `calls.mongo_write.0`).

- **`lint --deny-warnings`** ([#283]) — `lint` now prints advisory findings on
  stderr and this flag makes them fail the command. One finding today:
  JSONLogic in a connector field that folds `{"var": …}` and nothing else, so
  the expression is written through as a literal. `POST /workflows/validate`
  reports the same findings in its `warnings` array.

  Advisory rather than an error on purpose. The operator vocabulary includes
  `length`, `type`, `in`, `keys`, `sort` and `map`, which are ordinary field
  names; a document that legitimately holds a stored rule is a real payload;
  and a hard error would refuse updates to workflows that have been serving for
  months.

- **`dry-run` prints the run's documents** ([#283]) — `data`, `metadata`,
  `temp_data`, `audit_trail` and `calls` — from the same builder the case
  runner reads, so they are the same set in the same shape a case's `expect`
  roots address and a path pastes across unchanged. `output` stays as an alias
  of `data`; CI `jq` filters read it.

### Changed

- **`package lint` and `lint <dir>` now share one implementation** ([#286]).
  The cross-reference pass lived in `package_cli::run_lint` because a promotion
  artifact was its only consumer. It is now `definitions::check` over a
  `DefinitionSet`, with the artifact and the directory as two loaders and
  `requires` generalised into a `Boundary`. A second validator beside the first
  is how the two containers come to disagree about what a valid set is.

  `package lint` gains the checks the artifact form never had — connector type,
  duplicate `route_pattern`, the unresolvable-JSONLogic advisory, and `env://`
  collection — and its findings now carry a stable `check` id and a severity,
  so a warning no longer has to be a failure or invisible.

- **BREAKING: an `expect` path must name its root** ([#283]). A leading `data.`
  used to be optional, which made the case file the only surface in Orion that
  accepted an unrooted path — every mapping `path` in every shipped workflow
  already spells one. The cost of the exception was silence: `metadata.foo` was
  read as `data.metadata.foo`, came back absent, and because an expected `null`
  matches an absent path, `"metadata.foo": null` *passed*. A typo'd root
  (`"dat.order.id"`) failed the same way.

  A path naming no root now fails the case before the workflow runs, with the
  fix in the message. To migrate a suite, prepend `data.` to every unrooted
  key:

  ```bash
  jq '.expect |= with_entries(
        if (.key | test("^(data|metadata|temp_data|calls|audit_trail)([.\\[]|$)"))
        then . else .key |= "data." + . end)' case.json
  ```

- **dataflow-rs 3.6 → 3.7, and Orion stops mirroring the engine.** 3.7 is "the
  host surface" release: it publishes the facts a service that stores,
  validates and operates workflow definitions previously had to re-derive. Six
  Orion mirrors are gone, each replaced by the engine's own answer.

  - **The authored step walk.** `engine/steps.rs` held its own traversal, its
    own group test and its own depth constant; it is now an adapter over
    `walk_authored_steps` / `is_group` / `MAX_GROUP_DEPTH`. The public shape
    (`walk_steps`, `leaf_tasks`, `Steps`) is unchanged, so no caller moved.
  - **The task-shape catch-all.** `validate_workflow_tasks_schema` ended in a
    round-trip `from_value::<Workflow>` that reported the parser's first
    failure against a bare `tasks` path. It now runs
    `Workflow::validate_authored`, which reports *every* remaining problem, each
    at the coordinate the author typed (`tasks[1].tasks[0].id`), and which runs
    `Workflow::validate()` as well as the parse.
  - **The handler-registry screen.** `check_custom_inputs` tested membership of
    a hand-kept name list, `match`ed the one handler with a typed `Input`, and
    compiled `channel_call`'s templates against a locally-built datalogic
    engine standing in for the crate-private `TemplateCompiler` — an
    approximation its own comment flagged. `Engine::check_workflow` does all
    three against the real registry and the real compiler.
  - **Rollout arithmetic.** The bucket-offset accumulator and its `!= 100`
    check are `Rollout::partition`, error direction included.
  - **The JSONLogic operator vocabulary.** `OPERATOR_NAMES` was 75
    hand-maintained names; `operators::operator_names()` asks a built engine,
    which since datalogic-rs 5.3 derives its core half from datalogic's own
    opcode table. The two agreed exactly at the swap. They would not have
    stayed agreed: the enumeration follows the features actually compiled in,
    including a family enabled by some other crate in the graph, which a typed
    list would have called typos.
  - **The retry loop.** `RetryPolicy` and `retry_with_policy` moved upstream
    verbatim — same fields, same capped exponential backoff, same whole-loop
    deadline that skips a backoff it cannot afford. Orion re-exports them. The
    classification (`DataflowError::retryable`) and the mechanism reading it
    now live together.

  Two lists Orion must still keep — `CUSTOM_HANDLER_FUNCTIONS` and the
  `/admin/functions` catalogue — cannot be derived, because both are consulted
  before an engine exists. They are now *pinned* instead: new tests walk a live
  `AppState`'s engine and assert set equality with `can_dispatch` and
  `dispatchable_functions` in both directions. That is the drift net #288 did
  not have.

- **`orion_workflow_duration_seconds{workflow}`** — per-workflow-run latency,
  from 3.7's `workflow_finished` observer callback. Subtracting the
  `orion_task_duration_seconds` sum for the same workflow gives the engine's
  own overhead: condition evaluation, group gating, loop bookkeeping, audit
  writes, arena management. That figure existed only as `workflow_overhead_ms`
  in the opt-in per-request profile — a residual got by subtraction, so it
  absorbed everything else unmeasured. This is a direct measurement on the
  always-on path. A workflow its condition or rollout gate rejected is not
  recorded; a looping workflow records once for the whole loop.

- **An offline run labels each recorded connector call with the task that made
  it.** `TaskContext::task_id` (3.7) is read as the call is recorded. It used
  to be attached afterwards by walking the execution trace and pairing
  recorded-function steps with recorded calls — best-effort by construction,
  since a run that died partway desynced the two sequences and the pairing
  bailed out rather than mislabel. `CallLog::correlate` and the task-to-function
  map the caller had to build for it are gone.

### Fixed

- **A workflow nesting task groups exactly 8 deep was refused at create**,
  with a message claiming the engine would refuse to build it. It would not:
  Orion counted nesting depth from 1 while the parser counts enclosing groups
  from 0, so the mirrored limit was one level tighter than the real one. The
  documented "groups nest, up to 8 deep" was correct and the code was not.
  Reading `MAX_GROUP_DEPTH` makes the two the same statement.

- **A workflow with `"tasks": []` was accepted at create and then broke the
  whole engine build on activation.** An empty task list parses fine and fails
  `Workflow::validate()`, which `Engine::build` runs fail-loud — so, like a
  duplicate task id, one such row took down every channel on every node rather
  than quarantining itself. `Workflow::validate_authored` runs `validate()`
  too, so it is now a 400 at create.

- **An `enrich`, `http_call` or `publish_kafka` task in a stored row is
  screened at load.** These deserialize into typed built-in variants, so they
  never reached the `Custom` arm the old screen inspected: a stored workflow
  naming one with no handler behind it built cleanly and then failed every
  request with `FunctionNotFound`. `check_workflow` reports it as
  `MissingHandler` and the channel is quarantined instead.

[#283]: https://github.com/GoPlasmatic/Orion/issues/283
[#285]: https://github.com/GoPlasmatic/Orion/issues/285
[#286]: https://github.com/GoPlasmatic/Orion/issues/286
[#288]: https://github.com/GoPlasmatic/Orion/issues/288
[#289]: https://github.com/GoPlasmatic/Orion/pull/289


## [1.1.0] - 2026-08-21

### Added

- **`crypto`** ([#259]) — digests, HMACs and password hashing as one operation
  envelope: `hash`, `hmac`, `hmac_verify`, `password_hash`, `password_verify`.

  Self-contained — no connector, no egress — so `orion-server dry-run` and
  `orion-server test` execute it for real rather than against a stub. The
  op × algorithm capability table is shared between execution and
  create/validate/lint, so a misuse like `password_hash` with `sha256` is
  unrepresentable rather than a runtime surprise. `hmac_verify` and
  `password_verify` compare in constant time, and a wrong secret answers
  `false` while a *malformed* stored hash or an undecodable signature is a task
  error — data corruption is never mistaken for a bad credential.

  `password_hash` runs argon2id (default) or bcrypt on the blocking pool with
  OWASP-default costs and bounded tuning; `password_verify` auto-detects the
  scheme from the stored hash's PHC prefix, which is also the rehash-on-login
  discriminator. Keys resolve through the connector secret vocabulary
  (`env://`, `vault://`) and never appear in context, errors or traces.
  Vector-tested against RFC 4231, RFC 2202 and the NIST digests.

- **JSONLogic encoding operators** ([#259]) — `base64_encode`/`base64_decode`,
  `base64url_encode`/`base64url_decode` and `hex_encode`/`hex_decode`,
  registered by Orion on **every** engine it builds: bootstrap, reload,
  dry-run, the `/test` endpoint, the guard engine and both validation-parity
  engines, so an expression means the same thing wherever it is evaluated.
  `base64url` is unpadded — the JWS form — and the decoders are strict about
  the alphabet while tolerating absent padding.

- **`http_call` gains `body_format` and `response_format`** ([#261]) — `json`,
  `form` or `text` on the way out and `json` or `text` on the way back, so an
  upstream speaking `application/x-www-form-urlencoded`, or answering with
  XML or CSV, no longer needs a workaround.

  Form encoding handles scalars, arrays as repeated keys, and skips nulls;
  nesting is refused at create time for a static `body` and per request for a
  `body_logic` one. `text` is string passthrough. Each format stamps its own
  content-type unless an explicit header names one — previously a task
  content-type was *appended alongside* the JSON stamp, putting two
  `Content-Type` headers on the wire. The connector test probe now captures
  text as well, so a healthy non-JSON `200` no longer fails its own probe.

- **The expression vocabulary is pinned to datalogic-rs 5.2** ([#266]) —
  `group_by`, `distinct` and `keys`/`values`/`entries` are documented and
  asserted against the engine, along with the IANA-zone argument on
  `format_date`/`parse_date`. The vocabulary test renders a UTC instant as
  `Asia/Kolkata` wall-clock, so a build that loses `chrono-tz` fails the suite
  rather than silently degrading to UTC.

- **`random`** ([#260]) — CSPRNG value generation in expressions, kind-selected:
  `uuid` (v4, or v7 for a time-sortable id), `digits` (an exactly-n-digit
  string with leading zeros kept — the OTP shape, and the full 10ⁿ space that
  integer bounds cannot express), `int` (inclusive, confined to ±2⁵³−1 so every
  JSON consumer sees exact values), `string` (named alphabets, or a custom set
  of 2–256 *distinct* characters — duplicates are refused because they would
  bias sampling) and `bytes` (hex, base64 or base64url through the same encoder
  as the operators above).

  Safe by engine structure rather than by convention: datalogic's optimizers
  classify custom operators as opaque and impure, so `random` is never
  constant-folded and never CSE-memoized — the same treatment `now` gets.
  Sampling is uniform through `rand`'s range machinery, so there is no modulo
  bias, and there is no seed parameter at any layer.

- **`smtp` connector and `send_email`** ([#262]) — a sixth connector type and
  one send function over it.

  The connector carries host and port, `tls` (`starttls`, `implicit` or `none`
  — rustls against the platform trust store, with deliberately no
  skip-verification knob, and `none` drawing a validation warning), `auth`
  (`none` or `basic`, secret references accepted), a default `from` behind an
  `allow_from_override` gate, `allow_private_urls` with the same posture as
  every other endpoint type — checked on the pool-open path — and `timeout_ms`.
  Transports pool in an `SmtpPoolCache` beside the SQL, Mongo and Redis caches
  and are evicted with them on connector change.
  `POST /connectors/{name}/test` probes connect, EHLO, TLS and auth through
  that same pooled transport without sending mail.

  `send_email` takes `to`/`cc`/`bcc` (bare or `Name <addr>` forms, with parse
  errors located per field and index), `subject`, `text` and/or `html` (both
  send `multipart/alternative`), `reply_to`, the gated `from`, and extra
  headers against a protected-name denylist — `List-Unsubscribe`-class headers
  in, `From`/`Subject`/`Content-Type`-class names refused at create **and** at
  send. The result is `{message_id, response}` with a client-generated
  `Message-ID` for correlation.

  **Deliberately no automatic retries.** A timeout after `DATA` is
  indistinguishable from an accepted message and SMTP has no idempotency key,
  so a retry is a duplicate email. The circuit breaker still applies through
  the usual connector shell.

- **`mongo_write` and `mongo_aggregate`, and extended-JSON values in the
  portable dialect** ([#263]) — the raw-native MongoDB surface reaches parity
  with the SQL one.

  `mongo_write` is the write twin of `mongo_read`: `insert_one`,
  `insert_many`, `update_one`, `update_many`, `replace_one`, `delete_one` and
  `delete_many`, with op-conditional field requirements and misplaced-field
  refusals at authoring time, per-op connector gates (`upsert: true` switches
  update and replace onto the upsert gate), the unfiltered-mutation double
  opt-in on every filtered op — the `_one` forms included — `write.max_rows`
  capping `insert_many`, and partial-batch classification: an ordered write
  reports applied, failed and never-attempted separately under a `207`.

  `mongo_aggregate` allowlists 29 read-only pipeline stages as data. `$out` and
  `$merge` sit behind a new per-connector `aggregate_write_stages` opt-in that
  defaults to **false** — the one deliberate default-deny — and stages are
  re-validated *after* `{"var"}` folding, so message data cannot smuggle one
  in. Results are bounded by `query.max_limit`.

  The portable dialect gains two backend-neutral values, `ObjectId` and
  `DateTime`, accepted from `{"$oid"}` and `{"$date"}` wrappers in filters,
  in-haystacks and `data_write` values. They lower to native BSON on MongoDB
  and to `FeatureUnsupportedByTarget` on SQL and Elasticsearch — parity or an
  error, never a silently different comparison. `{"param"}` composes inside a
  wrapper and a param value carrying a wrapper coerces, so `mongo_read` output
  round-trips into the next filter.

  `mongo_read` also gains projection, sort, limit and skip, bounded by the same
  query caps; an absent limit keeps the drain-with-cap contract.

- **Channel `auth.mode = "hmac"` generalizes to any webhook provider** ([#264])
  — a new provider is configuration, never code.

  The signing string is now a strictly-parsed template: literals plus `{body}`
  (required), `{header:<name>}`, and `{header:<name>:<key>}` for packed `k=v`
  headers — which is what makes Stripe's `t=<ts>,v1=<sig>` shape expressible in
  the flat config, with `signature_key` extracting every `v1` and each one
  tried. `algorithm` (`sha1`/`sha256`/`sha512`) and `encoding`
  (`hex`/`base64`/`base64url`, absent keeping the pre-1.1 auto-detection) are
  open value tables; `secrets` adds zero-downtime rotation with every candidate
  verified in constant time; and `timestamp` with `tolerance_secs` gives
  timestamped schemes a replay window checked *before* any MAC work, with
  either field alone refused as half a guard.

  Six presets — `zoom`, `slack`, `stripe`, `github`, `shopify`, `webex` — are
  pure data rows expanding to the explicit fields, which override them. Twilio
  is a named non-goal: its base string is a per-provider algorithm, not a
  concatenation.

  Request-time rules are pinned: one uniform `401` for every cause; a template
  header missing from the request **refuses** rather than substituting empty,
  so an attacker cannot shrink the signed message; an absent body still signs
  zero bytes. This is a strict generalization — an untouched config still means
  raw-body HMAC-SHA256 with auto-detected encoding, and every pre-existing auth
  test passes unchanged.

  Auth validation also moved earlier. The structural half of `CompiledAuth`
  compilation — everything except secret resolution, which stays at load time
  so bundles validate on hosts without production secrets — now runs at channel
  create/update/validate/import for **all** modes, so `api_key` without `keys`
  and `hmac` without a secret are `400`s naming the field instead of
  reload-time quarantines.

- **`storage` connector with `storage_presign` and `storage_head`** ([#265]) —
  a seventh connector type, under a scoping rule: **zero data path**.
  `storage_presign` is local SigV4 arithmetic over connector credentials and
  `storage_head` is one bounded signed `HEAD`; object bytes never move through
  the runtime.

  SigV4 is hand-rolled over the in-tree hmac/sha2 — no `aws-sigv4`, no SDK —
  and pinned by AWS's published vectors: the documented S3 presigned-GET
  example and the official suite's `get-vanilla` header case both verify byte
  for byte. Query and header signing share one canonicalization, and STS
  session tokens sign as `X-Amz-Security-Token`.

  The connector carries `provider` (tagged; `s3` today, GCS and Azure as future
  values), endpoint, region and bucket — bucket deliberately connector-owned,
  since the connector is the scoping unit and a second bucket is a second
  connector — credentials through the secret vocabulary, `force_path_style` for
  self-hosted stores, `allow_private_urls`, and `presign_get`/`presign_put`/
  `head` gates. The probe does one signed `HEAD` of the bucket.

  `storage_presign` covers GET — `response_content_type` and
  `response_content_disposition` ride the signed query — and PUT, with a signed
  content-type the uploader must match. `expires_in` takes seconds or
  `<n>s|m|h|d`, capped at S3's 7-day ceiling and checked at create time for
  literals. `storage_head` answers `{exists: false}` on a `404`, because
  absence is the data the function exists to report, while `403` and transport
  failures fail the task.

- **Channel `auth.mode = "jwt"`, plus `jwt_sign` and `jwt_verify`** ([#267]) —
  per-user identity reaches workflows without a header-forwarding proxy whose
  stripping rules Orion cannot validate.

  One verification core, three surfaces, RFC 8725 throughout: the mandatory
  algorithm allowlist is checked before anything else about a token, so
  `alg: none` and downgrades are unrepresentable; nothing from the header is
  trusted beyond `kid` routing; and HS secrets shorter than the hash length are
  refused per RFC 7518. The JWS set is HS/RS/PS 256–512, ES256/384 and EdDSA —
  ES512 deliberately absent, the library having no P-521. The JWKS cache is
  process-wide with a `Cache-Control` TTL clamped to [60s, 24h], single-flight,
  stale-serve (stale *public* keys never weaken verification) and a kid-miss
  refetch floored at 30s, so issuer rotation is invisible.

  On a channel, verified claims land at `metadata.auth.claims.*` — filtered by
  `claims_to_metadata`, all by default, since verified claims are not secrets
  from the workflow that admitted them — join the `validation_logic` context,
  ride the async queue and propagate through `channel_call`, so one request
  means one identity. `authorization_logic` evaluates over those claims after
  verification: falsy or an evaluation error answers `403 insufficient_scope`,
  because authorization is not validation and the wire should say so.
  `required: false` admits missing tokens identity-less while still rejecting
  invalid ones. `401`s carry `WWW-Authenticate: Bearer`, and **only expiry is
  named on the wire** — the one failure a client answers with a refresh; every
  other reason is uniform and typed only in `orion_jwt_rejections_total` and
  traces.

  `jwt_sign` stamps `iat`, requires a deliberate expiry (`expires_in` or an
  explicit `exp` claim), takes claims as a resolvable object, and resolves keys
  per call — never into context, errors or traces. `jwt_verify` shares the JWKS
  cache and returns typed, branchable task errors. OIDC flows and mTLS stay out
  of scope.

- **`oauth2` connector auth** ([#268]) — Orion acquires, caches, refreshes and
  persists the token itself, on `http` connectors (`es` refuses it at
  authoring).

  Grants are an open value set — `client_credentials` and `refresh_token`
  today, with jwt-bearer, token-exchange and device-code as future values on
  grant-agnostic lifecycle machinery. Provider quirks are additive fields:
  `client_auth` (`basic` or `body`, per RFC 6749 §2.3.1), `audience`,
  `resource`, and `extra_params` with reserved-name refusal. ROPC is
  deliberately absent.

  The lifecycle lives on the connector registry beside the circuit breakers:
  lazy acquisition, a per-connector cache fingerprinted against the auth block
  so editing the connector invalidates state — the burned-token recovery story
  — `refresh_margin_secs` early refresh, single-flight per connector, RFC 6749
  form POSTs through the shared SSRF-pinned client, and `expires_in` defaulted
  conservatively and floored against stampedes.

  Rotation persists to a new `connector_oauth_state` table, encrypted at rest
  exactly like `config_json`, carrying the access token and expiry alongside
  the refresh token so cluster nodes adopt the winner's token: the refreshing
  node takes a job lease and the losers poll the state row instead of re-racing
  the rotation. The `connectors` table is never mutated; `env://`, `vault://`
  and literal seeds all persist identically; and deleting a connector removes
  its state row.

  Failures split by kind: an unreachable token endpoint is retryable and trips
  the breaker, while `invalid_grant` and `invalid_client` are non-retryable
  with a 30-second negative cache and the error code surfaced (the description
  only logged). A `401` from the API invalidates the cached token, so a
  revocation self-heals. `orion_oauth_token_requests_total{connector, outcome}`
  counts every request, and `POST /connectors/{id}/test` acquires a real token,
  validating the whole setup.
- **`rate_limit.key_headers`** ([#275]) — a per-channel list of extra request
  headers `key_logic` may read, making per-device, per-partner and
  per-API-client limits expressible for the first time. The list is **merged
  with** the built-in eight rather than replacing them, so no stored
  `key_logic` changes meaning, and names are matched case-insensitively.
  `key_headers` joins `(requests_per_second, burst, key_logic)` in the
  limiter-reuse identity, so editing it rebuilds the limiter instead of
  carrying per-key state across a re-dimensioning.

- **`metadata._orion_errors`** ([#280]) — the **code** of each failed task,
  readable by later task logic under `continue_on_error`.

  A task's failure *reason* was unreachable by any spelling: the JSONLogic
  context is exactly `{data, metadata, temp_data}` and `Message::errors` is a
  private sibling field, so `{"var": "errors"}` resolved to nothing. The only
  branching signal was `metadata.progress`, which is a single slot overwritten
  by every subsequent task and hard-codes `status_code: 500` on failure — so
  `HTTP_ERROR`, `IO_ERROR`, `TIMEOUT_ERROR` and an open circuit were
  indistinguishable, which is exactly the transport-vs-protocol distinction a
  migrated error contract needs.

  Records carry `{workflow_id, task_id, code, status}` — **never a message**.
  That is the load-bearing constraint, not a detail: a task error's message can
  embed the upstream URL and response body, and the only thing keeping that
  from an anonymous caller is `sanitize_errors` plus the production refusal of
  `verbose_errors`. A workflow-visible message would route around both in one
  `map` task, since `data` is returned unsanitized.

  The key is cleared at every ingress and reset on `channel_call`, because the
  `_orion_` prefix is a naming convention rather than an enforced namespace —
  a caller can supply `_orion_call_depth` today and nothing strips it. So a
  caller cannot pre-seed failures, and a called channel never reports its
  caller's.

  Also fills two pre-existing documentation gaps the work surfaced:
  `metadata.progress` was an intentional, readable branching surface that was
  entirely undocumented (and contradicted by the workflows reference), and
  **task-level** `continue_on_error` was absent from the task field table
  despite being supported.

- **`server.data_mounts`** ([#279]) — serve the data plane at additional path
  prefixes, so deployed clients calling legacy paths no longer need a reverse
  proxy whose only job is to prepend `/api/v1/data`.

  `route_pattern`s were already multi-segment and unrestricted, so
  `"/zoom/meetings/user"` was a legal pattern before this — only the mount
  point was missing. **Additive, never a movable prefix**: `/api/v1/data` stays
  mounted, so every existing client, `orion-cli` command and MCP data call
  keeps working, and no `orion-client` change was needed.

  Two security fixes ship with it, because a mount changes what `MatchedPath`
  reports. Under a root mount it becomes the literal `/{*path}`, which does not
  start with `/api/v1/admin` — so **admin auth would have waved
  `/api/v1/admin/<anything-unregistered>` through to the unauthenticated data
  plane**, letting a channel claim such a path and be served anonymously; and
  the rate limiter would have classified root-mounted data traffic as
  `Operational`, metering it against the default budget instead of
  `rate_limit.endpoints.data_rps`. Both now resolve against the raw URI when
  the matched path is a data catch-all, and the self-authenticating
  single-trace read keeps its carve-out.

  A mount cannot claim a platform route — `/api` (covering the admin plane, the
  data plane, the OpenAPI document and any future `/api/v2`), `/health`,
  `/healthz`, `/readyz`, `/metrics`, `/docs` — nor nest inside another mount,
  which would be a router-construction panic. The literal `"/"` is accepted as
  an explicit escape hatch, with a startup warning: it re-opens the hazard that
  a future platform route could shadow a channel already serving that path.
  Channel activation refuses a channel whose served path would fall under a
  platform route, so that hazard is reported rather than silent.

- **`mongo_write.array_filters`** ([#274]) — `$[identifier]` array-element
  updates for `update_one`/`update_many`.

  Scoped honestly: the single-element case does **not** need it. `$` — the
  positional operator — already updates the first element the filter matched,
  atomically and in one round trip, because Orion never inspects the nested
  field paths inside `$set`. `$[]` (every element, unconditionally) works too.
  `array_filters` is for what neither can express: updating **every** element
  matching a predicate, reaching nested arrays (`$[a].items.$[b]`), and using
  several independent identifiers in one update. The `$` and `$[]` forms are
  now documented alongside it, so it does not get cargo-culted into places
  where `$` was simpler and already worked.

  Most of the value is in the validation. A server-side rejection — MongoDB's
  genuinely useful *"No array filter found for identifier 's' in path
  'sessions.$[s].active'"* — reaches the author as an opaque **500
  `ENGINE_ERROR` with the text discarded**, because a driver error becomes
  `function_execution` and the catch-all arm replaces the message. Orion now
  cross-checks the update's `$[identifier]` paths against the declared filters
  *before* the driver call, so an identifier with no filter, a filter nothing
  uses, an empty list, or `array_filters` with no `$[identifier]` anywhere is a
  `400` naming the problem — and, being in `validate_static_input`, is also
  caught at workflow create/import and by `orion-server lint`.

- **Channel `response.error_bodies`** ([#269]) — per-status replacement bodies
  for ingress guard rejections, for migrating an API whose deployed clients
  parse a different error shape and cannot be changed.

  It also finishes a job `response.mode = "shaped"` started. A shaped channel
  already replaces the envelope on the success path, including a `200`
  carrying task errors — but any `OrionError` returns `Err` from the handler
  and never reached shaping, so a shaped channel spoke its own dialect for a
  `200` and Orion's for a `401`. Nine response-shaping tests existed and none
  covered a guard rejection.

  **The platform still decides the status; the channel decides the bytes.**
  Keyed by HTTP status (plus an optional `"default"`), never by rejection
  *cause* — a uniform `401` is an anti-oracle, and keying by cause would
  rebuild exactly the credential oracle it exists to prevent. Placeholders are
  a closed set (`{status}`, `{code}`, `{message}`, `{request_id}`,
  `{channel}`, `{timestamp}`), all already on the wire, and an unknown one is
  refused at authoring time rather than shipped as a literal. `details` is
  deliberately absent.

  Error-owned headers survive the swap — `retry-after` on a `429`,
  `WWW-Authenticate` on a refused token — because the error builds its own
  response and only the body is replaced. Rejection metrics fire before the
  response is built, so an operator loses no visibility. An unrenderable
  template falls back to the platform envelope rather than 500ing, templates
  are capped at 4 KiB, and there is no JSONLogic: there is no engine at guard
  time, and evaluating expressions over attacker-influenced input on the
  cheapest-must-be path would be new attack surface for no gain.

- **Channel `request.cookies_to_metadata`** ([#270]) — an opt-in allowlist
  copying named request cookies to `metadata.cookies.*`.

  `cookie` is masked to `"******"` before request metadata is built, and that
  masking is right — the map is persisted verbatim into `traces.result_json`
  and `trace_dlq.metadata_json`. But the consequence was absolute: **no**
  cookie value could reach a workflow by any route, so a flow keyed on an
  opaque browser id — matching the cookie against records the workflow itself
  stores — was unbuildable end to end. Orion could already emit the
  `Set-Cookie`; it just could not read the value back.

  Scoped to opaque identifiers a workflow matches against its own state. A
  session token, JWT or CSRF token still goes through `auth.mode: "jwt"` with
  `source: {"cookie": …}`, where the token is consumed at verification rather
  than copied into the context. The raw header stays masked either way, and the
  default is nothing rather than everything — a cookie jar is unverified caller
  input, so defaulting to all of it would silently start persisting every
  visitor's session cookies into the traces of every existing channel.

  `metadata.cookies` is platform-reserved: stamped from the allowlist and
  **stripped** otherwise, so a caller cannot supply one in an envelope. That
  matters because `build_request_metadata` uses the caller's `metadata` as its
  base and `params`/`query` are stamped only when non-empty — the same shape
  for cookies would have been session forgery.

  Also replaces `JwtSource::Cookie`'s inline parser with one shared RFC 6265
  implementation, fixing two defects it carried: a quoted value (`name="abc"`,
  legal per §4.1.1) came back with its quotes, and `name = value` with spaces
  around the `=` did not match at all.

- **Channel `request.body_mode`** ([#278]) — opt out of envelope detection when
  a request model owns the name `data`.

  `auto` (the default, and today's behaviour) treats **any** object carrying a
  top-level `data` or `metadata` key as the Orion envelope: it takes that key
  as the payload and **discards every sibling field**, silently, with a normal
  `200`. No log, no metric, no trace annotation, and no way for a workflow to
  recover the dropped fields — the raw body reaches only HMAC signing, never
  the engine message. On a write endpoint that is data corruption the caller
  never learns about, and `data` as a top-level sibling is not an exotic name:
  it is the standard FCM/push payload shape. The same channel reached over
  Kafka got its whole body; over HTTP it was truncated.

  `"payload"` takes the parsed body verbatim. The two modes differ for exactly
  one input shape — a top-level object carrying those keys — so arrays,
  scalars and objects without them are unaffected. In `payload` mode a caller
  cannot supply `metadata` at all, which is a documented trade-off and a small
  security win: under `auto` a caller-supplied `metadata.params`/`.query`
  survives when the server has none of its own to stamp.

  Inert by default. Note that flipping a **live** channel changes its wire
  contract for any caller currently sending a legitimate envelope, and that
  `orion-cli send` wraps unconditionally, so it cannot address a payload-mode
  channel until `send --raw` lands — use `curl` for those.

- **`account_credentials` OAuth2 grant** ([#273]) — Zoom Server-to-Server
  OAuth, the grant Zoom moved every server-side integration to when it retired
  JWT apps in 2023. Exchanges `grant_type=account_credentials` plus a new
  `auth.account_id` field with Basic client auth.

  No workaround existed. A workflow can fetch a Zoom token, but cannot use it:
  `http_call`'s `headers` are static (`resolvable: false`, and no
  `headers_logic` exists in Orion or dataflow-rs), and `apply_auth`'s OAuth2
  arm deliberately sends nothing — the only runtime-token injection is driven
  by connector config.

  The grant inherits caching, the refresh margin, single-flight, the
  retryable/non-retryable failure split, 401 self-healing and the admin probe
  with no new code, and correctly gets no rotation persistence or cluster lease
  — it re-acquires from static credentials, so there is no state to keep.
  `account_id` is readable on admin reads: it is a tenant identifier, not a
  credential, and masking it would leave package diffs unable to say which
  account a connector talks to.

- **`http` connector `query_params`** ([#277]) — secret-resolvable, masked
  query parameters for APIs that authenticate in the query string (legacy
  SMS/telecom gateways, older payment and lookup APIs). It was the one
  credential shape with no safe home: `auth` offers Bearer, Basic, an arbitrary
  header and managed OAuth2, and `http_call` has no `query` field.

  The only prior option was baking credentials into the connector `url`, which
  fails two ways. Export masks a query value whose *name* looks secret (`pwd`
  is masked, `pass` is not) and re-import then refuses the mask, so the
  connector cannot be promoted between instances. And the resolved URL is
  interpolated into every timeout and failure message, reaching traces, the
  DLQ, logs, OTel spans, the trace read API — which a *caller* can reach with
  its own `x-trace-token`, not only an admin — and the admin connector probe's
  response body.

  Values are therefore applied at the request builder and **never merged into
  the URL**, so the SSRF-validated URL and every error message stay
  credential-free, the parameters cannot ride a cross-host redirect, and they
  are percent-encoded so a secret containing `&`, `=` or a space works. Stored
  sorted, because parameter order is observable on the wire and matters to
  signature-based gateways. A name already present in the connector URL's query
  is refused at authoring time rather than sent twice.

- **JSONLogic string operators `url_encode`, `url_decode` and `join`**
  ([#276]). Orion-registered like the encoding and randomness operators, so
  they are not gated by a cargo feature and work on every expression surface.

  `url_encode` closes a hole with no previous workaround: `http_call` has no
  `query` field, so an outbound query string is assembled with `cat` into
  `path`/`path_logic`, and nothing reachable from JSONLogic could
  percent-encode. Interpolating an unescaped value silently restructures the
  URL — a `&` ends the parameter, a `#` starts a fragment, a `+` is decoded as
  a space by any form-decoding server. It follows RFC 3986 (space is `%20`,
  `+` is `%2B`), reusing the same encoder as SigV4 signing so the two cannot
  disagree, and is stricter than JavaScript's `encodeURIComponent`, which
  leaves `!'()*` literal.

  `join` replaces an idiom that is verifiably wrong. `{"cat": [arr, "|"]}`
  appends the separator once at the end rather than between elements, and the
  `reduce`-with-sentinel alternative cannot distinguish "first element" from
  "first element is empty" — `["", "b"]` joins to `"b"` instead of `", b"`,
  which an author cannot fix without a second unrepresentable sentinel.
  `{"join": [arr, ""]}` is exactly `{"cat": [arr]}`, and
  `{"join": [{"split": [s, from]}, to]}` is literal-substring `replace`, which
  is why no `replace` operator was added.

- **`orion_rate_limit_key_unavailable_total{channel}`** — counts rate-limit
  refusals caused by an uncomputable key, as distinct from a caller being over
  its limit. The two demand opposite responses: over-limit is the control
  working, while an uncomputable key is a misconfiguration that disables the
  control for every caller, and it was previously indistinguishable inside the
  aggregate `orion_rate_limit_rejections_total`. A non-zero rate here always
  means a channel's `key_logic` or `key_headers` needs attention.

- **`[cors]` gains four settings** ([#271]) — `additional_allowed_headers`,
  `additional_exposed_headers`, `allow_credentials` and `max_age_secs`, with
  `ORION_CORS__*` env overrides and Helm values for each. Previously `[cors]`
  had exactly one key and the rest was hard-coded in Rust, so a browser client
  sending a custom request header (a `deviceid`, a tenant id, a trace header)
  could not pass preflight under **any** production-legal configuration, and
  credentialed cross-origin calls were not expressible at all.

  The two header lists **add to** the built-in sets rather than replacing them.
  A replacing key would let an operator adding `deviceid` silently drop
  `authorization`, `content-type` and `x-api-key` — breaking admin auth, the
  dedup guard and every browser JSON call, with no server-side error anywhere.

  Validation refuses the combinations that would otherwise **panic the process
  at boot**, since tower-http asserts on them inside `Layer::layer`:
  `allow_credentials` with a wildcard origin (also forbidden by the Fetch
  spec), and a literal `"*"` in either header list, which silently converts an
  explicit list back into a wildcard. An entry that is not a valid header name
  is refused at startup rather than dropped with a warning, and `max_age_secs`
  is capped at `86400` because browsers clamp it anyway.

### Changed

- **dataflow-rs 3.5.0 is the engine baseline** (from 3.3), carrying the opt-in
  error-context path `metadata._orion_errors` is built on. The context is
  bounded — `with_error_context_cap`, default 32 — so a looping workflow with a
  failing body cannot grow the trace quadratically, and `message`/`detail` are
  deliberately not recorded, because the context is serialized back to callers.

- **The SMTP stack moved from `lettre` to `mail-send`/`mail-builder`**, removing
  the tree's one bare-0BSD dependency. `quoted_printable` arrives through
  `lettre`'s `builder` feature, which `send_email` used heavily, so dropping the
  feature would have meant hand-writing RFC 5322 header folding, RFC 2047
  encoded-words, MIME boundaries and a quoted-printable encoder — reimplementing
  the code whose licence was the objection. The replacements are Apache-2.0 OR
  MIT throughout, so `deny.toml`'s allow-list is unchanged: the offending crate
  is gone rather than permitted, and the net crate count is flat.

  Address parsing, the one load-bearing capability `mail-builder` does not
  provide, is rebuilt on `mail-parser`'s grammar with a line-break refusal in
  front of it — the injection vector is a lenient parser that stops at the
  break. No behaviour changes for authors: `Name <addr>` forms, multipart
  bodies, custom headers and `Message-ID` correlation all work as before, and
  the in-process SMTP server tests assert it against a real listener.
- **A failed task reports why it failed in `errors[].code`** — where it used to
  report a flat `TASK_ERROR`. This is wire-visible: a client or alert matching
  `errors[].code == "TASK_ERROR"` on a `continue_on_error: true` channel stops
  matching.

  The codes are the ones [#280] made branchable, and they now reach the response
  as well as `metadata._orion_errors`. A connection that could not be
  established is `IO_ERROR`, a slow one `TIMEOUT_ERROR`, a request rejected
  before any socket was opened — SSRF protection, a closed operation gate —
  `FUNCTION_ERROR`, and a request shed by an open breaker carries the
  connector's own service kind, `circuit_open`, lower-case and verbatim.
  `TASK_ERROR` remains the fallback for an engine-owned error with no more
  specific classification, so it does not disappear from the vocabulary.

  Match on the specific code, or on the set, rather than on `TASK_ERROR` alone.
  Note this is only visible where the failure reaches `errors[]` at all: with
  the default `continue_on_error: false` the request still fails with the
  top-level error envelope and its own code, which is unchanged.

### Fixed

- **SQLite read-then-write transactions no longer fail under concurrent admin
  traffic** (D30). The four SQLite-reachable transactions that `SELECT` before
  their first write — workflow and channel activate, `update_rollout`, and
  `packages.put` — used sqlx's default deferred `BEGIN`. The first `SELECT`
  pins a WAL read snapshot, and when another connection commits before the
  transaction's first write — in production, the async audit-log writer
  draining the previous admin request's rows — the read-to-write upgrade fails
  with `SQLITE_BUSY_SNAPSHOT` (517), which `busy_timeout` never retries; or
  with a plain `SQLITE_BUSY` (5) while the writer still holds the lock, where
  SQLite skips the busy handler because waiting cannot make the snapshot
  fresher. Package apply surfaced this as an intermittent
  `500 STORAGE_ERROR`.

  Those transactions now begin `IMMEDIATE` on SQLite, taking the write lock at
  `BEGIN` so no stale snapshot can exist and contention degrades to plain
  `BUSY`, which `busy_timeout` absorbs. PostgreSQL and MySQL take row locks at
  first write and keep a plain `begin()`. Two concurrent activates now
  serialize at `BEGIN` instead of both reading the same actives list.
- **A browser preflight carrying `Authorization` no longer fails on the default
  config** ([#271]). `allowed_origins = ["*"]` — the shipped default — took the
  `CorsLayer::permissive()` branch, which emits a literal
  `Access-Control-Allow-Headers: *`. Per the Fetch Standard `Authorization` is a
  *CORS non-wildcard request-header name*: `*` never covers it, and it must be
  listed explicitly. So on a default install a browser calling the admin API
  with a bearer token failed preflight, while the named-origin branch worked
  because it listed `AUTHORIZATION` by name. The single end-to-end preflight
  test never sent `Access-Control-Request-Headers`, which is why it went
  unnoticed.

  Orion now sends the explicit allow-headers and expose-headers lists on
  **both** branches, never `Any`. This is a behaviour change on the default
  config and a strictly widening one — it authorizes everything `*` did, plus
  the header `*` silently withheld.

- **A `rate_limit.key_logic` that resolves to nothing no longer collapses every
  caller into one bucket** ([#275]). `key_logic` could only read eight
  hard-coded header names, and a reference to any other header was not an
  error: a missing path resolves to `null` in datalogic, and the guard
  serialized that into the key — so the bucket became the literal string
  `"null"` for **every** caller on the channel. An intended per-device or
  per-partner quota silently became one shared channel-wide bucket, with no
  log, no warning and no metric. A single typo (`deviceid` vs `device-id`) was
  enough.

  A key that resolves to `null` or an empty string is now refused with
  `429 RATE_LIMITED` (`RateLimitKeyUnavailable`), exactly as a key that fails
  to evaluate already was — the N5 rule that a request whose key cannot be
  computed is rejected rather than counted in the wrong bucket. This is what
  `docs/src/reference/channel-config.md` has always said happens.

  **Behaviour change on upgrade.** A channel with a typo'd header name
  previously appeared to work and will now begin refusing requests. That
  configuration was never enforcing the limit it declared — it was admitting
  unbounded traffic against a control that read as active — so the refusal
  surfaces a defect rather than creating one. To make it visible before
  traffic arrives, Orion now **warns at channel load** when a `key_logic`
  statically reads a header the key context will not carry, naming the channel
  and the header. Fix such a channel by adding the name to the new
  `rate_limit.key_headers` (below), or by correcting the path.

- **`build_url` no longer corrupts a connector URL that carries a query**
  ([#277]). Base and path were concatenated unconditionally, so
  `https://h/api?a=1` plus a task path `/orders` produced
  `https://h/api?a=1/orders` — the path spliced into the query value. The two
  query strings are now kept apart, base first and task query appended. None
  of the function's four tests used a base with a query, which is why it
  survived.

- **Connector `headers` values are masked whatever they are called** ([#277]).
  The masking allowlist is flat and keyed by leaf name, so a header whose name
  collided with a structural key was served **readable** — `headers: {"from":
  "x"}` among them, despite the module documenting header values as "all
  masked", and with no test covering it. `username`, `method`, `host`, `port`,
  `url`, `type`, `region`, `bucket`, `topic`, `resource` and `audience` would
  all have collided the same way. Every descendant of `headers`,
  `query_params` and `extra_params` now masks by container rather than by
  name; `env://` references still survive, so export → import is unaffected.

- **`jwt_sign` honors an explicit `claims.iat`** ([#272]). It was stamped
  unconditionally, silently discarding an author-supplied value — while every
  other registered claim the function touches (`iss`, `aud`, `nbf`, `exp`) is
  author-controllable. `iat` is the one with no dedicated input field, so
  nothing more specific existed to beat a claims entry and the entry should
  simply have won.

  Two consequences went with it. A revocation-pivot scheme — compare
  `claims.iat` against a stored `last_login_at` — could not forward-date a
  token minted in the same second as the pivot it must survive, pushing a
  tolerance into every consumer. And because `iat` moved every run, minted
  token bytes moved every run, so **no offline `orion-server test` case could
  ever assert a `jwt_sign` result** — a gap in Orion's own regression story for
  the flows JWT support was added for. A fully pinned claim set now mints
  byte-identical tokens.

  `iat` and `exp` supplied through `claims` are now also required to be numbers
  (NumericDate, RFC 7519 §2). `exp` previously got no such check, so a string
  date minted a token every verifier rejects later; both are refused at sign
  time instead.

- **`rate_limit.requests_per_second: 0` is refused at authoring time.** It was
  accepted and floored to `1` by the limiter, so asking for "admit nothing"
  quietly got one request per second. Stored channels are unaffected until
  rewritten.

### Security

- **Error messages no longer repeat a connector URL or an upstream error body
  verbatim** ([#281]).

  Every error `http_call` mints named the endpoint it failed to reach, and the
  non-2xx arm additionally copied up to `max_response_size` — 10 MB by default
  — of the *upstream* response body into the message. None of it passed through
  `redact_url_secrets_or_raw`, the helper that is otherwise the single home of
  the redact-or-verbatim policy. The strings are persisted to `traces` and
  `trace_dlq`, logged, and attached to an OTel span; the timeout arm also
  reached an unauthenticated caller in a `504`, which is not gated on
  `verbose_errors`. An async caller can read its own trace back with the
  `trace_token` from the `202`, so an admin credential was not the only key.

  URLs are now redacted at every site — userinfo password and secret-named
  query values — including the `url` span field and the `Invalid URL` message,
  which reaches the caller in a `400`. Transport errors additionally drop
  reqwest's own appended copy of the URL via `without_url()`; the same fix
  applies to `storage_head` and the Elasticsearch send path, which named the
  connector deliberately and had the URL put back underneath them.

  Upstream error bodies are now a 512-byte preview marked `… (truncated)`.
  This also bounds the row: a 10 MB error body no longer becomes a 10 MB trace.

  Redaction is name-keyed, so `?pwd=` masks and `?pass=` does not — it is a
  backstop, not the control. Keep credentials out of the URL with `auth` or
  `query_params`.

## [1.0.0] - 2026-08-14

**Highlights.** The promotion story: an `orion-server package` CLI
(export / lint / plan / apply / diff) over a new receipts API, upsert
import, activation pre-flight and deferred reload. Secret references
(`vault://`, `aws-sm://`, `gcp-sm://`, `env://`) resolve at load time so
stored configs never hold credentials. `orion-server preflight` scans a
stored estate for upgrade breaks before you upgrade. MySQL joins SQLite and
PostgreSQL as a runtime-selected backend, and multi-node cluster mode runs
on PostgreSQL/MySQL. Every workflow task is timed in Prometheus. The error
code surface was cleaned up before the 1.x line freezes it. The full record
follows.

### Security

- **`vault://` secret references resolve.** With the standard `VAULT_ADDR` +
  `VAULT_TOKEN` environment present, `vault://<api-path>#<field>` in a
  connector config or channel `auth` block reads HashiCorp Vault (KV v2 and
  v1 shapes) at load time — the stored config never holds the value, and a
  reference that cannot resolve quarantines its channel or fails its
  connector load rather than being used as the literal credential. The
  resolver trait went async for this; `aws-sm://`, `gcp-sm://` and
  `azure-kv://` remain fail-closed pending a dependency decision. (H3)

- **Optional encryption at rest for connector configs.**
  `storage.connector_encryption_key` (64-hex, `openssl rand -hex 32`; prefer
  the `ORION_STORAGE__CONNECTOR_ENCRYPTION_KEY` env form) makes the
  repository AES-256-GCM-encrypt `connectors.config_json` on every write and
  decrypt on every read — a database dump, replica or backup shows an
  `enc:v1:` envelope instead of credentials. Plaintext rows written before
  the key keep loading and re-encrypt on their next write; an encrypted row
  with no (or the wrong) key is a loud error, never served raw. (H3)

- **Read-only admin keys.** `admin_auth.read_only_api_keys` holds keys that
  authorise `GET`/`HEAD` on the admin plane only; every mutating method
  answers `403` without touching the failed-auth backoff. Same entry forms as
  `api_keys` (plaintext or `sha256:` digest), same strength rules, counted
  under `admin_auth_failures_total{reason="read_only_write"}`. Every key was
  previously a full superuser. (S13)

- **Secret masking fails closed.** Connector configs are masked by
  *allowlist*: only the structural vocabulary the connector types define is
  served readable, and every other value — including all `headers` values and
  any key no denylist anticipated — answers `"******"`. A drift test forces
  every new connector config field to be classified as readable or masked.
  Channel configs, previously returned verbatim, now mask `auth.keys` and
  `auth.secret`, with the full GET → edit → PUT round-trip: masked values
  restore from the stored config on update, and an unmatched sentinel is
  refused rather than persisted as the credential. `env://` references pass
  through unmasked on both surfaces. (H3)

- **Admin credential guessing is now metered and throttled.** The middleware
  stack was registered so that admin auth ran *outside* rate limiting; since it
  returns 401 without invoking the inner service, a wrong key never reached the
  limiter. The layer order is corrected, and failed admin authentication now
  applies a per-client exponential backoff (5 free attempts, then 500 ms
  doubling to a 30 s cap, cleared on success). Failures are counted by the new
  `admin_auth_failures_total{reason}` metric instead of the shared
  `orion_errors_total{reason="auth_failure"}`.
- **`fields` and `sort` no longer bypass the schema entirely.** `resolve_field`
  had exactly one call site — the filter lowerer — so the projection and sort
  keys reached SQL, MongoDB and Elasticsearch as **raw logical strings**. Three
  consequences, all silent:

  - With `{"secret": {"queryable": false}}`, `fields: ["secret"]` still emitted
    `SELECT "secret"` and returned the value. The allowlist the dialect
    documents protected the filter and nothing else. **A schema relying on
    `queryable: false` was not hiding the column from a projection.**
  - `sort` could order by a column the caller may not read.
  - A column rename applied to the filter and not to the projection, so
    `fields: ["email"]` against `{"email": {"name": "email_addr"}}` selected a
    quoted *literal* rather than the renamed column.

  The whole envelope — `fields`, `sort` and `include.fields` — is now resolved
  before any backend sees the spec, so no renderer can receive a logical name.
  Relation join keys resolve too (renames and identifier rules, deliberately
  not the caller-facing allowlist: they are operator-declared structure, not
  caller input), which fixes include grouping against a renamed key column.

- **Any workflow author could reach every table the connector's database user
  could see (breaking).** `UnmappedPolicy::Identity` was the default and both
  dialect handlers fell back to an empty registry when a task declared no
  `schema`, so every logical name in `data_query`/`data_write` resolved
  straight through to a physical one — read *and* write. The safe mode existed
  but was opt-in **per task**, so one forgotten `schema` key reopened the whole
  connector. **The default is now `reject`, and every 0.x dialect task fails at
  its first request** — nothing fails at startup, and stored workflows keep
  loading and activating, so this surfaces on live traffic. The error names
  both routes forward:

  ```json
  "schema": { "entities": { "orders": { "columns": { "id": {}, "total": {} } } } }
  ```

  declaring the entities and columns that task uses, **or** the one-line
  pass-through that restores pre-1.0 behaviour exactly:

  ```json
  "schema": { "unmapped": "identity" }
  ```

  Two further gaps closed with it. `reject` bounded only *columns*, so a query
  naming no fields at all — `{"source": "secrets"}`, which renders `SELECT *`
  and resolves nothing — reached any table even in allowlist mode, because no
  field resolution ever ran; an undeclared entity is now refused before any
  backend sees it. And nothing at the connector could stop a task opting itself
  back into identity mode, which `dialect.require_schema` now can (see Added).

  A relation's `to` target is deliberately exempt from the declaration
  requirement — like a relation's join keys, it is structure the schema's
  author wrote rather than caller input — but naming one of its columns is not.
  Combined with F27 below, that means an `include` over an **undeclared** target
  cannot plan at all under the default policy: the `sort` key it is now obliged
  to name is a column the allowlist refuses. Declare the target entity to use
  an `include` (F24, F27).

- **A read that named no `fields` bypassed the column allowlist entirely.**
  `fields: []` renders `SELECT *` — and no `_source` on Elasticsearch, no
  projection document on MongoDB — while name resolution only ever walked the
  columns a caller named. So with `{"password_hash": {"queryable": false}}`
  declared, `{"source": "users"}` still returned it: `queryable` meant "you may
  not *name* this column", not "you may not read it". This is the third and
  last place `queryable: false` was not hiding a column. A field-less read is
  now projected to the entity's declared queryable columns. An entity that
  declares no columns at all (relation-only or write-only) has no allowlist to
  apply and still reads every column; one that declares columns and marks every
  one non-queryable is refused rather than widened back to `SELECT *` (F24).

- **`returning` no longer bypasses the read allowlist.** `data_write`'s
  `returning` resolved through a helper that fell through to the raw column
  name regardless of policy, so `{"op": "insert", …, "returning": ["secret"]}`
  read back **any column the database user could see** — including one the
  schema declared `queryable: false`, and any column at all under
  `unmapped: "reject"`. The helper's doc comment justified skipping the
  *`writable`* check and silently skipped the allowlist with it. It now
  resolves through the same path as `filter`, gated on `queryable` (reading
  back a non-writable column stays legitimate). **A schema that relied on
  `queryable: false` to hide a column was not hiding it.**

- **Identifier validation is now one rule across the read and write paths.**
  The read path rejected empty and dotted names; the write path checked
  nothing; **neither rejected a leading `$`**. Three silent consequences:
  `{"field": "$where"}` in identity mode reached MongoDB as a raw document key
  (where `$`-prefixed keys are operators); `values: {"a.b": 1}` wrote a nested
  path on MongoDB but a literal column named `a.b` on SQL — one envelope, two
  meanings; and `values: {"": 1}` emitted `INSERT INTO "users" ("")`. A shared
  `validate_identifier` now runs wherever a logical name becomes a physical
  one, including rename targets and `physical` table names, and also rejects
  quote, escape and control characters as defence in depth around F25.

- **A filter matching every row no longer bypasses the `data_write` safety
  guard.** `{"op":"delete","target":"t","filter":{"and":[]}}` and other
  tautological filters skipped both the `"all": true` acknowledgement and
  `write.allow_unfiltered`, deleting every row. The guard now derives from the
  lowered condition rather than the presence of a `filter` key.

- **A many-to-many junction reached the SQL renderer unvalidated.**
  `resolve_relation` copied a junction's `table`, `local` and `foreign` names
  into the renderer without `validate_identifier` — the one identifier channel
  that skipped the boundary rule, so a schema carrying quote characters in a
  junction name reached `Alias::new` raw. The gap is closed, and identifier
  safety no longer rests untested on a transitive dependency: two property
  tests now fuzz every identifier channel of the write path (insert `target`
  and inserted column, update `set` column and `returning` name) and the read
  path (`source`, `fields`, `sort`, filter fields) with quotes, backslashes and
  unicode, asserting boundary rejection or safe quoting on all three SQL
  dialects (F25).

- **The SSRF validator never looked at the URL scheme.**
  `validate_url_not_private` parsed the URL and vetted every resolved address,
  so `gopher://public.example:70/` passed, and the `unwrap_or(80)` port default
  could pin the wrong `SocketAddr` set for a non-http scheme. Anything outside
  `http`/`https` is now rejected before any host or DNS work — *"only http and
  https are allowed"*. Every caller is an HTTP egress path, so nothing
  legitimate is lost (S7).

- **Connector secrets in URL query strings round-tripped in the clear.** URL
  redaction covered userinfo only, so `?api_key=SECRET` and friends came back
  verbatim from `GET /api/v1/admin/connectors`. Query parameters whose name
  satisfies the same secret-key predicate as object keys are now masked
  (`?api_key=…`, `?sig=…`, `?X-Amz-Signature=…`), and the denylist gains
  `bearer`, `dsn`, `webhook` (substrings) plus `pat` and `sig` (exact matches).

  Because one string can now carry several maskable positions, the mask
  round-trip guard is positional: each masked position — the userinfo password,
  each secret-named query value — is restored independently from the stored
  value, so rotating one in-URL secret while sending the other back masked
  restores the masked one instead of persisting `******` as the live
  credential. A masked position with no stored counterpart — including a
  literal `******` query value under a non-secret parameter name, which masking
  can never produce — is refused with `400` on create and update.

  **Still shown in the clear:** a capability token embedded in a URL *path* (a
  Slack-style webhook) under a generic key, because a path segment carries no
  name to judge. Store it under a secret-looking key (`webhook_url`) and the
  key-name rule masks the whole value (S18).

- **`/docs` and `/api/v1/openapi.json` were served unconditionally, to
  anonymous callers, in production (breaking).** Both endpoints are
  unauthenticated and the spec publishes the complete admin API surface — route
  shapes, request schemas, the `admin_auth.header` semantics — so every
  production deployment advertised it. The new `server.docs.enabled`
  (`ORION_SERVER__DOCS__ENABLED`) gates them: unset serves them only when
  `environment` is not a production variant (the same prefix rule that turns
  the admin-auth and CORS-wildcard checks fatal), an explicit `true`/`false`
  always wins, and disabled means the routes are not registered at all — `404`,
  not `401`, so their existence is not advertised. Production tooling that
  reads the served spec should set `server.docs.enabled = true` or switch to
  `orion-server dump-openapi`, which works offline regardless (S17).

- **A workflow could poison its own channel's dedup store and response cache
  (breaking).** Every `backend: "memory"` cache connector, the built-in dedup
  store and the response cache shared one in-process instance, so a workflow
  `cache_write` with a crafted `dedup:{channel}:{key}` key manufactured a `409`
  for a real request, a forged `cache:{channel}:{hash}` entry was served as a
  cached response, two memory connectors silently shared one keyspace, and a
  hot workflow cache evicted dedup entries out of the single shared LRU budget.
  In-memory backends are now distinct instances per purpose (workflow / dedup /
  response cache) and connector name, each with its own
  `engine.max_memory_cache_entries` budget.

  That makes the setting a **per-namespace** bound rather than a shared one —
  worst-case resident entries are `max_memory_cache_entries` × (2 built-in
  stores + up to 3 namespaces per memory connector) — so a memory-constrained
  host sized against the old single bound should divide the setting by its
  namespace count. Memory state never survived a restart, so migration is a
  no-op. Redis backends are deliberately *not* partitioned: they are external,
  shared across nodes, and legitimately read keys other systems wrote — use
  separate Redis databases where you need isolation (S19, N11).

- **The audit log could not tell two admin keys apart, and lost the last
  mutation before every restart (breaking).** The actor was the first eight
  characters of the presented key (or of its `sha256:` digest), so any
  generator with a fixed leader (`orion_sk_…`) collapsed every key into one
  indistinguishable actor — while, for a plaintext key, leaking eight literal
  characters of a live credential into a database table and every log sink
  downstream. Rows carried no IP, no user-agent and no
  request id, so a mutation could not be tied to the session that made it. And
  the write was a detached `tokio::spawn` that nothing awaited: a mutation
  accepted moments before `SIGTERM` was answered `200` and then never recorded
  — the last thing an operator did before a rolling restart is exactly the row
  an investigation wants.

  Audit v2 replaces all three. `audit_logs.principal` is now a derived,
  non-reversible `key-<16 hex>` — `SHA-256("orion:audit:key-id:v1" ‖
  SHA-256(key))` truncated to 8 bytes — stable across the plaintext and
  `sha256:` config forms. **Existing rows keep their old 8-character values, so
  a saved `?principal=` filter matches the old rows and nothing new**;
  recompute the id from your configured keys to map them back. `details` now
  carries `request_id`, `client_ip` (resolved with the
  `rate_limit.trusted_proxies` policy, so a forged `X-Forwarded-For` cannot
  dictate it) and a truncated `user_agent`. And the write goes onto a bounded
  queue (`audit.max_pending`) that one in-order writer drains at shutdown
  (`audit.drain_timeout_secs`, bounded so a stalled database cannot hold the
  process open). Any row that still does not make it is counted in
  `orion_audit_events_dropped_total{reason}` and logged at `error` — alert on
  that counter existing at all, not on a threshold (O7).

- **`POST /admin/workflows/{id}/test` now writes an audit event.** It reads as
  a harmless dry run and is not one: it executes the workflow's tasks against
  **live connectors**, so it will POST to real webhooks, write to real
  databases and publish to real topics. It emitted no audit event at all,
  making the most side-effecting call on the admin plane the one operation the
  trail could not show. The event (`action: "test"`) is recorded before
  execution, so the attempt is on the record even when the run itself fails.
  Audit-volume alerts will see traffic from this endpoint for the first
  time (O7).

- **Credential headers are masked before entering workflow metadata.**
  `authorization`, `cookie`, `proxy-authorization` and `x-api-key` arrive in
  `metadata.headers` as `"******"` — previously their plaintext values were
  persisted into `traces.result_json` (async) and `trace_dlq.metadata_json`.
  Header *presence* is still testable from `validation_logic`. If a channel
  used `rollout.sticky_header` with a credential header, switch to a
  non-credential header — all callers now hash to one bucket otherwise.
- **Trace reads no longer expose the submitter's request context.**
  `GET /api/v1/data/traces/{id}` strips `context.metadata` (the request
  header map) from the served message, and `GET /api/v1/data/traces` returns
  payload-free rows — `input_json`, `result_json` and `task_trace_json` are
  served only by the single-trace GET. Rows written before this release
  still hold plaintext headers at rest; the projection covers reads of them.
- **Async trace reads are scoped to the submitter.** The 202 from
  `POST /{channel}/async` now carries a one-time-shown `trace_token`;
  polling `GET /traces/{id}` requires it (`x-trace-token` header or
  `?token=`) or an admin credential. Update polling clients to pass the
  token. Sync traces and pre-upgrade rows keep the admin trust model.
  New migration adds `traces.access_token_hash` on all three backends.
- **`/health` serves topology detail only to authorized callers** when
  admin auth is enabled: anonymous callers get status, version, uptime and
  coarse per-component states; `git_hash`, `build_timestamp`,
  `workflows_loaded`, the circuit-breaker map, connector load failures and
  quarantined channels (names and reasons) require the admin key. With auth
  disabled the body is unchanged.

### Breaking

- **A channel name may belong to only one `channel_id`.** Create, update and
  import answer **409** when another channel's current version already holds
  the name, and activation refuses a name another *active* channel holds.
  Before 1.0 the collision stored cleanly and was resolved silently at
  runtime: the data plane and `channel_call` address channels by **name**, so
  one of the two won the registry slot and the other's requests ran the
  winner's workflow. Enforced at the repository rather than DDL — the
  invariant is per-id and MySQL cannot express the partial unique index.
  `orion-server preflight` gains a `channel-names` check that reports every
  pre-1.0 duplicate before an upgrade. (K7)

- **Channel activation refuses a missing or inactive workflow.**
  `PATCH /admin/channels/{id}/status` to `active` answers **400** when the
  channel's `workflow_id` is unset, names a workflow that does not exist, or
  names one with no active version — the gate the docs and the `/validate`
  warning always claimed existed. It used to succeed and quarantine the
  channel at the next engine load: the same outcome, discovered later, with
  no error to the caller. Activate in dependency order — connectors →
  workflows → channels — and use `?dry_run=true` on the same endpoint to
  pre-flight the gate without writing. (K8)

- **The `BAD_REQUEST` error code is retired; every 400 answers
  `VALIDATION_ERROR`.** Two codes existed for one condition, and which one a
  refusal carried was an accident of the internal error variant the code path
  happened to construct — validators mixed them freely, and two identical
  internal helpers existed purely to convert one into the other. The
  `OrionError::BadRequest` variant is deleted; messages and statuses are
  unchanged, and connector create/update refusals now carry the per-field
  `details[]` array the channel and workflow validators already produced.
  Clients branching on the literal `BAD_REQUEST` string must branch on
  `VALIDATION_ERROR` (or on the 400 status). (G11)

- **`RESPONSE_TOO_LARGE` answers 500, not 502.** The condition is a workflow
  result exceeding the operator's own `trace_queue.max_result_size_bytes` cap —
  no upstream is involved, so `502 Bad Gateway` was the wrong claim. The code
  string and message are unchanged; only the status moves. (G11)

- **Engine timeouts answer `TIMEOUT`, not `TIMEOUT_ERROR`.** A 504 carried one
  of two codes depending on which layer timed out: the channel guard said
  `TIMEOUT`, the engine said `TIMEOUT_ERROR`. A caller keying retry or paging
  rules on the 504 `code` should not have to know which layer fired, so both
  now answer `TIMEOUT`. The status and message are unchanged; clients matching
  the literal `TIMEOUT_ERROR` string must match `TIMEOUT` (or the 504 status).
  (G13)

- **Updating or activating an entity with no draft answers 404, not 400.** The
  no-draft miss was the one missing-row lookup in the admin API that answered
  `400 Bad Request`; every sibling answers `404`. All of them agree now, with
  the message unchanged. The repository layer was normalised with it (D22):
  every list filter is one DTO deriving `Deserialize + IntoParams`, so the
  trace, DLQ, audit-log and version-history query parameters appear in the
  OpenAPI document; connector listings accept `sort_by`/`sort_order`
  (previously hard-wired `name ASC`, which stays the default); the audit-log
  route's parallel string-typed query DTO is gone (timestamp parsing lives on
  the filter itself); version history pages through the same
  `limit`/`offset` filter as every other list; `ping()` moved off
  `WorkflowRepository` onto the pool, where the health probes always wanted
  it; and the previously untimed `list_versions`, `get_version` and ping
  paths now report to `orion_db_query_duration_seconds` (as
  `workflows.list_versions`, `channels.list_versions`,
  `workflows.get_version`, `channels.get_version`, `db.ping`).

- **1.0 ships no deprecated spellings.** Three compatibility shims that would
  otherwise have to be carried through the whole 1.x line are removed before
  the tag, and the versioning policy in `docs/src/reference/support.md` now
  says so under a new *Deprecations* section. The shims claimed three different
  lifetimes between them — "for one release", "removed in a later major", and
  nothing at all — and the first contradicted the same document's rule that
  breaking changes to workflow and channel definitions are reserved for a
  major. Removed:

  - `cors: { allowed_origins: [...] }` on a channel, superseded by
    `origin_allow_list`.
  - `backpressure.max_concurrent`, superseded by `max_concurrent_per_node`.
  - The flat `data_write` envelope, superseded by nesting it under `write`.

  All three are refused rather than ignored, which for the first two is the
  security-relevant outcome: an accepted-and-dropped `cors` key would leave the
  channel serving with no origin allow-list, and an accepted `max_concurrent`
  would admit N× the intended concurrency on an N-replica deployment. Both fail
  the config instead, quarantining the channel. `orion-server preflight` lists
  every affected stored entity before the upgrade.

  `http_call.response_path` remains accepted and is *not* a deprecation: that
  alias belongs to `HttpCallConfig` in dataflow-rs, which Orion does not own.
  It is documented under *Accepted alternate spellings*.

- **A channel config rejects keys it does not recognise.** `ChannelConfig` is
  `deny_unknown_fields`. Every key in that struct is a guard, so an
  unrecognised one is a guard that never runs — and because nothing
  re-serialises `config_json`, the mistake survived every reload: a stored
  `"deduplicaton"` meant no idempotency, no error, ever. `validate_channel_config_blob`
  called itself a strict validation while only checking the shape of the keys
  it recognised. The config file, the connector configs and both dialect
  envelopes (W5/W6) already rejected unknown keys; this was the last surface
  that did not. A stored channel carrying one is now quarantined at load —
  refused at every ingress rather than served with a guard quietly absent — and
  a create or update naming one gets a `400`. This is also the mechanism that
  refuses the two renamed keys above, and it retires `backpressure.queue_depth`,
  which the upgrade guide previously said could be left in place.

- **Task identity is validated at authoring time, not discovered at first
  request.** Every task must now carry a non-empty `id` and a `name` key, and
  ids must be unique within a workflow. All three were already hard requirements
  of the engine — `dataflow_rs::Task::id` and `::name` are required `String`s, and
  `LogicCompiler::compile_workflows` calls `Workflow::validate()`, which rejects
  duplicates — but nothing checked them until the workflow was loaded. Verified
  end to end, all three were accepted with a `201` and then failed later:

  | Authored | Create | What actually happened |
  |---|---|---|
  | task without `id` | `201` | `503` on every request; the channel is quarantined at load |
  | task without `name` | `201` | the same |
  | two tasks sharing an `id` | `201` | `500` on activate — **the entire engine reload fails**, and at boot `Engine::new` aborts the process |

  The duplicate case is the one that matters most: it is not contained by the
  per-channel quarantine, so a single repeated id takes down every channel on
  every node. All four authoring paths share one check
  (`validate_workflow_tasks_schema`), so create, update, bulk import,
  `POST /admin/workflows/validate` and the CLI agree, and the response names the
  offending `tasks[{i}].id`.

  These track dataflow-rs's parsing rules rather than tightening them, so
  "Orion accepts it" and "the engine can load it" stay the same statement. An
  **empty** `name` is still accepted, because it deserializes and runs — only
  the missing key is refused. `id` is the single deliberate step beyond the
  parse: `""` deserializes too, but it collides with any second blank id on
  `Workflow::validate()`, and even alone it writes an empty `task_id` into every
  trace step, audit entry and metric label.

  Nothing that currently serves traffic is affected — a workflow in any of these
  shapes is already failing. `PUT` on such a stored workflow now reports the
  problem instead of accepting the edit.

  The alternative was to have the engine generate ids for tasks that omit them.
  That was raised upstream as GoPlasmatic/dataflow-rs#35 and withdrawn: `task_id`
  reaches `metadata.progress`, which workflow conditions can read, so it is
  control flow rather than a log label — and a generated `task_3` that
  corresponds to nothing in the author's document turns a loud parse error into a
  quiet misattribution in traces and metrics.

- **dataflow-rs 3.0 → 3.3, and the workarounds it exists to remove are gone.**
  Six behaviour changes come with the 3.1 step, described below; the rest of
  the upgrade is internal. 3.2 added the `all-operators` feature — enabled
  here, which is what makes the `datetime`, `ext-string`, `ext-array`,
  `ext-math`, `ext-control` and `error-handling` operator families available
  to expressions — and 3.3 added the workflow `loop` recorded under Added.

  - **The versioned rollout traffic split now actually splits traffic.** This
    is a live defect being fixed, not a new feature. Each version's condition
    was wrapped with `{">=": [{"var": "_rollout_bucket"}, min]}` — a
    **context-root** lookup — while every ingress injected the key at
    `data._rollout_bucket`. Conditions evaluate against the whole
    `{data, metadata, temp_data}` object as root and datalogic's root-scope
    `var` has no scope fallback, so the lookup yielded null, null coerced to
    `0`, and `0 >= bucket_min` held only for the version whose range starts at
    zero. **100% of traffic went to the newest version regardless of the
    configured percentages**, and the only test covering it asserted on the
    shape of the generated condition without ever evaluating one. Routing is
    now `Workflow::rollout` matched against `Message::routing_bucket`, checked
    before any arena work and never touching the caller-visible `data`
    namespace. **A deployment relying on the observed (broken) behaviour will
    see traffic move to the percentages it configured.**
  - **`enrich` is refused at workflow create.** It is a dataflow-rs built-in
    *name*, but not a self-contained one: it deserializes into a typed built-in
    variant — so the engine accepts it and the custom-input check skips it —
    and then dispatches to a handler registered under the same name. Orion
    registers none, so such a workflow activated cleanly and failed **every**
    request with `FunctionNotFound`, forever. The function-name gate now keys
    on `BuiltinKind` instead of membership in a hand-copied list, so it asks
    whether this engine can run the task rather than whether the name is
    spelled correctly. Stored `enrich` workflows are unaffected until edited;
    they did not work before either.
  - **`http_call`, `enrich` and `publish_kafka` inputs reject unknown keys.**
    Upstream made those config structs `deny_unknown_fields`. A misspelled key
    previously parsed cleanly and was discarded — an `http_call` would make its
    request and silently throw the response away. Orion's own input schema now
    reports it at create, as a `400` naming the field, rather than letting the
    workflow activate and then quarantine its channel at engine load.
  - **`output` and `response_path` may not both be set on an `http_call`.**
    `response_path` is now a real serde alias upstream, which replaced the
    `output` → `response_path` rewrite Orion did while loading workflow JSON in
    a storage repository. An alias cannot express a precedence rule, so the
    documented "`output` wins" behaviour becomes a refusal — raised at create,
    naming both spellings. Either key alone works exactly as before.
  - **`POST /api/v1/admin/workflows/{id}/test` returns `200` with a partial
    trace where it used to return `5xx`.** The trace was built as a local and
    moved into the success arm, so a hard failure discarded every step already
    recorded — on the one endpoint whose purpose is showing them. The response
    now carries the steps that ran plus an `error` field. Note the failing
    task's own step is still absent: the engine propagates before appending it,
    so the trace ends at the last known-good step. `orion-server dry-run`
    prints the same partial trace and still exits non-zero, so it remains
    usable as a CI gate.
  - **Metadata keys are taken literally.** Request metadata was seeded one
    `set_nested_value("metadata.{key}")` per key, which re-read each key as a
    *path*: a caller-supplied `"a.b"` became nested `metadata.a.b`, and `"#20"`
    became `metadata["20"]`. Seeding through `MessageBuilder` keeps them flat.
    This changes the shape of `context.metadata`, of `traces.result_json`, and
    of what `{"var": "metadata.a.b"}` resolves to, for callers that send dotted
    metadata keys.

- **Every connector's endpoint is now scheme- and address-checked, not just
  `http` (S6).** Until now `validate_url_not_private` was called from exactly
  two places — the HTTP handler and the Elasticsearch helper — so no db, cache,
  mongo or kafka path checked anything, and a connector holding
  `connection_string: "postgres://…@169.254.169.254/…"` was accepted and
  dialled. Two layers close it:

  - **A scheme allow-list at create/update.** `db` accepts `postgres`,
    `postgresql`, `mysql`, `mariadb`, `sqlite`, `mongodb` and `mongodb+srv`;
    `cache` (redis) accepts `redis` and `rediss`; `es` accepts `http` and
    `https`; Kafka `brokers` must be bare `host:port`, not URLs. Schemes only —
    storing a connector never depends on DNS. **A stored connector using
    anything else is refused the next time it is created or updated**, and an
    existing one keeps loading until then.
  - **A private-address check when the connection is first opened**, with the
    same `allow_private_urls` opt-out `http` and `es` already had, now also on
    `db`, `cache` and `kafka`. Skipped where there is no address to judge:
    `sqlite:` opens a file and `backend: "memory"` opens nothing. MongoDB is
    checked against the hosts the driver resolved, so replica-set URIs are
    checked host by host and `mongodb+srv://` after its SRV lookup.

  **Most deployments will need to set `allow_private_urls: true` on their
  database and cache connectors**, because those normally *are* on a private
  network — that is the intended outcome, so that reaching an internal address
  is a stated decision rather than the default. Symptom if you miss one: the
  connector stores fine and the first request through it fails, naming the
  address and the flag.

- **A REST channel's `route_pattern` and `methods` must now be well-formed.**
  They were checked for non-emptiness and nothing else, so
  `methods: ["POTS"]` and `route_pattern: "orders/{id"` were created,
  activated and reloaded — and then never matched a request, with nothing
  reporting it. The channel was simply dead.

  A pattern must start with `/`, have no empty segments, and write each
  parameter as a whole `{name}` segment with a valid identifier and no
  duplicates; a method must be one Orion can route (`GET`, `POST`, `PUT`,
  `PATCH`, `DELETE`, `HEAD`, `OPTIONS`). Checked on **update** as well as
  create, and every problem comes back in one response. Existing active
  channels are not re-validated; you meet this when you next edit one.

- **A channel cannot be activated onto a route another active channel already
  claims.** Two channels declaring `GET /orders/{id}` resolved by database row
  order: which one served could differ between nodes and change on any reload,
  and the loser's declared path silently ran the winner's workflow. The
  incumbent wins, so activating a channel can never take a running one down.
  Parameter names are not part of the match — `/orders/{id}` and
  `/orders/{order_id}` collide. Rows stored before this resolve
  deterministically now and log a warning naming both sides.

- **`POST /api/v1/data/{channel}/async` always returns a usable `trace_id`.**
  With `trace_storage.mode = "off"` it used to mint a throwaway UUID, answer
  202 with `{"trace_id": null, "trace_token": null}` plus a `Warning: 299`
  header, and enqueue the work anyway — a receipt whose documented follow-up
  (`GET /admin/traces/{id}`) was structurally impossible. Appending `/async`
  *is* the request for a result to be fetched later, so the trace row is
  written before the 202 and the worker persists the outcome. `off` still
  applies in full to the synchronous endpoint, where the caller already has
  the answer. `trace_id` and `trace_token` are now required in the schema and
  the `Warning` header is gone.

- **`_orion.profile` is now `version: 2`.** See below.

- **`POST /admin/workflows/import` reports per-item failures instead of
  aborting.** One malformed item used to abort the whole batch with a 400,
  while the identical mistake against `/channels/import` or
  `/connectors/import` produced one failed entry and imported the rest —
  three endpoints, one documented request shape, two behaviours. All three now
  share one driver.

- **`POST /admin/workflows/validate` field paths are the create-path ones.**
  `name` is now `workflow.name`, and so on: the endpoint runs
  `validate_create_workflow` itself rather than a parallel re-implementation,
  which is what makes `valid: true` mean "create would accept this". See
  Fixed.

- **Entity ids that collide with a static admin sub-resource are refused.**
  `import`, `export`, `validate`, `versions`, `status`, `rollout`, `test`,
  `circuit-breakers`, `purge`, `requeue` and `reload` cannot be used as a
  workflow, channel or connector id: those paths sit alongside `/{id}`, so an
  entity named `import` was unaddressable and `DELETE /admin/workflows/import`
  audit-logged a delete of nothing.

- **A bare JSON object is now accepted as the data-plane payload.** The
  endpoint had three behaviours for three body shapes and documented one:
  `{"data": …}` was the envelope, an empty body became `{"data":{}}`, and
  `{"amount": 5}` — the obvious thing to send — failed with *missing field
  `data`*. One rule now: an object carrying `data` or `metadata` is the
  envelope, anything else is the payload. Strictly widening; previously-400
  requests now succeed.

- **`retry` is gone from the `db` and `es` connector configs.** It was declared,
  validated and documented on both, and the only reader of a retry policy is
  `http_call` — so `{"type":"db", …, "retry":{"max_retries":5}}` did exactly
  nothing while the field table promised "retry with exponential backoff".

  It is not coming back as a working field: a database or `_bulk` call that
  timed out may already have been applied, so a blind re-send duplicates it —
  the same hazard that made `http_call` retry idempotent methods only. Bound
  those calls with `connect_timeout_ms` / `query_timeout_ms` /
  `request_timeout_ms`, and let the circuit breaker shed load from a dependency
  in trouble. **Stored connectors carrying the key still load**; it is ignored,
  as it always effectively was.

- **A workflow cannot activate against a connector of the wrong type.**
  Activation checked only that the referenced connector *existed*, so a task
  pointing `cache_read` at a `db` connector — or `publish_kafka` at anything
  that is not `kafka` — activated cleanly and then returned 500 on its first
  request. Each function now declares the connector types it can run against,
  and a mismatch is a 400 at `PATCH /admin/workflows/{id}/status` naming the
  function, the connector and what was required.

  The same check covers the one cross-field rule the static schema cannot
  express: `data_query` / `data_write` against a **MongoDB** connector must set
  `database`, because a Mongo connection string carries no default one. The
  field stays optional in the schema — the identical task shape is valid against
  SQL and Elasticsearch — and is required once the connector is known.

- **Renaming a connector is refused while an active workflow references it.**
  Workflows bind connectors by name, and nothing tied the two together: a rename
  left every referencing workflow resolving to nothing, which is a 500 per
  request with no error at rename time. Repoint or archive those workflows
  first. Pool eviction now covers both the old and the new name, so the old
  entry no longer holds TCP connections against the remote database's
  `max_connections` until the LRU happens to reclaim it.

- **`_orion.profile` is now `version: 2`.** `handlers[].nested` lists only the
  calls that actually ran inside that `channel_call`; v1 attached every nested
  sample to every top-level one, so a workflow fanning out to two channels
  reported each one's children under both. A call with no children now omits the
  key rather than emitting an empty array. Branch on `version`.

- **`data_write`'s mutation envelope is nested under `write`,** mirroring
  `data_query`'s `query`. It used to be flat: `op`, `target`, `values`, `set`,
  `filter`, `on_conflict`, `returning` and `all` sat alongside the handler's own
  `connector`, `schema`, `params`, `database` and `output`. So the two halves of
  one dialect read differently, the envelope could never grow a field named like
  any of those five, and there was no single JSON value that *was* the envelope
  for validation, logging or a builder UI.

  ```jsonc
  // before                                  // after
  { "connector": "db",                       { "connector": "db",
    "op": "update", "target": "users",         "params": { "id": {"var": "data.id"} },
    "set": { "status": "off" },                "output": "data.w",
    "params": { "id": {"var": "data.id"} },    "write": {
    "output": "data.w" }                         "op": "update", "target": "users",
                                                 "set": { "status": "off" } } }
  ```

  **The flat form is not accepted.** `write` is a required input, so a task in
  the old shape is refused at create, update, bulk import,
  `POST /admin/workflows/validate` and `orion-server lint`, naming `write`;
  `orion-server preflight` lists stored tasks still using it. Validation errors
  are reported under `…function.input.write.<field>`. Stale flat keys left by a
  half-finished migration are inert.

- **Four config sections renamed, and audit-log retention split out of
  `[queue]`.** Each of these cost a paragraph of documentation to explain what
  the key actually did:

  | Pre-1.0 | 1.0 | Why |
  |---|---|---|
  | `[queue]` | `[trace_queue]` | It only ever configured the async *trace* queue |
  | `queue.trace_retention_hours` | `trace_queue.retention_hours` | Prefix redundant inside the renamed section |
  | `queue.trace_cleanup_interval_secs` | `trace_queue.cleanup_interval_secs` | Drove both cleanup jobs, named for one |
  | `queue.audit_retention_days` | `audit.retention_days` | Audit rows have nothing to do with the trace queue |
  | — | `audit.cleanup_interval_secs` (new, default `3600`) | Audit cleanup no longer borrows the trace job's cadence |
  | `[channels]` | `[channel_filter]` | It selects which channels to load; it does not configure channels |
  | `[tracing.storage]` | `[trace_storage]` | `[tracing]` is OTLP export; this is Orion's own trace rows — two unrelated concerns under one section |
  | `ORION_ENV` | `ORION_ENVIRONMENT` | The last name breaking the `ORION_` + field-path rule |

  Environment variables follow their keys: `ORION_QUEUE__*` → `ORION_TRACE_QUEUE__*`,
  `ORION_CHANNELS__*` → `ORION_CHANNEL_FILTER__*`, `ORION_TRACING__STORAGE__*` →
  `ORION_TRACE_STORAGE__*`.

  **A retired variable is a startup error, not a silent no-op.** Overrides are
  matched by name rather than deserialized, so `deny_unknown_fields` cannot see
  them — a renamed section would otherwise leave `ORION_QUEUE__WORKERS` set and
  quietly ignored. For `ORION_ENV` that would have been a security regression:
  falling back to `development` turns the production admin-auth and wildcard-CORS
  checks from startup errors back into warnings. Orion now refuses to boot and
  names every offender at once. Retired *file* keys are caught by
  `deny_unknown_fields`.

- **One response envelope across the admin plane.** Every admin 2xx body now
  carries its payload under a top-level `data` key; list endpoints add `total`,
  `limit` and `offset` alongside it and nothing else — bar the trace list, whose
  deviation is the next entry. Three envelopes used to
  coexist, and ten handlers returned their fields bare at the top level:

  | Endpoint | Was | Now |
  |---|---|---|
  | `GET /admin/engine/status` | `{version, uptime_seconds, …}` | `{"data": {…}}` |
  | `POST /admin/engine/reload` | `{reloaded, workflows_count}` | `{"data": {…}}` |
  | `GET /admin/connectors/circuit-breakers` | `{enabled, breakers}` | `{"data": {…}}` |
  | `POST /admin/connectors/circuit-breakers/{key}` | `{reset, key}` | `{"data": {…}}` |
  | `POST /admin/trace-dlq/purge` | `{purged, older_than_hours}` | `{"data": {…}}` |
  | `POST /admin/workflows/{id}/test` | `{matched, trace, output, errors}` | `{"data": {…}}` |
  | `POST /admin/workflows/validate` | `{valid, errors, warnings}` | `{"data": {…}}` |
  | `POST /admin/{workflows,channels,connectors}/import` | `{imported, failed, errors}` | `{"data": {…}}` |
  | `GET /admin/traces/{id}` | bare trace object | `{"data": {…}}` |

  `POST /admin/backups` and `GET /admin/functions` hand-rolled the `{"data": …}`
  wrapper and are unchanged on the wire. `GET /admin/traces` hand-rolled the
  pagination envelope and keeps its shape, but **not its fields** — see "The
  trace list drops `total` unless you ask for it" below. All three now go
  through the shared helpers, so they cannot drift again.

- **The trace list drops `total` unless you ask for it, and offers a cursor.**
  `GET /api/v1/admin/traces` returned `{data, total, limit, offset}` on every
  page, and `total` cost a `COUNT(*)` over the whole filtered set — a full scan
  on PostgreSQL and InnoDB, paid per page on the largest table in the schema
  whether or not the caller read the number. **`total` is now absent from the
  body unless the request carries `?include_total=true`**; anything doing
  `.total` gets a missing key, and nothing errors. `data`, `limit` and `offset`
  are exactly where they were.

  A page in the default `created_at` ordering also carries `next_cursor`; pass
  it back as `?cursor=` to fetch the next page with no `OFFSET` for the
  database to count past, so page 500 costs what page 1 costs. Its absence is
  how you know you have reached the end, and its value is **opaque** — the
  encoding is not part of the contract. `cursor` is refused with a `400`
  alongside a non-zero `offset` (two paging modes, pass one) or with `sort_by`
  set to anything but `created_at` — `updated_at` is rewritten in place by
  every status change, so a cursor over it would silently skip rows. `?offset=`
  still works exactly as before for every sort column.

  **This is the one list endpoint that deviates from the shared pagination
  contract**; every other one still returns `total` unconditionally (D8).

- **Bulk import returns the same four fields whether or not it is a dry run.**
  `?dry_run=true` used to answer with six fields for two facts —
  `would_create` and `would_fail` next to a hardcoded `imported: 0` and a
  `failed` that always equalled `would_fail`. Both modes now return
  `{dry_run, imported, failed, errors}`; in a dry run `imported` is the count
  that *would* be created rather than a constant 0. Read `dry_run` to tell the
  modes apart.

  Unchanged: all three imports still return **200** even when every item
  failed. Callers must check `failed`, not the status code.

- **The trace read endpoints moved to the admin plane:**
  `GET /api/v1/data/traces` → `GET /api/v1/admin/traces`, and
  `GET /api/v1/data/traces/{id}` → `GET /api/v1/admin/traces/{id}`. **No
  redirect** — the old paths now resolve as channel names, so a stale client
  gets 404.

  The list was already admin-guarded, so its placement on the data plane was a
  naming lie — and a functional one: both were static routes, which axum
  resolves ahead of the `/{*path}` catch-all, so **a channel named `traces` was
  permanently unreachable** (`POST /api/v1/data/traces` returned 405) with no
  reserved-name check to explain it. The data plane is now a single catch-all
  and the rate limiter's `traces` special case is gone.

  Access rules are unchanged. `GET /api/v1/admin/traces/{id}` still accepts
  *either* an admin credential or the submission's `trace_token`, making it the
  one path under `/api/v1/admin` exempt from the blanket admin guard.
- **Seven connector config fields that were never read are removed:**
  `db.driver`, `db.auth`, `cache.default_ttl_secs`, `cache.max_connections`,
  `cache.auth`, `cache.retry`, and `kafka.group_id` (the connector one — the
  `[kafka] group_id` server setting is unaffected). Each was accepted,
  validated, persisted, returned by `GET /connectors`, and documented with a
  default. `db.driver` was the worst: it looked like the thing that selects the
  backend and is not, so `driver: "mysql"` with a `postgres://` URL connected
  to Postgres.

  **Stored connector configs keep loading** — connector configs do not use
  `deny_unknown_fields`, so a 0.3.x row carrying these keys deserializes fine
  and they are ignored, exactly as they always effectively were. Nothing to do
  on upgrade; delete them from your configs at leisure. Credentials go in
  `connection_string` / `url`; cache TTL is per-`cache_write` via `ttl_secs`.
- **The `storage` connector type is removed.** It was accepted, validated,
  persisted and listed by `GET /connectors` for the whole 0.x line with no
  handler behind it — `POST /connectors` returned 201 and every workflow
  referencing the connector failed at request time. The documentation
  advertised S3, GCS and local-filesystem support with a full field table for
  something that did not exist; that section is gone.

  `connector_type: "storage"` is now rejected at create. An existing stored row
  is reported as a connector load issue (`stage: "removed_type"`) naming the
  removal, visible on `/health` and `GET /api/v1/admin/connectors` — and fatal
  at boot when `engine.fail_on_connector_load_error = true`. **Delete or
  disable such connectors before upgrading.** Nothing that worked stops
  working: there was never a working configuration to preserve.
- **An unknown key in the config file is now a startup error.** Every config
  struct was `#[serde(default)]` with no unknown-field rejection, so
  `[server] wrokers = 4`, or a whole misspelled section, booted clean with
  defaults and no way to notice. All 24 structs now carry
  `deny_unknown_fields`, and the error names the offending key.

  The environment is now held to the same standard — see the next entry.
- **A misspelled environment override is now a startup error.** Overrides are
  matched by name rather than deserialized, so `ORION_SERVER__PORTT=3000` did
  exactly nothing and the mistake surfaced as a port number in a log line.
  Startup scans the process environment and refuses any variable that follows
  the override grammar without being one, naming every offender and the nearest
  real key in a single message —
  `ORION_SERVER__PORTT (did you mean ORION_SERVER__PORT?)`. The allowlist is
  derived by running the overrides themselves, so it cannot drift from the
  code.

  **The grammar is what is checked, not the prefix.** An override is `ORION_`
  plus the config field path with `__` between levels, so a name without a
  `__` cannot be a misspelling of a setting and is ignored. That is
  deliberate: `ORION_` is not Orion's to claim. Kubernetes injects
  `ORION_SERVICE_HOST`, `ORION_PORT` and friends into every pod whose namespace
  holds a Service called `orion` unless the PodSpec sets
  `enableServiceLinks: false`, and `orion-cli` reads `ORION_SERVER_URL` and
  `ORION_API_KEY`. The cost is that a setting typed with a single underscore
  (`ORION_SERVER_PORT`) is indistinguishable from a service link and is still
  ignored. `ORION_ENVIRONMENT` is the one setting with no separator of its own
  and is checked by proximity instead.

  Three things stay accepted: retired names, which keep their own more specific
  "renamed to X" error; names the config file references through `${VAR}`
  substitution; and the reserved `ORION_SECRET_*` namespace, which Orion never
  interprets — use it for `env://` connector secrets and any other
  operator-owned value that must live under `ORION_`, since connectors live in
  the database and cannot be enumerated while the config is loading. Run
  `env | grep -oE '^ORION_[A-Z0-9_]+' | grep '__'` against your deployment and
  check each name against `docs/src/configuration/reference.md` (C4d).

  Relatedly: **`ORION_ADMIN_AUTH__API_KEY` never existed.** The deployability
  page documented the singular name; the override is
  `ORION_ADMIN_AUTH__API_KEYS`. Anyone who copied it enabled admin auth with no
  keys loaded. The page is fixed, and the singular name is now refused at
  startup instead of ignored.
- **A production cluster may no longer migrate at boot.**
  `cluster.enabled = true` with `storage.auto_migrate = true` used to warn —
  from a log line emitted *after* the migration it warns about, so the
  guardrail fired after the race it existed to prevent. With `environment`
  starting `prod` it is now a config error raised during validation, before
  anything opens a connection, so `orion-server validate-config` catches it at
  review time. Set `storage.auto_migrate = false` and run `orion-server
  migrate` as a deploy step — the Helm chart already ships a
  pre-install/pre-upgrade Job and `docker-compose.ha.yml` a one-shot `migrate`
  service, so both reference topologies are already in the safe shape.
  Single-node installs are untouched (cluster mode is off by default, and
  migrating at boot is what makes the single binary self-installing), and
  non-production clusters keep the warning, which is what lets the chart's
  throwaway `devStack` boot without a migrate step (C7).
- **One output-field name across every function: `output`.** `http_call` and
  `channel_call` called their destination path `response_path` while the other
  eight handlers called it `output` — two names for one concept, and the
  most-touched field in the task JSON contract. Both handlers now take
  `output`. `response_path` is still accepted so 0.3.x workflows load
  unchanged; when a task carries both, `output` wins.

  | Function | Pre-1.0 | 1.0 | Default if omitted |
  |---|---|---|---|
  | `http_call` | `response_path` | `output` | response discarded |
  | `channel_call` | `response_path` | `output` | `"data"` |
  | the other eight | `output` | `output` | `"data"` |

  The differing defaults are deliberate and unchanged in this release.
- **Every metric is renamed with an `orion_` prefix** — `messages_total` is now
  `orion_messages_total`, and so on for the whole set bar one the pass missed,
  which O14 below finishes. The bare names were
  generic enough to collide in a shared registry (`errors_total`,
  `active_workflows`, `db_pool_size`). **Update dashboards and alert rules
  before upgrading.**
- **Histograms are now real Prometheus histograms.** Without configured buckets
  the exporter rendered all seven `*_seconds` families as *summaries with
  pre-computed quantiles*, which cannot be aggregated across replicas —
  directly at odds with cluster mode. Queries using
  `histogram_quantile()` over `_bucket` series now work; queries reading the
  old summary quantiles must be rewritten.
- **In cluster mode every metric carries an `instance` label** identifying the
  replica. Recording rules that aggregate without `by`/`without` may need
  updating.
- **Three more metric families changed name or labels — rewrite these
  selectors before upgrading.** The failure mode is the silent one: PromQL
  returns an empty result for a name or label that does not exist, so a panel
  renders blank and an alert built on it **stops firing** instead of erroring.

  | Before | After |
  |---|---|
  | `orion_channel_executions_total{channel}` | *removed* — use `sum by (channel) (orion_messages_total)` |
  | `orion_errors_total{type="…"}` | `orion_errors_total{reason="…"}` |
  | `kafka_consumer_lag{topic, partition}` | `orion_kafka_consumer_lag_messages{topic, partition}` |

  `orion_channel_executions_total` was incremented immediately next to the
  `status="ok"` arm of `orion_messages_total`, so it was that counter minus the
  label that says how the message ended — strictly less information under a
  second name — and it was never called from the Kafka ingest or DLQ paths, so
  it undercounted on any deployment using them. The replacement is a
  **superset**, not an identity. `type` was the only error-classification label
  not named `reason`; its *values* are unchanged. `kafka_consumer_lag` was the one family carrying neither the
  `orion_` prefix — so it could collide with any other exporter's gauge of that
  name in a shared registry, the exact collision the convention exists to
  prevent — nor a unit; its labels are unchanged.

  A new drift guard parses every `counter!` / `gauge!` / `histogram!`
  invocation in `src/metrics/mod.rs` and asserts the observability page lists
  exactly those names with exactly those labels, and that every name carries
  the prefix. The page was documenting fewer than half of them and still
  carried the pre-1.0 `client` label on `orion_rate_limit_rejections_total`; it
  now covers all 38 (O14).
- **`/metrics` is registered only when metrics are collected.**
  `metrics.enabled = false` used to answer `200` with an empty body rendered
  from an orphan recorder, so a deployment with metrics switched off was
  indistinguishable from a working scrape target that happened to have no
  series. The path now `404`s like any other unknown route — including when
  `admin_auth.enabled = true`, where an unregistered path falls through to the
  404 fallback rather than answering `401`, matching the `/docs` gate (S17). If
  a scrape job goes red on upgrade, that is the misconfiguration becoming
  visible: set `metrics.enabled = true`, or point the job at the new
  `metrics.bind_addr` listener. Setting `bind_addr` also removes `/metrics`
  from the main listener — it is a move, not a copy (O12).
- **Plaintext `admin_auth.api_keys` entries must be at least 32 characters.**
  Previously `api_keys = ["a"]` was a valid production credential. Shorter keys
  are a hard config error when `environment` starts with `prod`, and a warning
  otherwise. `sha256:` entries are exempt. Generate keys with
  `openssl rand -hex 32`.
- **`rate_limit.endpoints.admin_rps` now defaults to `20`** instead of being
  unset. Previously the admin plane fell back to `default_rps` (100) — the same
  budget as the anonymous data plane. Set it to `null` (or an empty string via
  the environment variable) to restore the fall-back.
- **401, 429 and recovered-panic 500 responses now carry security headers and
  `x-request-id`,** and the error envelope for them includes `request_id`.
  Clients asserting on the absence of these will see new headers and one new
  body field.
- **Browser preflight (`OPTIONS`) to `/api/v1/admin/*` is now answered by the
  CORS layer** rather than rejected with 401. Any client relying on preflight
  failing closed should note the admin API was previously unusable from a
  browser whenever `admin_auth.enabled = true`.
- **Both dialect envelopes and the inline `schema` are strict.** Unknown keys
  in the `data_query` envelope, the `data_write` envelope, `include` selections
  and `on_conflict` were silently ignored: `"fileds"` selected every column,
  `"lmit": 5000` fell back to the default 100, `"retuning"` returned nothing,
  and a misspelled `filter` key made a mutation unfiltered. They are now
  rejected with an error naming the offending key — fix the key it names. The
  pre-1.0 flat `data_write` form keeps working: the handler strips its own keys
  before the strict parse.

  Every `schema` struct rejects unknown keys too, which surfaces a trap the
  documentation's own example set. It used `"table"` where the field is
  `physical` — silently dropped, so authors got a wide-open identity-mode
  registry believing they had configured a rename — and `"type": "string"`,
  which is not a `FieldType`. The example is fixed, and a test parses it
  verbatim and asserts it means what the prose says. A stored schema carrying a
  stray key now fails loudly instead of silently not applying (W6, W5).
- **`include` and many-to-many filters raise a capability error on MongoDB and
  Elasticsearch.** `include` was parsed and silently dropped by both doc-store
  translators — the caller got parents with no children and no error — and a
  `some`/`all`/`none` over a `through` relation rendered as a plain
  `$elemMatch`/`nested` on the relation name, returning wrong rows. Both now
  raise `FeatureUnsupportedByTarget`, the same gate include planning already
  applied to m2m on SQL, and the parity table documents both rows. On a doc
  store, fetch the related documents with a second query, or model them
  embedded/nested and filter with `some` (F26, W11).
- **Mongo projections no longer leak `_id`.** `fields: ["name"]` returned
  `{name}` on SQL and Elasticsearch and `{_id, name}` on MongoDB — one
  envelope, two result shapes. `_id` is now suppressed unless explicitly
  projected; project it if you relied on it (W9).
- **`skip` is capped on every backend.** Only Elasticsearch bounded it (via its
  result window); SQL and MongoDB scanned arbitrarily deep. The new
  `query.max_skip` (default `10000`) rejects — never clamps — a larger offset
  on all three. Raise it (or `ORION_QUERY__MAX_SKIP`) if you genuinely page
  deeper (W12).
- **An `include` selection must state a `sort`, and its page is bounded per
  parent.** The child query emitted no `LIMIT` and no `ORDER BY`: the handler
  fetched every child of every parent on the page and truncated in memory, so
  1000 parents × 10 000 children materialised ten million rows to return five
  apiece — and with nothing ordering them, `include.limit` returned an
  *arbitrary* subset that could differ run to run. The per-parent page is now
  cut in SQL with `ROW_NUMBER() OVER (PARTITION BY <fk> ORDER BY <sort>)`, so
  the envelope has to name an order key, and `include.limit` follows the
  envelope's own page policy applied **per parent**: absent means
  `query.default_limit` (100), and a value above `query.max_limit` (1000) is
  rejected rather than clamped. Hydration is bounded by `parents × limit`.

  **Stored workflows using `include` without a `sort` fail at request time** —
  activation does not validate the dialect envelope, so this surfaces on live
  traffic, not on deploy. Add a `sort` to every selection
  (`"sort": [{"id": "asc"}]` is the usual choice); it may name a column
  `fields` does not, because it is projected into the windowed sub-select for
  the outer `ORDER BY` and then stripped, so nested children still carry
  exactly the requested `fields`. The requirement is the SQL planner's, not the
  envelope parser's: MongoDB and Elasticsearch cannot answer an `include` at
  all, so a doc-store caller still gets `FeatureUnsupportedByTarget` — the
  error that says `include` is SQL-only — rather than being asked for a sort
  key that would not have helped (F27).
- **Null ordering is inverted on SQL and Elasticsearch: a null now sorts as the
  smallest value** — nulls first on `asc`, last on `desc`. SQL emulated "nulls
  last on `asc`" (with an `IS NULL` prefix sort key on MySQL) and Elasticsearch
  set `"missing": "_last"`, while MongoDB's `find` cannot express that rule at
  all — so the same envelope paged differently on Mongo, silently, against a
  documented promise of deterministic null ordering across backends. The shared
  rule is now the one every backend states natively, so the other four move to
  meet Mongo and MySQL's emulated prefix sort key is gone. Nothing errors: a
  page ordered by a nullable column simply comes back the other way round. If
  the position of nulls matters, filter them out
  (`{"!=": [{"field": "col"}, null]}`) or sort on a non-nullable column
  first (W8).
- **MongoDB no longer maps a logical `id` onto `_id` implicitly.** Any physical
  name equal to `id` — in filters, projections, sorts, inserted documents,
  `set` clauses and `on_conflict` targets — was silently rewritten to `_id`, so
  a schema deliberately mapping a key onto `id` meant `_id`, and a collection
  carrying a genuine non-key `id` field was unqueryable. The Elasticsearch
  renderer two files away documented the opposite rule, so one dialect had two
  contradictory answers to "what is `id`?". Both document stores now pass names
  through exactly as the schema resolved them. **A Mongo filter on `id` no
  longer finds documents written before the upgrade**, whose key is in `_id`;
  declare the rename — `{"columns": {"id": {"name": "_id"}}}` — which is the
  declaration Elasticsearch already required. Without it, `id` is an ordinary
  field on every backend (W10).
- **A bulk `data_write` that applied only some of its items no longer aborts
  the workflow, and every result carries a `status`.** One row count covered
  three genuinely different failure models: SQL is atomic, MongoDB
  `insert_many` is ordered so a mid-array failure commits the prefix and never
  attempts the rest, and Elasticsearch `_bulk` applies each action
  independently so any subset can land. Both doc stores surfaced one opaque
  error over half-written state — ES returned a single `first_error` and
  discarded `items`, Mongo's error carried no index — and failed the call, so
  the applied documents stayed in place, unnamed.

  Both now return a per-item outcome array carrying the caller's own `values`
  indices, distinguishing applied, rejected and never-attempted items. A call
  that applied some but not all of them is `"status": "partial"` and reports
  audit status **207** rather than aborting, so the applied prefix is named
  instead of lost; a bulk where nothing landed stays a hard error. The SQL
  write now runs inside an explicit transaction, making its all-or-nothing
  guarantee a property of the handler rather than of the renderer's shape.

  **Two things to do.** Anything asserting on the exact result object sees one
  extra key — `status` is `"ok"` on every non-partial write, SQL included.
  And a workflow that relied on a failed bulk erroring to halt the pipeline
  must now branch on `status` and compensate; the `items` array names exactly
  which indices to retry or roll back (F28).
- **REST route matching is byte-exact and percent-decodes path parameters
  exactly once.** Static segments matched case-insensitively, and the data
  plane matched a path axum had already percent-decoded — so `%2F` acted as a
  segment separator before matching, a literal `/` was inexpressible inside a
  parameter, and the rate-limit middleware (which matches the raw URI) could
  resolve a different channel than the handler. Matching now splits on raw `/`
  first and decodes each segment exactly once: `/ORDERS/1` no longer matches
  `/orders/{id}` (RFC 3986 paths are case-sensitive; `%6F` still equals `o` —
  encoding an unreserved character is equivalence, not difference),
  `metadata.params` arrive decoded (`/orders/a%2Fb` yields `id == "a/b"`), and
  an invalid percent-sequence (`%ZZ`) is answered with `400` instead of being
  matched literally.

  Fix client URLs whose casing no longer matches, and drop any hand-decoding a
  workflow did on a param — decoding twice changes meaning. Route-conflict
  canonicalisation is case-preserving to match, so two casings of one path are
  two co-activatable routes. `route_pattern` also rejects `%` now, on create,
  update and import: patterns are written literally and requests match by
  their decoded value, so write the character itself. Already-active channels
  keep their (unreachable-as-written) behaviour until you next edit one (N10).
- **`backpressure.max_concurrent` is renamed `max_concurrent_per_node`.** The
  semaphore is per process, but the name read as an absolute cap while sitting
  beside dedup and rate-limit controls that *are* shared in cluster mode — two
  controls in one config block with opposite cluster semantics and no naming
  difference. The old key is accepted as a deserialization alias for one
  release, so stored configs keep working; rename it the next time you edit the
  channel (N9).
- **A channel's ingress guards apply on every transport, not the subset each
  one happened to implement.** Kafka — the highest-volume path — applied no
  rate limit, no deduplication and no backpressure, so `max_concurrent_per_node`
  bounded every path except the one that needed it and an at-least-once
  redelivery ran the workflow twice; `channel_call` applied no rate limit; and
  a channel's `timeout_ms` was honoured on two paths of four, so the same
  channel timed out at its configured value over HTTP, at
  `kafka.processing_timeout_ms` over Kafka, and at
  `trace_queue.processing_timeout_ms` over `/async`. One `GuardSet`
  per transport, applied by one `apply_guards`, now decides which guards run,
  so the remaining exclusions — no origin allow-list off HTTP, no dedup on
  `channel_call`, response cache on synchronous HTTP only — are data carrying a
  reason rather than comments spread over four call sites.

  Four things to check before upgrading. **Kafka rate limit and backpressure:**
  a refused record is *not* dead-lettered — the offset is left uncommitted and
  the consumer's existing capped retry backoff throttles the topic, so expect
  consumer lag rather than errors, counted as
  `orion_errors_total{reason="kafka_guard_deferred"}`. Size
  `requests_per_second` / `max_concurrent_per_node` against the topic's real
  throughput first. **Kafka deduplication:** the idempotency key is the record
  header named by `deduplication.header`, or, absent that, the **record key** —
  which is usually a partition key, so if yours identifies an *entity* rather
  than an *event*, every record after the first inside `window_secs` is
  suppressed (`orion_messages_total{status="duplicate"}`). Set the header on
  the producer, or drop `deduplication` from channels fed by such a topic.
  **`timeout_ms` on Kafka and `/async` is clamped** to
  `kafka.processing_timeout_ms` and `trace_queue.processing_timeout_ms`
  respectively: those are ceilings, not defaults, because a Kafka dispatch
  blocks the consumer's poll loop and an `/async` dispatch occupies one of a
  fixed number of queue workers. A channel may shorten its deadline anywhere
  and lengthen it only where nothing shared depends on it — raise the transport
  setting if a channel genuinely needs longer. **`channel_call` now spends the
  target channel's rate-limit budget** (bucket key: the calling channel, unless
  `key_logic` says otherwise), so a fan-out calling one channel N times per
  request needs headroom for N.

  A ✅ in the rate-limit row means *the same limiter is consulted*, not that the
  four ingresses share one bucket: the key defaults to whatever caller identity
  the transport has — client IP over HTTP, topic on Kafka, calling channel for
  `channel_call` — so only a `key_logic` returning a transport-independent
  value makes it one shared throughput cap (N16).
- **A channel's `rate_limit` applies whether or not the platform limiter is
  on.** It was enforced inside the rate-limit middleware, which pulls
  `AppState` and re-resolves the target channel from the URI — neither of which
  a Kafka record or an in-process call has, and which is also why the limit
  silently did nothing unless `[rate_limit] enabled` happened to be true. The
  limiter moved into the channel guards, where every transport runs it; the
  middleware keeps only the platform budget, and the data plane stops matching
  the route table twice per request. Two consequences: **if you relied on
  `[rate_limit] enabled = false` to disable all throttling, remove the
  `rate_limit` blocks from the channels too**, and a data request is now
  metered twice — once against `data_rps`/`default_rps`, once against its
  channel's own limit — so a channel whose `requests_per_second` exceeds the
  platform budget is capped by the platform budget as well (S15).
- **`config.cors.allowed_origins` on a channel is now `origin_allow_list`.** It
  set no `Access-Control-Allow-Origin` and never saw a preflight — the platform
  CORS layer short-circuits every `OPTIONS` before a channel is resolved — so
  the list could only ever narrow the platform policy, never authorize an
  origin, and a channel naming an origin the server config omitted stayed
  unreachable from it with nothing to say so. The control is real and worth
  keeping; only the name was a lie. The key is flattened as well as renamed —
  `{"cors": {"allowed_origins": [...]}}` becomes
  `{"origin_allow_list": [...]}` — and the old spelling is still parsed, so
  stored channels keep their check and no migration is required. If per-channel
  origins are not taking effect in a browser, set `[cors] allowed_origins` to
  the union of what your channels accept and narrow from there (N24).
- **`CacheBackend::check_and_insert` is gone,** replaced by
  `claim_dedup_key(key, owner, window_secs) -> Option<holder>` and
  `remove(key)`. A bare "is this key new?" boolean cannot tell a second
  delivery from a redelivery of the same one, and treating the latter as a
  duplicate committed Kafka records that had never run. Only relevant if you
  implement the trait out of tree.
- **Async trace results are never sampled away.** `trace_storage.sample_rate`
  on the async path dropped the *result* while the pending/running/completed
  status rows were still written — the caller polled a `completed` trace with
  nothing in it, and the storage was spent anyway. `for_async_submission` now
  pins `sample_rate` to `1.0`, exactly as it upgrades `mode = "off"` to `sync`:
  a 202 is a receipt for a fetchable result. **Async trace storage for sampled
  channels will grow** — bound it with `errors_only` or a shorter
  `trace_queue.retention_hours` instead. Sampling applies in full on the sync
  path, where the draw happens once per trace at the single point persistence
  is decided and a sampled-out trace produces no rows at all (N22).
- **`kafka.max_inflight` is removed — it advertised concurrency that never
  existed.** The consumer created the semaphore, acquired a permit and then
  awaited each message inline, so concurrency was always exactly 1 whatever the
  value said; the field, its validation and the startup log line all described
  behaviour the code never had. Sequential processing is load-bearing for the
  at-least-once contract — committing an offset implicitly commits every
  earlier offset on the partition — so the honest fix is removal, not
  parallelism. A config file still carrying the key fails startup via
  `deny_unknown_fields`, and a manifest still setting
  `ORION_KAFKA__MAX_INFLIGHT` is refused at startup with the removal reason.
  Delete both; scale throughput by running more instances in the same consumer
  group (K4).
- **`orion-server validate-config` prints the full effective config, and stops
  printing database credentials.** The old output was a hand-maintained summary
  of a dozen settings that omitted `[cluster]` entirely, all the DLQ knobs,
  `[trace_storage]`, `[ingest]`, `[query]`, `[write]`, most of `[engine]` and
  `[kafka.auth]` — exactly the settings most likely to be wrong in production —
  and printed `storage.url` verbatim, embedded password included. The default
  output is now the entire merged config (defaults + file + `ORION_*`
  overrides), serialized from the same structs the server runs on, so a new
  section can never be omitted again; secrets are masked with the same policy
  as the connector API. Anything that grepped the old summary shape, or scraped
  a credential out of it, breaks: parse stdout as TOML, or pass
  `--format json`; `--format summary` restores a short human summary (now also
  masked). Under `toml` and `json` the validity note moves to stderr so stdout
  stays machine-parseable; `--format summary` keeps it on stdout. Exit codes are
  unchanged — `validate-config || exit 1` needs no edit (O15).
- **Five OpenAPI response schemas are renamed.** Only the last one's body
  changed; for the other four the JSON field sets are identical, so only
  clients generated from the spec, which take their type names from component
  names, are affected. Regenerate and rename the referenced types.

  | Before | After |
  |---|---|
  | `Connector` | `ConnectorResponse` |
  | `AuditLogEntry` | `AuditLogEntryResponse` |
  | `TraceDlqEntry` | `TraceDlqEntryResponse` |
  | `PaginatedEnvelope_TraceDlqEntry` | `PaginatedEnvelope_TraceDlqSummaryResponse` |
  | `PaginatedEnvelope_TraceListItem` | `TracePageEnvelope` |

  The trace-list envelope is the exception: it was renamed because its shape
  changed rather than its row type — `total` is now conditional and
  `next_cursor` is new (see the trace-pagination entry above).

  The generic envelope names follow (`DataEnvelope_Connector` →
  `DataEnvelope_ConnectorResponse`, and so on). The names now match the
  `WorkflowResponse` / `ChannelResponse` convention already in use and say what
  they are: the DTO the endpoint returns, not the storage row it is built from
  — which those rows can no longer be, since they no longer derive `Serialize`
  at all (D28).

- **Rate-limit client identity is the TCP peer address.** Forwarded headers
  (`X-Forwarded-For` / `X-Real-IP`) are honored only when the peer falls inside
  the new `rate_limit.trusted_proxies` CIDR list, which is **empty by default**.
  Deployments behind a proxy, load balancer, or ingress that do not configure it
  will collapse every client into a single bucket. Applies when
  `rate_limit.enabled = true` (still `false` by default), and to per-channel
  `key_logic` expressions referencing `client_ip`. A malformed entry is a hard
  startup error even when rate limiting is disabled.
- **Metrics labels changed — dashboards and alerts will break silently.**
  `rate_limit_rejections_total` lost its unbounded `client` label and gained
  `scope` (channel name, or `admin` / `data` / `operational`). Channel-labelled
  metrics (`messages_total`, `message_duration_seconds`,
  `channel_executions_total`) now emit the literal `_unknown` for unregistered
  channels on the HTTP and queue paths. No metric was renamed or removed.
- **Channels with unparseable `config_json` or uncompilable `validation_logic`
  refuse to load, in all modes.** Previously a warning, after which the channel
  served with its validation, dedup, rate limit, cache, and backpressure guards
  silently disabled. A stored config that was quietly broken now exits the
  process at startup, and fails engine reload — plus every admin mutation that
  triggers one — with `500 CONFIG_ERROR`. Registry rebuilds are all-or-nothing,
  so a refusal leaves the running engine untouched.
- **Kafka delivery is at-least-once.** Offsets advance only on successful
  processing or a *confirmed* DLQ write. With `kafka.dlq.enabled = false` (the
  default) a poison message now blocks the consumer and retries with capped
  backoff (1s → 60s) instead of being dropped; because messages are processed
  sequentially, this halts every subscribed partition on that instance.
  **Enabling `[kafka.dlq]` is the recommended action.**
- **Data-plane error bodies are sanitized.** Entries in `errors[]` are reduced
  to a code, a fixed generic message, and an optional `task_id`; correlate via
  the top-level `request_id` (also the `x-request-id` header) and read full
  detail from the persisted trace. Cached responses store the sanitized body.
- **Response cache keys fold in method, route params, and query string.**
  Existing cached entries are orphaned — never mis-served — and expire by
  `cache.ttl_secs` (default 300s).
- **Open circuit breakers return `503 CIRCUIT_OPEN`** instead of
  `500 ENGINE_ERROR`. No `Retry-After` header. With `continue_on_error: true`
  the request still returns `200` with a sanitized `TASK_ERROR`; alert on
  `circuit_breaker_rejections_total` rather than the status code.
- **A full trace queue returns `503`** (code `SERVICE_UNAVAILABLE`, message
  `Trace queue is full …`) on the async submission path instead of blocking
  indefinitely. Sized by `queue.buffer_size` / `queue.max_queue_memory_bytes`.
- **Unimplemented secret schemes are rejected.** `vault://`, `aws-sm://`,
  `gcp-sm://`, and `azure-kv://` in connector configs were passed through and
  **used as literal passwords**; the connector is now skipped at load with an
  `ERROR` log. A connector that appeared to work was never authenticating as
  intended — rotate the credential.
- **`GET /api/v1/admin/connectors` redacts userinfo inside URL-shaped values**
  at any depth (`https://user:******@host`), which finally covers `url` and
  `brokers[]`. Credential-free URLs are still returned in full. Do not
  round-trip a connector config through `GET` → `PUT`: updates replace
  `config_json` wholesale and would persist the mask.
- **`GET /api/v1/admin/audit-logs` rejects unknown query parameters with `400`**
  instead of silently returning unfiltered results. No other endpoint changed.
- **`db_read` returns values for `float4`/`REAL` and blob columns** instead of
  `null`, and errors on genuinely undecodable columns and non-finite floats.
  Blobs stringify as UTF-8 when valid, else lowercase hex. Also affects
  `data_query` and `data_write`'s `RETURNING` path. A `null` in a result now
  means only SQL NULL.
- **Trace read endpoints require admin auth.** `GET /api/v1/data/traces` and
  `/traces/{id}` return `401` for previously-open callers when
  `admin_auth.enabled = true`. No effect when admin auth is disabled.
- **Rollout bucketing is caller-stable, not random per request** — see Added.
  A canary now exposes a stable subset of callers rather than re-drawing on
  every call; aggregate percentages are unchanged.
- **The Helm chart and HA compose default to `ORION_ENVIRONMENT=production` and require
  admin API keys.** `helm install` without `adminAuth.apiKeys` or
  `adminAuth.existingSecret` fails at template time by design
  (`devStack.enabled=true` is the dev escape hatch); `docker compose -f
  docker-compose.ha.yml up` aborts without `ORION_ADMIN_API_KEYS`. Note that
  `environment = "production"` also rejects the CORS wildcard, and the default
  `cors.allowed_origins` is `["*"]` — set explicit origins before flipping.
- **Removed:** the unread `backpressure.queue_depth` channel-config field.
  Stored configs still carrying the key deserialize normally (there is no
  `deny_unknown_fields`), so this needs no migration.
- **Removed:** the unread `cors.allowed_methods` and `cors.allowed_headers`
  channel-config fields. Only `cors.allowed_origins` was ever enforced —
  per-channel preflight is not implemented — so setting them was a silent
  no-op. Same no-migration note as above.
- **Every admin list endpoint shares one pagination envelope.**
  `GET /api/v1/admin/audit-logs` (and the new trace-DLQ list) now return the
  flat `{data, total, limit, offset}` shape the workflow/channel/connector
  lists always used, instead of a nested `{data, pagination: {…}}`.
- **Malformed admin request bodies are rejected uniformly with 400 + field
  details.** The four workflow endpoints that still surfaced axum's plain-text
  422 (`PATCH …/status`, `PUT …/rollout`, `POST …/test`,
  `POST /workflows/import`) now use the same extractor as every other admin
  route, and query-string parse failures return the standard JSON error
  envelope instead of plain text.

### Added

- **Workflows can loop over a task list.** A workflow may carry a `loop`
  object — `{counter, init, increment, max}` — that repeats its tasks with a
  counter in scope, unlocking per-element fan-out (one HTTP call per item of
  an array) without a task per element. Stored in a new nullable `loop_json`
  column (sqlite/012, postgres/016, mysql/015) and projected into the content
  hash only when present, so existing workflows keep their hashes. Iterations
  are bounded by `engine.max_loop_iterations` (default 10000). The loop body's
  break condition must be a filter task inside the loop, not the workflow
  condition — `data` is empty when the workflow condition is evaluated — and
  a workflow that gets this wrong is refused at write time by
  `validate_workflow_loop_schema` rather than at reload. Requires the
  dataflow-rs 3.3 engine.
- **`orion-server package` promotes a bundle of connectors, workflows and
  channels between environments.** Five verbs compose the promotion story:
  `export` computes a package's closure from a running instance — selected
  channels (`--tag` / `--channels`), their workflows, and every connector
  those workflows reference, with out-of-selection `channel_call` targets
  landing in `requires`; `lint` runs the same create-path validators the
  POST endpoints run, offline, plus closure-completeness and unique-name
  checks; `plan` reports per-entity would-be actions with zero writes,
  running every activation gate — findings that apply's own ordering will
  satisfy are reported as pending, not failed; `apply` claims a receipt,
  stages all three kinds, activates in dependency order with deferred
  reloads, then triggers one engine rebuild — idempotent, and a failed apply
  leaves the receipt staged and says exactly what is live vs staged; `diff`
  reports drift between the artifact and the server's content hashes with a
  non-zero exit on any difference. Every apply call is stamped
  `X-Orion-Change-Context: package=<name>@<version>`. (K stream;
  `src/package_cli.rs`)

- **Package receipts make apply idempotent and rollback honest.** A
  `packages` table on all three backends behind
  `GET`/`PUT /api/v1/admin/packages*`. The PUT enforces applied-version
  immutability atomically: the same version with a different `content_hash`
  answers **409**; staged receipts update in place (only a draft can be
  updated); an applied version never demotes; and re-putting an older
  applied version touches it current again — the rollback path. (K14)

- **The examples are packages now.** Each example directory moved to
  `examples/packages/<name>/` — the package in source form: the channel,
  workflow, and (when needed) connector of one service, every entity tagged
  `pkg:<name>` so a deployed example exports as a versioned artifact with
  `orion-server package export --tag`. A new docs page,
  [Packages & Promotion](https://goplasmatic.github.io/Orion/topology/packages.html),
  walks the modular-monolith model and the export → lint → plan → apply →
  diff flow between instances.

- **Upsert import: `?on_conflict=fail|skip|new_version` on all three
  `/import` endpoints.** `new_version` replaces an existing draft in place,
  cuts a new draft version over an active entity, and reports
  content-identical items as `unchanged` (DB-owned fields excluded) — so
  re-running the same artifact is a no-op. The response gains
  `unchanged`/`skipped` counts and a per-item `results[]` array; `dry_run`
  composes and reports the would-be action; in-batch duplicate keys are
  refused in the upsert modes. The default `on_conflict=fail` behaves
  exactly as before. A real import also writes one audit row per written
  entity alongside the `"{n} imported"` summary row, and an
  `X-Orion-Change-Context` request header is recorded in audit `details`.
  (K2, K5)

- **Activation pre-flight and deferred reload.** `?dry_run=true` on both
  `PATCH /{id}/status` endpoints runs every gate the real transition runs —
  the same functions, so it cannot drift — and answers the `/validate`
  envelope without writing (K3). `?reload=defer` on status and rollout
  changes commits the row but skips the engine rebuild and cluster epoch
  bump, so `POST /engine/reload` batches N entity promotions into one
  rebuild and one bump (K4).

- **Channels and connectors carry `tags` with `?tag=` filtering, matching
  workflows.** Tag filtering applies to list and export, and tags
  participate in the import content comparison (K6).
  `CreateConnectorRequest` also carries `enabled`, and create/import write
  the column — a disabled connector no longer promotes as enabled through
  export→import (K1).

- **Dependency introspection, content hashes, and consistent exports.**
  `GET /workflows/{id}/dependencies` reports a workflow's connector
  references (with the referencing function), static `channel_call` targets,
  and a `has_dynamic_channel_calls` flag when `channel_logic` makes the
  static list incomplete (K9). Every workflow, channel and connector
  response carries `content_hash` — sha256 over the canonical importable
  content, DB-owned fields excluded; one definition in `storage::content`
  shared by the import unchanged-detection, the DTOs, and the CLI (K10).
  Every `/export` reads its pages inside one transaction (REPEATABLE READ
  forced on Postgres), retiring the documented "exports are not a consistent
  snapshot" caveat (K12).

- **`orion-server preflight` scans the stored estate before an upgrade.** The
  0.3.0 → 1.0.0 guide carries an 18-row checklist, and several rows were only
  answerable by running SQL against the `channels` and `workflows` tables by
  hand. This runs those checks in the binary that knows the rules: channel
  configs that no longer parse (naming the replacement key for the two
  renames), workflows whose tasks the create validator would reject, and —
  the one nothing else could catch in advance — `data_query`/`data_write`
  tasks with no `schema`.

  That last one is why the command exists. Every other high-impact 1.0 break
  either fails at startup or is visible in a config file; a schema-less dialect
  task keeps loading and activating and fails at its *first request*, which
  means production traffic. Read-only, needs only `storage.url`, reports each
  finding against its checklist row, and exits non-zero so it can gate a
  deploy. Config-file and `ORION_*` problems are already reported by
  `validate-config`, which is what preflight's header says rather than
  duplicating them.

- **`orion_task_duration_seconds{workflow,task,function}` times every task,
  including the eight built-ins nothing could reach.** `map`, `validate`,
  `filter`, `parse_json`, `parse_xml`, `publish_json`, `publish_xml` and `log`
  are dispatched inside a private executor method and never reach the handler
  registry, so Orion's `observed_handler_named` wrapper could not time them at
  any price — their cost showed up only as `workflow_overhead_ms`, a residual
  computed by subtraction in the opt-in profile surface. dataflow-rs 3.1's
  `ExecutionObserver` is the seam; it is always on and allocates nothing per
  request, unlike a trace. Keyed by task, so three `db_read` tasks in one
  workflow are finally distinguishable.
  `orion_connector_request_duration_seconds` is unchanged and remains the
  per-connector view. Labels are authored ids, not caller input, so no request
  can grow the label space.

- **Request headers can no longer be read back through `task_trace_json`
  (S14, completing it).** `context.metadata` was stripped from `result_json`
  on read because it carries the request header map — but `task_trace_json`
  was returned verbatim, and every `ExecutionStep` inside it holds a full
  `Message` clone carrying the same metadata, as do a `map` task's per-mapping
  context snapshots. Only four header names are masked at ingress, so
  everything else (`x-auth-token`, `x-amz-security-token`, …) was readable one
  field further down. New traces never capture it —
  `TraceOptions::redact_paths` prunes `metadata.headers` as the snapshot is
  built, so it is not cloned in the first place — and rows already on disk are
  covered by a read-side walk over the steps.

- **Captured traces are bounded at capture time, not trimmed afterwards.**
  With `tracing.task_details = true` each step deep-cloned the whole message
  *including the accumulated audit trail*, making trace size unbounded in
  message size and quadratic in task count: a 6-task workflow over a ~1 MB
  context serialized to ~12 MB. `serialize_task_trace_capped` caught the result
  but only after the clones and the serialization were already paid. Capture
  now carries the same byte budget (`queue.max_result_size_bytes`), keeps only
  each task's own audit entry, and records the per-task diff — which is what
  the feature is for, and which is correctly attributed on a skipped task,
  unlike reading `audit_trail.last()`. The post-hoc cap stays as a backstop.

- **The Helm chart can serve metrics, and ships a ServiceMonitor (P3).** The
  chart never emitted `ORION_METRICS__ENABLED`, and `metrics.enabled` defaults
  to `false` — so since O12 the route was not registered at all and `/metrics`
  404'd on every chart install, while 1.0 ships `orion_trace_dlq_depth`,
  `orion_trace_queue_rejected_total` and `orion_circuit_breaker_rejections_total`
  and tells operators to alert on them. `metrics.enabled` now defaults to
  `true` and binds the **dedicated** listener (`metrics.port`, default `9090`)
  rather than putting `/metrics` on the main one, where admin auth would make
  every scraper hold a credential that can also rewrite workflows and read
  trace payloads. The port is published on the Service and on a second
  containerPort, with opt-in `metrics.serviceMonitor` and `metrics.podMonitor`
  templates (Prometheus Operator CRDs) and `metrics.prometheusAnnotations` for
  annotation-based discovery. The listener is unauthenticated by design, so it
  is deliberately not routed by the Ingress. A `metrics.port` colliding with
  `server.port` now fails at render time instead of at boot.

- **Chart housekeeping: `values.schema.json`, `.helmignore`, `helm test` hooks,
  and Chart.yaml maintainers (P9).** Every required value on this chart is a
  string, so `--set cluster.enabld=true` silently no-opped; the schema rejects
  it at render time. `helm test` now runs two hooks — `test-connectivity` (the
  binary's own subcommand: opens the storage pool, counts pending migrations,
  probes Kafka when enabled) and `test-api` (`/health`, `/readyz`, and that the
  metrics port serves Prometheus exposition without a credential).

- **`server.max_admin_body_size`** (default 8 MB) bounds admin request bodies
  independently of the data plane. The limit was a single global layer set
  from `ingest.max_payload_size` — a name that says *data plane* — so raising
  it for a bulk import also raised it for anonymous channel traffic.

- **`query.max_skip`** (default `10000`) — hard cap on the `data_query` `skip`
  offset, enforced identically on SQL, MongoDB and Elasticsearch. A query
  skipping deeper is rejected, never clamped, exactly like `query.max_limit`.
  Override with `ORION_QUERY__MAX_SKIP` (W12).

- **`on_backend_error: "allow" | "deny"` on a channel's `rate_limit` and
  `deduplication`.** Both guards failed open on Redis errors unconditionally,
  with fail-open pinned as a trait contract — a Redis blip silently removed all
  rate limiting and all idempotency cluster-wide, and `/readyz` catches only a
  full outage. The default stays `allow` (availability wins); payment and
  idempotency workloads can opt into `deny`, which refuses with `503` — never a
  lying `409` or `429`, because the key or limit is unverifiable rather than
  violated — until the backend recovers (N7).

- **`storage.backup_retention_count`** bounds SQLite backups: after each
  successful `POST /api/v1/admin/backups` the oldest `orion_backup_*.db` files
  are pruned so at most N remain (the prune is logged, and only files matching
  the backup naming pattern are ever candidates). Backups land on the same disk
  as the live database, so an unbounded set was a backup mechanism that could
  cause the outage it exists to recover from. Unset keeps every backup — the
  previous behaviour; `0` is refused at startup, because "keep none" is not a
  retention policy. Env override `ORION_STORAGE__BACKUP_RETENTION_COUNT` (O6).

- **`orion_job_last_success_timestamp_seconds{job}`** — a gauge stamped with
  the unix time of each background job's last fully successful tick:
  `trace_cleanup`, `audit_cleanup`, `dlq_retry`, `epoch_watcher` (cluster mode)
  and `kafka_lag` (Kafka enabled). The periodic jobs deliberately swallow
  per-tick errors and keep looping, so a sustained DB blip silently stopped
  trace cleanup and DLQ retry cluster-wide with no alertable signal. Alert on
  `time() - orion_job_last_success_timestamp_seconds{job="…"}` exceeding a few
  tick intervals. In cluster mode only the lease-holding node stamps the
  lease-gated jobs — a node that loses the lease honestly goes stale rather
  than lying about freshness — and the lag poller stamps only when both the
  committed offsets and every watermark lookup answered, so a broker that
  freezes the lag gauges freezes the stamp with them (O3).

- **`/health` and `/readyz` observe Kafka ingestion.** Both probes carry a
  `kafka` component — present only when `kafka.enabled`, so non-Kafka
  deployments get byte-identical bodies — reporting `error` while ingestion is
  degraded or the consume loop has died. `/readyz` includes it in readiness, so
  a node that consumes nothing returns `503` and leaves the load-balancer
  rotation; `/health` reports `status: "degraded"` while HTTP itself keeps
  serving. The probes take the consumer handle with a non-blocking lock, so a
  routine reload restart can never stall them. The new
  `orion_kafka_ingest_degraded` gauge (0/1) carries the same signal for
  Prometheus (O10, K7).

- **`orion_build_info{version, git_hash, build_timestamp}`** — the standard way
  to answer "which build is each replica running?" from Prometheus. Previously
  that information existed only in `--version`, one boot log line, and the
  admin-gated `/health` body, none of which a scrape can join against.
- **`orion_admin_auth_failures_total{reason}`** — rejected admin credentials,
  split out from the shared `orion_errors_total{reason="auth_failure"}` so credential
  guessing can be alerted on without also matching `panic`, `dedup_backend`
  and a dozen other unrelated call sites.

- **`metrics.bind_addr`** — an optional dedicated listener serving only
  `GET /metrics`. The endpoint lived only on the main listener, so with
  `admin_auth.enabled = true` every Prometheus scraper had to hold an admin API
  key — a credential that can also rewrite workflows and read trace payloads,
  for an endpoint that needs neither. The second listener is plain HTTP
  (`server.tls` governs the main one only) and **unauthenticated**: the address
  is the access control, so point it at a loopback interface, a pod IP or a
  private Compose network. Startup warns if it is not loopback, refuses if it
  collides with `server.host`/`server.port`, and binds it before the main
  server starts — a clash or a permission problem is a startup failure, not a
  silently missing scrape target. It joins the graceful-shutdown path, so the
  last scrape of a draining node still succeeds. Requires
  `metrics.enabled = true`; set alone it warns and raises no listener. Setting
  it removes `/metrics` from the main listener, so update the scrape config in
  the same change (O12).

- **`audit.max_pending`** (default `1000`) and **`audit.drain_timeout_secs`**
  (default `5`) — the audit write moved from a detached task onto a bounded
  queue drained at shutdown, so a mutation accepted moments before `SIGTERM` is
  still recorded. Both are refused at `0`. Raise `max_pending` when a bursty
  admin plane (large `/import` batches) overruns the writer, and
  `drain_timeout_secs` when shutdown reports abandoned rows on a slow database.
  Anything not recorded is counted in
  `orion_audit_events_dropped_total{reason}` (`queue_full`, `write_failed`,
  `drain_timeout`, `writer_stopped`) and shown live by
  `orion_audit_queue_depth` (O7).

- **`storage.connect_retry_secs`** (default `60`, `0` restores fail-fast) — a
  database that was briefly unreachable at startup used to be a hard exit.
  `.connect()` is eager on all three backends and `min_connections = 5` means
  five live connections must exist before boot succeeds, so every replica
  crash-looped for the duration of any PostgreSQL or MySQL failover and the
  container restart backoff outlived the outage it was reacting to. The
  readiness probe already keeps traffic off a pod that has not finished
  booting, so failing fast bought nothing. The initial connect is now retried
  with a 250 ms → 5 s exponential backoff bounded by this window, one `WARN`
  line per attempt. **A genuinely wrong `storage.url` now takes up to ~60 s to
  fail instead of ~3 s** — set `0` where a fast exit is the point (pre-flight
  smoke tests, CI health gates, connectivity-checking init containers). Two
  things stay fail-fast on purpose: SQLite, whose failures — bad path, bad
  permissions, corrupt file — do not heal on their own, and the
  pending-migration check under `auto_migrate = false`, which is about schema
  state rather than reachability (D14).

- **Per-operation gates on every connector type, not just `db` and `es`.**
  A cache connector could not be made read-only, a Kafka connector could not be
  made publish-proof, and an HTTP connector — the one handler that can mutate
  an upstream nobody else in the deployment controls — had no method
  allow-list at all. Each type now carries the gates its own operations need:
  `operations: { read, write }` on `cache`, `operations: { publish }` on
  `kafka`, and `operations: { methods: ["GET"] }` on `http`, an allow-list that
  is exhaustive once non-empty and makes a connector read-only regardless of
  what any workflow asks for. Everything defaults to allowed, so no stored
  connector changes behaviour, and each gate is enforced by the handler that
  performs the operation with the same validation error the `db`/`es` gates
  produce. A method the allow-list names is checked against the methods
  `http_call` can issue when the connector is saved, so a typo is a `400`
  rather than a connector that refuses every call at request time; so is a gate
  key the type does not have. A `cache` connector's `write` gate covers every
  write through it, including a channel dedup store or response cache backed by
  it — which is a channel load failure, not a silent downgrade. There is
  deliberately no cache `delete` gate: the backend trait has no delete, and a
  gate over an operation that cannot be performed is a setting that reads as
  meaningful and is not (F22e).

- **`dialect.require_schema` and `dialect.allowed_entities` on `db` and `es`
  connectors**, both defaulting to off. `require_schema` refuses a call that
  declared no entities or asked for `"unmapped": "identity"`, closing the
  per-task opt-out so one forgotten key cannot reopen the connector.
  `allowed_entities` is a physical table/collection/index allow-list matched
  **after** schema renames — because the allow-list is the connector owner's
  and the schema is authored per task, so a rename onto a forbidden table must
  not step around it — covering the envelope's `source`/`target`, relation
  targets and many-to-many junction tables. Both bound `data_query`/`data_write`
  only: `db_read`, `db_write` and `mongo_read` name no entity and are gated by
  `operations` alone. Unknown keys inside `dialect` are rejected rather than
  silently leaving the guard off. Set these on connectors whose workflows you
  do not author (F24).

- **The dialect's central claim — one envelope, one answer on every backend —
  is now executable.** Cross-backend coverage was a single CRUD round-trip
  exercising `>`, `==`, one sort and one projection, and the per-backend unit
  tests are goldens that assert *shape*, never agreement; every silent
  divergence in this release was invisible to the suite by construction.
  `tests/integration/data_parity_test.rs` is a table: one fixture dataset, 35
  envelopes, the same ordered row set asserted on all five backends, and an
  `expected_error` column for the capability-gated combinations. SQLite runs by
  default; PostgreSQL/MySQL/MongoDB/Elasticsearch stay behind the container
  gate. The parity table in the data-dialect reference is that table (W21).

- **A cross-backend schema-parity test.** Nothing compared the three migration
  sets, which is why a migration added to two of three backends stayed
  invisible until a container test happened to touch the column — the only
  introspection anywhere checked three widened columns on PostgreSQL.
  `cargo test --test schema_parity -- --ignored` migrates SQLite, PostgreSQL
  and MySQL from scratch and asserts the three agree on every table, every
  column's normalised type and nullability, every `idx_*` index **with its
  ordered column list**, and the columns each view exposes. The normaliser is
  deliberately loose — `varchar(255)` and `text` are one type, so are
  `datetime` and `timestamp`, and SQLite's width-free `integer` matches any
  declared width — but an int4-vs-int8 mismatch between two backends that both
  declare widths still fails, which is the shape of the 0.3.0 incident. Seven
  objects are allow-listed by name with a written reason. Only the SQLite half
  runs without Docker; the cross-backend comparison is `#[ignore]`d and wired
  into CI, so every failure names the table, the column and both types (D10).

- HTTP connector `retry_non_idempotent` (default `false`): opt POST/PATCH back
  into the retry loop. Off by default because a timed-out POST may already
  have been applied — enable only where the endpoint honours an idempotency
  key the workflow sets in `headers`.
- Elasticsearch connector `max_response_size` (default 10 MB), matching the
  HTTP connector's cap.
- `POST /{channel}/async` responses carry `trace_token`; `GET /traces/{id}`
  accepts it via the `x-trace-token` header or a `?token=` query parameter.
- `engine.fail_on_connector_load_error` (default `false`): refuse to start when
  an enabled connector cannot be loaded, so a bad rollout fails at boot where
  the orchestrator catches it rather than at request time hours later. Startup
  only — a hot reload never takes a running process down.
- `GET /health` reports two new degraded states: `components.connectors` with
  the failures under `connectors.failed_to_load`, and `components.channels`
  with the quarantined set under `channels.quarantined`. Both keep the HTTP
  status at 200 — alert on the fields, not the status code, since a 503 would
  pull the node out of its load balancer over something nothing in flight may
  be using.
- `GET /api/v1/admin/connectors` gives every row a `load_status` (`loaded`,
  `failed`, `disabled`) plus `load_error` and `load_error_stage`.
- Environment overrides for the four settings that had none:
  `ORION_SERVER__COMPRESSION__ENABLED`,
  `ORION_ENGINE__CACHE_CLEANUP_INTERVAL_SECS`,
  `ORION_RATE_LIMIT__ENDPOINTS__ADMIN_RPS` and `…__DATA_RPS`. The two endpoint
  limits are optional, so their variables are three-state: unset keeps the
  config-file value, a number sets it, an empty string clears it.
- **Kafka SASL/TLS authentication** — `[kafka.auth]` (`security_protocol`,
  `sasl_mechanism`, `sasl_username`, `sasl_password`, `ssl_ca_location`) plus a
  `kafka.extra_config` passthrough for arbitrary librdkafka properties, applied
  to both the consumer and the producer. Orion can now connect to Confluent
  Cloud, MSK, Aiven, and any secured broker; previously PLAINTEXT was the only
  reachable configuration. Do not set `enable.auto.commit` via the passthrough —
  it would defeat the at-least-once guarantee below.
- **Message data in every connector function** — `db_read`, `db_write`,
  `cache_read`, `cache_write`, and `mongo_read` now resolve `{"var": "…"}`
  references in their `key`, `value`, `ttl_secs`, `params`, and `filter` inputs,
  so keys, bind parameters, and Mongo filters can depend on the message instead
  of being fixed constants. `data_query`/`data_write` share the same resolver.
  `connector` and raw `query` text stay literal by design.
- **Trace DLQ operator API** — `/api/v1/admin/trace-dlq` with paginated list
  (payload-free projection), get-by-id, requeue, and purge. Failed async traces
  were previously invisible and unreplayable.
- **Audit-log filtering** — `action`, `resource_type`, `resource_id`,
  `principal`, and time-range filters on `GET /api/v1/admin/audit-logs`; unknown
  query parameters are now rejected with 400 rather than silently ignored. The
  `details` column is populated (starting with `request_id`) — it was dead
  before, and writing to it produced malformed SQL.
- **Audit-log retention** — `audit.retention_days` (default 90, `0` keeps
  forever) with a lease-gated cleanup job. The table previously grew forever
  with no supported way to prune it.
- **Operational metrics** — `trace_dlq_depth`, `trace_dlq_retries_total`,
  `trace_queue_rejected_total{reason}`, and `trace_persistence_failures_total`.
  The three conditions most worth alerting on were previously invisible.
- **Rate-limit proxy trust** — `rate_limit.trusted_proxies` (CIDR list, empty by
  default). Forwarded headers are honoured only from listed peers; otherwise the
  TCP peer address is the client identity.
- **Hashed admin keys** — `admin_auth.api_keys` accepts `sha256:<64-hex>`
  entries so keys need not sit in config as plaintext. Plaintext still works.
- **Bounded in-memory cache** — `engine.max_memory_cache_entries` (default
  100 000, `0` = unbounded) with LRU eviction. The dedup store and response
  cache were previously unbounded maps reachable from workflow config alone.
- **`[cluster]` config section** — `enabled`, `redis_url`,
  `epoch_poll_interval_ms`, `instance_id` (auto-generated UUID when empty).
  Cluster mode requires Postgres/MySQL storage and a shared Redis; startup
  refuses SQLite.
- **Config-change propagation** — every admin mutation (channels, workflows,
  rollout, connectors, manual reload) advances a `config_epoch` row; each node
  polls it and resyncs from the DB, so a change made through any node reaches
  all nodes. This also fixes connector edits, which previously propagated to
  no other node at all. Circuit-breaker resets fan out over the same bus.
- **Cluster-shared dedup, response cache, and rate limits** — channels without
  an explicit cache connector use the shared cluster Redis for idempotency
  dedup and response caching; per-channel rate limits enforce as a shared
  Redis fixed window (~configured rate across ALL replicas combined). In
  cluster mode, a channel whose backend would silently fall back to per-node
  memory refuses to load instead.
- **DLQ claim leases + job single-flight** — DLQ retries claim rows via
  `FOR UPDATE SKIP LOCKED` leases (each entry retried by exactly one node;
  expired leases self-recover), and trace cleanup / DLQ retry acquire a job
  lease per tick so only one node runs them. New `queue.dlq_batch_size` /
  `queue.dlq_lease_secs`.
- **`storage.auto_migrate`** (default `true`) — set `false` in multi-replica
  deployments: pending migrations become a hard startup error and
  `orion-server migrate` runs as a deploy step.
- **Kafka static membership** — cluster mode sets `group.instance.id` and
  `kafka.session_timeout_ms` so rolling restarts rejoin without a full group
  rebalance; epoch-driven consumer restarts are jittered 0–5 s.
- **Postgres/MySQL storage-backend test binaries** (`storage_postgres`,
  `storage_mysql`) and a multi-node cluster test binary (`cluster`) running
  two full nodes against Postgres + Redis testcontainers in CI.
- **Helm chart** (`deploy/helm/orion`) — cluster-mode Deployment with
  readyz/healthz probes, pre-upgrade migration Job, HPA, PDB, and an optional
  throwaway dev Postgres/Redis; validated on a 3-replica kind install.
- **HA reference compose** (`docker-compose.ha.yml`) — nginx LB → 2× Orion
  (cluster mode) → shared Postgres + Redis with a one-shot migrate service,
  plus `deploy/ha/rolling-drill.sh`, a zero-downtime rolling-deploy drill.
- **Sticky canary rollouts** — the rollout bucket is now a stable hash of the
  caller identity (`engine.rollout_sticky_header`, else the forwarded client
  IP), so the same caller gets the same version on every request and replica;
  previously assignment was random per request.
- **Per-instance observability** — `service.instance.id` OTel resource
  attribute and `instance_id` on request spans in cluster mode.
- Env overrides: `ORION_CLUSTER__*`, `ORION_STORAGE__AUTO_MIGRATE`,
  `ORION_STORAGE__{MAX,MIN}_CONNECTIONS`, `ORION_STORAGE__IDLE_TIMEOUT_SECS`,
  `ORION_TRACE_QUEUE__DLQ_{RETRY_ENABLED,MAX_RETRIES,POLL_INTERVAL_SECS,BATCH_SIZE,LEASE_SECS}`,
  `ORION_KAFKA__SESSION_TIMEOUT_MS`, `ORION_SERVER__SHUTDOWN_FORCE_TIMEOUT_SECS`,
  `ORION_KAFKA__TOPICS`, `ORION_KAFKA__DLQ__{ENABLED,TOPIC}`,
  `ORION_CORS__ALLOWED_ORIGINS`, `ORION_CHANNEL_FILTER__{INCLUDE,EXCLUDE}`,
  `ORION_ENGINE__ROLLOUT_STICKY_HEADER`.

### Fixed

- **`?dry_run=true` on an import now reads the database.** It performed no DB
  reads at all, as its own doc comment said. The stated use case is CI
  pre-flight and the most common real failure is a name conflict, which is
  exactly what a no-DB dry-run cannot see — so a green dry-run said nothing
  about whether the real import would work. It now reports conflicts against
  stored rows *and* duplicates within the batch; the second was free and
  previously missed entirely.

- **`POST /admin/workflows/validate` no longer green-lights payloads
  `POST /admin/workflows` rejects.** `validate_workflow_tasks_schema` carried
  the doc comment *"Public so the `/validate` endpoint can reuse it"* and had
  **zero external callers**; the endpoint re-implemented the same walk and the
  two disagreed by design — an unknown `function.name` was a hard error at
  create and a *warning* here. A linter that green-lights a rejected payload
  is worse than no linter. The create-path validator now runs first and
  verbatim; the endpoint's remaining checks are only ever additional.

- **A poisoned profile mutex no longer fails the request.** The per-request
  profiler took its locks inside the request future and `.expect()`ed them, so
  one panic anywhere poisoned the mutex for the collector's lifetime and turned
  every subsequent profiled request into an opaque 500 — with no request id and
  no security headers, because it surfaced through the panic-catch layer. The
  same layer sat behind `json_response`, which `.expect()`ed a
  `Response::builder()` result on **every successful data request**; the
  response is now assembled directly, with no `Result` to assert past.

- **A second render of a profile is no longer blank.** `to_json` drained the
  engine-lock, workflow-total and trace-store timings as it read them, and the
  sync path renders one profile for the response and another for the persisted
  trace — so the stored copy had its phase timings missing and
  `workflow_overhead_ms` recomputed from nothing.

- **`channel_call` is attributed in `by_connector`.** It passed no label, so the
  one handler whose fan-out most needs attribution showed up as unattributed
  entries with no way to tell which target was slow. Samples are now labelled
  with the target channel, static or resolved from `channel_logic`.

- **A connector task missing `connector` says so first.** The handlers resolved
  `key` / `filter` / `params` against the message before checking that a
  connector was even named, so a task missing both reported the other field —
  the author fixed that, re-ran, and only then learned about `connector`.

- **The circuit breaker now guards all nine egress paths, not just
  `http_call`.** `db_read`, `db_write`, `data_query`, `data_write`,
  `mongo_read`, `cache_read`, `cache_write` and `publish_kafka` reached their
  pools directly, so `[engine.circuit_breaker]` read as global resilience while
  a hung PostgreSQL or Redis pinned every worker.

  **Only retryable failures trip it.** A query the backend *rejected* — a syntax
  error, a constraint violation, a row-cap breach — says nothing about the
  dependency's health, and counting it would let one bad workflow trip the
  breaker on a healthy database and take down every other channel using it. The
  error taxonomy above is what makes "retryable" mean "the dependency is in
  trouble" rather than "something went wrong".

  Breaker keys keep their `channel:connector` shape, and the whole thing stays a
  no-op while `engine.circuit_breaker.enabled` is false (still the default). If
  you enable it, expect breakers for database and cache connectors that
  previously only appeared for HTTP.

- **Connector failures are classified instead of all becoming non-retryable
  500s.** Every non-HTTP connector error went through one constructor producing
  `FunctionExecution { source: None }`, which dataflow-rs classifies as **not
  retryable**. Two consequences:

  - A dead PostgreSQL, Redis or MongoDB was a non-retryable 500, while the
    *identical* HTTP outage was a retryable `Io` — so **DLQ retry policy
    diverged by backend** for no principled reason. Failures to *reach* a
    backend now produce `Io` and retry like the HTTP path; a query the backend
    rejected stays non-retryable, which is correct.
  - A caller-fixable limit reported through the 500 path, so its message was
    replaced by the generic internal-error text. `db_read`'s row cap — *"add a
    LIMIT to the query or raise the cap"* — was sanitised away exactly when the
    caller needed it. Limits are now **400** with the guidance intact.

  **`GET`-style row-cap failures change status from 500 to 400.** If you alert
  on 5xx from the data plane, a previously-500 row-cap breach now shows as a
  client error, which is what it is.

- **An async REST channel's `route_pattern` is no longer silently ignored.**
  The route table filtered to `channel_type == "sync"`, while channel validation
  *requires* a `route_pattern` for the `rest`/`http` protocols regardless of
  type. So an async REST channel was forced to declare a route, accepted with a
  201, activated cleanly — and its declared route 404'd forever, reachable only
  by channel name. REST/HTTP channels now register their route whatever their
  type; `/async` is stripped before route matching, so an async channel's
  pattern works at `POST /api/v1/data/{pattern}/async`.

- **Workflows using the `enrich` built-in were rejected at create.**
  `KNOWN_FUNCTIONS` — the list that gates workflow creation — omitted
  dataflow-rs's `enrich`, so `POST /admin/workflows` refused any task using it
  with `unknown_function`, even though the engine runs it fine. The list is now
  pinned by a test that derives the authoritative set from the engine's own
  `FunctionNotFound` message, so a dependency bump that adds or renames a
  built-in fails CI instead of silently rejecting valid workflows.

- **Circuit-breaker reads no longer present node-local state as cluster-wide.**
  `GET /admin/connectors/circuit-breakers` and `/health` returned one replica's
  breaker map unqualified. That read as cluster state precisely because its
  sibling — the *reset* — **is** cluster-aware and fans out over the epoch bus.
  Both payloads now carry `scope: "node"` and the `instance_id` whose map it is.

  Relatedly, `POST /admin/connectors/circuit-breakers/{key}` no longer returns
  **404 in cluster mode** when the key is not open on the receiving node. Breakers
  are per-replica, so the key an operator wants to clear is usually open on a
  different node than the one the load balancer picked — and the fan-out is what
  actually clears it. The response gained `found_on_this_node` to distinguish the
  two cases. Single-node deployments still 404.

- **Connector metrics are now emitted by default.** `connector_requests_total`
  and `connector_request_duration_seconds` were emitted from exactly one place
  — inside the circuit-breaker wrapper — which only `http_call` reached, and
  only when `engine.circuit_breaker.enabled` was true. That defaults to
  **`false`**, so a default install emitted **zero** connector-level request
  counts or latencies for *any* of the ten handlers: every external dependency
  was dark in Prometheus until an operator flipped an unrelated resilience flag.

  All nine connector handlers (`http_call`, `db_read`, `db_write`,
  `data_query`, `data_write`, `mongo_read`, `cache_read`, `cache_write`,
  `publish_kafka`) now record both metrics unconditionally. Observability no
  longer depends on resilience configuration.

  **Not changed:** the circuit breaker itself still only wraps `http_call`. The
  eight other egress paths reach their pools directly, so a hung Postgres or
  Redis is still not breaker-protected.

- **Retention cleanup no longer runs as one unbounded `DELETE`.** All three
  retention jobs — traces, audit logs and DLQ purge — issued a single
  `DELETE … WHERE created_at < cutoff` per tick. The first tick after enabling
  retention is then one transaction over potentially millions of rows: SQLite
  holds the write lock for its whole duration, so **every other writer hits the
  5 s `busy_timeout` and fails**; PostgreSQL bloats WAL and blocks autovacuum;
  MySQL can exceed `innodb_lock_wait_timeout`. In cluster mode the job lease
  (`interval_secs + 60`) could expire mid-delete, letting a second node start a
  duplicate.

  Deletes now run in 1 000-row chunks, yielding between them, capped at 5 000
  chunks per tick with the remainder left for the next one. The statement is
  identical on all three backends — the nested derived table is what makes
  MySQL accept a subquery over the table being deleted (error 1093).

  No configuration change and no behaviour change beyond the locking profile:
  the same rows are removed.

- **The OpenAPI document now describes every response it serves.** Measured
  against the committed `docs/openapi.json`: **44 of the 48** 2xx responses had
  no `content` block, as did **30** declared 4xx/5xx — the spec named a status
  and said nothing about its body, so generated clients got `any` where a type
  belonged. All 45 body-carrying 2xx and all 141 error responses are now typed
  (`204` stays bodiless, as it must). Two tests hold the line: one fails on any
  response that declares a status without a schema, the other on any storage row
  struct being published.

  Also corrected: `Workflow`, `Channel` and `Trace` were registered as schemas
  and referenced by **nothing**. They describe database rows — `condition_json`
  and `tasks_json` as opaque **strings** — while the endpoints return
  `WorkflowResponse`/`ChannelResponse` with those fields parsed. The row structs
  are gone from the document and the DTOs are published in their place.
  `Connector`, `TraceDlqEntry` and `AuditLogEntry` stay: their handlers do
  return them verbatim.

  This is spec-only — no endpoint changed shape. Regenerate clients to pick up
  the types.

- **A duplicate `create` answers `409`, not `500`.** `POST /admin/workflows`
  and `POST /admin/channels` with an existing id returned
  `{"code":"INTERNAL_ERROR"}` for a plain client error — connectors already
  said `409`, and the existing tests asserted only `is_err()`, so they passed
  on the wrong status. A shared `map_duplicate` helper now maps both duplicate
  shapes to `CONFLICT`: the structured `UniqueViolation` kind (Postgres'
  partial unique index, primary-key collisions on every backend) and the
  generic errors the SQLite/MySQL single-draft triggers raise, which carry no
  kind sqlx can classify and are matched on the trigger's message. **Retry
  logic that treated the 500 as transient must treat the 409 as permanent** —
  pick a different id, or use the import endpoints, which report conflicts per
  item without failing the batch (D16).

- **`GET /admin/workflows/export` no longer materialises every workflow in one
  query.** `WorkflowRepository::list` ignored the `limit`/`offset` its own
  filter carries and skipped the `timed_db_op` wrapper every sibling has — and
  it backed export, so one admin request loaded every current workflow with
  full `tasks_json` at once. `list` now honours the filter (clamped to the same
  50-default / 1000-cap as every list, with a `workflow_id` tiebreaker so
  paging cannot skip or repeat rows) and is instrumented; export pages through
  it 500 rows at a time until exhausted and still returns the complete result.

  **The export is no longer a point-in-time snapshot.** Each page is an
  independent query with no transaction spanning them, so a workflow created,
  deleted or renamed mid-export can be missed or appear twice in one response.
  Quiesce workflow mutations (or re-export until two consecutive responses
  match) if you use export as a backup (D7).

- **`claim_pending` no longer formats a runtime value into SQL text.** The DLQ
  claim was the last hand-written SQL under `src/storage/`: three backend arms
  interpolated `limit` into the statement and hand-wrote six column
  identifiers, so a rename in `schema.rs` compiled and failed only at runtime,
  on all three backends. Every arm is sea-query built now — the limit travels
  as a bound parameter, identifiers come from the `Iden` enum, and the
  exhaustion predicate is built once and shared with the DLQ list filter and
  purge. A per-backend rendered-SQL shape test pins `RETURNING`,
  `FOR UPDATE SKIP LOCKED` and the placeholder limit (D25).

- **Per-channel limiter and backpressure state survives an engine reload.**
  Every admin mutation — and, in cluster mode, every epoch resync on every node
  — rebuilt the channel registry with fresh rate limiters and semaphores,
  refilling every consumed burst and forgetting every in-flight permit, so a
  caller could bypass a per-channel limit by causing (or waiting for) a reload.
  The registry now reuses a channel's limiter while `(requests_per_second,
  burst, key_logic)` is unchanged and its semaphore while
  `max_concurrent_per_node` is unchanged (N6).

- **A failed Kafka consumer restart no longer stops ingestion permanently with
  every probe green.** Engine reload took the consumer handle out of its mutex
  and, when the restart errored, only logged — so a transient broker outage
  during any reload silenced ingestion for the process lifetime while the pod
  stayed in rotation. The restart path now flags ingestion degraded (mirrored
  to the `orion_kafka_ingest_degraded` gauge) and spawns a single-occupancy
  supervisor that retries with capped exponential backoff (1 s doubling to
  60 s), re-reading the active channel list on each attempt so topic changes
  made while ingestion was down are honoured, and standing down on recovery,
  when no topics remain, or when the node drains. The supervisor releases its
  occupancy slot while still holding the consumer-handle mutex, closing a
  window where a reload failing between the unlock and the release spawned no
  replacement and left the node degraded with no supervisor. Boot, reload and
  the supervisor now start consumers through one shared builder, so the three
  paths cannot drift (K7).

- **Rebalances no longer lose in-flight offset commits.** The consumer ran with
  rdkafka's default context — no `pre_rebalance`, no `post_rebalance`, no
  `commit_callback` — while committing asynchronously, so an unconfirmed commit
  was simply lost on revocation and failures were logged at the enqueue site
  and nowhere else. A `ConsumerContext` now flushes unconfirmed commits
  synchronously in `pre_rebalance` while the consumer still owns the
  partitions, records revoked partitions in shared state the message loop
  checks before working a message and again before committing it (abandoning it
  uncommitted for its new owner), and surfaces async commit failures through
  `commit_callback` with an `orion_errors_total{reason="kafka_commit"}` count
  instead of silence (K8).

- **A failing Kafka message no longer retries its consumer out of the group.**
  The in-place retry loop blocks polling, so retrying without a cap meant
  eviction once the poll gap passed `max.poll.interval.ms` — while the consumer
  kept working, and would finally commit, a partition it no longer owned.
  Retrying in place is now bounded to 80% of `max.poll.interval.ms` (240 s
  against librdkafka's 300 s default, derived from `kafka.extra_config` when it
  sets the property). On expiry the consumer seeks the partition back to the
  message's offset and returns to the poll loop, so the message is redelivered
  — neither committed nor dropped, at-least-once intact — rebalance callbacks
  fire, and group membership is kept. Head-of-line blocking on a poison message
  is unchanged: enabling `[kafka.dlq]` remains the fix. Each expiry counts
  `orion_errors_total{reason="kafka_retry_budget_exhausted"}` alongside the
  existing `kafka_retry` counter (K8).

- `default_resolvers()` is built once per connector reload instead of once per
  connector inside the load loop (N23).

- **MongoDB connectors now honour `max_connections` and `connect_timeout_ms`.**
  Both live on the same `db` connector struct the SQL path reads, and the SQL
  pool applied both while the Mongo client applied neither — so an unreachable
  Mongo host waited on the driver's 30 s server-selection default instead of
  the configured timeout, stalling the request rather than failing it. The
  timeout now caps server selection as well as connection.
- **The shipped `config.toml.example` now loads on a clean machine.**
  Placeholder substitution runs over the raw file text before TOML parsing, so
  the `${VAR}` in the header comment that *documents* the placeholder syntax —
  and three `${CONFLUENT_API_KEY}`-style examples further down — were read as
  required variables. Copying the example and starting Orion failed with
  *"Required environment variable 'VAR' is not set"*. The comments now use the
  `$$` escape, and the drift test loads the file through the real entry point
  instead of only parsing it as TOML.
- **One unusable workflow no longer takes down the whole instance.** Task input
  parsing runs inside engine construction, after the loader has decided what to
  load, so a stored row that fails it aborted the process at boot and took every
  channel on every node down on reload — defeating the per-channel quarantine.
  Unregistered function names and malformed `channel_call` inputs are now
  detected during the load and quarantine only their own channel.
- **`channel_call` accepts a `channel_logic`-only task.** The schema, docs and
  validation rule all declare `channel` optional when `channel_logic` is given,
  but the input struct required it, so such a workflow passed admin validation
  and then failed the engine build with `missing field 'channel'`.
- Unknown or archived channels return 404 on the data plane rather than a
  generic engine error.
- `channel_call` refuses a missing target instead of failing opaquely; the
  recursion depth and cycle guards are now covered by tests.
- TTL stores and circuit-breaker cooldowns use a monotonic, pausable clock, so
  a wall-clock step no longer extends or shortens either.

- **`include` grouping compared join keys by their JSON text, so a key column
  typed differently on the two sides produced silently empty child arrays.** A
  parent key read back as `"7"` from a `TEXT` column never matched a child
  foreign key read back as `7` from a `BIGINT` one — the parents came back with
  `[]`, no error and no warning anywhere. Grouping now goes through a typed key
  that normalises integral values however the driver rendered them (`7`, `7.0`,
  `"7"`), while `"007"` and `" 7"` keep their own identity (W14).

- **Kafka deduplication no longer destroys a record that needs a retry.** The
  idempotency key is claimed before the workflow runs and settled by the commit
  decision. A redelivery of an offset that was never committed presents the
  record's own coordinates, is recognised as the same delivery, and runs; only
  a delivery arriving after the key was settled is skipped. A backpressure
  refusal releases the key it claimed, and only a deterministic refusal —
  `validation_logic`, or a `rate_limit.key_logic` that cannot be evaluated — is
  dead-lettered (N16).

- **A per-channel rate limit refused by `channel_call` reaches the caller as
  its own `429`/`503`** instead of a generic `500 ENGINE_ERROR`. Clients
  matching on `ENGINE_ERROR` for these conditions need updating (S15).

- **`rate_limit.trusted_proxies` now applies with `rate_limit.enabled =
  false`.** It was reachable only through the platform limiter's state, which
  is absent in exactly the configuration per-channel limits exist for — so
  behind any proxy every client keyed on the proxy's address and shared one
  bucket. It is read from config now, so the per-channel limit, the audit
  trail's `details.client_ip` and the failed-auth backoff all resolve the
  caller honestly on a proxied deployment that has rate limiting off (the
  default). Previously the last two recorded the load balancer's address
  (S15, O7).

- **Every whitelisted trace sort column is now indexed, and the index migration
  does not block writes.** `sort_by=updated_at` had been in the sort whitelist
  since 0.1 with no index behind it on any backend, so each page full-scanned
  and filesorted the hottest table in the schema. New migrations add
  `idx_traces_updated_at` and replace `idx_traces_created_at` with
  `(created_at, id)` — a strict superset that also serves the retention
  delete's `created_at < cutoff` and turns the keyset predicate into one index
  range scan. On PostgreSQL the work is done `CONCURRENTLY` across three
  single-statement migrations — `010` and `011` build, `012` drops the
  superseded index: a plain `CREATE INDEX` takes a
  `SHARE` lock that blocks every insert and update for the length of the build,
  which on a large `traces` table is a write outage. MySQL states
  `ALGORITHM=INPLACE LOCK=NONE` so an engine that cannot build online fails the
  migration rather than locking the table silently. SQLite has no online build
  and needs none — it is single-node and the migration runs before the listener
  binds (D8).

- **A channel broken in two ways at once could panic the reload's log line.**
  Such a channel contributes two entries to the issue list and one to the
  quarantine map, so the "some channels failed to load" message's
  `channels.len() - issues.len()` could underflow. Both counts are now taken
  from the published maps (N17).

- **`GET /api/v1/admin/trace-dlq` advertised `payload_json` and `metadata_json`
  in its OpenAPI schema and has never returned either** — it selects a
  payload-free projection so one request cannot dump every failed request's
  body, but the published schema claimed both fields because the row struct
  that *did* have them was also the wire type. The listing's response body is
  unchanged; the document now describes it, via a `TraceDlqSummaryResponse`
  schema distinct from the single-entry `TraceDlqEntryResponse`. Fetch a single
  entry for the payload (D28).

- **A driver error on the audit-log listing's data read reported
  `INTERNAL_ERROR` while the count half of the same query reported
  `STORAGE_ERROR`.** Both now report `STORAGE_ERROR`, matching every other list
  endpoint. Still a 500 (D18).

- **Channel runtime controls hold (proposal N2, N5).** Responses carrying task
  errors are no longer cached, so a transient downstream failure is not pinned
  for the full TTL and replayed to every caller. A `rate_limit.key_logic` that
  does not compile now quarantines the channel, and an evaluation failure
  rejects the request with `429` — previously both fell back to `client_ip`,
  silently turning a per-tenant limit into a per-IP one.
- **Egress correctness round (proposal F8, F10–F13, F17).** `http_call` no
  longer retries non-idempotent methods by default — a timed-out POST was
  re-sent up to 3× with no idempotency key; set the new HTTP-connector
  `retry_non_idempotent` to restore the old behaviour. Retries are also
  bounded by a deadline instead of running attempts plus backoff past the
  channel timeout. `publish_kafka` now publishes to the brokers its
  connector names rather than always the globally configured cluster.
  `db_read`/`mongo_read` enforce `query.max_limit` as a hard row cap, every
  MongoDB path honours `query_timeout_ms`, Elasticsearch responses respect a
  new `max_response_size` on the ES connector, and evicted connector pools
  are closed instead of leaking their connections on every connector edit
  and cluster epoch resync.
- **Routing and rollout truth (proposal F30, F33, R5, F32).** Channels whose
  workflows cannot be built — missing or unconvertible workflow, or rollout
  percentages that don't sum to 100 — are now quarantined with the reason on
  `/health` instead of silently serving engine errors or blackholing part of
  the traffic. Workflows with unknown functions are rejected at create;
  activation requires every referenced connector to exist. The channel
  include/exclude glob matcher gained real backtracking and boot logs the
  resolved channel list when filters are configured.
- **Queue durability round (proposal Q4–Q8, N15, D4).** A DLQ backoff shift
  overflow no longer kills the retry task (`dlq_max_retries` is now bounded
  1–16); a DB error on the "mark running" write routes the message to the DLQ
  instead of dropping it with the trace stuck `pending`; failed persistence
  writes retry (50ms/250ms) before being counted and dropped, and batch
  buffers are no longer cleared on error before that retry;
  `async_workers`/`batch_workers` > 1 now actually run in parallel (per-worker
  receivers, round-robin fan-out); `trace_storage.batch_size` is bounded at
  1000 so batch flushes cannot exceed SQLite's bind limit; `task_trace_json`
  is capped by `queue.max_result_size_bytes` on both paths; and trace
  retention reclaims pending/running rows older than twice the retention
  window instead of leaking them forever.

- **Connectors authored the documented way never loaded.** `ConnectorConfig` is
  internally tagged on `type`, but the type lives in its own column and the API
  takes it as a sibling `connector_type` — so a config without a redundant
  `"type"` inside it failed to deserialize and the connector was silently
  skipped. That is the shape every example, the OpenAPI spec and any admin UI
  produce. The stored column is now the single source of truth.
- **A connector `GET` → edit → `PUT` round-trip persisted `"******"` as the
  credential.** Fields returned masked are now restored from the stored row,
  and a mask with no stored counterpart is a 400 naming the field instead of a
  silent credential overwrite.
- **Per-channel rate limits never applied to REST-routed channels.** The
  middleware matched the first path segment against channel names; for a REST
  channel that segment is the route prefix, so the limiter was never found and
  the channel fell through to the platform-wide limit. Channel resolution now
  mirrors the data handler exactly, including the `/async` suffix.
- **One broken channel made the instance unmanageable.** A channel whose stored
  config no longer parses used to fail *every* admin operation that triggers a
  reload — activate, archive, delete, rollout — with a 500, and stopped the
  cluster epoch watcher resyncing all nodes. Such channels are now quarantined
  individually: still refused at every ingress (with a 503 naming the reason,
  and routed to the DLQ on the Kafka path), but the rest of the reload
  succeeds. Boot no longer aborts over one bad row either.
- **Connectors that failed to load vanished without a signal.** Env
  substitution, JSON parsing, secret resolution and deserialization failures
  are now recorded and reported on `/health` and the admin list.
- **`ORION_RATE_LIMIT__ENABLED=true` with no config file failed startup
  validation.** `RateLimitConfig` and `AppConfig` derived `Default` while also
  carrying `#[serde(default = "…")]` attributes with different values, so "the
  default" depended on how the config was produced. Both now implement
  `Default` in terms of their `default_*` functions, and the config-docs drift
  test fails on any future divergence.
- **The async path and Kafka ingest bypassed every per-channel control.**
  CORS, `validation_logic`, deduplication, and backpressure lived only in the
  sync HTTP path, so appending `/async` to a URL defeated a channel's input
  contract, and `channel_call` skipped the target channel's guards entirely.
  All ingress paths now share one guard layer, and an async request holds its
  backpressure permit for the whole of processing, so `max_concurrent` bounds
  sync and async traffic together.
- **The response cache could serve one caller's data to another.** The key
  hashed only the request body, so for a REST channel with a path parameter and
  an empty body (`GET /orders/{id}`) every id collided onto one entry. Method,
  route parameters, and query string are now part of the key.
- **Kafka messages were lost on failure.** Offsets committed unconditionally,
  so with the DLQ disabled (the default) a workflow error, timeout, or unmapped
  topic silently discarded the message. Delivery is now at-least-once: offsets
  advance only on success or a confirmed DLQ write, and UTF-8-decode failures,
  empty payloads, and unmapped topics are dead-lettered instead of dropped.
- **Poison messages retried forever.** A failing async trace re-entered the DLQ
  as a fresh row at `retry_count = 0`, so `dlq_max_retries` could never be
  reached and each cycle inserted another `traces` row. The retry count now
  travels with the message and exhausts as documented.
- **The trace queue blocked instead of shedding.** A full buffer parked the
  request indefinitely; it now returns 503, as the configuration already
  documented.
- **Postgres DLQ retry and exhaustion silently failed.** Clearing a claim lease
  bound a TEXT parameter to a `timestamp` column (Postgres error 42804) and all
  three call sites discarded the error, so in cluster mode on Postgres entries
  never backed off, never exhausted, and were re-claimed forever.
- **SSRF protection was incomplete.** Redirects were followed without
  re-validation (reaching cloud metadata via a 302), the validated DNS result
  was discarded and re-resolved (rebinding), IPv6 private ranges were largely
  unchecked, and the Elasticsearch connector skipped validation entirely.
  Redirects are now followed manually with per-hop validation, connections are
  pinned to validated addresses, and the private-range coverage is complete.
- **Rate limiting was trivially bypassed.** The client identity came from
  unvalidated forwarded headers with no peer-address fallback, so direct
  clients shared one bucket and proxied clients could mint a new identity per
  request. See `rate_limit.trusted_proxies` above.
- **Channels with broken configuration served unguarded.** An unparseable
  `config_json` silently loaded a default (no rate limit, validation, dedup,
  backpressure, timeout, or cache) and an uncompilable `validation_logic` was
  dropped with a warning. Both now refuse to load the channel.
- **`db_read` turned unreadable columns into `null`.** `REAL`/`float4` and blob
  columns silently read back as null on every SQL read path; genuinely
  unsupported types now error rather than looking like a NULL value.
- **Unimplemented secret schemes were used as literal passwords.**
  `vault://…`, `aws-sm://…`, and friends passed through verbatim as the
  credential; they are now rejected at connector load.
- **Credentials embedded in URLs leaked through the admin API.** Masking was a
  flat key-name denylist that missed `url` and `brokers[]`, so
  `redis://:PASSWORD@host` was returned in full. Masking is recursive and
  strips userinfo from URL-shaped values at any depth.
- **A restart of cluster Redis broke every node permanently.** The shared
  connection never re-established, silently disabling distributed dedup,
  response caching, and rate limiting until pods restarted.
- **An open circuit breaker returned 500 `ENGINE_ERROR`** instead of the
  documented 503 `CIRCUIT_OPEN`, so callers could not distinguish shed load
  from a server fault and the DLQ retry classifier never saw it as retryable.
  Timeouts and 503s are now classified retryable.
- **Internal error detail leaked to anonymous callers.** Success bodies
  embedded raw upstream URLs, sqlx errors, and connector names; the data plane
  now returns a code, a generic message, and a request id, with full detail
  kept in the persisted trace.
- **Unbounded Prometheus label cardinality.** Rate-limit rejections were
  labelled with a spoofable client IP, and channel-labelled metrics accepted any
  attacker-supplied path segment.
- **`PUT /channels/{id}` ran no validation at all**, and `PUT /connectors/{id}`
  skipped config validation unless the type was resent. Both now validate
  against the stored record.
- **A CORS list mixing `"*"` with explicit origins passed validation and then
  panicked at router build**, killing the server at boot; `PATCH` was missing
  from the allowed methods, making the admin status and rollout endpoints
  unusable cross-origin.
- **Admin API keys were compared with an early length check**, leaking key
  length by timing.
- **TLS was unusable — `server.tls.enabled = true` panicked at boot.**
  `RustlsConfig::from_pem_file` failed with *"Could not automatically determine
  the process-level CryptoProvider"*: rustls 0.23 auto-selects a backend only
  when exactly one is enabled, and Orion's dependency graph enables both
  (`axum-server` + `reqwest` pull `rustls/aws-lc-rs`; `mongodb` + `sqlx` pull
  `rustls/ring`). The server now installs the `aws-lc-rs` provider explicitly
  before loading certificates. **If you tried HTTPS, hit the panic, and
  terminated TLS at a proxy instead, it works now.** Covered by new TLS
  integration tests — the test debt was the bug.
- **MySQL as Orion's own storage backend never worked** — the migration set
  used mysql-client `DELIMITER` directives, TEXT columns with defaults, and
  TEXT primary keys, none of which MySQL/sqlx accept. Rewritten with the
  VARCHAR/datetime idiom; covered by container tests.
- **Postgres storage was unusable at runtime** — models decode `i64` but
  columns were `INT4` (every repository read failed), and chrono timestamps
  were bound as TEXT, which Postgres rejects against timestamp columns. Both
  fixed (new `004_bigint_columns.sql`); covered by container tests.
- **Dedup idempotency keys are now channel-scoped** (`dedup:{channel}:{token}`)
  — raw tokens previously collided across channels sharing a backend — and a
  dedup-store outage now fails open (requests allowed, `dedup_backend` error
  metric) instead of rejecting everything with 409.
- **Trace read endpoints require admin auth** — `GET /api/v1/data/traces` and
  `/traces/{id}` return full payloads but were unauthenticated even with
  `admin_auth.enabled = true`.
- **Rolling-deploy drain** — on SIGTERM, `/readyz` now flips to 503
  immediately while the node keeps serving through `shutdown_drain_secs`
  (so the LB drains it gracefully), then stops accepting and bounds the
  in-flight wait with `server.shutdown_force_timeout_secs`. Previously TLS
  stopped accepting instantly and plain HTTP never withdrew readiness.
- **`queue.dlq_max_retries` is honored** (the enqueue path hardcoded 5) and
  values `< 1` are rejected at startup; `traces.channel_id` is now populated
  on every insert path; active-immutability triggers now exist on Postgres
  and MySQL, not just SQLite.

### Changed

- **Shared wire-contract and transport crates.** The response DTOs, domain
  enums, error envelope (with a stable `error.code` registry) and the
  bulk-import report moved to the workspace's `orion-api` crate, and the
  admin-API HTTP client behind `orion-server package` moved to
  `orion-client` — the same crates orion-cli builds on, so a client can no
  longer drift from the server's wire shapes. Server code paths re-export
  everything under their old names and the wire is byte-compatible; the one
  spec-visible improvement is that the OpenAPI document now describes
  `ImportResult.errors[]`/`results[]` with typed
  `ImportItemError`/`ImportItemResult` components instead of untyped arrays.
  Neither library crate is released on its own: crates-publish ships them as
  skip-if-published riders right before a binary crate.
- **orion-cli is versioned in lockstep with the server and released with it.**
  A bare `vX.Y.Z` tag now announces both packages, so the CLI's installers,
  Homebrew formula and crates.io release ship from the same tag as the
  server's rather than from a separate `orion-cli-v*` cycle. The prefixed
  tags remain available for shipping one package alone.
- **Handler classification moved off the error message and onto the error.**
  A handler must return a `DataflowError`, and before 3.1 that enum was closed
  with no extension point — so three classifications lived as *prefixes on the
  message text* (`orion.circuit_open: `, `orion.connector_detail: `,
  `orion.channel_refused: `), matched back out with order-sensitive
  `starts_with` guards. They also leaked verbatim into `traces.error` and
  `trace_dlq` rows, because the async path hands `e.to_string()` straight to
  the failure handler and nothing stripped them. `DataflowError::Service`
  carries `kind` as a field the engine never interprets, plus an operator-only
  `detail` that `Display` never renders. Responses are unchanged; the tokens
  are gone from persisted text, and a genuine downstream `429`/`503` relayed by
  `http_call` can no longer be confused with one of Orion's own refusals.

- **`datalogic-rs` and `datavalue` are no longer direct dependencies.**
  dataflow-rs's public API is expressed in terms of both — `TaskContext::datalogic()`
  returns `&Arc<datalogic_rs::Engine>`, `TaskContext::eval` takes a
  `&datalogic_rs::Logic`, and the context and dot-path surface is
  `datavalue::OwnedDataValue` — so naming those types required a second,
  independently versioned pin of each. 3.1 re-exports both, and Orion now reaches
  them as `dataflow_rs::datalogic_rs` / `dataflow_rs::datavalue`, which locks
  their major versions to whatever dataflow-rs links. Without that, a future
  dataflow-rs moving to datalogic-rs 6 would put *both* majors in the graph and
  make `engine.datalogic()` return a type nominally identical to, but
  incompatible with, the `Logic` values Orion holds in its channel registry.
  Nothing changes at runtime: the resolved versions and the enabled feature set
  are identical, because Orion's features were already a subset of dataflow-rs's.
  The cost is that Orion can no longer turn on a datalogic feature unilaterally —
  `ext-string`, `ext-array`, `ext-math` and the date operators now depend on
  dataflow-rs enabling them.

- **`channel_call` compiles its JSONLogic once instead of per message.**
  `channel_logic` and `data_logic` were the last two
  `ctx.datalogic().compile(..)` calls in the handler surface, so both
  expressions were re-parsed and re-compiled on **every** message while
  `http_call` and `publish_kafka` got a compiled expression for free from
  dataflow-rs's typed configs. Both are now `Template` fields compiled at
  engine construction, and evaluate on the worker's pooled arena rather than
  allocating one per call. A malformed expression is still a per-channel load
  issue, not a boot abort: the loader compiles them ahead of `Engine::new`.

- **`migrate` output names the backend and every pending migration (D13).**
  Migration version numbers are per-backend and are not comparable: `004` is
  `cluster_coordination` on SQLite, `bigint_columns` on PostgreSQL and
  `active_immutability` on MySQL. `--dry-run` printed `4 — cluster_coordination`,
  so reading it correctly meant already knowing which backend you were on.
  Both `migrate` and `migrate --dry-run` now print the backend in the header
  and on every row (`postgres 013 — json column suffixes`), and the dry run
  states outright that the numbering does not line up across backends. The
  apply path lists what it is about to run rather than only a count.

  A single backend-agnostic version space was considered and rejected: it would
  have forced every existing deployment through a hand-run `_sqlx_migrations`
  rewrite, where a mistake stops the database from starting — a large one-way
  risk to fix a labelling problem. Referring to migrations by name solves the
  same thing. `CONTRIBUTING.md` and the upgrade guide now say so explicitly.
  (The `migrate` subcommand also gained its first tests through the binary.)

- **The last two JSON columns got the suffix every sibling has.**
  `workflows.tags` is now `workflows.tags_json` and `channels.methods` is now
  `channels.methods_json`. They were the only two columns holding a serialized
  JSON document without the `_json` that `condition_json`, `tasks_json`,
  `config_json`, `input_json`, `result_json` and the rest all carry — and since
  the storage type is `text` either way, the suffix is the only signal a reader
  gets that a value has to go through `serde_json` before it means anything.

  **The HTTP API is unchanged.** `tags` and `methods` are still the field names
  every workflow and channel endpoint publishes; `docs/openapi.json` is
  byte-identical. The renames are physical, and the DTOs that reach the network
  name their own fields. What breaks is anything reading Orion's tables
  directly — a dashboard, an ETL job, a hand-maintained view.

  The rename is not one migration three times. SQLite rewrites the stored SQL
  of dependent views and triggers, so its migration is two `ALTER` statements.
  PostgreSQL and MySQL store view target lists and trigger bodies as resolved
  text, and **neither fails at rename time**: Postgres would leave
  `current_workflows` publishing a column called `tags` over a table whose
  column is `tags_json`, then raise `record "old" has no field "tags"` from the
  active-immutability guard on the next update of an active row; MySQL's views
  would go *invalid* rather than stale — gone from `information_schema`, taking
  every latest-version read with them — and its triggers would fail
  `Unknown column 'tags' in 'OLD'`. Both migrations therefore drop and recreate
  both views and both immutability triggers around the rename, and both upgrade
  paths are exercised over seeded rows rather than empty tables (D26).
- **`traces.access_token_hash` is `char(64)` on MySQL** (`text` elsewhere,
  which is already the right answer there). It holds one thing — the hex
  encoding of a SHA-256 digest — and MySQL is the only backend that cannot
  index a `TEXT` column without a prefix length, so the declared type quietly
  forbade the obvious index. Nothing indexes the column today; this removes the
  trap rather than adding an index (D26).
- CI and CodeQL now run on `release/**` and `v*` branches. The release
  workflows require a successful CI run at the tag SHA, which no commit on a
  release branch could previously have.
- **The shipped deployment artefacts are hardened and pinned.** The Helm chart
  had no `securityContext` anywhere, failing Pod Security Standards
  `restricted` and every policy scanner out of the box; it inherited
  Kubernetes' `maxUnavailable: 25%`, which at 2 replicas removes a pod before
  its replacement is Ready and defeats the graceful-drain design; the migrate
  Job carried the full `postgres://user:pass@…` URL as a plain env value,
  visible in `kubectl get job -o yaml` and every audit sink; and the compose
  files floated on `:latest` while `docker-compose.ha.yml` set `build:`
  alongside `image:`, so `docker compose build` silently overwrote the
  published tag with a dev build.

  The Deployment and the migrate Job now run non-root with a read-only root
  filesystem, `allowPrivilegeEscalation: false`, all capabilities dropped and
  the `RuntimeDefault` seccomp profile (all values-overridable); the image
  pins its user to numeric UID/GID `10001` — the kubelet cannot verify
  `runAsNonRoot` against a named `USER` — and the chart's
  `runAsUser`/`runAsGroup`/`fsGroup` match, which also makes freshly
  provisioned PVCs writable. The read-only rootfs gets an emptyDir at `/tmp`
  and a data volume at `/app/data`, with new `persistence.*` values providing a
  kept-on-uninstall PVC for single-node SQLite installs (and `backup_dir`
  pointed at it — with a read-only rootfs, `POST /admin/backups` needs either
  `persistence.enabled` or a `storage.backup_dir` under a writable mount).
  `spec.strategy` is explicit (`maxUnavailable: 0`,
  `maxSurge: 1`), a soft pod anti-affinity spreads replicas across nodes with a
  `topologySpreadConstraints` passthrough, and a `startupProbe` on `/healthz`
  gives boot a five-minute budget before liveness takes over. The migrate Job
  reads the URL through `secretKeyRef` in both the install and the upgrade case
  via a hook-scoped copy of the storage Secret, leaving the Secret the server
  reads a normal release resource. All three compose topologies pin
  `ghcr.io/goplasmatic/orion:${ORION_VERSION:-1.0.0}`, with local HA builds
  moved to the `docker-compose.ha.build.yml` override that retags them as
  `orion:local`. Finally, `.dockerignore` excludes `.git/`, so every released
  container reported `git_hash=unknown` from `/health`, `/metrics` and
  `--version` — the Dockerfile now takes `ARG GIT_HASH`, `build.rs` prefers an
  already-set env var, and both the release and CI image builds pass the commit
  SHA (P2, P4, P5, P6, P7, P10, P11, C23).
- **CI gates licenses and supply chain, not just advisories.** `cargo audit`
  covered advisories only: no license-compatibility check across the ~600-crate
  tree, and unmaintained or yanked crates passed silently. `cargo deny check`
  replaces it against a new `deny.toml` gating advisories (carrying over the
  documented RUSTSEC-2023-0071 `rsa`/sqlx-mysql ignore), an Apache-2.0-compatible
  license allow-list, wildcard and source bans, and yanked crates — which
  surfaced and removed the yanked `spin 0.9.8`. Alongside it: Dependabot version
  updates for `cargo` and `github-actions`, weekly — cargo minor/patch bumps
  grouped with majors raised separately, Actions bumps grouped together — the
  automation `SECURITY.md` already claimed — plus a `CODEOWNERS`
  file routing every PR to the active maintainer; a pinned-mdbook build job on
  every PR with `create-missing = false`, so a dangling `SUMMARY.md` entry fails
  the build instead of fabricating an empty page; concurrency groups that cancel
  a superseded PR run instead of burning the full matrix, while branch pushes
  group by commit SHA so every pushed SHA runs to completion and the
  release-time gate always finds a completed run at the tagged SHA; and
  `tests/README.md` back in step with CI's container-test filter, which was
  missing `db_column_types_test` and `dynamic_inputs_test`
  (T12, T17, T19, T25, C17).
- `resolve_write` enforces the `TooManyRows` / `UnfilteredMutation` /
  `UnfilteredNotAllowed` guards itself, behind a `&WriteConfig`, instead of
  leaving them to the `data_write` handler — the function documented as doing
  "the whole backend-neutral transformation" was unsafe to call alone.
  Handler-visible behaviour is identical (W15).

- **The channel registry rebuilds only what changed, and publishes one
  snapshot.** `ChannelRegistry::reload` re-deserialised every channel's
  `config_json`, recompiled two datalogic programs, re-resolved two cache
  backends and cloned the `Channel` row twice — for every channel, on every
  admin mutation, and in cluster mode on every epoch tick on every node — then
  rebuilt the whole route table, whose conflict scan is quadratic in
  route-bearing channels. So the price of a reload scaled with the number of
  channels rather than the number of changed ones. A channel whose stored row
  (`channel_id`, `version`, `updated_at`) and dependency fingerprint are
  unchanged now keeps the exact runtime configuration it already had, and an
  unchanged serviceable set keeps the built route table. The saving reaches
  remote nodes because `ConnectorRegistry`'s config generation now advances
  only when a load actually changed the connector set: the epoch resync reloads
  connectors on every tick whatever the mutation touched, so a per-load token
  would have invalidated every channel on every node but the mutating one.
  Rate limiters and backpressure semaphores continue to carry their state
  across reloads.

  `by_name`, the route table and the quarantine map were also three locks
  swapped one after another, so a request landing mid-reload could pair a new
  serving map with an old route table — and a channel that reload had just
  quarantined read as neither serving nor quarantined, which the data plane
  answers `404 unknown channel` instead of `503`. They are one immutable
  snapshot published in a single atomic store, so every read sees one
  self-consistent generation and the registry's five read paths are wait-free.

  **Two log lines change cadence as a result:** `RouteTable`'s conflict warning
  is not re-emitted while the serviceable set is unchanged, and a channel
  running on the in-memory fallback because its cache connector is unavailable
  does not re-log that on every reload. Both are still reported on the reload
  that introduces or changes the condition, and both remain visible in the
  state they describe. Alert on a first occurrence or on state, not on
  recurrence (N17).

- **The set of quarantined channels has one representation.**
  `ChannelRegistry::reload` returned a list of load issues that duplicated the
  map it had just written — the boot path read the list, the engine-reload path
  discarded it, and `/health` read the map, so two sources could disagree about
  a channel broken in both the engine build and its own config. `reload` now
  returns nothing and every caller reads the map (N21).

- **Text-match case sensitivity is stated per backend instead of hidden in a
  code comment.** `starts_with` / `ends_with` / substring `in` behave five
  different ways — PostgreSQL `LIKE` is case-sensitive, SQLite's folds ASCII
  only, MySQL follows the column's collation, MongoDB `$regex` is
  case-sensitive, and Elasticsearch depends on the field's analyzer — and the
  only record of any of it was a parenthetical about MySQL in the SQL renderer.
  It is a property of the stored data rather than of the query, and no
  query-time flag can make an analyzed Elasticsearch `text` field
  case-sensitive again, so the parity table now carries the per-backend truth
  and each renderer points at it rather than half-normalising. No code
  behaviour changed (W13).

- **A trace's `access_token_hash` — a credential verifier — rode on a struct
  that was `Serialize + ToSchema`, kept off the wire by one `skip_serializing`
  attribute, and the trace listing read it out of the database for every row on
  every page via `SELECT *`.** Row structs no longer derive `Serialize` or
  `ToSchema` at all, so "does this column leave the process?" is a property of
  the type rather than of an attribute someone remembered; the trace listing
  now names its eleven columns and decodes into a narrower row, leaving the
  token hash and all three payload columns in the database (D27).

- **`models.rs` mixed row structs, response DTOs, domain enums, wire constants,
  a handler helper and 213 lines of tests, while two more row structs lived in
  the repositories that read them — so there was no answer to where a new type
  belongs.** Split into `models/{rows,dto,enums}.rs`, one rule per file, with
  `TraceDlqSummary` moved in alongside the other rows and `StatusAction` moved
  out to the admin route module that is its only caller. The rule "a row struct
  never derives `Serialize` or `ToSchema`" is compiler- and test-enforced: a
  scan of `rows.rs`'s own derives, plus an OpenAPI check that rejects every
  row-struct name in the published document instead of exempting three of them.
  Connectors, audit logs and the two DLQ reads now serve `*Response` DTOs;
  field sets are unchanged and pinned by tests (D28).

- **`versioned::paginate` served two of seven list paths; traces, the trace
  DLQ, audit logs, connectors and version history each re-implemented
  count-then-page, so every pagination fix had to land six more times than it
  should have.** It is now `helpers::paginate`, taking a `Page` with an
  explicit `Projection` for the two lists that read a deliberately narrow
  column set. Six call it directly; the trace list composes the same halves
  (`page_select` + `count_where`) because its `total` is conditional and it
  appends a cursor — it shares the `Page` and the contract, not the call. A
  contract test asserts one paging
  behaviour — `total` ignores `limit`/`offset`, `limit` clamps to `[1, 1000]`,
  `offset` skips, an absent `limit` means 50 — against every one of them (D18).

- **The three-backend `FromRow` bound was written out in eight signatures
  across `storage/mod.rs` and `helpers.rs`, though the collapsing trait already
  existed** — `pub(crate)`, and named `VersionedRow` after the one module that
  did not need it. Renamed `DbRow`, moved next to the `DbPool` fetches that
  define it, and applied to all eight (D19).

- **If you embed Orion as a crate rather than running the binary,** four
  signatures moved with the above: `storage::models::{Workflow, Channel,
  Connector, Trace, TraceDlqEntry, TraceDlqSummary, AuditLogEntry}` no longer
  implement `Serialize` (nor `ToSchema` for `Connector`, `TraceDlqEntry` and
  `AuditLogEntry`, the three that had it) — convert to the
  matching type in `storage::models::dto` first, and for connectors use
  `connector::mask_connector`, now the only constructor of `ConnectorResponse`
  and the one that masks secrets on the way;
  `TraceRepository::list_paginated` returns `TracePage` (with
  `total: Option<i64>` and `next_cursor`) rather than `PaginatedResult<Trace>`,
  so custom implementations and test doubles need the new return type;
  `storage::models::TraceDlqSummary` replaces
  `storage::repositories::trace_dlq::TraceDlqSummary`; and
  `storage::repositories::versioned::{paginate, VersionedRow}` are gone — use
  `storage::repositories::helpers::paginate` with a `Page`, and
  `storage::DbRow` for the row bound (D8, D18, D19, D27, D28).

- `queue.dlq_max_retries` is now validated as 1–16 and connector
  `retry.max_retries` as ≤ 16 (both are exponents in a doubling backoff);
  `trace_storage.batch_size` is capped at 1000 (the batch INSERT binds ~11
  parameters per row against SQLite's 32 766-bind statement limit). Configs
  outside these ranges are rejected at startup instead of failing at runtime.
- Workflow create/update rejects unknown `function.name` values, and workflow
  activation requires every referenced connector to exist. Both were lint
  warnings; the workflow failed at its first request instead.
- Trace retention now also deletes `pending`/`running` rows older than twice
  `trace_queue.retention_hours` — previously they were never reclaimed.
- Filesystem backups (`/api/v1/admin/backups`) return `400` in cluster mode —
  the file would land on one arbitrary node; use managed-DB snapshots/PITR.
- `docs/src/features/scalability.md` and `availability.md` rewritten around
  cluster mode (the multi-node curl-loop reload workaround is obsolete).

- **Dependency major upgrades.** `sea-query` 0.32 → 1.0, which moved the
  comparison operators (`.eq`, `.lte`, `.is_in`, `.like`, …) off `Expr` onto
  the `ExprTrait` trait, and replaced `sea-query-binder` 0.7 with its
  successor `sea-query-sqlx` 0.8 as the sqlx binder. Also `aes-gcm` 0.10 →
  0.11 (nonce generation moved to `Generate`), `sha2` 0.10 → 0.11, `hmac`
  0.12 → 0.13, `base64` 0.22 → 0.23, `tower-http` 0.6 → 0.7, `ctor` 0.4 →
  1.0, and — in the CLI — `rmcp` 1.4 → 3.1 and `tabled` 0.20 → 0.21. No
  behaviour change is intended by any of these; they land before 1.0 so the
  1.x line starts on current majors.

- The documentation moved from GitHub Pages to
  [docs.goplasmatic.io](https://docs.goplasmatic.io/) on Cloudflare, and the
  book build now lives in this repo. Crate `homepage`/`documentation`
  metadata points at the new address.

### Removed

- **`src/storage/migration_gen.rs`.** 803 test-only lines that could not
  produce the shipped schema — no `audit_logs`, `config_epoch` or `job_leases`,
  four columns missing (`traces.task_trace_json`, `traces.access_token_hash`,
  `trace_dlq.claimed_by`, `trace_dlq.claimed_until`),
  `text`/`timestamp` on MySQL where the shipped set needs
  `varchar(n)`/`datetime` — and whose module doc instructed contributors to
  regenerate checksum-frozen migrations. CONTRIBUTING.md now documents what the
  project actually does: copy the newest backend's `001_initial.sql` and adapt
  the dialect by hand (D12).

- Unread `backpressure.queue_depth` channel-config field (backpressure
  rejects immediately at `max_concurrent`; there is no wait queue).

## [0.3.0] - 2026-07-18

This release introduces the portable data dialect: backend-neutral `data_query` and
`data_write` task functions that render one declarative filter/envelope format to
SQL (SQLite/PostgreSQL/MySQL), MongoDB, and Elasticsearch — so workflows can read
and write data without embedding backend-specific queries. `db_read`/`db_write`
remain available as the raw-SQL escape hatch.

### Added

- **`data_query` portable read dialect** — declarative, backend-neutral queries
  (filter, sort, pagination, projection) rendered per connector backend: SQL,
  MongoDB `find`, and Elasticsearch. Supports an inline schema registry with
  relations, and `include` for fetching nested related records with hydration.
- **`data_write` portable write dialect** — insert/update/delete/upsert with
  SQL/MongoDB/Elasticsearch parity and a cross-backend end-to-end test suite.
- **Per-operation connector gates** — db/es connector configs accept
  `operations: { read, insert, update, delete, upsert, raw_write }` (all default
  `true`), enforced by the data handlers; e.g. set `"delete": false` to make a
  connector delete-proof.
- **One-command quickstart** (`examples/quickstart.sh`), a connector-backed
  `postgres-orders` example, and Getting Started guides (CLI setup, first
  connector, AI prompt pack). All examples are linted and deployed end-to-end in CI.
- **Docs**: Dev & Prod topology pages with interactive architecture diagrams,
  terminal recordings (GIFs + asciinema), a comparison page, and a benchmark chart.
- **AI-consumable docs**: `llms.txt` and generated `llms-full.txt` published with
  the docs site, alongside the checked-in OpenAPI 3.1 spec.
- **Security & community**: `SECURITY.md`, `CODE_OF_CONDUCT.md`, issue templates,
  CodeQL (security-extended) and cargo-audit in CI, `ADOPTERS.md`.

### Changed

- Dependency upgrades: `datalogic-rs` 5.0 → 5.1, `dataflow-rs` 3.0.1 → 3.0.2,
  `datavalue-rs` 0.2.2 → 0.2.3 (benchmarked perf-neutral), `redis` 1.2 → 1.3.
- Docker release workflow publishes to GHCR only (ACR mirror removed).

### Security

- Updated `Cargo.lock` to clear RUSTSEC-2026-0185 (`quinn-proto`).

## [0.2.0] - 2026-05-27

This release upgrades the workflow engine to dataflow-rs 3.0 / datalogic-rs 5 and
adds a large set of governance, validation, and operability features. JSONLogic
compilation now happens at engine-construction time, yielding sizeable throughput
gains (+48% on complex workflows, +120% on multi-workflow scenarios) and lower P99
latency across every benchmark scenario versus the v0.1.x baseline.

### Breaking Changes

- **Engine upgrade to dataflow-rs 3.0 + datalogic-rs 5.** JSONLogic is compiled once
  at engine build time rather than per request.
- **Connector `api_key` field removed** in favour of `api_keys`. Update any connector
  configs still using the singular field.
- **Channel/connector create & update DTOs are now strongly typed enums.** Invalid
  `channel_type`, `protocol`, or connector `type` values are rejected at
  deserialization with `400` (values remain case-insensitive; v0.1 lowercase wire
  values are still accepted).
- **Profile output is namespaced** under `_orion.profile` with `version: 1`.

### Added

- **Configurable trace storage modes** — `sync`, `async`, `batch`, or `off` — as a
  global default with per-channel override via `config.tracing`.
- **Per-request workflow profile mode** for timing/inspecting task execution.
- **Per-task execution traces** captured when a channel opts in.
- **Structured error envelope** with field-pathed `FieldError` details, plus
  collection of all protocol-required-field errors in a single response.
- **Per-function input schema validation** for workflow task functions.
- **Bulk import** for channels and connectors, with `?dry_run=true` preview.
- **Strict validation of channel `config_json` at create time.**
- **Config & connector variable substitution** — `${VAR}` / `${VAR:-default}` in
  config TOML and connector configs.
- **`env://` secret references** resolved in connector configs.
- **New CLI subcommands:** `lint`, `dry-run`, and `test-connectivity`.
- **OpenAPI coverage** for the audit, backup/restore, and functions endpoints.

### Changed

- **Performance:** roughly halved per-request CPU by sharing `AppState` via `Arc` and
  gating compression/metrics work.
- **OpenTelemetry** bumped to 0.32 / 0.33; refreshed transitive dependencies
  (`rand` 0.10.1, `tokio` 1.52, and others).
- Distributed config validation into per-struct implementations; decomposed the
  `main.rs` startup sequence; split oversized handlers and centralised admin reload,
  trace-filter, and error-mapping logic.
- Renamed `connector::types` module to `connector::config`.
- Refreshed README, `docs/`, and `tests/README`, and added v0.2.0 / v3.0.0 benchmark
  result sets alongside the v2.1.5 baseline and trace-mode comparison.

### Fixed

- Clippy lints and formatting cleaned up across the crate and test suite.

## [0.1.1] - 2026-04-11

Earlier release. See the Git history for details.

## [0.1.0]

Initial release.

[#259]: https://github.com/GoPlasmatic/Orion/issues/259
[#260]: https://github.com/GoPlasmatic/Orion/issues/260
[#261]: https://github.com/GoPlasmatic/Orion/issues/261
[#262]: https://github.com/GoPlasmatic/Orion/issues/262
[#263]: https://github.com/GoPlasmatic/Orion/issues/263
[#264]: https://github.com/GoPlasmatic/Orion/issues/264
[#265]: https://github.com/GoPlasmatic/Orion/issues/265
[#266]: https://github.com/GoPlasmatic/Orion/issues/266
[#267]: https://github.com/GoPlasmatic/Orion/issues/267
[#268]: https://github.com/GoPlasmatic/Orion/issues/268
[#269]: https://github.com/GoPlasmatic/Orion/issues/269
[#270]: https://github.com/GoPlasmatic/Orion/issues/270
[#271]: https://github.com/GoPlasmatic/Orion/issues/271
[#272]: https://github.com/GoPlasmatic/Orion/issues/272
[#273]: https://github.com/GoPlasmatic/Orion/issues/273
[#274]: https://github.com/GoPlasmatic/Orion/issues/274
[#275]: https://github.com/GoPlasmatic/Orion/issues/275
[#276]: https://github.com/GoPlasmatic/Orion/issues/276
[#277]: https://github.com/GoPlasmatic/Orion/issues/277
[#278]: https://github.com/GoPlasmatic/Orion/issues/278
[#279]: https://github.com/GoPlasmatic/Orion/issues/279
[#280]: https://github.com/GoPlasmatic/Orion/issues/280
[#281]: https://github.com/GoPlasmatic/Orion/issues/281

[Unreleased]: https://github.com/GoPlasmatic/Orion/compare/v1.2.0...HEAD
[1.2.0]: https://github.com/GoPlasmatic/Orion/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/GoPlasmatic/Orion/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/GoPlasmatic/Orion/compare/v0.3.0...v1.0.0
[0.3.0]: https://github.com/GoPlasmatic/Orion/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/GoPlasmatic/Orion/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/GoPlasmatic/Orion/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/GoPlasmatic/Orion/releases/tag/v0.1.0

# Server-backed use cases

Each `*.json` file here is a complete, end-to-end scenario built on one of
the [example packages](../packages/): the requests to send and the exact
responses to expect from a **live Orion server** running that package's
workflow. They are the data-driven half of the repo's e2e suite — and proof
that the shipped examples behave as documented, asserted in CI on every PR.

The workflow under test is **referenced, never copied**: each case points at
the package's `workflow.json` (the same way the offline
[`../workflow-tests/`](../workflow-tests/) cases do), so a package edit is
automatically tested here and the two can't drift apart. The difference
between the two suites: `workflow-tests` runs the workflow through the
engine offline (`orion-server test`), while these cases exercise the whole
runtime — HTTP channel routing, activation lifecycle, engine reloads —
through the `orion-cli` binary against a real `orion-server`.

> Cases that test **runtime behaviour** rather than a scenario (archive
> quarantine, dry-run traces, secret masking, connector error handling)
> live with the suite itself, in `tests/e2e/cases/`, using this same format.

## Run them

```bash
just e2e                # the whole e2e suite (from the repo root)
./tests/e2e/run.sh 13   # just the use-case suite
```

The runner (`tests/e2e/`, suite `13_use_cases.sh`) picks up every `*.json`
file in this directory — adding a case needs no code changes. For each case
it resets server state, creates and activates the connectors and workflows,
creates an active channel for every channel name the tests reference (bound
to the case's first workflow), reloads the engine, then runs the tests in
order.

## Case format

```jsonc
{
  "name": "E-Commerce Order Classification",
  "description": "What the scenario demonstrates",
  "connectors": [ { /* connector create body */ } ],           // optional
  "workflows":  [ { "file": "../packages/order-classification/workflow.json" } ],
  "tests": [
    {
      "name": "VIP order gets tier and discount",
      "channel": "orders",                 // sync send to this channel
      "input": { "amount": 750 },          // request payload
      "expect": {                          // jq expr -> expected (string) value
        ".status": "ok",
        ".data.order.tier": "vip"
      }
    }
  ]
}
```

A `workflows` entry is either `{ "file": "<path>" }` — a workflow JSON file,
relative to the case file; use this for anything that exists as a package —
or an inline workflow create body, for throwaway workflows only a behaviour
test needs.

An optional case-level `"channel_config": { … }` is applied as the `config` of
every channel the case creates — how a case exercises a config-dependent
ingress such as `request.body_mode`. It is case-level because the channels are
all bound to the case's first workflow; a case needing channels to differ from
each other declares them explicitly instead (see `"channels"` in helpers.sh).

Each test sends `input` to `channel` and checks every `expect` entry: the
key is a `jq` expression evaluated against the JSON response, the value is
the expected result compared as a string (`jq -r`).

### Test modes

Instead of the default sync send, a test can set one of:

| Key | Meaning |
|-----|---------|
| `"dry_run_rule": <n>` | Run `workflows test` (server-side dry run) against the case's *n*-th workflow with `input` — no traffic served |
| `"read_connector": <n>` | Assert on `connectors get` for the case's *n*-th connector (e.g. secret masking) |
| `"raw": true` | Send `input` via `send --raw`, unwrapped — the only way to address a channel whose `request.body_mode` is `"payload"` |

### Lifecycle steps and failure paths

| Key | Meaning |
|-----|---------|
| `"before": [...]` | Actions to run first: `{"action": "archive_rule", "rule_index": 0}`, `{"action": "activate_rule", ...}`, or `{"action": "reload"}` |
| `"expect_error": true` | The command must **fail** (e.g. a send to a channel quarantined by archiving its workflow); `expect` is not evaluated |

## The cases

| File | Package under test | Demonstrates |
|------|--------------------|--------------|
| `ecommerce-classification.json` | [`order-classification`](../packages/order-classification/) | Value-tier classification with computed discounts |
| `iot-sensor-alerts.json` | [`iot-sensor-alert`](../packages/iot-sensor-alert/) | Severity ranges with validation and filtering |
| `webhook-transformation.json` | [`webhook-transform`](../packages/webhook-transform/) | Payload normalization via null-safe `var` mapping |
| `notification-routing.json` | [`notification-routing`](../packages/notification-routing/) | Progressive routing (log / email / SMS) by severity |
| `channel-composition.json` | [`channel-composition`](../packages/channel-composition/) | One service calling another in-process via `channel_call` |

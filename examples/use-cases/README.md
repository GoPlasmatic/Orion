# Server-backed use cases

Each `*.json` file here is a complete, end-to-end scenario built on one of
the [example packages](../packages/): the requests to send and the exact
responses to expect from a **live Orion server** running that package's
workflow. They are the data-driven half of the repo's e2e suite — and proof
that the shipped examples behave as documented, asserted in CI on every PR.

The package under test is **named, never duplicated**: each case names the
package directory, and the runner deploys it as it ships — its own
channels, workflows, plugin and connector — so a package edit is
automatically tested here and the two can't drift apart. The difference from
[`../workflow-tests/`](../workflow-tests/): `workflow-tests` runs a workflow
through the engine offline (`orion-cli test`), while these cases exercise
the whole runtime — `orion-cli compile`, `orion-cli deployments apply -d`,
HTTP channel routing and the engine — against a real `orion-server`.

> Cases that test **runtime behaviour** rather than a scenario (definition
> test traces, connection secret locators, connector error handling, body
> modes) live with the suite itself, in `tests/e2e/cases/`, using this same
> format.

## Run them

```bash
just e2e                # the whole e2e suite (from the repo root)
./tests/e2e/run.sh 06   # just the use-case suite
```

The runner (`tests/e2e/`, suite `06_use_cases.sh`) picks up every `*.json`
file in this directory — adding a case needs no code changes. For each case
it writes one definition set (the packages, plugins, connectors, workflows
and channels the case lists), compiles it with `orion-cli compile`,
deploys it with `orion-cli deployments apply -d` — replacing whatever the
previous case deployed — then runs the tests in order.

## Case format

```jsonc
{
  "name": "E-Commerce Order Classification",
  "description": "What the scenario demonstrates",
  "packages":   [ "../packages/order-classification" ],     // deployed whole
  "plugins":    [ { "manifest": "../somewhere/plugin.toml" } ], // optional
  "connectors": [ { /* connector definition */ } ],           // optional
  "workflows":  [ { /* workflow definition */ } ],            // optional
  "tests": [
    {
      "name": "VIP order gets tier and discount",
      "channel": "order-tiers",            // sync send to this channel
      "input": { "amount": 750 },          // request payload
      "expect": {                          // jq expr -> expected (string) value
        ".status": "ok",
        ".data.order.tier": "vip"
      }
    }
  ]
}
```

A `packages` entry is a definition-set directory, relative to the case file;
the runner copies it into the case's set, so its channels serve under the
names (and routes) the package gives them. This is how every scenario here
names its package.

A `workflows` entry is either `{ "file": "<path>" }` — a workflow JSON file,
relative to the case file — or an inline workflow definition, for throwaway
workflows only a behaviour test needs. An inline one without a
`workflow_id` gets `wf-<n>`.

A `plugins` entry names a `plugin.toml` relative to the case file; the
runner copies it and the component it names into the set, so the plugin is a
member of the same deployment as the workflows that call it. The server must
have `plugins.enabled = true` — the e2e harness sets it.

A `connectors` entry is a connector definition, written into the set as a
file; `compile` turns it into a connection. A credential must be an
`env://` reference (it becomes one of the connection's `secret_locators`),
which the server resolves from its own environment — the harness exports
the ones `tests/e2e/cases/` use.

Channels: a case with `workflows` gets one sync HTTP channel per channel
name its tests send to, routed at `/<name>` and bound to its first workflow,
with the case-level `"channel_config": { … }` as each channel's `config` —
how a case exercises a config-dependent ingress such as
`request.body_mode`. A case whose channels must *not* all bind to the first
workflow declares them instead:

```jsonc
"channels": [ { "name": "customer-lookup", "workflow_id": "customer-lookup" } ]
```

A case built from `packages` alone uses the packages' own channels.

Each test sends `input` to `channel` and checks every `expect` entry: the
key is a `jq` expression evaluated against the JSON response, the value is
the expected result compared as a string (`jq -r`).

### Test modes

Instead of the default sync send, a test can set one of:

| Key | Meaning |
|-----|---------|
| `"dry_run": "<workflow_id>"` | Run that workflow, as the case's deployment compiled it, once through `POST /api/v2/admin/definitions/test` with `input` — the answer carries `matched`, `output`, `trace` and `errors` |
| `"read_connection": <n>` | Assert on the stored connection revision the case's *n*-th connector compiled to, as `orion-cli connections list` answers it (its `public_config` and `secret_locators`) |
| `"raw": true` | Send `input` via `send --raw`, unwrapped — the only way to address a channel whose `request.body_mode` is `"payload"` |

### Failure paths

| Key | Meaning |
|-----|---------|
| `"expect_error": true` | The command must **fail** (e.g. a send to a channel the deployment does not carry); `expect` is not evaluated |

A case whose set does not compile or deploy fails as one test named
`<case name>: deploy`, with the compiler's or the server's reason.

## The cases

| File | Package under test | Demonstrates |
|------|--------------------|--------------|
| `ecommerce-classification.json` | [`order-classification`](../packages/order-classification/) | Value-tier classification with computed discounts |
| `iot-sensor-alerts.json` | [`iot-sensor-alert`](../packages/iot-sensor-alert/) | Severity ranges with validation and filtering |
| `webhook-transformation.json` | [`webhook-transform`](../packages/webhook-transform/) | Payload normalization via null-safe `var` mapping |
| `notification-routing.json` | [`notification-routing`](../packages/notification-routing/) | Progressive routing (log / email / SMS) by severity |
| `channel-composition.json` | [`channel-composition`](../packages/channel-composition/) | One service calling another in-process via `channel_call` |
| `fixed-width-statement.json` | [`fixed-width-statement`](../packages/fixed-width-statement/) | A plugin function on the serving path: the codec deploys in the same deployment as its workflow |

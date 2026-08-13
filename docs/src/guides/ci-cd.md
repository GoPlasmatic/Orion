# CI/CD with Packages

A promotion pipeline for Orion has three jobs: prove the logic offline, check it
against the target without writing, then apply it. Each maps to one command, and
the first two need no server and no secrets.

**The artifact is the deployable.** `orion-server package export` produces one
versioned JSON file; you commit it, CI gates it, and CI applies it. Nothing is
rebuilt between environments — the same bytes that passed staging go to
production.

## Repository layout

```
services/payments/
  workflow.json              # source form: readable, reviewable, diffable
  channel.json
  connector.json
  tests/
    flags-high-value.case.json
artifacts/
  payments-1.4.0.json        # exported artifact: what actually ships
```

Author against a dev instance, then export the artifact and commit it:

```bash
export ORION_ADMIN_TOKEN=…
orion-server package export -s https://dev.orion.internal \
  --tag pkg:payments --name payments --version 1.4.0 \
  -o artifacts/payments-1.4.0.json
```

The version in that filename is the unit of promotion. An applied version is
content-immutable, so any content change needs a bump — which makes the commit
that bumps it the reviewable record of the change.

## The pull-request gate

Everything here runs offline. No server, no database, no credentials — so it
works on a fork's pull request.

```yaml
name: Validate

on:
  pull_request:
    paths:
      - 'services/**'
      - 'artifacts/**'

jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install orion-server
        run: |
          curl --proto '=https' --tlsv1.2 -LsSf \
            https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.sh | sh
          echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"

      - name: Lint every workflow
        run: |
          find services -name 'workflow.json' -print0 \
            | xargs -0 -n1 orion-server lint

      - name: Run the offline regression suite
        run: orion-server test services

      - name: Lint the artifacts
        run: |
          for f in artifacts/*.json; do
            orion-server package lint -f "$f"
          done
```

Three checks, three failure modes caught before review: a workflow the API would
reject, logic that changed behaviour, and an artifact whose closure or hash is
wrong.

> [!TIP]
> `orion-server test` walks a directory for `*.case.json` files, so pointing it
> at `services` picks up every service's cases. Cases resolve `workflow` paths
> relative to themselves, so each service's tests live next to the workflow they
> test.

## The staging deploy

`plan` writes nothing and reports exactly what `apply` would do, so run it as a
gate and `apply` as the action.

```yaml
name: Deploy to staging

on:
  push:
    branches: [main]
    paths: ['artifacts/**']

jobs:
  deploy:
    runs-on: ubuntu-latest
    environment: staging
    env:
      ORION_ADMIN_TOKEN: ${{ secrets.ORION_ADMIN_TOKEN }}
      ORION_URL: https://staging.orion.internal
    steps:
      - uses: actions/checkout@v4

      - name: Install orion-server
        run: |
          curl --proto '=https' --tlsv1.2 -LsSf \
            https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.sh | sh
          echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"

      - name: Plan
        run: orion-server package plan -s "$ORION_URL" -f artifacts/payments-1.4.0.json

      - name: Apply
        run: orion-server package apply -s "$ORION_URL" -f artifacts/payments-1.4.0.json

      - name: Confirm no drift
        run: orion-server package diff -s "$ORION_URL" -f artifacts/payments-1.4.0.json
```

`apply` is idempotent: re-running an identical artifact reports
`already applied with identical content — nothing to do`, so a re-run of the
whole job is safe. A changed artifact reusing an applied version is refused with
a `409` — the pipeline tells you to bump the version rather than silently
mutating what staging is running.

## The production promotion

Promote on a tag, with the same artifact and a manual approval gate:

```yaml
name: Promote to production

on:
  push:
    tags: ['payments-v*']

jobs:
  promote:
    runs-on: ubuntu-latest
    environment: production      # attach required reviewers here
    env:
      ORION_ADMIN_TOKEN: ${{ secrets.ORION_ADMIN_TOKEN }}
      ORION_URL: https://prod.orion.internal
    steps:
      - uses: actions/checkout@v4

      - name: Install orion-server
        run: |
          curl --proto '=https' --tlsv1.2 -LsSf \
            https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.sh | sh
          echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"

      - name: Plan
        run: orion-server package plan -s "$ORION_URL" -f "artifacts/payments-${GITHUB_REF_NAME#payments-v}.json"

      - name: Apply
        run: orion-server package apply -s "$ORION_URL" -f "artifacts/payments-${GITHUB_REF_NAME#payments-v}.json"
```

GitHub's `environment` is where the approval lives — the job pauses before its
first step until a reviewer approves, and the production token is scoped to that
environment rather than to the repository.

## Detect drift on a schedule

```yaml
name: Drift check

on:
  schedule:
    - cron: '0 7 * * *'
  workflow_dispatch:

jobs:
  diff:
    runs-on: ubuntu-latest
    env:
      ORION_ADMIN_TOKEN: ${{ secrets.ORION_ADMIN_TOKEN }}
    steps:
      - uses: actions/checkout@v4

      - name: Install orion-server
        run: |
          curl --proto '=https' --tlsv1.2 -LsSf \
            https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.sh | sh
          echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"

      - name: Compare production against the shipped artifacts
        run: |
          for f in artifacts/*.json; do
            orion-server package diff -s https://prod.orion.internal -f "$f"
          done
```

`diff` exits non-zero when the instance's content hashes differ from the
artifact's, which is how you learn that somebody changed production by hand. Run
it nightly and treat a failure as an incident, not a chore.

## Rolling back

A rollback is a promotion of an older artifact:

```bash
orion-server package apply -s https://prod.orion.internal -f artifacts/payments-1.3.0.json
```

There is no separate command and no separate pipeline — re-run the production
job against the previous tag. Entities roll forward carrying the older content,
and the receipt history records both moves.

## Secrets never travel

Connector exports are masked, so an artifact carries `env://STRIPE_KEY`, not the
key. Each environment supplies its own value, which is why the same artifact can
go to staging and production unchanged.

`package lint` treats an `env://` reference unset **on the runner** as a warning
rather than an error — the pull-request job above holds no production secrets
and still checks the artifact.

> [!WARNING]
> A connector authored with a **literal** credential exports as `"******"` and is
> refused on import, so it cannot be promoted at all. If `apply` fails on a
> connector, this is the first thing to check.

## Related

- [Promote Between Environments](../operate/promotion.md) — the five verbs, the
  receipt model, and what a mid-apply failure leaves behind.
- [Test Workflows Offline](../build/testing.md) — the gates the first job runs.
- [Packages](../concepts/packages.md) — why the artifact is the unit.
- [Audit Logs](../operate/audit-logs.md) — every apply is stamped with its
  package name and version.

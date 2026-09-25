<!-- description: A three-stage Orion promotion pipeline: prove the logic offline, plan against the target with zero writes, then apply the same versioned artifact. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# CI/CD with packages

A promotion pipeline for Orion has three jobs: prove the logic offline, check it against the target without writing, then apply it. Each maps to one command. The offline checks need no server or secrets; planning and applying need access to the target instance.

The artifact is the deployable. `orion-server package export` produces one versioned JSON file; you commit it, CI gates it, and CI applies it. Nothing is rebuilt between environments, so the same bytes that passed staging go to production. There are two ways to produce one. `package export` captures a dev instance you authored against. [`compile`](../../reference/cli/orion-server/compile.md) builds the same artifact from a definition directory with no instance in the loop. That is what you want when the definitions are the source of truth. Everything downstream is identical either way.

## Before you start

Tested with Orion 1.10.0. You need Git, `orion-server`, a repository for the definitions, and a CI system that can run shell commands. Planning and applying also need network access to the target instance and an admin token from the CI secret store. The GitHub Actions YAML below is an example; adapt its secret names and installation policy for your provider.

## Lay out the repository

Keep source form and artifacts apart:

```text
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

Or build it from the definitions themselves, which needs no instance and no token:

```bash
orion-server compile services/payments \
  --name payments --version 1.4.0 \
  -o artifacts/payments-1.4.0.json
```

The version in that filename is the unit of promotion. An applied version is content-immutable, so any content change needs a bump. The commit that bumps it is the reviewable record of the change.

## Gate the pull request

Everything here runs offline. No server, no database, no credentials, so it works on a fork's pull request:

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

      - name: Check formatting
        run: orion-server fmt --check services

      - name: Lint the definition set
        run: orion-server lint services --deny-warnings

      - name: Advisory checks
        run: orion-server clippy services

      - name: Run the offline regression suite
        run: orion-server test services

      - name: Lint the artifacts
        run: |
          for f in artifacts/*.json; do
            orion-server package lint -f "$f"
          done
```

Five checks catch five failure modes before review. `fmt` catches a file not in the house style. `lint` catches a definition the API would reject, or one that disagrees with the definitions beside it. `clippy`'s `deny` rules catch a definition that is valid but cannot behave as written; its warnings print and do not fail the step. `test` catches logic that changed behaviour, and `package lint` catches an artifact whose closure or hash is wrong.

Point `lint` at the directory, not at each file in turn. A per-file loop validates each workflow in isolation and cannot see whether it is consistent with the channels and connectors next to it. It misses a `channel_call` to a channel that exists nowhere, a task naming a connector of the wrong type, and two channels claiming one route. Set mode resolves those, and resolves any shared `$from` or `use` references at the same time. Add `--requires-channel` or `--requires-connector` for names the set deliberately expects the target to already have.

> [!TIP]
> `orion-server test` walks a directory for `*.case.json` files, so pointing it at `services` picks up every service's cases. Cases resolve `workflow` paths relative to themselves, so each service's tests live next to the workflow they test.

## Deploy to staging

`plan` writes nothing and reports exactly what `apply` would do, so run it as a gate and `apply` as the action:

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

`apply` is idempotent. Re-running an identical artifact reports that the version is already applied with identical content, so a re-run of the whole job is safe. A changed artifact reusing an applied version is refused with a `409`. The pipeline tells you to bump the version rather than silently mutating what staging is running.

## Promote to production

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

GitHub's `environment` is where the approval lives. The job pauses before its first step until a reviewer approves, and the production token is scoped to that environment rather than to the repository.

## Verify

Detect drift on a schedule, and treat a failure as an incident rather than a chore:

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

`diff` exits non-zero when the instance's content hashes differ from the artifact's, which is how you learn that somebody changed production by hand.

## Roll back

A rollback is a promotion of an older artifact:

```bash
orion-server package apply -s https://prod.orion.internal -f artifacts/payments-1.3.0.json
```

There is no separate command and no separate pipeline. Re-run the production job against the previous tag. Entities roll forward carrying the older content, and the receipt history records both moves.

## Keep secrets out of the artifact

Connector exports are masked, so an artifact carries `env://STRIPE_KEY`, not the key. Each environment supplies its own value, which is why the same artifact can go to staging and production unchanged. `package lint` treats an `env://` reference unset on the runner as a warning rather than an error. The pull-request job above holds no production secrets and still checks the artifact.

> [!WARNING]
> A connector authored with a literal credential exports as `"******"` and is refused on import, so it cannot be promoted at all. If `apply` fails on a connector, this is the first thing to check.

## Next steps

- [Promote between environments](../../operate/maintain/promotion.md): the five verbs, the receipt model, and what a mid-apply failure leaves behind.
- [Test a workflow offline](../author/testing.md): the gates the first job runs.
- [Packages](../../concepts/packages.md): why the artifact is the unit.
- [Audit logs](../../operate/run/audit-logs.md): every apply is stamped with its package name and version.

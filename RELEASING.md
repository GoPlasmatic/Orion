# Releasing Orion

Cut releases from `main` (or a `release/X.Y` maintenance branch) after the workspace gate and the current release
matrix pass. Long-lived integration branches (such as `dataflow-4`) must merge to `main` before tagging because `ci.yml`, `ha-drill.yml` and `release.yml` trigger on `main`, `release/**` and `v*` tags; `main` tracks the active 2.x line, and any maintenance release for an older line is cut from a `release/X.Y` branch. The v2 migration changes the API, deployment model and state
schema; use the [v2 release evidence](RELEASE-EVIDENCE/release-v2-evidence.md) to track the
remaining gates and their owners.

## Prepare the candidate

1. Run `just check`: workspace formatting, Clippy with `-D warnings`, workspace
   tests, default-member doctests and workspace Rustdoc with `-D warnings`.
   Run `cargo +1.98 check --workspace --all-targets --locked` for the MSRV.
2. Run all **six** `just test-containers` legs. Rebuild all three plugin
   components and run the SDK/ABI/runtime suites against the fresh bytes. With
   [Orion-cli](https://github.com/codetiger/Orion-cli) built against the
   candidate, run its 11-suite e2e runner and its examples job: this repo's
   examples linted, their offline cases run (the plugin examples against the
   fresh components), and live quickstart/package deployments twice. Use the
   commands in [TESTING.md](TESTING.md), `.github/workflows/ci.yml` and
   Orion-cli's CI; record actual selected counts and failures, without
   carrying forward v1 suite totals.
3. Run `cargo deny check bans sources licenses`. Preserve justified duplicate
   warnings. Check advisories through the current CI policy too. Documentation
   lint and the book build run in
   [GoPlasmatic/Orion-docs](https://github.com/GoPlasmatic/Orion-docs).
4. Verify the registry-only dependency graph. Swagger UI 10.0.2 contains
   utoipa#1645's ZIP 9 security fixes; this branch removes the temporary utoipa
   workspace patches and git-source exception. Keep the minimum at 10.0.2 or
   newer so registry consumers receive the fix too. Run the affected tests,
   OpenAPI drift guard, dependency policy and workspace gate after updates.
5. Reconcile `orion-server`'s `Unreleased` notes (and the CLI's, in
   Orion-cli) with the final behavior. Document
   strict v2 definitions, the `{initial, steps, truncated}` trace, matching
   server/CLI API versions, Community removals and the fresh-database move from
   1.x. Preserve dated release history. Do not advertise reference-only
   Enterprise work as shipped Community capability.
6. Capture the HTTP and run-view benchmark evidence below. Run current HA
   rolling, plugin and cron drills against the candidate image; record the
   image digest. Until a v2 image exists, the drill scripts use
   `deploy/compose/ha.build.yml` for a local v2 build. Verify the published
   candidate before removing that fallback or changing the v1 image pin.
7. Confirm Console v2 support in Orion-ui before refreshing its recording and
   guide. Track that repository's release as an external dependency.

The workspace has fourteen crates sharing `workspace.package.version`;
`orion-server` is the one binary this repository distributes. The CLI is
released from Orion-cli, at the version of the Orion tag it builds against. Use `cargo release` for the
lockstep bump. Its workspace configuration updates the version requirements,
commits and tags, and disables automatic push and publication. Do not bump a
member manifest by hand.

For a candidate, rehearse `cargo release version --workspace <candidate-version>`
and run it with `--execute` after reviewing the plan. This version-only subcommand
leaves the commit and tag until the generated changes are ready. Every bump also
needs the Helm chart's `version` and `appVersion` and the README's "current
workspace release" line (`release_stamps_drift_test` fails until they match).
Once the tag exists, Orion-docs moves its pin to it, regenerating its
`openapi.json` and the `Tested with Orion <version>.` tutorial stamps there;
move a tutorial's `last_verified` date only after rerunning its documented
path. Run the gate again, commit the version/generated changes, and tag that final commit.
Cut the dated changelog section on the actual release day, leaving a
new empty `Unreleased` section and updated compare links.

Run `cargo package --locked --workspace` from a **clean candidate checkout**
after committing the bump and resolving the registry prerequisite. Packaging
a dirty development tree with `--allow-dirty` is diagnostic evidence only.

## Publish and verify

Push a release-shaped tag only after the required CI run passes for its exact
commit. A bare `vX.Y.Z` or `vX.Y.Z-rc.N` tag releases `orion-server`; use the full
ref (`git push origin refs/tags/<tag>`). The tag's version must match the
workspace version. Orion-cli and Orion-docs build against this tag, so it must
be pushed before either can move to it.

A server release starts these independent pipelines behind `ci-gate.yml`:

- `release.yml` uses cargo-dist to build archives and installers, attach
  attestations, and publish the GitHub release and Homebrew formulae. Candidate
  versions produce prereleases.
- `docker-release.yml` builds per-architecture images and merges their manifest,
  signs it with cosign, attaches the SPDX SBOM and provenance, then verifies the
  published output. A prerelease never becomes `latest`. The CLI image is
  built by Orion-cli's own release.
- `crates-publish.yml` publishes twelve library riders in dependency order,
  then `orion-server`, using `.github/scripts/publish-crates.sh`. It skips a
  version already present in the sparse registry index and fails if the index
  cannot be read. Prerelease tags skip registry publication. `orion-cli` is
  not published here; Orion-cli ships it through dist, Homebrew, GHCR and
  `cargo install --git`.

Check `CRATES_IO_TOKEN` and the cross-repository `GH_PAT` before tagging.
A `workflow_dispatch` dry run skips the image merge/signing job, so a real
candidate tag must prove that path. Verify its image and a downloaded dist
archive from outside the producing runner:

```bash
cosign verify ghcr.io/goplasmatic/orion:<candidate-version> \
  --certificate-identity-regexp 'github.com/GoPlasmatic/Orion' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
gh attestation verify oci://ghcr.io/goplasmatic/orion:<candidate-version> -R GoPlasmatic/Orion
gh attestation verify <downloaded-archive> -R GoPlasmatic/Orion
```

Record run URLs and digests. Fix a failed candidate with a new candidate
version. Repeat the image and HA checks for the artifact you intend to promote.
The documentation site is published from Orion-docs (Cloudflare Workers
Builds); once its pin moves to the release, verify the served upgrade guide
and version in its deployment dashboard.

## Benchmark evidence

The [v1.0.0 session](crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md)
is historical. Keep README figures labeled with their measured version until
a dedicated v2 session replaces them. A debug smoke run proves the benchmark
setup works; it does not supply release throughput or a speedup claim.

On idle, dedicated hardware, record CPU, cores, memory, OS, toolchain, exact
commit and binary versions. Build the server from that commit and point
`ORION_CLI` at an `orion-cli` built from Orion-cli against the same Orion
version:

```bash
cargo build --release --locked -p orion-server
ORION_CLI=/path/to/orion-cli BENCH_RELEASE=1 BENCH_SKIP_BUILD=1 BENCH_DURATION=30s \
  BENCH_OUTPUT_DIR=/tmp/orion-http-candidate \
  bash crates/orion-server/tests/benchmark/bench.sh
python3 crates/orion-server/tests/benchmark/run-view.py --profile dev --rounds 3 \
  --output /tmp/orion-run-view-candidate.json
```

The HTTP runner compiles and applies complete v2 deployments. Its seven local
scenarios emit eleven rows: health, simple, complex, twelve-channel estate,
four concurrency levels, deployment replacement, Wasm echo and JSONLogic echo.
The estate carries twelve copies of the simple workload in **one head**.
Replacement alternates two definitions and requires a new committed revision;
an unchanged apply would measure no reload. Output probes precede timing, and
request failures make the runner fail. Run-view allocations and whole-process
RSS have different measurement windows; follow [the report's methodology](crates/orion-server/tests/benchmark/RUN_VIEW.md).

The opt-in `cluster` scenario deploys to a disposable v2 HA stack through its
load balancer and restores the prior head with compare-and-swap. Set
`ORION_ADMIN_API_KEYS` and point `BENCH_CLUSTER_URL` at the LB. Start the stack
with `deploy/compose/ha.build.yml` while no published v2 image exists. For three
local nodes, enable the `n3` profile and select the three-node load balancer config:

```bash
HA_NGINX=nginx.n3.conf docker compose --profile n3 \
  -f deploy/compose/ha.yml -f deploy/compose/ha.build.yml up -d --wait
NODES="orion-a orion-b orion-c" CONSECUTIVE=30 deploy/ha/plugin-drill.sh
```

The third node shares the base node's runtime settings, and the build override
selects the same local image for it. For a published candidate, set
`ORION_VERSION` and omit the build override. `NODES` makes the drill helpers
load the matching topology and wait for all requested nodes. Capture N=2 and
N=3, the rolling restart drill and the plugin/cron drill verdicts. Require all upstreams healthy
before comparing per-node overhead or scaling efficiency.

Commit raw outputs and a hardware/methodology `SUMMARY.md` under a new
`crates/orion-server/tests/benchmark/results/<candidate>/` directory. Add its
explicit `.gitignore` exception. Compare only matching workloads/configuration;
the old multi-selection and activation-reload scenarios have different
semantics. Publish both plugin and JSONLogic rows. Update the README's table,
SVGs and plugin performance reference only from the dedicated release record.

## Musl promotion checkpoint

`x86_64-unknown-linux-musl` compiles on every relevant PR
(`cross-os-build.yml`) but is deliberately not in the dist targets — the
native -sys builds are the risk, and promotion must be earned by a green
history, not asserted. At each rc/release checkpoint:
review the musl job's run history; once it has accumulated a meaningful
green streak, add the target to `dist-workspace.toml`, regenerate
`release.yml` with `dist init`, and update the deployability page's platform
list. Until then, leave it.

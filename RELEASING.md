# Releasing Orion

Cut releases from `main` after the workspace gate and the current release
matrix pass. The v2 migration changes the API, deployment model and state
schema; use the [v2 release evidence](docs/release-v2-evidence.md) to track the
remaining gates and their owners. Historical 1.0 proof below does not validate
an unbuilt v2 release candidate.

## Prepare the candidate

1. Run `just check`: workspace formatting, Clippy with `-D warnings`, workspace
   tests, default-member doctests and workspace Rustdoc with `-D warnings`.
   Run `cargo +1.98 check --workspace --all-targets --locked` for the MSRV.
2. Run all **seven** `just test-containers` legs, the 11-suite e2e runner,
   examples lint/offline cases and live quickstart/package deployments twice.
   Rebuild all three plugin components and run the SDK/ABI/runtime and offline
   example cases against the fresh bytes. Use the commands in
   [TESTING.md](TESTING.md) and `.github/workflows/ci.yml`; record actual selected
   counts and failures, without carrying forward v1 suite totals.
3. Run `cargo deny check bans sources licenses`, documentation lint and the
   book build. Preserve justified duplicate warnings. Check advisories through
   the current CI policy too.
4. Verify the registry-only dependency graph. Swagger UI 10.0.2 contains
   utoipa#1645's ZIP 9 security fixes; this branch removes the temporary utoipa
   workspace patches and git-source exception. Keep the minimum at 10.0.2 or
   newer so registry consumers receive the fix too. Run the affected tests,
   OpenAPI drift guard, dependency policy and workspace gate after updates.
5. Reconcile both crates' `Unreleased` notes with the final behavior. Document
   strict v2 definitions, the `{initial, steps, truncated}` trace, matching
   server/CLI API versions, Community removals and the fresh-database move from
   1.x. Preserve dated release history. Do not advertise reference-only
   Enterprise work as shipped Community capability.
6. Capture the HTTP and run-view benchmark evidence below. Run current HA
   rolling, plugin and cron drills against the candidate image; record the
   image digest. Until a v2 image exists, the drill scripts use
   `docker-compose.ha.build.yml` for a local v2 build. Verify the published
   candidate before removing that fallback or changing the v1 image pin.
7. Confirm Console v2 support in Orion-ui before refreshing its recording and
   guide. Track that repository's release as an external dependency.

The workspace has fifteen crates sharing `workspace.package.version`; the
server and CLI are the two distributed binaries. Use `cargo release` for the
lockstep bump. Its workspace configuration updates the version requirements,
commits and tags, and disables automatic push and publication. Do not bump a
member manifest by hand.

For a candidate, rehearse `cargo release version --workspace <candidate-version>`
and run it with `--execute` after reviewing the plan. This version-only subcommand
leaves the commit and tag until the generated changes are ready. Every bump also
needs a regenerated `docs/openapi.json` (`cargo run -- dump-openapi`) and the
new `Tested with Orion <version>.` tutorial stamps. Move a tutorial's
`last_verified` date only after rerunning its documented path. Run the gate
again, commit the version/generated changes, and tag that final commit.
Cut the dated changelog section on the actual release day, leaving a
new empty `Unreleased` section and updated compare links.

Run `cargo package --locked --workspace` from a **clean candidate checkout**
after committing the bump and resolving the registry prerequisite. Packaging
a dirty development tree with `--allow-dirty` is diagnostic evidence only.

## Publish and verify

Push a release-shaped tag only after the required CI run passes for its exact
commit. A bare `vX.Y.Z` or `vX.Y.Z-rc.N` tag releases both binaries; use the full
ref (`git push origin refs/tags/<tag>`). Package-prefixed tags
`orion-server-vX.Y.Z` and `orion-cli-vX.Y.Z` select just that binary. The tag's
version must match the workspace version.

A server release starts these independent pipelines behind `ci-gate.yml`:

- `release.yml` uses cargo-dist to build archives and installers, attach
  attestations, and publish the GitHub release and Homebrew formulae. Candidate
  versions produce prereleases.
- `docker-release.yml` builds per-architecture images and merges their manifest,
  signs it with cosign, attaches the SPDX SBOM and provenance, then verifies the
  published output. A prerelease never becomes `latest`. The CLI image has its
  own `docker-release-cli.yml` workflow.
- `crates-publish.yml` publishes twelve library riders in dependency order,
  then `orion-server`, using `.github/scripts/publish-crates.sh`. It skips a
  version already present in the sparse registry index and fails if the index
  cannot be read. Prerelease tags skip registry publication. `orion-cli` does
  not publish to crates.io because another project owns that name; it ships
  through dist, Homebrew, GHCR and `cargo install --git`.

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
Cloudflare Workers Builds publishes docs from changes on `main`; verify the
served upgrade guide and version in its deployment dashboard.

## Benchmark evidence

The [v1.0.0 session](crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md)
is historical. Keep README figures labeled with their measured version until
a dedicated v2 session replaces them. A debug smoke run proves the benchmark
setup works; it does not supply release throughput or a speedup claim.

On idle, dedicated hardware, record CPU, cores, memory, OS, toolchain, exact
commit and binary versions. Build both binaries from that commit:

```bash
cargo build --release --locked -p orion-server -p orion-cli
BENCH_RELEASE=1 BENCH_SKIP_BUILD=1 BENCH_DURATION=30s \
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
with `docker-compose.ha.build.yml` while no published v2 image exists; use
`docker-compose.ha.n3.yml` for three nodes. Capture N=2 and N=3, the rolling
restart drill and the plugin/cron drill verdicts. Require all upstreams healthy
before comparing per-node overhead or scaling efficiency.

Commit raw outputs and a hardware/methodology `SUMMARY.md` under a new
`crates/orion-server/tests/benchmark/results/<candidate>/` directory. Add its
explicit `.gitignore` exception. Compare only matching workloads/configuration;
the old multi-selection and activation-reload scenarios have different
semantics. Publish both plugin and JSONLogic rows. Update the README's table,
SVGs and plugin performance reference only from the dedicated release record.

## First signed release: the `v1.0.0-rc.1` run (P12)

**P12 is closed for 1.0.0.** The signing/attestation pipeline had shipped
without ever executing; the `v1.0.0-rc.1` tag on `ff7b15a2` ran it and it
passed, both in CI and independently off-runner:

- The `merge` job's *"Verify what is attached to the published tag"* step
  passed `cosign verify` + `gh attestation verify` —
  <https://github.com/GoPlasmatic/Orion/actions/runs/31780773339>
- Re-verified from a developer machine (2026-08-14): `cosign verify` reported
  all three checks, and `gh attestation verify` exited 0, both resolving
  `ghcr.io/goplasmatic/orion:1.0.0-rc.1` to digest
  `sha256:9cc1fa0cb7e78860e59855a1a56bd393bcbc6a75b2ab8df916dbb2496040b40b`
  carrying a cosign signature, an SPDX SBOM and SLSA provenance.

Keep those URLs and the 1.0 digest as historical signing evidence. For a new
candidate, follow the current procedure above and record its own results.

## Musl promotion checkpoint (P13)

`x86_64-unknown-linux-musl` compiles on every relevant PR
(`cross-os-build.yml`) but is deliberately not in the dist targets — the
native -sys builds are the risk, and promotion must be earned by a green
history, not asserted. At each rc/release checkpoint:
review the musl job's run history; once it has accumulated a meaningful
green streak, add the target to `dist-workspace.toml`, regenerate
`release.yml` with `dist init`, and update the deployability page's platform
list. Until then, leave it.

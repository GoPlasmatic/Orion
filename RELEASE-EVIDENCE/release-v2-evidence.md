# Community v2 release evidence

> **Status:** Local pre-candidate verification only; not final published release evidence until [#385](https://github.com/GoPlasmatic/Orion/issues/385) closes with a published candidate digest.

This records the pre-candidate validation at `8d940cfb`. For the current local
`2.0.0-rc.1` candidate, see [candidate evidence](release-v2-rc1.md).

Status on 2026-10-28, branch `dataflow-4`, based on `12968e1b` plus the changes
in the commit containing this record. This is the current checklist for
[Orion #385](https://github.com/GoPlasmatic/Orion/issues/385). Keep that issue's
2026-10-17 plan and decision comments as historical migration records.

The current Community release scope is strict v2 definitions and deployments,
dataflow-rs 4.0.3 throughout serving/offline tooling, and one atomically published
runtime generation. The server and CLI still carry the development workspace
version 1.12.0; a v2 candidate bump/tag and publication remain release actions.

## Migration decisions

| Historical item | Current disposition |
|---|---|
| O11b / O12 / O13 | v4 serving, authoring/catalogue and offline/test cutovers are implemented. `execute.rs` uses `run_with`; definition testing uses `/api/v2/admin/definitions/test`. Stub output parity landed in `5d73ba30`. |
| O14 | v3 execution, differential runner and alias rewriter are removed. `steps.rs` retains the raw authored-JSON walker needed for diagnostics on invalid definitions, using the upstream depth bound. |
| Task R | Published dataflow-rs 4.0.3 supersedes the old local engine branch/pin procedure. Orion branch merge and release remain separate. |
| Stored workflow fingerprint | Use immutable definition/manifest digests and the engine version for release diagnosis. Add another stored identity only for a defined reader or use case. |
| K54 | `4ce1423e` replaced the spinner race with a controlled permit holder. |
| K55 | `fad42b6d` fixes live-instance accounting on cancellation. `8244a842` characterizes workflow numeric boundaries; strings preserve exact identifiers beyond the signed integer range. |
| K56 | Keep the existing cron-target refusal spacing as a deliberate compatibility decision. |
| K57 | `fa34f98a` transfers child result data. Forwarded payload/metadata copies require profiling before another API change. |
| K58 / K59 | The alias rewrite and package-promotion paths were retired. Strict refusal hints and current deployment tests cover the surviving surfaces. |
| #371 / #372 | Closed with pinned evidence from `5b9e37d6` and closure handoff `12968e1b`. See [the run-view report](../crates/orion-server/tests/benchmark/RUN_VIEW.md). |

## Current validation

Validated on macOS 27.0.1 ARM64 with rustc 1.99.1, rustc 1.98.1 for the
`+1.98` MSRV command, Docker 29.9.2 and wasm-tools 1.258.0. The workspace remains
at version 1.12.0; these are branch checks, not signed candidate evidence.

| Gate | Result |
|---|---|
| `just check` equivalent | All five recipe commands passed: formatting, strict workspace Clippy, workspace tests, default-member doctests and warnings-as-errors workspace Rustdoc. `just` is absent, so the commands ran directly in order. |
| Workspace tests | 3,107 passed, 79 ignored, zero failed, including OpenAPI drift and the current load/refusal regressions. |
| Containers | All seven legs passed: data-sql 32, CLI SQL 4, Vault 1, storage PostgreSQL 9, storage MySQL 8, schema parity 1, cluster 21 (76 total). |
| MSRV | `CARGO_INCREMENTAL=0 cargo +1.98 check --workspace --all-targets --locked` passed on 1.98.1. |
| E2E | All 11 suites: 87 passed, zero failed or skipped. |
| Example lint / offline | All 14 workflow files linted; 20 offline cases passed. |
| Live examples | Quickstart and all 13 packages deployed twice against an isolated server and PostgreSQL 18. Each second invocation kept the same head revision. |
| Fresh plugin components | Fixture, fixed-width and c4-rules rebuilt and validated; 40 ABI/runtime/SDK/admin-plugin regressions, SDK tests and 20 offline cases passed against the rebuilt bytes. Original committed components were restored after the check; no guest, SDK or WIT source changed. |
| Dependency policy | Full `cargo deny check` passed (advisories, bans, sources and licenses); duplicate warnings remain enabled. |
| Docs | Structure lint, mdBook build, Vale sync and the CI-style advisory Vale invocation passed. |
| HTTP benchmark smoke | All eleven local rows passed with zero request errors at 2 seconds per row (concurrency 4 except the four fixed concurrency cases). Three deployment replacements committed under load. |
| Benchmark edge cases | Repeating `simple` exercised unchanged apply and passed. Redirecting the load generator to a real missing HTTP route produced errors and exit 1, as required. |
| Packaging | `cargo package --locked --workspace` assembled and verify-built all fifteen packages from a clean source export of the reviewed changes (separate target directory, `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`). The version remains 1.12.0; repeat on the actual candidate. |
| Local image | Docker build passed with the dist profile, 16 codegen units and LTO off, matching CI build knobs. The builder now installs curl for Swagger assets; nested guest build targets stay out of the context. |
| HA, two nodes | Rolling: 2,618 requests, zero non-2xx. Plugin replacement: 1,419 requests, zero non-2xx, both nodes converged. Cron restart: 44 distinct instants, 19 completed, 25 skipped singleton, maximum sampled running 1, zero overlap samples. Cluster HTTP smoke passed with zero errors and restored the prior head. |

The initial full dependency check found RUSTSEC-2026-0384 (Tokio),
RUSTSEC-2026-0382 (rustls-webpki) and RUSTSEC-2026-0385 (ctutils). Compatible
updates to 1.54.1, 0.103.16 and 0.4.6 clear them without new policy exceptions.
Published utoipa 6.0.0 / utoipa-gen 6.0.1 / Swagger UI 10.0.2 replace the git
patches; the HTTP manifest requires at least Swagger UI 10.0.2 for ZIP 9.

The final dependency graph passed the workspace gate and MSRV after retrying
builds that ran out of disk space. Only compiler caches created during this
session were cleared. The fresh-component regression and offline checks also passed after the final
security updates. [Command results, source hashes and rebuilt-component hashes](release-v2-validation.json)
accompany this record.

HTTP smoke traffic ran alongside validation work. Its timing is not a release
performance baseline. The run-view allocation report remains the measured
engine evidence; a dedicated release-build HTTP/scaling session stays open.

The first image attempt exposed a missing curl binary in the builder. The first
plugin drill exposed an immediate post-apply replica read returning no revision.
The corrected drill reads its expected member revision from the committed
manifest, refuses an empty revision and pauses between failed convergence windows.
It passed against a fresh stack after the fix. The local image ID is
`sha256:09a9b68bd8e3c12e7dadc7c25d6d8baf2264280788753ff630ea37aafe4eecde`
(`orion:v2-release-check-20261028`, build identity `12968e1b-working-tree`). The
isolated HA stack and its volume were removed after validation. Published-image,
N=3 scaling and signing checks remain release work.

Logs live under `/private/tmp/orion-v2-release-checks/` on the validation machine.

## Release handoffs

The repository's default owner is **@shankar-gpio** (`.github/CODEOWNERS`).
That names the maintainer responsible for tracking these handoffs; it does not
claim an external team has accepted a new assignment.

| Remaining work | Accountable owner / tracker | Exit evidence |
|---|---|---|
| Candidate version, clean packaging, release artifacts and signing | @shankar-gpio / #385 | Clean candidate checks, green CI for its exact commit, archive/image digests and independent signature/attestation verification. |
| Dedicated HTTP performance record | @shankar-gpio / #385 | Release build on dedicated hardware; raw eleven-row local output, N=2/N=3 cluster results, methodology and comparable baselines. Debug smoke throughput does not satisfy this gate. |
| Published v2 image and HA | @shankar-gpio / #385, F-1 in local pending list | Repeat the passing local rolling, plugin and cron drills against the published candidate digest. Replace the v1 image pin and remove the temporary build fallback only after that image exists. |
| Console compatibility | @shankar-gpio tracking Orion-ui / F-1 | Linked Orion-ui v2 support and release; then update and rerun the UI recording and Console guide. |
| Enterprise model planning | @shankar-gpio tracking Orion-ext / #378 | Transfer or link an Enterprise tracker, implement the shape fix in a runnable provider, benchmark supported builds. Community does not ship the extracted reference. |
| Compatible dependency convergence | @shankar-gpio tracking upstream owners / #389 | Adopt compatible releases, refresh inverse trees and verify affected behavior. See [dependency owners](dependency-duplicates.md); justified duplicate warnings remain enabled. |

Keep #385 open until the applicable gates have evidence and the remaining
handoffs have linked trackers. This session does not create a candidate tag,
publish artifacts, merge the branch or change GitHub issue text.

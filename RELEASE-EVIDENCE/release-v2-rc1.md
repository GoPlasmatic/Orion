# Orion 2.0.0-rc.1 local candidate

> **Status:** Local candidate verification only; not final published release evidence until [#385](https://github.com/GoPlasmatic/Orion/issues/385) closes with a published candidate digest.

Prepared on 2026-10-28 from `8d940cfb` and the changes in the commit containing
this record. The version update used `cargo release version --workspace` in an
isolated clean checkout, followed by its generated manifest/lockfile diff.

The candidate is local. No release tag, push, registry publication or GitHub
issue update forms part of this preparation. Keep the Unreleased changelog until
the actual release day. The earlier [branch validation](release-v2-evidence.md)
records the pre-candidate 1.12.0 build.

## Candidate contents

All fifteen workspace crates and eleven shared dependency requirements move to
`2.0.0-rc.1`. OpenAPI reports that version. Tutorial version stamps move with the
workspace; their verification dates stay unchanged. The Console guide scopes its
candidate stamp to the CLI alternatives and retains the browser instructions'
1.12.0 scope. Published installers and images remain an external dependency.

The three guest lockfiles follow the candidate SDK. The fixture and both example
components are rebuilt from that SDK for the candidate's plugin checks.

The three-node overlay inherits the base node's settings, including plugin
enablement. Its local-build overlay pins node C to the same image as nodes A/B.
The drill helpers select that topology when `NODES` includes `orion-c`. HA CI
now verifies plugin replacement on three nodes and cleans up all three services.

## Validation

The workspace gate passed all five `just check` commands, run directly because
`just` is absent: formatting, strict workspace Clippy, 3,107 tests (79 ignored),
default-member doctests and warnings-as-errors workspace Rustdoc. The MSRV check
passed on Rust 1.98.1 with incremental compilation disabled. The normal toolchain
is Rust 1.99.1 on macOS 27.0.1 ARM64.

All seven container legs passed: SQL data 32, CLI SQL 4, Vault 1, storage
PostgreSQL 9, storage MySQL 8, schema parity 1 and cluster 21 (76 total).

All 87 e2e tests and 20 offline workflow cases passed. Fourteen workflow files
linted. Quickstart and all thirteen example packages deployed twice against an
isolated server and PostgreSQL; each second apply kept the same revision. The
Console guide's `health`, `engine status` and `traces list --limit 5` commands
passed against the candidate. The browser Console remains a 1.x dependency.

`cargo package --locked --workspace` assembled and verify-built all fifteen
candidate tarballs from a clean source export, using a separate target directory,
`CARGO_INCREMENTAL=0` and `CARGO_PROFILE_DEV_DEBUG=0`. No registry publication was
needed for the workspace's prerelease dependencies. Full `cargo deny check`,
documentation lint, mdBook build and the advisory Vale invocation passed.

The initial workspace attempt found the nightly rollup's obsolete `<2`
requirement. Its v2 definition set now requires `>=2.0.0, <3`; the documented
version guard still checks prereleases as their release version. The passing
workspace and live-example runs include this correction.

The local image `orion:2.0.0-rc.1-local` built with CI's dist settings (16 codegen
units, LTO off), from build identity `8d940cfb-rc1-working-tree`. Its image ID is
`sha256:851b88885300e6e0598899761de5941eb1c4307da3029088b25a8eaf0b62d5b1`.
Each node reported `2.0.0-rc.1` before its drills.

| Candidate topology | Rolling restart | Plugin replacement | Cron restart | HTTP cluster smoke |
|---|---|---|---|---|
| Two nodes | 2,051 requests, zero non-2xx | 916 requests, zero non-2xx | 44 distinct instants; maximum sampled running 1; zero overlap samples | Passed, zero request errors |
| Three nodes | 2,529 requests, zero non-2xx; node C rolled | 2,037 requests, zero non-2xx; 30 consecutive catalogue reads per convergence window | 45 distinct instants; maximum sampled running 1; zero overlap samples; node C restarted | Passed, zero request errors |

Both isolated stacks and their volumes were removed. These short load runs
executed alongside validation work and do not establish scaling efficiency or
release performance. Four Compose renders checked two-node local, three-node
local, an explicit local candidate image and the published-image configuration.
ShellCheck and six final topology-selection checks passed, including explicit
node-list parsing for local and published-image modes.

[Machine-readable command results, counts and source/component hashes](release-v2-rc1-validation.json)
accompany this record. Raw logs stayed on the validation machine and are not
part of this record.

## Remaining release work

- Required remote CI for the final candidate commit, followed by an approved
  release tag and artifact/signature verification.
- Dedicated release-build HTTP and N=2/N=3 scaling measurements, with comparable
  baseline and hardware details. Functional smoke runs do not meet this gate.
- Repeat HA against the published candidate digest, then move the pinned HA image
  and retire the temporary local-build defaults.
- Orion-ui v2 support and updated UI recording; compatible dependency
  maintenance under #389.

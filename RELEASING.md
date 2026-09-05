# Releasing Orion

The release process for maintainers: what a version tag triggers, how the
first signed release is proven (P12), and the benchmark session the 1.0
README numbers come from (C13). Everything here assumes a clean tree on
`main` with `cargo fmt` / `clippy --all-targets` / `cargo test` green and
the container-gated suites verified.

Releases are cut from `main` directly. There was a long-lived `v1.0.0`
branch while the monorepo restructure was in flight; it was merged and
deleted once `main` became the workspace, and nothing here needs a release
branch. If one is ever reintroduced, the tag steps below still work — they
name refs explicitly rather than relying on the current branch.

## What a release-shaped tag triggers

The workspace has two releasable packages, `orion-server` and `orion-cli`,
**versioned in lockstep** — as is everything else in the workspace. There is
one version number, `workspace.package.version` in the root `Cargo.toml`, and
all five crates inherit it with `version.workspace = true`. A bare `v*` tag
(e.g. `v1.0.0`, `v1.0.0-rc.1`) is the joint release: dist announces every
dist-able package sitting at the tagged version, so one tag yields one GitHub
release carrying both sets of archives, both installers and both tap formulae,
and both docker-release workflows build their image.

`cargo release <level|version>` is the tool for the bump. It rewrites
`workspace.package.version` and the two `workspace.dependencies` requirements
(the only other places the number appears — a path dependency still needs a
`version` to be publishable), commits, and tags. It is configured not to push
or publish: the tag is what starts the pipelines, and publishing stays with
`crates-publish.yml`, which is idempotent and knows the rider order.

```bash
cargo release patch --execute      # or minor / major / 1.4.0-rc.1
git push origin refs/tags/v1.3.2
```

Editing `workspace.package.version` by hand and tagging works just as well;
the tool exists so the three numbers cannot drift apart.

**Only `orion-server` is published to crates.io.** `orion-cli` is not, and
this is deliberate: the `orion-cli` name on crates.io was registered in
January 2021 by an unrelated Lisp compiler
([github.com/wafelack/orion](https://github.com/wafelack/orion)) and is owned
by that author's account. All three of its versions are yanked, but yanking
never frees a name, so a publish would fail with "not an owner" — and it
would fail *after* the rider crates and `orion-server` were already live and
unyankable-in-place. Nothing regresses: the CLI has never shipped through
crates.io (before the monorepo merge it lived in `GoPlasmatic/Orion-cli`,
which had no crates.io pipeline at all), and it still reaches users through
the dist installers, the Homebrew tap, `ghcr.io/goplasmatic/orion-cli` and
`cargo install --git`, which is what the install docs describe. Securing the
name would be the only prerequisite to adding it back.

The package-prefixed tags are the out-of-band path for shipping one package
alone: `orion-server-vX.Y.Z` and `orion-cli-vX.Y.Z` each release exactly
their own package (release.yml's dist plan and docker-release-cli.yml both
match on the prefix; crates-publish does too, and an `orion-cli-v*` tag
simply gives it nothing to publish).

Because lockstep means a bare tag names a version one package may not have
reached, the CLI's pipelines degrade rather than fail on a mismatch:
docker-release-cli's `prepare` job skips the CLI image when a bare tag's
version disagrees with `crates/orion-cli/Cargo.toml`. A prefixed
`orion-cli-v*` tag that disagrees is still a hard error — that one is a
mistake, not a server-only release.

The three shared library crates (`orion-api`,
`orion-client`, `orion-plugin-sdk`) are never tagged: crates-publish publishes
them automatically as riders — in dependency order, skipping versions already
on crates.io — right before `orion-server`, since crates.io refuses a crate
whose dependency it doesn't host. `orion-plugin-sdk` depends on nothing else
in the workspace and is a rider for a different reason: it is what a plugin
author links against, so it has to be on crates.io for the SDK to be usable
at all.

**Lockstep is what makes the riders safe, and it is why there is nothing to
remember here.** Skip-if-present means a rider whose contents changed without
a version bump would be skipped at publish time, leaving the released binary
linked against older crates.io content while the repo builds the newer local
one — a divergence nothing in CI can see, because the workspace always
resolves the path dependency. Sharing one workspace version makes that
unrepresentable: a release moves the number, so every rider's version moves
with it.

It used to be a maintainer obligation instead — bump each rider you touched,
then bump every dependent whose requirement you had to edit, which made *that*
dependent a changed rider needing its own bump — policed by a `package`-job
step that diffed the rider directories against the push base. The step and the
cascade are both gone; if you find yourself hand-editing a `version` in a
member manifest, that is the mistake.

`cargo package --locked --workspace` — what the `package` job runs — is the
local rehearsal. It needs a **clean working tree**, so run it after committing
the bump, not before. A server-release tag starts three independent
pipelines, all gated on a successful CI run for the tagged commit (T10):

- **`release.yml`** (generated by cargo-dist): builds archives and
  installers per target, attests every artifact
  (`github-attestations = true` in `dist-workspace.toml`), and publishes a
  GitHub release. dist marks `-rc.*` versions as prereleases automatically.
- **`docker-release.yml`**: builds per-arch images by digest, then the
  `merge` job assembles the multi-arch manifest, signs it (keyless
  `cosign sign`), attaches an SBOM (`cosign attest --type spdxjson`) and
  build provenance, and **verifies its own output** — `cosign verify` +
  `gh attestation verify` against the published tag, so an unsigned release
  fails itself. `latest` is only applied to non-prerelease versions
  (`latest=auto`), so an rc never becomes `latest`. The Helm chart publishes
  after the manifest exists.
- **`crates-publish.yml`**: runs `.github/scripts/publish-crates.sh` —
  `cargo publish --locked` to crates.io for the three rider crates and
  `orion-server` (not `orion-cli` — see above), skipping any version that is
  already live so the job is safe to re-run. Presence is read from the sparse
  index (`index.crates.io`), never the crates.io JSON API: the API rate-limits
  and a 429 read as "not published yet" is what failed the v1.1.0 run. An
  index that cannot be read stops the release rather than guessing, and
  cargo's own *"already exists on crates.io index"* is treated as a
  successful skip. The workflow skips prerelease tags (anything containing
  `alpha`/`beta`/`rc`/`pre`), so the rc rehearsal does not publish a crate —
  the real tag is its first execution.

**Required repository secrets** — a missing one fails its pipeline only
*after* the tag exists, so check both before tagging:

- `CRATES_IO_TOKEN` — `crates-publish.yml`'s registry token.
- `GH_PAT` — `release.yml`'s Homebrew job pushes the formula to
  `GoPlasmatic/homebrew-tap` with it; the built-in `GITHUB_TOKEN` cannot
  write to another repository.

Nothing rides `workflow_dispatch` dry runs for signing: `dry_run` skips the
`merge` job entirely, which is why the first execution needs a real tag.

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

The procedure below is what was run, kept as the routine for the next
release. Nothing here is outstanding work for 1.0.0 — but do repeat it: the
rc run is what proves the pipeline still signs, and step 4's off-runner check
is what proves it independently of the runner that produced the artifact.

1. **Version bump (required):** dist refuses a tag whose version is not the
   package version. On `main` set `version = "1.0.0-rc.1"` in
   **both** `crates/orion-server/Cargo.toml` and `crates/orion-cli/Cargo.toml`
   — they release in lockstep, and a bare tag only announces the packages that
   match it (one commit; `Cargo.lock` updates with it), push, and wait
   for CI to go green on that commit — the release pipelines gate on it.

   **Regenerate the OpenAPI spec in the same commit**, or CI fails:

   ```bash
   cargo run -- dump-openapi > docs/openapi.json
   ```

   `docs/openapi.json` embeds `info.version` from `CARGO_PKG_VERSION`, and
   `openapi_test::committed_openapi_json_is_up_to_date` compares the committed
   file against what the binary emits. Every version bump therefore breaks
   three CI jobs (Test, Test Coverage and MSRV all run the suite) until the
   spec is regenerated. This applies to **each** bump — the one here and the
   one back to `1.0.0` in step 8.

   **Re-stamp the tutorial pages in the same commit**, for the same reason.
   Twelve pages under `docs/src/` carry
   `**Tested with:** Orion <version> · **Last reviewed:** <date>`, and
   `docs/lint.sh` (the `book` CI job) fails when that version is not the
   workspace version — `bash docs/lint.sh` names every page that is behind.
   The version is mechanical; the date is not. Move it only for a page whose
   documented path you actually re-ran, so a stale date stays visible rather
   than being laundered by the bump.
2. **Tag and push:**

   ```bash
   git tag v1.0.0-rc.1
   git push origin refs/tags/v1.0.0-rc.1
   ```

   Push the tag by its full `refs/tags/` name. This used to be mandatory:
   the release branch was itself called `v1.0.0`, so once the final tag
   existed the two shared a name and a bare `git push origin v1.0.0` failed
   with `src refspec v1.0.0 matches more than one`. That branch is gone, so
   the ambiguity is too — but keep the fully-qualified form. It is
   unambiguous whatever a branch is called, and the next release branch to
   be named after its version would reintroduce exactly this.

3. **Watch the pipelines** (`crates-publish` skips its publish job on an
   rc tag, so the live ones are `release.yml` and `docker-release.yml`).
   The whole of P12 is the `merge` job's
   *"Verify what is attached to the published tag"* step: it must pass
   `cosign verify` and `gh attestation verify` against
   `ghcr.io/goplasmatic/orion:1.0.0-rc.1`.
4. **Verify independently from a clean machine** (not the CI runner):

   ```bash
   cosign verify ghcr.io/goplasmatic/orion:1.0.0-rc.1 \
     --certificate-identity-regexp 'github.com/GoPlasmatic/Orion' \
     --certificate-oidc-issuer https://token.actions.githubusercontent.com
   gh attestation verify oci://ghcr.io/goplasmatic/orion:1.0.0-rc.1 -R GoPlasmatic/Orion
   # And one dist artifact, downloaded from the GitHub release:
   gh attestation verify <downloaded-archive> -R GoPlasmatic/Orion
   ```

5. **On failure:** fix, bump to `-rc.2`, repeat. The rc artifacts are honest
   prereleases — no cleanup needed either way.
6. **Close P12**: record the green run's URL in a commit whose message
   names P12 (the audit trackers are retired; `git log --grep=P12` is the
   index, per CONTRIBUTING's proposal-ID convention).
7. **Cut the CHANGELOG** (CONTRIBUTING §Cutting a Release, step 2): fold
   `## [Unreleased]` into the dated release heading, re-stamp the heading's
   date to the day the tag is actually cut, leave an empty `[Unreleased]` on
   top, and check the compare links at the foot of the file name the new
   tag. Do this after the last feature commit lands — an entry dated before
   its content silently drops whatever shipped in between.
8. **For the real release:** set `version = "1.0.0"` back in both crate
   manifests **and regenerate `docs/openapi.json` again** (step 1's command —
   the spec currently records the rc version), land, wait for CI, then
   `git tag v1.0.0 && git push origin refs/tags/v1.0.0`.
9. **Publish the docs:** nothing to merge — releases are cut from `main`, so
   the tagged commit is already the one `docs.goplasmatic.io` builds from.
   Cloudflare Workers Builds watches this repo and rebuilds on a push to
   `main` touching `docs/`, which means the docs went live with the commit
   you tagged rather than after it. Confirm rather than assume: the deploy
   runs outside GitHub, so check the Cloudflare dashboard (Workers →
   orion-docs → Deployments), not the Actions tab, and confirm the upgrade
   guide and the new version strings are actually being served.

   **Resolved for 1.0.0.** This Worker did not exist when the note above was
   first written — `docs.goplasmatic.io` resolved nowhere, which given
   `custom_domain: true` in `docs/wrangler.jsonc` meant no deploy had ever
   succeeded. It was created before the 1.0.0 tag and the hostname now serves
   (verified 2026-08-21). Nothing to do here at each release except the
   confirmation in the paragraph above; re-read this only if the hostname
   stops resolving, since `orion-server`'s `homepage` and `documentation`
   metadata point at it and are frozen into the crates.io listing at publish
   time.

## The benchmark session (C13)

**Done for 1.0.0, and outstanding ever since.** The 1.0.0 record is committed at
[`crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md`](crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md)
and the README's Performance section cites it — including the cluster scenario,
which was skipped in the original session and captured separately on
2026-08-15, so the N=2 and N=3 scaling numbers are published. **No record has
been captured since**: 1.1.0 through 1.5.1 all shipped citing the 1.0.0
figures, which was defensible each time — those releases added capability
rather than reworking the request path — but it has to stay a decision rather
than become an omission, and the README's "measured on v1.0.0" framing has to
stay accurate either way.

**1.6.0 is where that stops being defensible**, for two reasons. The runtime
generation is now one published value rather than two, which touches the hot
path of every request; and scenario H (`plugin`) is new, so there is no
published number for the sandbox at all — `docs/src/reference/plugins.md`
says as much under "Performance" and points here. Run the session below at
the 1.6.0 tag (or its rc) and land the numbers.

Numbers must come from **dedicated hardware** — a laptop running other work
produces numbers worse than none.

1. **Hardware:** a machine doing nothing else, on AC power, thermals
   settled. Record CPU model, core count, RAM, and OS in the results.
2. **Build:** `cargo build --release` at the tagged commit (or the rc).
3. **Single-instance scenarios** (needs `hey`, `jq`, `curl`):

   ```bash
   BENCH_RELEASE=1 BENCH_DURATION=30s ./crates/orion-server/tests/benchmark/bench.sh
   ```

4. **Cluster scenario**: bring up the HA compose stack
   (`docker compose -f docker-compose.ha.yml up -d --wait`, N=2), then:

   ```bash
   BENCH_RELEASE=1 ./crates/orion-server/tests/benchmark/bench.sh cluster
   ```

   Repeat at N=3 by adding the third-node overlay
   (`docker compose -f docker-compose.ha.yml -f docker-compose.ha.n3.yml up -d --wait`
   — the base stack is hard-wired to two nodes, and the overlay carries the
   three-server nginx upstream). Scenario G compares against scenario B for
   per-node cluster overhead; compute scaling efficiency at N=2 and N=3.

   While the stack is up, run the **plugin drill** too —
   `deploy/ha/plugin-drill.sh` — and record its verdict beside the numbers:
   a plugin activated on one node converges on the other, and a new version
   activated under load through the LB produces zero non-2xx. At N=3 the
   `CONSECUTIVE` window it uses to assert "every node" should be raised
   (`CONSECUTIVE=30`).

   Scenario H (`plugin`, in the default set since 1.6) is the plugin cost:
   the fixture's `identity` on the hot path against the same rewrite as a
   `map`. Publish both rows and their ratio in
   `docs/src/reference/plugins.md` under "Performance".
5. **Record:** commit the run outputs under
   `crates/orion-server/tests/benchmark/results/v<version>/` — one `.txt` per
   scenario plus a `SUMMARY.md` recording the hardware (CPU model, cores, RAM,
   OS) and the scaling-efficiency numbers, following the tracked
   `results/v1.0.0/` layout. `.gitignore` ignores scratch runs under
   `results/` and re-includes each release directory explicitly, so **add the
   new release's line before committing**. The `v0.2.0`, `v1.0.0` and `v1.1.0`
   lines are already there — `v1.1.0` speculatively, for a session that was
   never run, so it re-includes a directory that does not exist.
6. **Publish:** regenerate `docs/media/benchmark-light.svg` /
   `docs/media/benchmark-dark.svg` from the new numbers (same style, both color
   schemes), and replace the README's Performance section numbers — table,
   alt text, and the "measured on v1.0.0" framing — with the new ones,
   including cluster scaling efficiency.
7. **Close the checkpoint**: the commit that lands the numbers names C13 in
   its message (`git log --grep=C13` is the index).

## Musl promotion checkpoint (P13)

`x86_64-unknown-linux-musl` compiles on every relevant PR
(`cross-os-build.yml`) but is deliberately not in the dist targets —
rdkafka's cmake + vendored-OpenSSL build is the risk, and promotion must be
earned by a green history, not asserted. At each rc/release checkpoint:
review the musl job's run history; once it has accumulated a meaningful
green streak, add the target to `dist-workspace.toml`, regenerate
`release.yml` with `dist init`, and update the deployability page's platform
list. Until then, leave it.

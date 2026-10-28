# Proposed #385 status update

Publication draft: prepend this section to #385 after pushing the implementation
and evidence. Retain the existing 2026-10-17 plan and decision comments below it
under a historical-plan heading. No remote issue changes form part of this draft.

## Current v2 scope (2026-10-28)

The local candidate is `2.0.0-rc.1`. It uses published dataflow-rs 4.0.3 for serving,
authoring and offline/test execution. O11b, O12, O13 and v3 removal are complete.
The raw authored-JSON walker remains for invalid-definition diagnostics. The
old local engine branch/merge procedure is superseded by the published release.

K54's permit regression and K55's cancellation accounting are fixed. Workflow
numeric boundaries are characterized: use strings for exact identifiers beyond
the signed integer range. Child result transfer is complete; profile the
remaining forwarded-payload copies before adding an upstream API. K56's refusal
spacing remains a deliberate compatibility choice. The alias-rewrite and old
package-promotion paths behind K58/K59 were retired. #371 and #372 are closed
with pinned allocation and behavior evidence.

The HTTP benchmark now compiles and applies v2 deployments. Its estate contains
twelve workflows and channels in one head. Replacement alternates two definitions
and requires committed revisions, with output probes and request-error failure.
Swagger UI 10.0.2 supplies the published ZIP 9 security fix; temporary utoipa git
patches are gone. The release dependency check also updates Tokio,
rustls-webpki and ctutils for the advisories it found.

Use [candidate validation](https://github.com/GoPlasmatic/Orion/blob/dataflow-4/docs/release-v2-rc1.md),
[the earlier validation and owner record](https://github.com/GoPlasmatic/Orion/blob/dataflow-4/docs/release-v2-evidence.md)
and [the release procedure](https://github.com/GoPlasmatic/Orion/blob/dataflow-4/RELEASING.md)
for actual commands and results. Historical counts of 128 container tests,
18 e2e suites and 23 workflow cases no longer describe this Community tree.
Model/Kafka validation belongs to the repositories shipping those capabilities.
The final Community release notes describe the fresh-database move, strict v2
definitions, `/api/v2/`, the trace shape and matching server/CLI API versions.

## Remaining release work

- [ ] Pass required remote CI for the final candidate commit, then obtain the
  release artifacts and signature/attestation evidence.
- [ ] Capture dedicated release-build HTTP/cluster performance and comparable
  baseline evidence; debug smoke throughput does not satisfy this gate.
- [ ] Repeat the passing local two- and three-node rolling, plugin and cron HA
  drills on the published candidate image digest. Replace the v1 compose pin
  once a v2 image exists.
- [ ] Verify Console v2 compatibility in Orion-ui, then refresh its UI guide and
  recording.
- [ ] Transfer or link Enterprise planning #378 to a runnable Orion-ext provider.
- [ ] Keep compatible dependency convergence under #389, with the documented
  upstream owners and justified duplicate warnings.

@shankar-gpio owns Community release tracking and the cross-repository handoffs,
as the repository's default CODEOWNER. Keep #385 open until applicable gates
have evidence and deferred work has linked owners/trackers. The old migration
plan remains available below for its decisions and historical measurements.

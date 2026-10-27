# Bug analysis and implementation plan

Scope: the five open bugs B-2 through B-6 in `improvements.md`. The decision,
follow-up and upstream sections remain separate work. Existing uncommitted
deployment-catalog and HA-script edits must be preserved.

The user confirmed two presentation choices: an apply with a convergence wait
preserves the receipt and adds a `convergence` field; human health output shows
the build date in explicitly labelled UTC.

## B-2: duplicate reconciliation reports a failed build

Both the request handler and background reconciler call
`runtime::reconcile_deployment_revision`. The build's publication barrier refuses
`revision <= applied_revision`, including a revision the other caller just
successfully applied. That refusal restores the connector snapshot, sets
`reload_degraded`, increments failure backoff and becomes the response's
`local_error`. Convergence can consequently report success beside a build error.

The reload mutex currently protects generation publication but is released before
`NodeReconciler::build_and_publish_candidate` updates applied status. A second
caller can enter during that gap and build the same revision again.

Move mutex ownership to the outer reconciliation function, covering publication
and the applied-status update. Under that mutex, observe the desired head and
return the existing status when the positive target revision is already both
applied and desired. Do not rebuild, republish, clear quarantines, alter backoff
or flag degradation. Keep the existing refusal for genuinely older candidates
and keep real build failures visible.

Verify repeated and concurrent calls, one publication for a new revision, no
additional publication for duplicates, preserved quarantines, and apply/rollback
responses when the background reconciler wins the lock first. Update the existing
concurrency test, which currently requires duplicate calls to fail.

## B-3: convergence accepts an uncommitted revision

The convergence handler evaluates node status without looking up the revision.
Any positive future number therefore appears pending; revision zero can even
appear converged despite having no committed row.

Use the store's database-backed `revision` accessor before evaluating nodes and
return the standard `REVISION_NOT_FOUND` 404 when absent. A local-cache-only
check would incorrectly reject a peer's recent commit, so exercise a stale
replica too. Compacted revisions retain their metadata row and remain valid
convergence targets. Propagate database errors instead of mislabelling them 404.
Document the 404 in the route and regenerate the checked-in OpenAPI spec.

## B-4: apply and wait both write stdout

Apply prints `CreateDeploymentResponse`, then invokes a polling helper that also
prints. Success produces two objects with different shapes. Timeout appends a
JSON number, loses the last convergence report and returns exit code 3.

Make polling return its last typed convergence report. Let each command render
once. Apply preserves `committed_revision`, `receipt`, `warnings`, `evaluation`
and any `local_error`, adding `convergence` after a wait. A timeout prints this
same shape with `convergence.converged = false`, reports timeout on stderr and
exits 3. Quiet apply prints the revision once. Standalone wait prints its report
on success and timeout, or only the revision in quiet mode. A polling request
failure after commit must still print the durable apply receipt before returning
the error through the CLI's normal stderr path.

Verify stdout by running the actual CLI against a controlled HTTP server: JSON
must parse as one complete value, receipt fields must survive, pending nodes
must survive timeout, and quiet output must contain exactly one revision. Cover
JSON, YAML and standalone wait, plus a delayed convergence response and a polling
error. The existing real-server deployment test should also use `--wait-ms`.

## B-5: health displays build seconds without formatting

The server emits Unix seconds as a string and health's table path prints it
verbatim. Parse it with checked conversion to a UTC date using the workspace's
existing Chrono dependency, and display `YYYY-MM-DD HH:MM:SS UTC`. Preserve raw
JSON/YAML fields and quiet health status. Keep unparseable or out-of-range values
readable by displaying their original text; omit an absent/empty timestamp as
before. Check the epoch, a known date and invalid inputs.

## B-6: the test name describes an obsolete outcome

The test calls `assert_upload_refused` with the invalid `numeric_as` field's
path. Rename it to describe that refusal; its assertions already cover the
intended behavior, so no additional test is needed.

## Validation and delivery

Run focused regressions, then the commands from `just check` directly (`just` is
not installed): formatting, workspace Clippy with warnings denied, workspace
tests, default-member doctests, and workspace library rustdoc with warnings
denied. Container-gated tests retain their existing opt-in requirements.
Keep one commit per bug on `dataflow-4`, with a green check gate before committing,
and never push. Stage only this work, preserving the pre-existing edits. Remove
resolved entries from the pending-only `improvements.md` after validation.

## Completed validation

Implemented B-2 through B-6 with the confirmed output choices. The full workspace
suite passed: 3,086 tests passed, none failed, and 79 retained their existing
ignored status. This includes the real-server CLI deployment test, deterministic
apply/rollback races, preserved quarantine status, stale-replica convergence,
database-error propagation, JSON/YAML output and the OpenAPI drift check.

Formatting, workspace Clippy with warnings denied, default-member doctests and
workspace library rustdoc with warnings denied passed. The subprocess HTTP tests
require loopback access outside the filesystem/network sandbox. The existing
replica-read test's assertion was corrected to use the API's flattened `revision`
field; its deployment-read changes and the other pre-existing edits remain
uncommitted.

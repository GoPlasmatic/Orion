<!-- description: The dead-letter endpoints for asynchronous trace persistence: listing failures, retrying one, retrying a batch, and purging by age. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Trace dead-letter endpoints

Inspecting and re-driving traces the async pipeline could not persist.

An async submission that fails lands in the dead-letter queue and is retried automatically with backoff (see [Timeouts, Retries & Circuit Breakers](../../operate/run/failure-handling.md)). Two different failures put it there, and the queue does not distinguish them:

- **The run failed**: a task errored, or the workflow exceeded the channel's
  timeout. The trace is settled `failed` and the whole submission is queued for
  re-execution, so a downstream outage that has since recovered drains by
  itself.
- **The run never started**: the trace's pre-run status write failed, so the
  message is queued rather than dropped and re-runs once the database recovers.

A *result* write that fails after a successful run is the one failure that does **not** queue. The work is already done, so the trace is settled `failed` with `Result persistence failed after retries`. Re-running it would repeat every side effect. Only `/async` traffic reaches this queue at all — a sync request carries its own failure back to the caller, with nothing left to retry.

These endpoints are the operator view of that queue. Inspect what is stuck, put an entry back in line, or clear out entries that will never succeed.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/trace-dlq` | List DLQ entries, paginated (`?offset=`, `?limit=`). Summaries only — the failed payload is omitted; fetch one by id for it |
| GET | `/api/v1/admin/trace-dlq/{id}` | Get one entry including the failed payload and error metadata |
| POST | `/api/v1/admin/trace-dlq/{id}/requeue` | Reset the entry to `retry_count = 0` and schedule it for immediate retry — including one already exhausted |
| POST | `/api/v1/admin/trace-dlq/purge` | Delete **exhausted** entries (retries used up). Body: `{"older_than_hours": N}` (required; `0` purges every exhausted entry). Live entries are never purged |

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Trace queue settings](../configuration/trace-queue.md): the retry loop behind these endpoints.
- [Monitor and alert](../../operate/run/monitoring.md): the counters that say the DLQ is filling.
- [Handle failures](../../operate/run/failure-handling.md): what lands here, and why.

<!-- description: Choose the Orion learning path for your use case: REST API, webhook ingestion, Kafka processing, scheduled work, database access or AI-assisted authoring. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Choose a use case

Start with the path closest to the service you intend to build. Each path moves from a working example to the exact configuration and the production concerns that apply to it.

| I want to build | Start here | Then learn | Before production |
|---|---|---|---|
| A REST API or business-rule endpoint | [Build your first service](../tutorials/first-service.md) | [Author a workflow](../../guides/author/workflows.md) and [Configure a channel](../../guides/author/channels.md) | [Secure an instance](../../operate/run/security.md) |
| A webhook receiver and normalizer | [Webhook worked example](../../guides/ai/worked-examples.md#3-normalizing-webhook-payloads) | [Connect a database or API](../../guides/author/connectors.md) | [Authentication and validation](../../reference/channel-config/auth.md) |
| A Kafka event consumer | [Consume from Kafka](../../guides/patterns/kafka-channels.md) | [Channels](../../concepts/channels.md) and [Timeouts, retries and circuit breakers](../../operate/run/failure-handling.md) | [Monitor and alert](../../operate/run/monitoring.md) |
| A scheduled job | [Run work on a schedule](../../guides/patterns/scheduled-workflows.md) | [Channels](../../concepts/channels.md#scheduled-work) | [Cron occurrences](../../reference/admin-api/cron-occurrences.md) |
| A database-backed service | [Build an orders API end to end](../tutorials/orders-api.md) | [Portable data dialect](../../reference/data-dialect.md) | [Bound what connectors can reach](../../operate/run/security.md#bound-what-connectors-can-reach) |
| An AI-authored service | [Build a service with Claude Code](../../guides/ai/claude-code.md) | [Test a workflow offline](../../guides/author/testing.md) | [Version and roll out changes](../../guides/author/versioning.md) |

<!-- description: Choose the Orion learning path for your use case: REST API, webhook ingestion, Kafka processing, database access or AI-assisted authoring. -->
# Choose Your Use Case

**Page type:** Learning path · **Audience:** Developers who completed the quickstart

Start with the path closest to the service you intend to build. Each path moves
from a working example to the exact configuration and production concerns that
apply to it.

| I want to build | Start here | Then learn | Before production |
|---|---|---|---|
| REST API or business-rule endpoint | [Understand the HTTP Flow](./first-service.md) | [How to Author Workflows](../build/workflows.md) and [Configure Channels](../build/channels.md) | [Secure an Instance](../operate/security.md) |
| Webhook receiver and normalizer | [Webhook worked example](../guides/worked-examples.md#normalizing-webhook-payloads) | [Connect Databases & APIs](../build/connectors.md) | [Authentication and validation](../reference/channel-config.md#authentication) |
| Kafka event consumer | [Consume from Kafka](../guides/kafka-channels.md) | [Channels](../concepts/channels.md) and [failure handling](../operate/failure-handling.md) | [Monitoring & Alerts](../operate/monitoring.md) |
| Database-backed service | [Orders API golden path](../guides/orders-golden-path.md) | [Portable Data Dialect](../reference/data-dialect.md) | [Connector security](../operate/security.md#bound-what-connectors-can-reach) |
| AI-authored service | [Build with Claude Code](../ai/claude-code.md) | [Test Workflows Offline](../build/testing.md) | [Version & Roll Out Changes](../build/versioning.md) |

If the logic must pause for hours, run on a schedule, stream a response, or use
arbitrary application code, check [Is Orion Right for You?](../comparison.md)
before continuing.

## Shared path after the first example

Whichever use case you choose, the route to production is the same:

1. [Test the definitions offline](../build/testing.md).
2. [Group and version them as a package](../concepts/packages.md).
3. [Promote the package through CI/CD](../guides/ci-cd.md).
4. Work through the [Production Checklist](../operate/production-checklist.md).

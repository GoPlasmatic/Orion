<!-- description: Reusable workflow patterns and the integrations around them: Kafka ingress, scheduled work, CI/CD with packages and the Orion Console. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Patterns and integrations

The first page here is the shapes most workflows are built from. The rest are the integrations around a service. They cover how work arrives from a topic or a clock, how a change moves through a pipeline, and the browser view over it all.

- [Common workflow patterns](./workflow-patterns.md) states seven shapes, each with the problem it solves and the mistake it prevents: parse first, exclusive branches, in-process composition, one call per element, and more.
- [Consume from Kafka](./kafka-channels.md) runs a workflow for every record on a topic, with what "processed once" does and does not mean.
- [Run work on a schedule](./scheduled-workflows.md) binds a six-field schedule to a workflow, with misfire policies, non-overlapping runs, and a ledger of what ran.
- [CI/CD with packages](./ci-cd.md) is a three-stage pipeline: prove the logic offline, plan against the target with no writes, then apply the same versioned artifact.
- [Use the Orion Console](./console.md) is the operations console over the admin API: dashboards, the System Map, trace drill-downs and a Data Console.

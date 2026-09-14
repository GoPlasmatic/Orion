<!-- description: Complete one task at a time: author a service, extend Orion with plugins and models, author with an AI assistant, or apply a pattern or integration. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Guides

Each guide completes one task. It assumes you know the basics from the [quickstart](../get-started/quickstart.md) and the [concepts](../concepts/index.md), and it ends with the reference pages for what you used. Where a task has equivalent surfaces, they are tabs on one page.

The guides sit in four groups. [Author a service](./author/index.md) is the sequence a service is written in: the workflow, the channel, the connectors, the offline tests, and the versioned rollout. [Extend Orion](./extend/index.md) is for the two things a definition cannot express. A compiled codec ships as a plugin; a trained model is served from a bucket. [Author with AI](./ai/index.md) hands the authoring to an assistant. It holds the Claude Code session, the agent skill, a prompt pack for any LLM, and four worked examples. [Patterns and integrations](./patterns/index.md) holds the reusable workflow shapes and the integrations around them: Kafka, schedules, CI/CD and the Console.

| If you need to… | Start here |
|---|---|
| Write the logic | [Author a workflow](./author/workflows.md) |
| Expose it, and guard the door | [Configure a channel](./author/channels.md) |
| Reach a database or an API | [Connect a database or API](./author/connectors.md) |
| Prove it before it serves | [Test a workflow offline](./author/testing.md) |
| Ship a change safely | [Version and roll out changes](./author/versioning.md) |
| Add a function Orion does not have | [Build a plugin](./extend/plugins.md) |
| Run a trained model on the hot path | [Serve a model](./extend/models.md) |
| Let an assistant do the authoring | [Build a service with Claude Code](./ai/claude-code.md) |
| Consume a topic, or run on a clock | [Consume from Kafka](./patterns/kafka-channels.md), [Run work on a schedule](./patterns/scheduled-workflows.md) |
| Promote through a pipeline | [CI/CD with packages](./patterns/ci-cd.md) |

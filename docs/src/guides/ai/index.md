<!-- description: Author Orion services with an AI assistant: the Claude Code walkthrough, the agent skill, a prompt pack for any LLM and four worked examples. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Author with AI

Orion's deployment rules are what make delegating authoring to an assistant safe. Every definition can be dry-run offline, a change reaches a node only as a planned deployment, a deployed definition is immutable, and rollback is one call. These pages give an assistant the knowledge, and show what a session looks like.

- [Build a service with Claude Code](./claude-code.md) is a ten-minute session: install the skill, describe a service in a paragraph, and watch it written, tested, deployed and rolled back.
- [Set up the agent skill](./agent-skill.md) installs the skill for any agent that reads skills and can run a shell, and explains what it knows and what it deliberately leaves to the instance.
- [Prompt pack for any LLM](./prompt-pack.md) is the zero-install alternative: one block to paste into an assistant that has no shell and must use the REST API directly.
- [Worked examples: prompt to service](./worked-examples.md) shows four shipped services, each from the sentence that describes it to the JSON that CI deploys.

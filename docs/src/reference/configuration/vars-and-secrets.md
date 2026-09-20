<!-- description: The [vars] and [secrets] sections: the per-environment values a workflow reads by name, which is recorded in traces, and which value shapes each refuses. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# Vars and secrets

Two free-form sections holding the values that differ per environment. A definition is promoted between instances unchanged, so a topic prefix or a signing key cannot live inside it. It lives here, and the workflow reads it by name.

## Description

Which section a value belongs in is decided by one question: **should it appear in a trace?**

| Section | A workflow reads it as | Recorded in traces | Values must be |
|---|---|---|---|
| `[vars]` | `{"var": "metadata.vars.<name>"}` | **Yes**, deliberately | Literals |
| `[secrets]` | `{"secret": "<name>"}` | **No**, structurally | `env://` / `vault://` references |

Neither section has an environment-variable override: the names are the operator's own, so they do not fit the `ORION_SECTION__KEY` scheme. `${VAR}` in the value covers reading from the environment, and it runs on both. `${VAR:?message}` makes an input mandatory and says why when it is missing.

```toml
[vars]
kafka_topic_prefix = "${KAFKA_TOPIC_PREFIX:-dev}"
partner_base_url = "https://sandbox.partner.example"
max_retries = 3

[secrets]
partner_hmac = "env://PARTNER_HMAC_KEY"
```

**Vars** are stamped into every message's `metadata.vars` at ingress, HTTP and Kafka alike, overwriting whatever the caller sent. An envelope-mode request therefore cannot name its own topic prefix. They keep the type they were written as: `max_retries` compares against `3`, not `"3"`. And they *are* recorded, on purpose: an operator asking "which topic did this run publish to?" is asking to see them.

**Secrets** are held by the engine rather than by the message. That is what makes the guarantee structural instead of a policy. A secret is not part of a message, so it cannot reach a trace snapshot, a `map` mapping clone or a response body. There is nothing to strip. A workflow that reads one somewhere the engine would record the result is refused when the engine is built. So is one naming a secret the instance does not declare.

Each section refuses the other's value shape, because either mistake is silent:

- A **literal in `[secrets]`** is refused — a key written into a config file is a key in the deployment's file tree.
- A **reference in `[vars]`** is refused — nothing resolves one on its way into metadata, so the workflow would read the characters `env://PARTNER_HMAC_KEY` and send them to the partner.

A `[secrets]` reference that cannot be resolved stops the boot. That is the point of the syntax: the alternative is an instance that runs and fails at the remote system with nothing pointing back here.

See [Environment Variables](../environment-variables.md) for how these fit with the other ways a value reaches Orion, and [Expressions](../expressions.md#secrets) for the `secret` operator's rules.

## Related

- [Environment variables](../environment-variables.md): how these fit with `${VAR}`, `env://` and `var://`.
- [Expression language › Secrets](../expressions.md#secrets): the `secret` operator's rules.
- [Promote between environments](../../operate/maintain/promotion.md): why a definition carries no environment-specific value.
- [Server configuration](./index.md): every section, by what you are configuring.

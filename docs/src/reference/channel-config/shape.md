<!-- description: The request and response blocks of an Orion channel: request body mode and cookies, response shaping and error bodies, caching, timeouts and tracing. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Request and response

How the body becomes `data`, and how the reply is shaped and cached. Also the deadline and the per-channel trace policy.

| Page | Holds |
|---|---|
| [`request`](./request.md) | the auto and payload body modes, what each does to data and metadata, and the cookies a workflow may read. |
| [`response`](./response.md) | shaped status, headers, body and cookies from the workflow, and per-status error bodies for guard rejections. |
| [`cache`](./cache.md) | serving repeated identical sync requests from a stored response, the cache key, key_logic, TTL and the backing store. |
| [`timeout_ms`](./timeout_ms.md) | the workflow execution deadline, and how each ingress defaults or clamps it to a transport ceiling. |
| [`tracing`](./tracing.md) | a per-channel override of the trace persistence mode, sample rate and errors-only policy, plus task details. |

## Related

- [Channel configuration](./index.md): every key, with its page.
- [Guards by ingress](./guards-by-ingress.md): which guard runs where, and in what order.
- [Configure a channel](../../guides/author/channels.md): the same keys, as a walkthrough.

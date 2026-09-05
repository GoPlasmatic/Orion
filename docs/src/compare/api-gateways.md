<!-- description: Orion vs Kong, Envoy, APISIX and KrakenD: the gateway polices traffic, Orion terminates it. Which guards to keep on each side, and how they run together. -->
# Orion vs API Gateways

> **In one line.** A gateway polices traffic on its way to a service that
> handles it. Orion *is* that service — it terminates the request in a
> workflow. They are not alternatives, and in a fleet you will want both.

<div class="compare-meta">

**How it relates:** Pairs with Orion

**Where they overlap:** rate limiting, payload validation, deduplication, origin allow-lists

**Last reviewed:** 2026-08, against Kong 3.9 and Envoy 1.39

</div>

## Side by side

|  | API gateways | Orion |
|---|---|---|
| What it is | A proxy that routes and polices traffic to upstreams | The upstream — logic and endpoint in one runtime |
| Unit of work | A route, mapped to a backend | A [channel](../concepts/channels.md), bound to a workflow |
| How you write the logic | You don't; the logic is behind the gateway | JSON, posted to a running server |
| Where state lives | Not applicable — it forwards | Not applicable — it answers |
| How a change ships | Config reload, or a plugin deploy | One API call, hot-reloaded |
| Typical latency / cadence | Sub-millisecond added to a proxied hop | The whole response, in milliseconds |
| What it needs to run | The proxy, plus something behind it | [One binary](../getting-started/install.md) |

## What API gateways are good at

- **Fronting a whole fleet.** One place for TLS, DNS, routing and traffic
  policy across every service you run, whoever wrote them.
- **Identity.** JWT and OIDC verification, mTLS, OAuth flows, key management —
  the things Orion does not do.
- **Traffic control across upstreams.** Load balancing, health-based ejection,
  blue/green and weighted splits *between services*.
- **A plugin ecosystem.** Kong and Envoy have years of filters and plugins for
  problems you have not hit yet. KrakenD does it as declarative config.
- **Being the boundary.** WAF, IP policy, bot handling and DDoS controls belong
  at the edge, not in each service.

## What Orion does instead

- Terminates the request: [route resolution](../reference/data-api.md#route-resolution),
  guards, workflow, response — no upstream behind it.
- Polices its *own* ingress per channel:
  [rate limit, auth, origin, validation, dedup, cache and backpressure](../reference/channel-config.md),
  applied before any logic runs.
- Applies the same contract on every ingress, minus what the transport cannot
  carry — a Kafka channel gets the guards a Kafka record can support.

## Where they overlap

Rate limiting, payload validation, deduplication and origin allow-lists exist
on both sides, because an Orion channel polices what reaches it rather than
trusting whatever is in front.

In a fleet, keep both and let them do different jobs: the gateway enforces what
is true for *every* service — TLS, identity, IP policy, and the channel
enforces what is true for *this* service, like a per-tenant limit keyed by a
field in the payload. Orion is then an upstream that needs fewer of the
gateway's compensating features, not one that makes the gateway redundant.

## Choose an API gateway when

- You have services from more than one team, in more than one language.
- You need OIDC flows or mTLS, and you need them in one place. (JWT
  *verification* is built into Orion's channels; the IdP dance is not.)
- Identity, TLS and IP policy must be enforced before traffic reaches anything
  you wrote.
- You want traffic shifted between whole services, not between versions of one.

## Choose Orion when

- The question is what the service *does*, not how traffic reaches it.
- You want the limit keyed by something only the payload knows — a tenant id, a
  plan tier, which a gateway cannot see without parsing your schema.
- You want the endpoint to exist because you posted JSON, with no upstream to
  deploy first.

## Running both

The normal production shape: gateway at the edge, Orion behind it.

- Terminate TLS and verify identity at the gateway; let the channel's
  [`auth` block](../reference/channel-config.md) handle the service-specific
  check, or drop it entirely if the gateway is the only way in.
- Set a coarse platform limit at the gateway and a keyed per-channel limit in
  Orion.
- Forward `x-request-id`; Orion propagates it through logs, traces and the
  response.
- Configure [trusted proxies](../operate/security.md) so Orion reads the client
  address from the hop your proxy appended, not from a header a client can
  forge.

## What Orion cannot do here

- **No OIDC flows and no mTLS termination.** Channels verify `api_key`, HMAC
  signatures and JWT bearer tokens themselves; the IdP dance (discovery, PKCE,
  userinfo) and client certificates need a proxy in front. See
  [Secure an Instance](../operate/security.md).
- **No load balancing or health-based ejection across upstreams.** Orion is an
  upstream; something else spreads traffic across its replicas.
- **No WAF, bot management or DDoS controls.**
- **No plugin ecosystem.** Orion's [plugins](../concepts/plugins.md) are
  WebAssembly task functions that transform a message and nothing else —
  no request filters, no Lua, no hooks into the transport. It extends
  [by configuration, and by pure code in a sandbox](../concepts/how-orion-works.md#what-you-can-extend).
- **No ingress beyond REST, plain HTTP and Kafka.** No gRPC, no WebSockets, no
  streaming responses.

## Related

- [Is Orion Right for You?](../comparison.md): the chart, and the other neighbours.
- [Secure an Instance](../operate/security.md): admin auth, data-plane auth, TLS, trusted proxies.
- [Channel Configuration](../reference/channel-config.md): every guard a channel can declare.
- [Configure Channels](../build/channels.md): the same guards, as a walkthrough.

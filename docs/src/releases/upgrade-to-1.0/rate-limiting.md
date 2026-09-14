<!-- description: Orion 1.0 keys the rate limiter on the direct peer unless rate_limit.trusted_proxies names your proxy, on every channel that declares a limit. -->
<!-- type: migration -->
<!-- last_verified: 2026-09-14 -->

# Rate limiting behind a proxy

Break 1 of eleven in the 0.3.0 → 1.0.0 upgrade.

## Before you start

Read [Upgrade to 1.0.0](./index.md) first: it carries the checklist, the backup step and the `preflight` scan.

**This is the highest-impact change on the page.** It applies only when `rate_limit.enabled = true`, still `false` by default. Where it applies, it changes who shares a bucket.

**What changed.** The rate limiter's client identity used to be read straight from `X-Forwarded-For` (first element) or `X-Real-IP`. It fell back to the literal string `"unknown"` when neither was present. Any client could mint a fresh bucket by sending a made-up header. The identity is now the **TCP peer address**, and forwarded headers are honoured *only* when the peer address falls inside the new `rate_limit.trusted_proxies` list. That list is **empty by default**, which means "trust nothing". When the peer *is* trusted, the client is resolved from the **right end** of `X-Forwarded-For`, which is the hop your proxy appended. Hops that are themselves trusted proxies are skipped. The leftmost elements are whatever the client sent and are never used as the identity.

**How you'll notice.** If Orion sits behind a proxy, load balancer, ingress controller or service mesh, the TCP peer is always that hop. **Every client collapses into a single bucket**, and legitimate traffic starts getting `429`s far below the configured rate. Watch `orion_rate_limit_rejections_total` climb while real request volume is unchanged.

**What to do.** List the addresses your proxies connect from, as CIDR blocks or
bare IPs (IPv4 and IPv6 both accepted):

```toml
[rate_limit]
enabled = true
trusted_proxies = ["10.0.0.0/8", "192.168.1.1", "fd00::/8"]
```

Or by environment variable (comma-separated; it **replaces** the list, it does not append):

```bash
ORION_RATE_LIMIT__TRUSTED_PROXIES="10.0.0.0/8,fd00::/8"
```

On Kubernetes this is your pod or node CIDR; behind a cloud LB it is the LB's subnet, not the client's. Orion canonicalises IPv4-mapped IPv6 peers, so a server bound on `[::]` still matches an IPv4 CIDR.

Two things to know:

- **A malformed entry is a hard startup failure, even when
  `rate_limit.enabled = false`.** The message is
  `rate_limit.trusted_proxies: invalid entry '<x>': expected an IP address or CIDR block (e.g. "10.0.0.0/8")`.
  Run `orion-server validate-config` before you deploy.
- **Per-channel `rate_limit.key_logic` is affected too.** Any channel whose
  key expression references `{"var": "client_ip"}` now receives the peer
  address under the same rules.

> **It is no longer gated on `rate_limit.enabled`.** A channel's own
> `rate_limit` block is enforced on every ingress by the channel guards, keyed
> on the same trusted-proxy-gated client identity, whether or not the platform
> limiter is running, and the audit trail's `details.client_ip` and the
> failed-auth backoff read it too. If you deliberately left
> `[rate_limit] enabled = false` and rely on per-channel limits, you still need
> `trusted_proxies` set; otherwise every client behind the proxy keys on the
> proxy's own address and the whole fleet shares one bucket.

> **Not changed:** sticky rollout bucketing still reads forwarded headers
> directly and does not consult `trusted_proxies`. See
> [Sticky rollouts](./runtime-behaviour.md#sticky-canary-rollouts-are-now-caller-stable).

---

## Related

- [Upgrade to 1.0.0](./index.md): the checklist, and every other break.
- [Upgrades](../../operate/maintain/upgrades.md): the version-independent procedure.
- [`orion-server preflight`](../../reference/cli/orion-server/preflight.md): the scan that finds the stored ones.
- [Releases](./index.md): what changed in each version.

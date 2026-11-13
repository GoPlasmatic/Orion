# orion-extension-api

The extension surface of the [Orion](https://github.com/GoPlasmatic/Orion) runtime: what an
edition implements to add channel protocols, connector providers, connection-pool caches and a
package verifier to a node.

- `ChannelDriver`: a channel protocol (`rest`, `http`, `cron` in Community). A driver whose
  messages do not arrive over HTTP declares `ChannelDriverCapabilities::ingress` and returns a
  `RunningDriver` from `start`. The node reconciles it with the protocol's channels after every
  published generation, and it dispatches each message through `IngressHost::dispatch`. Dispatch
  applies the channel's guards, traces and runs the workflow, and answers a `DispatchOutcome`
  plus a `Settle` handle for the deduplication claim. See the `ingress` module.
- `ProviderBackend`: a connector provider. A `rust` backend compiled into an edition's
  binary needs no package: connections name it by a digest derived from its id (reported in
  `/runtime-capabilities`), and its `validate_connection` judges a revision's configuration at
  upload. A definition set names it with `"provider": "<driver id>"` (with an unstable `SourceConnectorFacet` hidden
  until the first source channel protocol ships), called with an `AttemptContext` and
  answering `Result<ConnectorOutput, ConnectorError>`.
- `ConnectionPoolCache`: a provider's pool cache, with per-connector and whole-cache
  eviction methods. Production lifecycle wiring is pending.
- `PackageVerifier`: whether a provider package may be admitted on the node.
- `Registrar`: the registration surface. `orion-server`'s `CompositionBuilder` implements it,
  so an edition's registration code depends on this crate alone; its binary hands the builder
  to `orion::run`.

The crate depends on `serde`, `serde_json`, `thiserror`, `async-trait` and `orion-api` (for
`Digest` and `FieldError`, which it shares with the wire). It ships in lockstep with
`orion-server`.

## Current limits and extension policy

Extend these APIs with their first consumer. If an edition needs a capability for
a release, include its implementation and validation in that release's work.

- `AttemptContext::connection_resolver` reads the authorized connection's public
  configuration and resolves its secret locators. Resolved secrets stay out of
  logs and traces (`SecretValue` redacts itself).
- Community's built-in digest lookup uses a static table. An edition's first
  compiled-in provider needs extensible digest registration across validation,
  deployment and reload.
- A channel binding carries a `ConnectionResolver` (and the revision's identity)
  for every Connection the channel's member requires. Unlike a connector call's
  attempt, the resolver does not re-check the node's revocation ledger; a driver
  holds it until its next reconcile.
- Offline `lint` and `compile` know an edition's protocol only through
  `--capabilities` (a node's `/runtime-capabilities`): its capabilities, guard
  row and `transport_schema`. A driver's own `validate_fields` and
  `validate_transport` run only on the node.
- Lifecycle: `ProviderBackend::dispose(identity)` is called once a published
  generation no longer pins that connection revision (removed, or replaced by a
  new revision); calls already admitted on the old generation may still hold
  what they acquired, so release idle resources only. `ProviderBackend::shutdown`
  is called once at node shutdown, after the channel drivers stop and before the
  trace queues drain. `ConnectionPoolCache::evict_all` still has no production
  caller.
- An edition binary builds its providers with the `orion` library's
  `extensions::kit` (`VersionedSessionPool`, the SSRF address checks, and
  `FoundationOptions` with the node's shared HTTP client, handed to a
  `CompositionBuilder::with_providers` setup), and reads node-wide settings from
  its own `[extensions.<id>]` config table (`AppConfig::extensions.section`).
- Community does not register a `source` protocol. A source provider, its driver
  lifecycle and runner wiring must ship together before Community accepts it.

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
  upload. A definition set names it with `"provider": "<driver id>"`. Its optional `describe`
  declares its operations (names, aliases, input/output schemas): undeclared operations are
  refused at workflow load and offline, and declared schemas are checked on every call. It is
  called with an `AttemptContext` and answers `Result<ConnectorOutput, ConnectorError>`.
- `ConnectionPoolCache`: a provider's pool cache, evicted per connector when a deployment
  changes that connection.
- `PackageVerifier`: whether a provider package may be admitted on the node.
- `Registrar`: the registration surface. `orion-server`'s `CompositionBuilder` implements it,
  so an edition's registration code depends on this crate alone. Its binary hands the builder
  to `orion::cli::main`, which gives it `orion-server`'s command line and operator subcommands,
  and its tests boot a real node over the same builder with `orion::testing::TestNode`.

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
  for every Connection the channel's member requires. Like a connector call's
  attempt, each resolution first consults the node's revocation ledger (the
  security lease, a revoked connection, provider or package), so a driver
  holding a resolver past a revocation is refused.
- Offline `lint` and `compile` know an edition's protocol only through
  `--capabilities` (a node's `/runtime-capabilities`): its capabilities, guard
  row and `transport_schema`. A driver's own `validate_fields` and
  `validate_transport` run only on the node.
- Lifecycle: `ProviderBackend::dispose(identity)` is called once a published
  generation no longer pins that connection revision (removed, or replaced by a
  new revision); calls already admitted on the old generation may still hold
  what they acquired, so release idle resources only. `ProviderBackend::shutdown`
  is called once at node shutdown, after the channel drivers stop and before the
  trace queues drain.
- An edition binary builds its providers with the `orion` library's
  `extensions::kit` (`VersionedSessionPool`, the SSRF address checks, and
  `FoundationOptions` with the node's shared HTTP client, handed to a
  `CompositionBuilder::with_providers` setup), and reads node-wide settings from
  its own `[extensions.<id>]` config table (`AppConfig::extensions.section`).
- There is no polling-source hook. A protocol that consumes from a queue or log
  is a `ChannelDriver` that runs its own listener (`start`) and dispatches each
  message through `IngressHost::dispatch`.

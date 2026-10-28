# orion-extension-api

The extension surface of the [Orion](https://github.com/GoPlasmatic/Orion) runtime: what an
edition implements to add channel protocols, connector providers, connection-pool caches and a
package verifier to a node.

- `ChannelDriver`: a channel protocol (`rest`, `http`, `cron` in Community).
- `ProviderBackend` and `SourceConnectorFacet`: a connector provider and its optional source
  side, called with an `AttemptContext` and answering `Result<ConnectorOutput, ConnectorError>`.
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

- `AttemptContext` identifies the authorized connection and revision, but has no
  public resolver for its resolved configuration and secrets. Add a node-owned
  resolver scoped to that connection when a provider first needs it; exclude
  resolved secrets from logs and traces.
- Community's built-in digest lookup uses a static table. An edition's first
  compiled-in provider needs extensible digest registration across validation,
  deployment and reload.
- `ChannelDriver` validates fields and declares capabilities. It has no runner
  start/stop hooks or way to supply a transport's own guard set. Its shared fields
  still include Kafka's `topic` and `consumer_group`. Build those extensions with
  a working transport, covering cancellation and deployment replacement.
- The runtime's pool registry exposes eviction methods without production
  callers. Check revision and secret identity before adding eviction for rotation
  or retirement; define how admitted runs drain their old resources.
- Community does not register a `source` protocol. A source provider, its driver
  lifecycle and runner wiring must ship together before Community accepts it.

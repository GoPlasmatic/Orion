# orion-extension-api

The extension surface of the [Orion](https://github.com/GoPlasmatic/Orion) runtime: what an
edition implements to add channel protocols, connector providers, connection-pool caches and a
package verifier to a node.

- `ChannelDriver`: a channel protocol (`rest`, `http`, `cron` in Community).
- `ProviderBackend` and `SourceConnectorFacet`: a connector provider and its optional source
  side, called with an `AttemptContext` and answering `Result<ConnectorOutput, ConnectorError>`.
- `ConnectionPoolCache`: a provider's pool cache, evicted per connector and on cluster resync.
- `PackageVerifier`: whether a provider package may be admitted on the node.
- `Registrar`: the registration surface. `orion-server`'s `CompositionBuilder` implements it,
  so an edition's registration code depends on this crate alone; its binary hands the builder
  to `orion::run`.

The crate depends on `serde`, `serde_json`, `thiserror`, `async-trait` and `orion-api` (for
`Digest` and `FieldError`, which it shares with the wire). It ships in lockstep with
`orion-server`.

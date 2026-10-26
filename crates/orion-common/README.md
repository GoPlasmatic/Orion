# orion-common

The shared primitives of the [Orion](https://github.com/GoPlasmatic/Orion)
runtime: the configuration model, the `OrionError` type, crypto helpers,
Prometheus metrics, request and trace context, canonical JSON and the bounded
egress body reader.

This crate is an implementation detail of `orion-server`. It has no release
cycle of its own and nothing in it is covered by semver.

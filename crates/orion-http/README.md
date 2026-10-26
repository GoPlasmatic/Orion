# orion-http

The HTTP layer of the [Orion](https://github.com/GoPlasmatic/Orion) runtime:
the axum router, the admin (`/api/v2/admin`) and data (`/api/v2/data`)
routes, the middleware stack, TLS serving and the OpenAPI document, over the
node `orion-runtime` assembles.

This crate is an implementation detail of `orion-server`. It has no release
cycle of its own and nothing in it is covered by semver.

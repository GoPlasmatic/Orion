# orion-engine

The workflow engine of the [Orion](https://github.com/GoPlasmatic/Orion)
runtime: Orion's `dataflow-rs` functions (`http_call`, `db_read`/`db_write`,
`data_query`/`data_write`, the cache family, `crypto`, the JWT pair,
`channel_call`, `connector_call`) and JSONLogic operators, the loader that
compiles stored channels and workflows, the connector registry and pools, the
portable data dialect, and the JWT core.

The three functions that reach back into a serving node (`channel_call`,
`connector_call`, `cache_invalidate`) dispatch through host traits the
runtime implements; nothing here names the channel estate or the deployment.

This crate is an implementation detail of `orion-server`. It has no release
cycle of its own and nothing in it is covered by semver.

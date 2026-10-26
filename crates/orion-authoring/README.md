# orion-authoring

The authoring layer of the [Orion](https://github.com/GoPlasmatic/Orion)
runtime: definition sets on disk (the cross-reference check, the compile
passes that turn source form into canonical definitions, `fmt`, `clippy` and
the analysis they read) and the checks every definition passes before a node
accepts it, shared by the offline tools and the admin API.

This crate is an implementation detail of `orion-server`. It has no release
cycle of its own and nothing in it is covered by semver.

# orion-runtime

The serving node of the [Orion](https://github.com/GoPlasmatic/Orion)
runtime: the channel estate and its ingress guards, the published runtime
generation and the deployment builds that produce it, the cron scheduler, the
trace and audit queues, cluster coordination, and the v2 extension boundary
(composition, providers, the immutable deployment store).

This crate is an implementation detail of `orion-server`. It has no release
cycle of its own and nothing in it is covered by semver.

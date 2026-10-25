<!-- description: Extend the runtime with your own code: ship a codec or calculation as a sandboxed WebAssembly plugin that a task calls like a built-in function. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Extend Orion

A definition can express most services. One thing it cannot is a compiled transformation that already exists as code. That has one extension point, and it only computes; anything with I/O stays a connector or a service of your own.

- [Build a plugin](./plugins.md) takes a pure JSON-to-JSON function through the SDK, the manifest, the build, the offline test and the deployment.

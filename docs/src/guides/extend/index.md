<!-- description: Extend the runtime with your own code: ship a codec or calculation as a sandboxed WebAssembly plugin, or serve an ONNX model behind model_infer. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Extend Orion

A definition can express most services. Two things it cannot: a compiled transformation that already exists as code, and a trained model. Each has one extension point, and both only compute; anything with I/O stays a connector or a service of your own.

- [Build a plugin](./plugins.md) takes a pure JSON-to-JSON function through the SDK, the manifest, the build, the offline test, the upload and the promotion.
- [Serve a model](./models.md) takes an ONNX graph through the manifest, the bucket, the registration and admission, the activation, and the `model_infer` call.

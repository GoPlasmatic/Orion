<!-- description: orion-server model digests, signs and verifies model artifacts with the Ed25519 keys [models.trust] checks — the plugin verbs, for models. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `orion-server model`

Digests, signs and verifies model artifacts: what a node with [`[models.trust]`](../../configuration/models.md) checks when it admits a model. The verbs, flags, file format and exit codes are those of [`orion-server plugin`](./plugin.md). This page lists only what differs.

## Synopsis

```bash
orion-server model digest <path>
orion-server model keygen -o <file> [--force]
orion-server model pubkey --key <file>
orion-server model sign <path> [--key <file>] [-o <file|dir>] [--by-id]
orion-server [-c <config.toml>] model verify <path> [--public-key <base64>]... [--signature <file> | --signatures <dir>]
```

## Description

`<path>` is a model manifest, a directory searched for model manifests, or any file. A manifest names its artifact in `artifact`, and the artifact's digest is what is signed, never the manifest's. A manifest whose artifact is not on disk is an error. A plugin manifest is refused with a pointer to `orion-server plugin`.

`verify` without `--public-key` reads `public_keys` under `[models.trust]` of the `-c` config. `--by-id` names each file `<model id>.sig`, which a directory of signatures usually needs, because most model files are called `model.onnx`.

## Examples

```bash
orion-server model sign models/fraud/model.json --key signer.pem
orion-server model verify models/ --public-key "$MODEL_TRUST_KEY"
```

## Related

- [`orion-server plugin`](./plugin.md): every verb and flag in full.
- [Server configuration › Models](../../configuration/models.md): `[models.trust]` and `public_keys`.
- [Models](../../../concepts/models.md): what admission checks before a version may serve.
- [`orion-cli models`](../orion-cli/models.md): registering a model with its signature.

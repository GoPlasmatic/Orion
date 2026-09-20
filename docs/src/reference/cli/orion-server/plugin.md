<!-- description: orion-server plugin digests, signs and verifies plugin components with the Ed25519 keys [plugins.trust] checks, with no OpenSSL pipeline. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `orion-server plugin`

Digests, signs and verifies plugin components: what a node with [`[plugins.trust]`](../../configuration/plugins.md) checks at upload and at load. The digest, the signature and the check are the server's own, so what these verbs produce is exactly what a trusting node verifies.

## Synopsis

```bash
orion-server plugin digest <path>
orion-server plugin keygen -o <file> [--force]
orion-server plugin pubkey --key <file>
orion-server plugin sign <path> [--key <file>] [-o <file|dir>] [--by-id]
orion-server [-c <config.toml>] plugin verify <path> [--public-key <base64>]... [--signature <file> | --signatures <dir>]
```

## Description

A signature is an Ed25519 signature over the ASCII digest string `sha256:<64 hex>`, not over the component's bytes, written as one line of standard base64. `<path>` names what is signed:

| `<path>` is | What is signed |
|---|---|
| A `plugin.toml` | The component the manifest's `component` names, beside it |
| A directory | The component of every `plugin.toml` under it |
| Any other file | The file itself |

A manifest whose component is not on disk is an error, and so is a directory with no manifest in it. Signing nothing never reports success. A model manifest is refused with a pointer to [`orion-server model`](./model.md), which takes the same verbs.

| Verb | What it does |
|---|---|
| `digest` | Prints the digest a signature is made over. For one target it prints the bare digest, so `$(orion-server plugin digest plugins/scoring)` works. For several it prints `digest  id  file` per line. |
| `keygen` | Writes a new private key as an unencrypted PKCS#8 PEM, readable only by its owner, and prints its `public_keys` value. The key is the form `openssl genpkey -algorithm ed25519` writes, so either tool reads the other's. An existing file is kept unless `--force` is given. |
| `pubkey` | Prints the `public_keys` value for an existing private key, including one OpenSSL generated. |
| `sign` | Signs each target and writes `<component>.sig` beside the component. The signature is checked against the key's own public half before it is written. |
| `verify` | Checks each target's signature against the trusted keys and prints `ok  <id>  <digest>` for each one that passes. |

`sign` reads the key from `--key`, or else the PEM text in the `ORION_SIGNING_KEY` environment variable. A CI secret store hands out variables, not files. With neither it stops, naming both.

`verify` needs keys to check against. They come from `--public-key`, or else from the `[plugins.trust]` `public_keys` of the `-c` config. No key from either is an error, not an `ok`, because a node with no keys checks nothing.

## Options

| Flag | Used by | Description |
|------|---------|-------------|
| `-o, --output <file>` | `keygen` | Where to write the private key. `-` writes it to stdout and the public key to stderr. |
| `--force` | `keygen` | Replace an existing key file. |
| `--key <file>` | `pubkey`, `sign` | A PEM private key. |
| `-o, --output <file\|dir>` | `sign` | A file, for one target, or a directory. A directory is created if needed and receives flat `<component>.sig` names, the layout a directory of signatures is read in. Several targets always write into a directory. |
| `--by-id` | `sign` | Name each file `<plugin id>.sig` rather than after the component. Needed when two plugins' components share a file name. |
| `--public-key <base64>` | `verify` | A trusted public key. Repeatable. |
| `--signature <file>` | `verify` | The signature file, for a single target. |
| `--signatures <dir>` | `verify` | A directory of `.sig` files, looked up by `<plugin id>.sig` and then by `<component>.sig`. Without this or `--signature`, the `.sig` beside each component is read the same way. |

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Every target was digested, signed or verified. |
| `1` | A signature did not verify or was missing, a manifest or key could not be read, or no key was given. |

## Examples

Create a key, then configure every node that should trust it with the printed value:

```console
$ orion-server plugin keygen -o signer.pem
wrote signer.pem
public_keys value: YpbiNmxkTrZbCBochFhVIDEKlXbVrHwdtUcw93+slbM=
```

Sign every plugin in a tree, check the result against a node's own config, and upload one:

```bash
orion-server plugin sign plugins/ --key signer.pem
orion-server -c config.toml plugin verify plugins/
orion-cli plugins create -f plugins/scoring/plugin.toml --signature plugins/scoring/scoring.wasm.sig
```

Write the signatures into one directory instead of beside each component:

```bash
orion-server plugin sign plugins/ --key signer.pem -o sigs/
```

The same message and encodings are what the OpenSSL pipeline on the [manifest page](../../plugin-manifest.md#trust) produces, so keys and `.sig` files move freely between the two.

## Related

- [Plugin manifest](../../plugin-manifest.md): the `component` field and the signature rules.
- [Server configuration › Plugins](../../configuration/plugins.md): `[plugins.trust]` and `public_keys`.
- [`orion-server model`](./model.md): the same verbs for model artifacts.
- [`orion-cli plugins`](../orion-cli/plugins.md): uploading a component with its `.sig`.

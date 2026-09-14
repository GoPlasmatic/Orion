<!-- description: The [models] settings: enabling ONNX models, the artifact cache, admission limits, inference ceilings, trust keys, runtimes, devices and overrides. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Model settings

ONNX models as governed entities. A model row names a storage connector, an object key and a digest. The node fetches the bytes, verifies them, caches them in `cache_dir`, and runs them through a runtime.

## Synopsis

```toml
[models]
enabled = false
cache_dir = ""
max_cache_bytes = 8589934592
max_loaded_bytes = 2147483648
preload = "referenced"
max_artifact_bytes = 536870912
max_parameters = 0
max_input_elements = 1048576
max_output_elements = 1048576
max_timeout_ms = 1000
max_probe_ms = 250
fetch_timeout_secs = 300
admission_timeout_secs = 900
max_concurrency_per_model = 16
max_concurrent_inferences = 0
overrides = []

[models.trust]
public_keys = []

[models.default_runtime]
onnx = "tract"

[models.runtimes.tract]
enabled = true
device = "cpu"
```

## Description

Models are off by default, and turning them on changes nothing until a model is uploaded and activated. Every limit here is the host's: a model requests nothing, and a per-model override may only lower a ceiling, never raise one.

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `models.enabled` | `false` | `ORION_MODELS__ENABLED` | Turn on to admit and run models on this node. A stored model on a node with models off quarantines the workflows naming it rather than aborting. |
| `models.cache_dir` | `""` | `ORION_MODELS__CACHE_DIR` | Where verified artifacts are kept, one file per digest. Required when `enabled`; refused empty. |
| `models.max_cache_bytes` | `8589934592` | `ORION_MODELS__MAX_CACHE_BYTES` | Ceiling on the cache directory (8 GiB). Swept least recently used after every fetch. |
| `models.max_loaded_bytes` | `2147483648` | `ORION_MODELS__MAX_LOADED_BYTES` | Ceiling on models resident in memory at once (2 GiB), across every runtime. A load that would cross it fails as a limit rather than evicting a model in use. |
| `models.preload` | `"referenced"` | `ORION_MODELS__PRELOAD` | Which admitted models a generation loads before it serves: `none` (the first inference pays the load), `referenced` (every model an active workflow names) or `all`. |
| `models.max_artifact_bytes` | `536870912` | `ORION_MODELS__MAX_ARTIFACT_BYTES` | Largest artifact an admission will fetch (512 MiB). Checked against the declared length before a byte is read, and again while the body streams. |
| `models.max_parameters` | `0` | `ORION_MODELS__MAX_PARAMETERS` | Ceiling on a model's parameter count, read from the graph at admission. `0` leaves it unbounded. |
| `models.max_input_elements` | `1048576` | `ORION_MODELS__MAX_INPUT_ELEMENTS` | Elements one inference may hand a model, summed over its inputs. |
| `models.max_output_elements` | `1048576` | `ORION_MODELS__MAX_OUTPUT_ELEMENTS` | Elements one inference may take back, summed over its outputs. |
| `models.max_timeout_ms` | `1000` | `ORION_MODELS__MAX_TIMEOUT_MS` | Wall-clock ceiling per inference; the task's own deadline applies too and the shorter wins. |
| `models.max_probe_ms` | `250` | `ORION_MODELS__MAX_PROBE_MS` | Ceiling on the admission probe — the median of five inferences at rest, over zero-filled inputs, that prove the graph runs here. A model slower than this idle could never meet `max_timeout_ms` under load, so it is refused. |
| `models.fetch_timeout_secs` | `300` | `ORION_MODELS__FETCH_TIMEOUT_SECS` | How long one artifact fetch may take, connection to last byte. |
| `models.admission_timeout_secs` | `900` | `ORION_MODELS__ADMISSION_TIMEOUT_SECS` | How long one admission may take end to end. A row still pending after this is marked failed with the stage it was in. |
| `models.max_concurrency_per_model` | `16` | `ORION_MODELS__MAX_CONCURRENCY_PER_MODEL` | Inferences of one model that may run at once; beyond it a task waits until its deadline and fails as a limit. |
| `models.max_concurrent_inferences` | `0` | `ORION_MODELS__MAX_CONCURRENT_INFERENCES` | Inferences that may run at once across every model. `0` means the host's available parallelism. |
| `models.trust.public_keys` | `[]` | `ORION_MODELS__TRUST__PUBLIC_KEYS` | When set, a model row must carry an Ed25519 signature over its artifact digest by one of these keys, verified by the node that admits it. |
| `models.default_runtime.onnx` | `"tract"` | — | The runtime a model whose manifest declares `format = "onnx"` runs on. One row per format; see [Runtimes and overrides](#runtimes-and-overrides). The `onnx` row is required: it is the only format a manifest can declare today. |
| `models.runtimes.tract.enabled` | `true` | — | Whether models may run on tract here. A disabled runtime keeps its entry so a row naming it is refused by name. |
| `models.runtimes.tract.device` | `"cpu"` | — | The device tract executes on: `cpu`, `metal` or `cuda`. A device the build lacks fails the load with the reason, not the config. Leave it at `cpu` unless you have measured otherwise — see [Devices](./models.md#devices). |
| `models.overrides` | `[]` | — | Per-model ceilings; see [Runtimes and overrides](#runtimes-and-overrides). |

```toml
[models]
enabled = true
cache_dir = "/var/cache/orion/models"

[models.default_runtime]
onnx = "tract"

[models.runtimes.tract]
device = "metal"

[[models.overrides]]
id = "ada.c4-tiny"
timeout_ms = 50
max_concurrency = 4
```

### Runtimes and overrides

`[models.runtimes]` is a map keyed by runtime name. The names a build knows are the ones `orion-server validate-config` accepts, and the only one today is `tract`. `[models.default_runtime]` is a map keyed by **artifact format**, the `format` a manifest declares. It names the runtime a model of that format runs on when nothing names one explicitly. Every value must be a key of `models.runtimes`, enabled there, and a runtime that serves the format; tract serves `onnx`. A key must be a format some runtime this build knows serves. The `onnx` row is required, because it is the only format a manifest can declare today. Declaring the table without it is refused at startup, as is the pre-table spelling `default_runtime = "tract"`. Adding a format or a
runtime later is a row here, not a schema change. Each `[[models.overrides]]`
block names a model `id` and any of `timeout_ms`, `max_concurrency`,
`max_input_elements` and `max_output_elements`. A value above the host
ceiling, a zero, or an `id` repeated across blocks is refused at startup.

### Devices

`cpu` is the default and, for the models Orion is built to serve, almost
always the right answer. What a build offers is narrower than the three names the config accepts. `metal` needs an Apple target with a Metal device; `cuda` needs a tract compiled with it and a toolkit present. A device the config names but the build lacks is not a startup error. It fails the load with stage `device`, naming what this build does offer, so a fleet of mixed hardware runs one config.

Two things to know before moving off `cpu`.

**An accelerator is slower for a small graph.** Dispatch overhead is a
per-call constant that does not shrink with the model, so it dominates
exactly the workload [Models](../../concepts/models.md) is for. Measured on an
M-series host against the 1479-parameter `c4-tiny` fixture, release build:

| Device | Load | Inference (median) |
|---|---:|---:|
| `cpu` | 7.5 ms | 0.0055 ms |
| `metal` | 794 ms | 0.20 ms |

Metal is 36× slower per inference here and 105× slower to load. The load is charged to a cold call's deadline, and `models.max_timeout_ms` defaults to 1000 ms. On an accelerator a cold call is therefore close to that ceiling on its own. `models.preload` (`referenced` by default) then does more work for you than it does on `cpu`. A graph large enough to repay the dispatch is worth checking against [what a model is for](../../concepts/models.md#what-a-model-is-for) before it reaches the hot path. To measure, register the model and read `stats.probe_ms` from `GET /models/{id}` on a node configured each way.

**An accelerator does not reproduce the CPU path bit for bit.** It computes the same thing in a different order, and in `f32` a different order is a different last digit. Against the same fixture the largest element-wise difference between `metal` and `cpu` is ~9.3e-8. That is nothing for a score
or a class boundary, and everything for a fleet that must agree exactly —
see [what the graph guarantees](../../concepts/models.md#the-model). Keep a
scored or audited deployment on one device.

## Related

- [Models](../../concepts/models.md): what a model is, and what admission checks.
- [Serve a model](../../guides/extend/models.md): registering and calling one.
- [`model_infer`](../functions/model_infer.md): the task function these ceilings bound.
- [Server configuration](./index.md): every section, by what you are configuring.
